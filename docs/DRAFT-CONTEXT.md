# Shared drafting context

Epic #1690 standardizes evidence preparation, not platform transport. The
implementation is split into focused layers:

- `augmentagent-store::conversation_context`: exact platform/account/thread
  selection, chronological source records, synthetic-compose exclusion.
- `channel-core::draft_context`: latest ten chat messages (twenty for the
  email baseline, supplementing existing native email history), owner and
  verified-person wiki pages, explicit missing/truncated evidence.
- `channel-core::draft_evidence`: bounded agent-directed search, source reads
  and wiki reads through existing message search. Search snippets locate
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
| Email | Code generation, repair, classic fallback; native thread/tone/ask context retained | Pending full regression |
| WhatsApp, Slack, Discord DMs, LinkedIn DMs | Code generation, repair, classic fallback | Pending full regression |
| Instagram and Twitter native adapters | Code generation, repair, classic fallback | Pending full regression |
| SocialAPI incoming DMs | Evidence before draft; existing image input retained | Pending full regression |
| Telegram bot replies | Evidence before draft; bot delivery unchanged | Pending full regression |
| Owner ask | Shared context command and receipt attachment | Pending provider/behavioral acceptance |
| Revise (email, iMessage, Slack, Discord, LinkedIn, SocialAPI, Telegram) | Shared evidence recovery and accumulated feedback | Pending revision regression |
| Public feed engagement/comments and GitHub proposals | Separate audience semantics; not migrated by this DM workflow | Explicit limitation; no claim of parity |

Tests must distinguish transport regression, deterministic evidence selection,
and actual-model behavior. Scripted model tests prove orchestration and source
citation constraints; they do not prove model judgment or draft faithfulness.
Live/model evaluations and rollout evidence belong on #1695, using synthetic
fixtures or private results without publishing real communication content.
