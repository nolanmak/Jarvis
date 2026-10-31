# iMessage owner surface

Status: implementation under test; not yet deployed. The existing Mac sender
works. A separate structured owner-event path captures native GUIDs and reactions;
the prose archive is not an authority source. Discord remains available during rollout.

## Outcome and decisions

1. Mirror the agent's owner-facing Discord experience into one explicitly bound
   private iMessage conversation: notifications, approval cards, agent answers,
   progress, and supported owner commands. Reuse the same query handler and action
   handler. Do not run a second agent turn just to mirror its result.
2. Use the Mac mini and existing verified iMessage outbox/sender; no Twilio.
   Owner notifications are authorized by enabling this surface; approving a
   mirrored draft still invokes the original action's approval handler.
3. Bind an exact chat GUID and accepted owner identity/direction, independently
   of contact defaults. The binding is explicitly configured and versioned.
   Ordinary contacts, group members, and unbound conversations cannot control
   the agent. A self-chat may have owner input marked is_from_me: distinguish it
   from agent output using recorded sent GUIDs, not direction alone. Quarantine
   self-chat events until all outstanding owner sends are reconciled. A verified
   sent GUID excludes an agent echo; unresolved sends keep ambiguous events
   quarantined. Never classify by body alone. Status offers explicit discard
   when GUID reconciliation cannot establish delivery.
4. Every approval presentation records action ID, approval-relevant content
   digest (body, recipient/envelope/operation and status), a monotonic proposal
   epoch, binding revision, and all
   native message GUIDs in a durable delivery record. A reaction or reply to an
   older draft version cannot approve its replacement. The displayed Ref hashes
   this presentation version, not just an action ID. A state/content round trip
   cannot revive the old Ref. Pure-SQL persistent epoch triggers cover Rust and
   Node writers. Reuse existing action
   CAS and cross-surface synchronization, never create a second approval action.
5. A native thumbs-up addition to the exact approval message means approve.
   Removing a reaction does not undo a send; all other reactions are inert.
   A typed thumbs-up or `approve` works when replying to a specific live card.
   `approve <ref>`, `skip <ref>`, and `revise <ref>: <feedback>` also work.
   Bare approve/skip/revise or thumbs-up without a reply target must ask which
   draft, even if only one is pending. Natural-language chat never becomes an
   implicit approval. Unknown command references produce a correction, not a
   model-authorized send.
6. Mirror every owner-visible application output on the configured Discord
   control/query surfaces, including notifications, cards, answers, progress,
   scheduled outcomes, and errors. Exclude third-party messages and internal
   logs. Delivery-health fallbacks stay on Discord to avoid mirroring into the
   failing transport. Inventory direct Serenity posts as well as ApprovalBroker methods.
   Deduplicate events by stable origin ID/revision and chunk index. Do not mirror
   mirrored content back into itself. Mirror failure must not block Discord.
7. iMessage cannot edit a delivered card: append a compact resolution/update,
   retire old card pointers, and issue a fresh version for Revise. The same
   action resolved on Discord becomes visibly settled in iMessage and vice
   versa. Revise never sends the new text without a new approval.
8. Text, native reply context, and thumbs-up controls are release requirements.
   Discord buttons/selects/modals map to explicit text commands and a help menu;
   unresolved choices request the missing value. File/image outputs preserve
   actionable existing links or an explicit attachment limitation. Do not claim
   native attachment/voice parity; audio/video calling remains the existing
   phone/Discord feature and is outside this text surface.

## Components and boundaries

- Store: binding, durable inbound dedup/cursor, mirror records and outbox links,
  exact-version approval association. Reuse surface types and approval pointers.
- Mac bridge: read-only structured chat.db capture of message GUID, ROWID,
  timestamp, chat GUID, service, sender/direction, reply target, associated
  message GUID/type, and body. Normalize only documented-in-code fixture forms
  of part-prefixed GUIDs. Unknown formats fail closed for control, not guessed.
  Reaction removals are distinct inserted events when exposed. Edits/deletions
  of existing native rows are not executable control inputs in this release.
  Check the current native row at capture, before durable acceptance. Already
  edited/retracted text is inert. Durable acceptance is the intent boundary:
  later editing/removing a message does not retract a command already accepted.
  Future mutation capture requires a native mutation revision in its event
  identity, not only GUID/type. Missing schema capability yields visible
  health failure. Submit via existing authenticated SSH CLI transport.
- Owner dispatcher: authenticate binding before parsing, distinguish typed
  commands from free chat, resolve presentation and version, invoke existing
  ApprovalActionHandler/QueryHandler. An at-most-once receipt prevents replay
  after restart. Interrupted/uncertain external actions use existing recovery;
  never blindly retry an approval or query that may have produced side effects.
- Presentation/mirror: transport rendering and delivery only; no model or
  approval authority. Enqueue durable owner messages through the existing Mac
  send machinery, record verified native GUIDs on completion.
- Daemon/one-shot wiring: broker fan-out, direct Discord owner-output inventory,
  query and command dispatch, bidirectional status updates, health/config/help.
  One-shot compose output must mirror exactly once too.

