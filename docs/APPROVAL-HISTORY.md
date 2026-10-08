# Approval decisions and conversational context

Confirmation-card interactions are recorded in SQLite before execution. Each
record carries the action ID, actor, surface, interaction identity, proposal
fingerprint, verb, summary, and timestamp. Execution outcome is separate from
consent. An approved request can fail; a successful UI acknowledgment is not an
external receipt.

`augmentagent approvals history --limit 20` is a read-only JSON view. Use
`--before <seq>` to page backward or `--action <id>` to inspect an action. The
limit is capped at 100. This is local owner data, like the existing approval
queue: do not expose it in an unauthenticated dashboard endpoint.

The owner query channel in Discord, owner Slack DMs, and the designated
non-group WhatsApp control chat receive bounded recent history on every model
call, including resumed native sessions. Global history is deliberately not
injected into other/shared conversations. Records identify the action and
account, so approvals on one private owner surface remain visible on another.
The prompt describes all record values as untrusted data, not instructions.
Full email bodies and revision feedback are not copied into this history.
Summary/error fields are bounded and use the existing credential redactor.

History also projects the current action status and available Gmail receipt,
so a scheduled decision can be distinguished from the later send. Calendar
receipts contain the event ID and optional URL. Old action rows are not migrated
into invented approval decisions; a legacy Calendar `sent` row without an event
ID remains unverified under the Calendar recovery rules.

## Card and control coverage

| Surface / controls | Shared decision boundary |
| --- | --- |
| Discord Approve, Skip | `ReplyApprover::approve/skip` |
| Discord Revise submission, quick refinement, missing-info submission | `ReplyApprover::revise` |
| Discord schedule selection/custom-time submission | `ReplyApprover::schedule` |
| Discord Send Now, Cancel, Back to queue, Recompose | Corresponding `ReplyApprover` method |
| Slack card and modal equivalents, typed approval commands | Same methods; reschedule has its own method |
| WhatsApp approve/decline/revise control messages | Same approve/skip/revise methods |
| Discord owner-alert acknowledge/resolve/snooze | `interaction::local_decision` |
| Discord reminder acknowledge/dismiss | `interaction::local_decision` |
| Opening or abandoning a modal; schedule previews | No decision or external execution |

All action kinds dispatched through `ReplyApprover` use the same boundary:
Gmail (including composed mail and invoice cards), Calendar creation, identity
merges, LinkedIn, Discord, Slack, Telegram, GitHub, SocialAPI and iMessage.
Transport authorization remains before handler entry. Metadata follows spawned
Discord tasks instead of depending on a task-local inherited by the spawn.
The persisted revision fingerprints include the proposal, draft, draft ID and
outbound envelope. New Discord Approve buttons also carry a draft fingerprint
to reject changed drafts; Slack retains its existing stale-card guard.

## Failure and restart behavior

The decision row is also a durable per-action execution fence. Concurrent
cross-surface operations cannot execute the same action simultaneously. A
replayed interaction does not become another decision. A database failure
before the decision is saved prevents execution. Failure to save an execution
outcome leaves the decision and fence intact, reports an unconfirmed outcome,
and prevents automatic replay.

`in_progress` after a restart may mean the external write happened. `unconfirmed`
means evidence is insufficient. Inspect provider state and the recorded action
before any manual recovery; neither state authorizes a repeat write. No running
process or ephemeral message is needed to deliver context: each next turn reads
the durable ledger again. UI-delivery failure therefore cannot lose context.

## Validation

Deterministic tests use isolated databases and fake HTTP transports. They cover
the real Calendar/Gmail approver, non-send controls, revision and replay,
cross-surface concurrency, stale draft refusal, faults before/after execution,
restart recovery, pagination/redaction, and an actual `WikiQuerier` native
session resumed after approval. The latter captures the reasoner prompt and
checks that another channel cannot see private approval details.

Run:

```sh
cargo test -p augmentagent-store --test approval_history
cargo test -p augmentagent-cli --bin augmentagent approval_context::tests
cargo test -p augmentagent-cli --bin augmentagent calendar_approval_tests
cargo test -p augmentagent-approval-discord -p augmentagent-channel-slack -p augmentagent-channel-whatsapp
```

`.github/workflows/approval-history.yml` runs the deterministic regression gate.
The Calendar live self-invite procedure in `CALENDAR-CREATE-QA.md` exercises the
same approver against Google. Its diagnostic accounts must belong to the owner;
external meeting participants must not be used for QA. Record provider readback,
invitation delivery, history receipt and diagnostic event cleanup independently
from the fake-transport tests. A deployed version and real transport check must
be identified before claiming production is fixed.
