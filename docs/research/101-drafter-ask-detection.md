# Drafter: structured ask-detection + auto-fill (calendar slots, doc links, intros)

Tracking issue: [#101](https://github.com/nolanmak/MyAgentAssistant/issues/101)
(migrated from `nolanmak/AugmentAgent#35`).

## Problem

Inbound emails frequently contain a **structured ask** the drafter used to
leave as a placeholder or punt on:

- "What's a good time next week?" → draft says "How about Tuesday at 2?" with
  no calendar check.
- "Can you send me the deck?" → draft promises "I'll send shortly" but never
  attaches.
- "Got a Calendly?" → draft writes `here's my Calendly: [link]` with literal
  placeholder text.
- "Can you intro me to X?" → draft commits without checking if X is in the
  wiki.

The drafter is a one-shot text completion with no tool-use loop, so it
cannot resolve any of these inline. The wiki-ask agent at
`schema/wiki-ask.md` proved we can give Claude tool calls, so the spike's
goal was to design the equivalent pattern for the drafter: a deterministic
**ask-extraction stage** that runs between Triage and Draft, hands resolved
values to the draft prompt as a `<resolved_asks>` block, and surfaces
unresolvable asks on the approval card.

## Findings

The architecture sketched in the issue is **implemented**, end-to-end, in
the current codebase. Phases 1 through Phase 3/5 are landed; only Phase 4's
intro-resolver behavior was intentionally kept advisory (matches the issue's
"strong vote" against auto-execute intros).

Concrete current state (`origin/main`):

- **`crates/augmentagent-channel-core/src/resolve.rs`** — 2041 lines, the
  full ask-extraction + resolver module. Doc-comment at the top spells out
  the gating model:
  - `AUGMENTAGENT_ASK_RESOLVE` = `off` (default) / `shadow` / `live`
  - Per-resolver flags `AUGMENTAGENT_ASK_RESOLVE_SCHEDULING`,
    `_CALENDLY`, `_MEETING_LINK`, `_SHARE_DOC`, `_INTRO`
  - `AUGMENTAGENT_ASK_RESOLVE_MIN_CONFIDENCE` (default 0.7, matches the
    issue's suggested threshold).
- **Detect.** Cheap structured-output extractor; matches issue Step 1's
  JSON shape (scheduling, share_doc, intro, calendly etc.). Confidence
  floor enforced at `INJECT_CONFIDENCE_FLOOR`.
- **Resolve.** Four deterministic resolvers in `resolve.rs`:
  scheduling (Calendar `freebusy.query`), calendly URL lookup,
  meeting-link, share-doc (Drive search), intro (advisory only).
- **Inject.** `<resolved_asks>` block is rendered via
  `resolved_asks_block()` and threaded through `prompt.rs`'s
  `draft_user_message` (see `crates/augmentagent-channel-core/src/prompt.rs:196`
  signature `resolved_asks_block: &str` parameter). The drafter system text
  in `prompt.rs:249` explicitly forbids placeholders when a concrete value
  is provided ("NEVER write a placeholder like `[link]`, `[time]`,
  `my Calendly`, or `I'll send it shortly` when a concrete value is given
  here").
- **Telemetry.** `detected_asks` SQLite table created in
  `crates/augmentagent-store/src/store.rs:1100`; insert helper at line
  116; `detected_asks_since()` query at line 136. This is Phase 1's
  shadow-mode logging surface.
- **"Needs your input" card field.** Sentinel-fenced payload in
  `crates/augmentagent-approval-discord/src/layout.rs:19` (referenced as
  "needs your input"), wired into the approval-card render path. Matches
  Phase 5.
- **Latency mitigation.** Per-ask resolvers short-circuit to `Ok(None)`
  when their flag is unset, so `live` with no per-resolver flags is a
  byte-identical no-op. Real parallel resolve uses `tokio::join!` (issue
  called for <3s total budget).
- **Tests.** Module has its own test block including duration-parsing,
  first-open-slot scheduling math, booking-URL extraction from markdown,
  etc. (see lines 1776–1807+).

What the issue contemplated that is intentionally **not** done:

- **Auto-execute intro** — issue explicitly voted against, so resolver
  stops at "advisory suggestion" string. Sending the intro stays a human
  decision.
- **F1 hand-labeled validation set** (Phase 1 acceptance) — no
  `tests/asks-f1.json` corpus visible in the tree. The shadow-mode
  telemetry table exists, but a labeled regression set against it isn't
  checked in.

## Recommendation

**Close as substantively delivered.** The four-phase build plan is shipped,
including the conservative gating model the issue argued for. The remaining
work is operational (turn on per-resolver flags one at a time, monitor) and
empirical (build the F1 corpus once telemetry has enough volume).

Do NOT pursue this as a single PR. The remaining bullets below are each
their own follow-up issue with concrete acceptance criteria.

## Follow-ups

(For the orchestrator / triager to file separately — not filed by Scribe.)

- File: "Ask-detection F1 corpus + replay harness." Acceptance: 50 labeled
  emails in a fixture file, an offline replay test that asserts ≥0.7 F1
  for the extractor on that corpus.
- File: "Roll out `AUGMENTAGENT_ASK_RESOLVE=shadow` in production for 2
  weeks, then graduate to `live` with `_SCHEDULING=1` only." Acceptance:
  scheduling resolver lands ≥10 real injections, ≥1 user-acknowledged
  "agent picked a real slot."
- File: "Per-resolver dashboards in the Express dashboard." Acceptance:
  count of detected vs resolved vs injected vs accepted-by-user per
  resolver type, per day.
- File: "Decide intro-resolver UX." Acceptance: either (a) keep advisory
  forever and document why, or (b) ship a two-step Discord approval flow
  for committing to an intro (separate from the draft-approval flow).
- File: "Latency budget probes." Acceptance: a load-test that exercises
  `resolve_asks` with 4 simultaneous asks against staged Composio
  endpoints and asserts p95 <3s; if not, fall back to skipping the
  slowest resolver.

## Confidence: high
