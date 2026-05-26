# Spike: Auto-learning loop — closing the inputs → behavior gap

Status: design proposal (medium confidence)
Issue: [#123](https://github.com/nolanmak/MyAgentAssistant/issues/123)
Related: #121 (skip capture), #122 (sender-type gate), #124 (closed-loop framing)

## Problem

The user's framing is the entire spec: **"learning is changed behavior."** Today the agent collects operator signal (revisions, skips, approves) and the prompt layer has *readers* for "learned" content, but nothing in the system actually *writes* those learned files. The next draft for sender X reads the same as the last one the operator rejected. So no learning has happened — only logging.

This spike answers: what should the writer look like, what should it write, on what cadence, and how do we keep it from going off the rails?

## Current state — inventory

### Inputs we already capture

| Signal | Where it lives | File:line |
|---|---|---|
| Draft revision triples `(original, feedback, revised)` | `draft_revisions` (SQLite) | `crates/augmentagent-store/src/store.rs:534-556` |
| Approve transitions (implicit positive signal) | `actions.status = Sent` | `crates/augmentagent-cli/src/main.rs:5171-5180` |
| Skip transitions (currently lost) | `actions.status = Rejected`; no separate row | `crates/augmentagent-cli/src/main.rs:5227-5236` |
| Triage decisions | `actions` rows (`status`, `reason`) | `crates/augmentagent-channel-email/src/channel.rs:376-444` |
| Tone examples (verbatim sent replies) | `tone_examples` | `crates/augmentagent-store/src/store.rs:~510` |

Skip capture is the gap addressed by atom #121 — once it lands, the negative-signal pipeline is complete at the *input* layer.

### Consumers — where learned content is read

| Consumer | What it reads | File:line |
|---|---|---|
| Triage prompt | `learned/*.json` aggregated into a "Learned Patterns" block | `crates/augmentagent-channel-core/src/prompt.rs:52-92` (`SkillPrompt::load_learned`) |
| Triage user message | learned block + wiki hint | `crates/augmentagent-channel-core/src/prompt.rs:98-112` (`triage_user_message`) |
| Draft prompt — tone | `tone_profile` block (cache-stable prefix position) | `crates/augmentagent-channel-core/src/prompt.rs:195-274` (`draft_user_message`) |
| Code-mode user message | mirrors classic draft — same tone/thread/archetype/resolved-asks blocks | `crates/augmentagent-channel-core/src/prompt.rs:332-399` |

### The gap

Nothing in the Rust workspace writes `learned/*.json`. `SKILL.md` documents a `notify({action: "learn_pattern"})` hook that does not exist. The `tone_profile` block is plumbed but the tone descriptor itself has to be constructed from somewhere — today it is either empty or hand-edited. End-to-end:

```
[skip / revise / approve] → SQLite rows → ??? → learned/*.json / tone_profile → next prompt
                                          ^^^
                                          this is what 123 is about
```

## Proposed architecture

A **periodic synthesis writer** is the right shape. Direct rule extraction is too brittle for tone work; few-shot accumulation blows up the prompt cache and doesn't update the *prefix*. Per-recipient model fine-tunes are too heavy. Periodic synthesis — a separate scheduled job that reads recent signals, asks Claude to summarize them into structured rules, and writes those rules to disk — gives us the right balance of cost, auditability, and revertibility.

**Scope.** Two writer surfaces, each owning a different file shape:

1. **Per-sender behavior overlay** — a JSON file per sender (or per `(sender, surface)`) at `skills/email-triage/learned/by-sender/<email>.json` holding a compact profile: `{tone_hints, format_hints, triage_overrides, evidence}`. Read at draft time and merged into the `tone_profile` block (stable cache prefix preserved). Read at triage time as an explicit per-sender bias on `reply/skip/flag`.
2. **Global learned-patterns** — the existing `learned/*.json` shape (already read by `SkillPrompt::load_learned`). Holds patterns that are *not* sender-specific: "drop the 'Hope you are well' opener," "shorten anything over 3 paragraphs by default," "newsletters from `*-marketing@*` always skip."

**Cadence and trigger.** End-of-cycle, not per-event. After each Gmail poll cycle (or on a separate timer every ~6h), a synthesis job:

1. Pulls signals from the last window: revisions, new skips, approves, since `last_synthesis_at`.
2. Groups by sender (and by `(domain, surface)` for the global side).
3. For each group with `>= K` new signals (K small, e.g. 3) **AND** at least one signal that conflicts with the current overlay, calls Claude with a `synthesize_overlay` prompt: "here are the operator's recent edits to drafts for sender X. Update the overlay JSON." Returns a small, well-typed JSON delta.
4. Applies the delta to disk; bumps `last_synthesis_at`; emits an audit row.

This makes "learning" inspectable, batched, and bounded — one Claude call per active sender per cycle, not per email.

**Sender scoping.** Per-sender by default. A revision to `friend@gmail.com` should not change drafts to `boss@work.com`. Domain-level rollups are for fallback only (when a sender has no individual overlay, the domain overlay applies). Global rules are reserved for things like "kill the standard greeting" — they require explicit operator approval before activating, via a Discord audit card. <!-- pii-ok: synthetic example addresses -->


**Eval-before-flip.** Every overlay change runs a tiny replay step before being committed live: take the last N revisions for that sender, re-render the draft prompt with the *new* overlay, diff against the *old* draft, and surface the diff to the operator in the audit card. If the operator approves, the overlay flips to active; if rejected, it's discarded. This pairs with the A/B replay harness from #165 — same primitive.

## Risks and failure modes

- **Overfitting to a noisy day.** Operator skips three emails because they were busy, not because the agent was wrong. Mitigation: require both signal count *and* signal agreement (e.g. ≥3 revisions touching the same paragraph, not 3 unrelated skips). Decay older signals.
- **Drift accumulation.** Overlays grow without bound. Mitigation: cap overlay size; every N synthesis cycles, ask Claude to *compress* the overlay (drop low-evidence rules).
- **Operator fatigue from audit cards.** If every micro-rule needs approval, the operator rubber-stamps everything. Mitigation: tier the approvals — per-sender tone tweaks can auto-flip; cross-sender global rules and blocklist promotions need explicit approval.
- **Conflicting signals.** Last week's revision said "warmer," this week's skip says "this warm draft is wrong for this sender." Mitigation: keep the evidence trail in the overlay so synthesis can resolve conflicts in context. The newer signal wins by default but the prior signal is not dropped — it stays as `evidence` and can be re-considered later.
- **Cache-prefix instability.** If the tone block content changes per draft, the prompt cache evicts. Mitigation: hash the overlay content into the rendered tone block; only invalidate when the overlay actually changed. The byte-stable-empty-blocks contract in `prompt.rs` already accommodates this.

## Recommendation

Ship in this order:

1. **#121 (skip capture)** — already in flight. Without skips persisted there is no negative-signal stream.
2. **#122 (sender-type gate)** — also in flight. Stops newsletter/bot noise from polluting the per-sender overlay before learning even starts.
3. **MVP synthesis writer (per-sender tone overlay)** — single sender, single surface (draft tone). End-of-cycle job, audit card on every flip, no global rules yet. This is the cheapest, most local-impact slice that proves "learning is changed behavior" in the user-visible sense: a draft that previously said "Hope you are well, Boss," now says "Hi Boss," because the operator deleted that opener three times last week.
4. **Eval harness + replay** (depends on #165) — without this, every overlay flip is a leap of faith.
5. **Global rules + auto-blocklist promotion** — only after MVP has shipped and the audit-card UX has been pressure-tested.

Each step is a separate PR; do not bundle.

## Follow-up issues to file

Once this design is accepted, file the following as concrete next-step tickets. Numbers are placeholders — file them at the canonical repo (`nolanmak/MyAgentAssistant`).

1. **Implement skip → learned-overlay writer (per-sender, end-of-cycle).** Owner: channel-email. Acceptance: after N skips of sender X with consistent reason, a `learned/by-sender/<x>.json` appears with a `triage_overrides` entry; next triage call sees it.
2. **Build per-sender tone-overlay synthesizer (revision-driven).** Owner: channel-core. Acceptance: after 3 revisions to drafts for sender X, an overlay JSON exists with tone hints; next draft for X reflects them; cache prefix stays stable.
3. **Wire eval-before-flip replay step into the synthesis path.** Owner: channel-core. Depends on #165 (A/B harness). Acceptance: every synthesis call produces a diff card; operator can approve or reject; only approved overlays go live.
4. **Audit-card UX in Discord for overlay flips.** Owner: approval-discord. Acceptance: card shows old vs. new overlay, "this rule was derived from skip events A, B, C," Approve / Revert buttons.
5. **Overlay decay + compression cycle.** Owner: channel-core. Acceptance: overlays older than N cycles get re-synthesized and shrunk; low-evidence rules drop out. Prevents unbounded growth and stale-rule drift.

## Out of scope (for now)

- Local fine-tuning / LoRA. The overlay approach gives 80% of the value at 1% of the operational cost.
- DSPy / OpenPipe framework adoption. Worth re-evaluating once the MVP has data, not before.
- Cross-channel learning (Discord DM signal → email tone). The architecture allows it but the first version is single-channel (email).