## Ordering, failure, and lifecycle

Enable from an explicit current cursor: historical imported text/reactions never
execute commands. Persist bounded batches in native ROWID order; advance capture
only after durable acceptance. Dispatcher dedup keys include binding revision and
event GUID/type. Receipts move accepted -> claimed -> completed or uncertain;
quarantined receipts cannot be claimed. Restart marks abandoned claimed receipts
uncertain without invocation; accepted receipts may resume. Serialize
conversational turns in native order. Four bounded deterministic control lanes
allow status/cancel and decisions on independent actions to proceed during a long
query or revision. Per-action presentation reservations and the existing
approval-history execution fence exclude competing decisions. An uncertain
conversation blocks later chat until explicit recovery, not independent controls. Outbound delivery records must
survive restart and retain sent GUID mappings. A lost completion is uncertain,
not permission to resend. Failures are visible in Discord and `status`; a later
health notice must not recursively mirror itself forever.

Disabling/rebinding cancels queued owner deliveries and rejects old events and
approval pointers. Refuse rebinding while deliveries are claimed/uncertain; a
claimed send cannot be retracted. Disable prevents new claims immediately and
reports in-flight delivery. Completion associates GUIDs only with the revision
that claimed the delivery. Rebinding does not grant access to old private outputs.
Limits: 8,000 characters per incoming command, 256,000 per logical mirrored
output page, 2,000 per outbound chunk including prefixes, 100 events per batch.
Long answers use labeled continuation pages. Rendered approval presentations above 256,000 total characters (including
recipient/operation metadata and instructions)
produce an explicit non-actionable notice directing the owner to the complete
Discord card; a truncated draft can never be approved.
Only a complete presentation can authorize its exact draft. Retain completed
event bodies for 30 days. Retain compact dedup tombstones for the complete
binding lineage, including completed queries and resolved actions. Old/disabled
binding revisions are independently rejected. Retain approval GUID associations
for unresolved actions. Reuse managed storage/deploy retention.

Approval version validation must be atomic with the existing action claim (or
protected by the same store transaction/lease used by every revision path).
Checking a digest and then calling approve with a race window is insufficient.
Replay of a command cannot execute a second query, send, revise, or skip.
Needs-input/material-warning and existing refusal gates remain in effect.

Owner recovery distinguishes input from delivery. `discard-input <event>` changes
a quarantined/uncertain inbound receipt to ignored, retains its replay tombstone,
and never releases it for execution. If the input holds a presentation
reservation, discard requires a finished approval-history receipt for that exact
native interaction and action before releasing the reservation. An unfinished or
unconfirmed operation stays reserved until its provider outcome is reconciled;
discard cannot manufacture completion evidence. Claimed work must first be stopped through
the existing cancellation/journal recovery path. This operation neither settles
nor retries an outbound delivery. An uncertain delivery is resolved only by a
verified native GUID or a verified terminal rejection from the Mac sender; if
neither is available, keep the surface disabled and report the unresolved send.
Do not clear that barrier just to resume self-chat or rebinding.

## Build order and testable exits

1. Store contracts: binding/revision, event receipt, delivery association and
   version-checked claims. Test isolation, restart, stale card and simultaneous
   Discord/text decisions against one action.
2. Mac event bridge: synthetic chat.db tests for self-chat, normal owner input,
   native reply, thumbs-up add/remove, duplicate GUIDs, unknown schema and
   bootstrap-without-replay. Integrate cursor/SSH and sent-GUID feedback.
3. Mirrored output: inventory broker and direct-owner output paths, readable
   complete cards, chunks, notifications and statuses; exactly-once enqueue,
   uncertain-send handling, no recursive mirror or historical burst.
4. Controls: deterministic grammar, verified owner dispatch, exact-version
   claim, revise feedback and skip, shared cross-surface status. Non-owner and
   unscoped inputs cannot approve; duplicate events have one effect.
5. Chat/UX: normal questions through existing owner query context, command/help
   parity, follow-up across surfaces, supported choices and attachment links,
   cancellation/interruption behavior. Mirror one result without another call.
6. Rollout: Linux/macOS tests, sanitized fixtures, managed snapshot and normal
   updater deployment, explicit owner binding, then real mirror delivery and
   owner-triggered thumbs-up/approve/revise/skip. Third-party sends require the
   owner's actual approval; never simulate it. Document measured latency and
   explicit exclusions. Clean only worktrees/branches created for this feature.

## Open threads and accepted constraints

- Native chat.db reply/reaction column shapes and self-chat identity: bridge
  implementation must verify the actual Mac schema read-only before activation.
  This blocks activation, not store/dispatcher work.
- Owner destination: reuse an existing explicitly configured owner endpoint only
  after checking its chat binding; otherwise ask for that missing endpoint once.
  No guessed wiki match can grant owner authority.
- No guarantee while the Mac is asleep/offline. Durable queue and visible
  uncertain/failure state are the compensating controls. Native media transport
  can be a later issue without changing approval/event identities.

Glossary: mirror delivery is a notification to the owner, action approval is
authorization for an existing external action, and alert acknowledgment only
marks an alert seen. None of these three states substitutes for another.

