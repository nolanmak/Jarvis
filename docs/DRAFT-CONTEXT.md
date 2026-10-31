# Shared drafting context

Epic #1690 standardizes evidence preparation, not platform transport. The
implementation is split into focused layers:

- `augmentagent-store::conversation_context`: exact platform/account/thread
  selection, chronological source records, synthetic-compose exclusion.
- `channel-core::draft_context`: latest ten chat messages (twenty for the
  email baseline, supplementing existing native email history), owner and
  verified-person wiki pages, explicit missing/truncated evidence.
- `channel-core::draft_evidence`: bounded agent-directed search, source reads
  and wiki reads through existing message search. Bounded literal wiki search
  locates project pages without treating search hits as read evidence. Search snippets locate
  evidence; only read sources can support cited facts. Source text is data,
  never an instruction channel.
- `channel-core::draft_workflow`: coordinate preparation, preserve material
  gaps on cards, and store private source receipts. No sending code.
- Existing adapters: platform formatting, original thread/envelope semantics,
  approval and delivery. Code-mode repair and classic fallback reuse the
  same prepared context.

The baseline is a starting window, not a claim that the entire relationship
fits in ten messages. The model decides whether a reply needs an older message,
email, WhatsApp discussion or project page, and requests it. A reference that
cannot be resolved must remain an explicit gap. Retrieving private knowledge
does not authorize disclosing it to a recipient or group.

## Owner-requested drafts

Find the intended incoming message through the existing message search tools,
then use the same workflow as automatic replies:

```
augmentagent messages context --message-id '<source-id>' --request 'Draft a reply about the plan in the email'
```

The JSON response contains the baseline, evidence review and `receipt_id`.
It does not create a card or send anything. After composing a card, attach its
returned action ID:

```
augmentagent messages context-attach --receipt-id '<receipt-id>' --action-id '<action-id>'
augmentagent messages context-receipts --action-id '<action-id>'
```

Attachment refuses a different platform, account or conversation. The context
command's arguments and evidence output are omitted from ordinary tool audit
notices. Receipt inspection is private. Explicit verbatim messages do not
require generation or evidence retrieval.

## Limits, gaps and rollback

Chat source bodies share a 32,000-character initial budget; each initial wiki
page is capped at 12,000 characters. Truncation is reported and relevant
remainders can be retrieved. Recovery is bounded to eight model decisions,
90 seconds and a 64,000-character model-input budget. Message and wiki reads
page 8,000 characters at a time. A tool failure, invalid decision or exhausted
limit is not a successful empty search; it produces a material context warning.
Attachment references are not treated as attachment contents.

`AUGMENTAGENT_DRAFT_CONTEXT_PLATFORMS=imessage,gmail,whatsapp` stages recovery on
those platforms. `all` (also the unset default) enables migrated adapters;
`off` disables shared recovery without changing send permissions or deleting
history. A disabled context CLI result says `enabled: false` and has no receipt.
Set the same policy for the daemon and child context commands; ask forwards it.

Receipts retain source IDs, wiki hashes, bounded selection reasons, freshness
metadata and unresolved gaps, rather than duplicate message bodies. Normal
receipt retention is thirty days, while receipts attached to unresolved
approvals remain available. Revision receipts retain their own evidence and
previous owner instructions are read from the existing revision history.

## Integration and validation status

This is an implementation matrix, not a declaration of completed acceptance.
Update the validation column with actual results before release.

| Surface | Shared path | Validation |
| --- | --- | --- |
| iMessage automatic reply | Baseline in triage; evidence before draft | 63 existing library tests passed during development |
| Email | Code generation, repair, classic fallback; native thread/tone/ask context retained | Existing adapter library regressions passed; CI verifies merged head |
| WhatsApp, Slack, Discord DMs, LinkedIn DMs | Code generation, repair, classic fallback | Existing adapter library regressions passed; CI verifies merged head |
| Instagram and Twitter native adapters | Code generation, repair, classic fallback | Existing adapter library regressions passed; CI verifies merged head |
| SocialAPI incoming DMs | Evidence before draft; existing image input retained | Existing adapter library regressions passed; CI verifies merged head |
| Telegram bot replies | Evidence before draft; bot delivery unchanged | Existing adapter library regressions passed; CI verifies merged head |
| Owner ask | Shared context command and receipt attachment | CLI context/attachment isolation passed; actual-model evidence-present/missing test passed |
| Revise (email, iMessage, Slack, Discord, LinkedIn, SocialAPI, Telegram) | Shared evidence recovery and accumulated feedback | Shared refresh/accumulated-feedback regression passed; routing regression in CI |
| iMessage and WhatsApp groups | Existing adapters reject group auto-drafts; unchanged | Existing group/own-echo regression tests |
| Slack/Discord subscribed channels | Shared context follows existing subscription policy; participant membership is not inferred from sender | Audience uncertainty is explicit; no claim of private-DM semantics |
| Public feed engagement/comments and GitHub proposals | Separate audience semantics; not migrated by this DM workflow | Explicit limitation; no claim of parity |

Tests must distinguish transport regression, deterministic evidence selection,
and actual-model behavior. Scripted model tests prove orchestration and source
citation constraints; they do not prove model judgment or draft faithfulness.
Live/model evaluations and rollout evidence belong on #1695, using synthetic
fixtures or private results without publishing real communication content.

## Rollout gates and measured limits

Before deployment, require green platform CI, existing adapter regressions,
shared source/receipt/revision contracts, CLI no-send and policy-off checks,
and an actual-model evidence-present/evidence-missing pair. Stage iMessage,
Gmail and WhatsApp first, inspect a grounded approval card and its receipt,
then enable the remaining migrated adapters. Every send still needs its
existing approval. Revert the context policy to `off` if a card invents a
material fact or exposes private evidence.

The synthetic model run on 2026-10-31 read the referenced email and used its
updated day/time (two source operations; 66 seconds including drafting).
The matched missing-email run finished with a material gap after two searches.
These are individual observations, not population estimates of recall or
faithfulness. There is no measured before/after owner-edit rate yet; collect
that through existing revision records rather than inventing a success metric.

Receipts cover the shared context step. Existing channel-specific draft tools
(such as additional wiki or image reads) retain their existing tracing; this
receipt is not a complete audit of every subsequent model tool call. Public
feed/comment drafting and new transport support remain outside this DM
migration, tracked explicitly under #1693/#1695 rather than claimed as parity.