## Command and context contract

- The configured Discord bot token/owner/query/approval channels are pinned to
  each enrollment through a digest. Context initialization checks the native bot
  identity, guild and owner's membership. Configuration drift disables dispatch
  until explicit re-enrollment; it never silently changes the owner account.
- Ordinary text uses the shared WikiQuerier and the same model configuration,
  knowledge base, communication search tools and approval handlers as Discord.
  A native reply to a mirrored query routes to its originating channel/thread.
  Earlier owner text turns come from existing receipts/outbox records (last ten,
  bounded to 10,000 characters), with delivered vs prepared output distinguished.
  Discord owner history is fetched separately. Older/full records are available
  through `augmentagent imessage owner history --guid <GUID> --offset <n>
  --limit <1..8000>`; listing is paginated and limited to twenty receipts.
  Listing, automatic history and GUID reads are scoped to the current binding
  revision, including when a known old GUID is supplied. Query tool grants and
  automatic injection additionally require a currently enabled enrollment whose
  pinned owner configuration still matches. Old/private histories never become
  available to a new enrollment or changed Discord owner configuration.
  Only this read-only command is added to authenticated owner query tool grants.
- `approve`, `revise`, `skip`, `fill`, `refine`, `schedule`, `reschedule`,
  `send-now`, `cancel-schedule`, `back-to-queue`, `recompose`, and calendar
  `decline` use an exact delivered presentation Ref or native reply. Commands
  needing a value use `verb <ref>: value`. `presets` lists the shared refinement
  presets. Thumbs-up only operates a pending approval card, never a scheduled or
  recovery notice. Needs-input gates apply before approve/schedule/send-now.
- `model`, `loop`, `!journal`, and `!loops` / `!processes` invoke the same services
  as Discord. `loop ack|dismiss <id>` covers reminder buttons. Owner alerts expose
  `alert ack|snooze|done <id>` and reuse the audited alert control service.
- `status`, `cancel`, `cancel all`, `reset` / `new`, `pending [offset]`, and `help`
  are deterministic. Cancel requests provider cancellation; only a confirmed
  cancellation outcome says cancelled. The legacy stateless provider path cannot
  confirm cleanup after a dropped future: its stop result explicitly says
  unconfirmed and parks the conversation until recovery. Cancel-all retires waiting ordinary chat,
  never approval decisions or scheduled loops. Reset refuses live turns and
  resolves interrupted query receipts without replay or action-fence release.
  Voice/calling stays on the existing phone/Discord transports.
- Capture runs independently of outbound sending. A disabled or re-enrolled
  control conversation stays excluded from the contact auto-replier, preventing
  owner control text from being drafted as a third-party conversation.

## Output inventory and operations

The approval broker fans out complete cards directly. Both broker fan-out and
Discord ingress resolve approval buttons to the canonical action presentation
and use `card:<action-id>:<presentation-ref>` as the durable origin key. Generic
ingress never also queues the raw Discord card. Native status/recovery notices
similarly use `state-card:<action-id>:<presentation-ref>`. This producer convergence
applies during gateway delivery, edits and REST catch-up; a revision gets a new
Ref and a fresh card. Generic Discord sends,
edits, query answers, loop results, flag/digest notifications and attachments are
mirrored from the actual bot message IDs in the configured root channels, their
threads and the verified owner DM. Private component/modal follow-ups and command
results are captured at their HTTP response boundary because Discord does not
publish them in gateway history. Reconnect catch-up follows durable per-channel
cursors, discovers active/public archived/joined-private threads, and rereads a
recent edit window, plus a rotating check of older known messages. Original messages before enrollment do not replay; a later
edit can produce an update. Attachment URLs retain Discord's access/expiry rules;
native iMessage media transfer is not included. Approval-card attachments are
forwarded as a separately keyed original-card/file link notice rather than a
raw duplicate card. Link notices carry no action association and cannot authorize
an approval; native replies/reactions must target the actual draft presentation.

A disabled mirror is excluded from fan-out success accounting. Action completion is also reconciled from stored state every fifteen seconds,
including Mac sender and dashboard transitions that do not create a Discord
message. State notifications validate their observed version atomically with
queueing and never become approval targets. Failed/uncertain mirror deliveries
produce a rate-limited Discord fallback with the dedicated “iMessage delivery
status” footer. Ingress excludes that fallback from native mirroring, preventing
a failure-notice loop; disabling/rebinding cancellations do not raise this alert. Generic notices
use the real Discord ingress and cannot falsely count as a successful independent
notification if Discord fails. Native cards still use the durable iMessage outbox.

`imessage owner status` reports capture freshness, sender freshness, mirror errors,
receipt states and uncertain/failed deliveries. `bind`, `disable`, `ingest`,
`capture-error`, and `discard-input` are operator/transport commands, not agent
query tool grants. Bind starts from the current native ROWID. The Mac owner job
runs every five seconds independently of the existing sender job. Retention
runs in the daemon's existing hourly body housekeeping, even when the mirror
is disabled. It clears completed input bodies and terminal generic output bodies
after thirty days while retaining identity tombstones, native GUIDs and approval associations.
