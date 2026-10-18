# Owner phone calls — issue #1496

Status: implementation in progress. Nothing here establishes production phone
availability or live ConversationRelay qualification. The previous one-way
Twilio test established credential validity and outbound dialing only.

Issue: https://github.com/nolanmak/Jarvis/issues/1496

## Current design

`augmentagent-phone` is shared by chat adapters and the agent tool facade. It
uses the existing store and does not own an independent agent conversation.
Its provider boundary is `Telephony`; `TwilioClient` implements bounded HTTP
requests without redialing an uncertain create. Callback URLs have a fixed
operator-configured HTTPS origin. Signature verification includes every form
field and uses the configured public URL instead of forwarded client headers.

`augmentagent-store::phone` owns additive SQLite tables for destination state,
verification challenges, dial claims, relay claims and cleanup attempts. Dial
reservation happens in an immediate transaction; only its first caller receives
a send permit. A pending/uncertain call keeps the owner's active-call slot even
after restart. Call receipts cannot serialize phone destinations. Explicit
operator import is one-time; a removed destination leaves a tombstone.

Phone grants require a stored surface owner and control conversation, plus an
explicit operator-owned mapping to a canonical owner ID. Verification is allowed
only in the stored direct conversation. A capability is bounded to its original
owner, conversation and owner-binding revision. It expires after five minutes
and rechecks authorization on use. Integration adapters must never mint grants
from ingested document content or model-supplied identities.

Verification uses a random six-digit code spoken by a fixed TwiML message.
The database contains only an HMAC bound to owner and request ID; the key is a
separate operator-provided secret. Private command handlers must consume codes
before history/agent ingestion. Codes expire after five minutes and five wrong
attempts. A pending replacement leaves the prior destination active.

Normal outbound calls redirect to a signed start callback. That records the
answer timestamp before returning a generic DTMF prompt. Only an accepted call
receives ConversationRelay TwiML. A watchdog enforces the 15-second acceptance
window independently of Twilio Gather's post-prompt timeout. Relay partial
prompts are disabled explicitly. Finalized utterances get distinct turn IDs;
identical words in separate utterances are not incorrectly deduplicated.

## Test-first evidence

Each initial suite was run before its implementation. Red runs exited 101 with
unresolved APIs; the final-output regression instead failed its behavioral
assertion before the fix. Local transcripts are under `/tmp/1496-*.log` in the
implementation workspace (not release receipts).

| Slice | First failing command | Expected failure |
| --- | --- | --- |
| Durable calls | `cargo test -p augmentagent-store --test phone_calls` | Missing phone module/methods |
| Verification and acceptance | Same command, expanded tests | Missing verification/accept APIs |
| Owner service | `cargo test -p augmentagent-phone --test service` | Missing service/authorization types |
| Twilio adapter | `cargo test -p augmentagent-phone --lib` | Missing client/signature validation |
| Signed callbacks | Service suite, expanded tests | Missing callback handlers |
| Relay protocol | `cargo test -p augmentagent-phone --test relay` | Missing protocol state |
| Final speech suppression | Relay suite, changed auto-final wording | Automatic final hit a conflicting speech receipt |
| Relay claims and maintenance | Store/service suites, expanded tests | Missing claim/maintenance APIs |

Run the current deterministic suites with:

```sh
AUGMENTAGENT_GH_DISABLE=1 cargo test -p augmentagent-store --test phone_calls
AUGMENTAGENT_GH_DISABLE=1 cargo test -p augmentagent-phone
```

## Acceptance audit — not complete

| Criteria | Evidence so far | Remaining proof/work |
| --- | --- | --- |
| AC01 | Disabled/config validation; signed ingress; all three daemon adapters; cleanup when normal startup is unavailable | Deployed ingress and live TLS receipts |
| AC02 | Grants, revocation, expiry, forged targets, authenticated MCP and surface boundaries | Complete live boundary evidence |
| AC03 | Concurrent reservations, durable deduplication and uncertain-create/restart tests | Missing-callback provider reconciliation and live runtime fault evidence |
| AC04 | Signed/reordered callbacks, local ingress, immutable final duration and signed event-time accounting | Live callbacks and reconciliation when final callbacks never arrive |
| AC05 | Signed acceptance, fake-clock deadline tests and concurrent runtime watchdog | Voicemail and live acceptance timing |
| AC06 | Both native providers: text → phone → text continuity; shared queue; deferred/cancellable initial history | Actual native tool/approval continuation and live evidence |
| AC07 | Relay/barge-in generations, final suppression, one-time claims, local WSS round trip and cancellation | Complete live streaming/interruption and tool-disconnect proof |
| AC08 | Discord, Slack and WhatsApp daemon adapters; authenticated tools; durable transcript outboxes | Exhaustive authenticated-surface/preset inventory and live origin receipts |
| AC09 | Persistent budgets, provider-duration callbacks, bounded shutdown and disabled/unavailable-start cleanup | Missing-callback reconciliation and live stop/restart/disable evidence |
| AC10 | Recorded-audio receipt validator and synthetic regression tests only | Actual consented audio, native traces and full live matrix; no live receipt exists |
| AC11 | No legacy voice removed yet | Replacement qualification, upgrade/rollback fixtures and retirement |
| AC12 | Required offline runner and local regression receipts; OS CI configured | Passing current-head OS CI, complete evidence map and live receipts |
| AC13–15 | Private commands, verification, durable settings and privacy tests across all three adapters | Live private setup/verification followed by a successful conversational call |

The old conversational voice stack must not be removed before replacement
qualification. Its shared session/history functionality and inbound voice-file
capture remain separate migration concerns. This document is a progress record,
not permission to weaken any requirement of #1496.

## First implementation checkpoint

On this Linux workspace, the first checkpoint passes 10 phone storage tests,
18 phone crate tests (4 provider, 3 protocol, 11 service/HTTP/WebSocket), all 148
existing store unit tests, and `cargo clippy -p augmentagent-phone --all-targets
--no-deps -- -D warnings`. The socket integration tests use loopback WebSockets
and scripted agent/provider implementations, not Twilio or native agents. They
are not evidence of live TLS routing, model session continuity or audible latency.

Next implementation work: daemon startup/maintenance, persisted origin context,
shared agent-turn integration, conversation-bound MCP tools, early private chat
command routing for all three platforms, configuration/runbook, CI, then live
qualification and legacy retirement. Keep the goal and issue open until the
entire acceptance map is proved.

## Native-session and MCP checkpoint

The phone agent bridge now enters `run_surface_turn` with the persisted origin.
That common runner serializes phone and text before loading the native binding
or claiming a turn. A scripted-provider test runs text → three phone turns →
text for both Claude and Codex and observes one session; a concurrent-turn test
observes at most one native turn executing. These are offline harness tests,
not live model/tool/approval qualification.

The private Unix-socket tool broker issues revocable owner/conversation grants.
The `phone-tool` CLI subcommand provides bounded stdio MCP frames; its schemas
accept no destination, origin or verification-code fields. The agent option
adapter preserves other tools and only adds phone tools inside trusted transport
context. `WikiQuerier` retains the grant for the provider call, but its endpoint
still needs installation at daemon startup. No production surface is claimed
connected by this checkpoint.

The structured private setup command implementation consumes numbers and codes
without returning them in replies. The actual chat routers still must intercept
these **before** inbox persistence, history capture or model invocation. In
particular, Slack currently persists an event before owner dispatch, so adding
phone handling only in its existing command dispatcher would violate AC15.
WhatsApp's existing CLI runtime is not wired and its control path ignores
outbound messages; an authenticated self-chat adapter is still required.

Test-first records from this workspace:

| Behavior | Failing run | Passing run |
| --- | --- | --- |
| Shared text/phone queue | `/tmp/1496-serialization-red.log` (concurrent native turn marked uncertain) | `/tmp/1496-serialization-green.log` |
| Persisted immutable call origin | `/tmp/1496-origin-red.log` (missing API) | `/tmp/1496-origin-green.log` |
| Private MCP grant/number commands | `/tmp/1496-mcp-red.log` (missing API) | `/tmp/1496-mcp-green.log` |
| Same native session through phone | `/tmp/1496-agent-bridge-red.log` (missing bridge) | `/tmp/1496-agent-bridge-green.log` |
| Bounded MCP facade and error handling | `/tmp/1496-mcp-facade-red.log` (missing module) | `/tmp/1496-mcp-facade-green.log` |
| Scoped tool injection | `/tmp/1496-tools-injection-red.log` (missing adapter) | `/tmp/1496-tools-injection-green.log` |

The `/tmp` logs are local development evidence. Required CI and live receipts
remain outstanding. AC06 still requires real tool and approval behavior; AC08
and AC13 still require actual Discord/Slack/WhatsApp adapter coverage. Runtime
watchdog scheduling, provider-duration accounting, configuration, durable mirror
adapters, live qualification and legacy retirement also remain open.

Validation for this checkpoint: 36 targeted integration tests passed (11 store,
13 phone service, 2 MCP facade, 2 phone-agent bridge/configuration, 8 common
session runner), plus `cargo check -p augmentagent-cli` and phone-crate Clippy
with warnings denied. Existing unrelated CLI/docs warnings remain in the CLI
check. This is Linux local validation only.

## Runtime, configuration and CI checkpoint

`PhoneRuntime` owns the phone-only listener, private tool broker and watchdog.
The runtime test holds one provider hangup indefinitely while another call reaches
its acceptance deadline: the second hangup begins within one second. Behavioral
red tests also found duplicate cleanup attempts from disconnect and explicit
stop/removal; all paths now share in-flight cleanup guards and durable retries.
The test confirms restart stop marking, rejection of starts after shutdown, and
an unrelated HTTP route returning 404.

Configuration tests cover default-off, missing/invalid settings, loopback-only
binding and explicit identity links without inventing authentication bindings.
The configuration/runtime contract and remaining deployment work are documented
in [runtime.md](runtime.md). The actual daemon and chat adapters still need wiring.

Red/green records: `/tmp/1496-runtime-{red,green}.log`,
`/tmp/1496-concurrent-cleanup-{red,green}.log`,
`/tmp/1496-stop-paths-{red,green}.log`, `/tmp/1496-config-{red,green}.log`, and
`/tmp/1496-ci-runner-{red,green}.log`. The cleanup regressions failed behaviorally;
new runtime/config APIs first failed compilation. The CI runner's test rejects
empty, skipped, filtered, short and failed results. The phone workflow adds
Linux/macOS offline jobs; actual remote execution remains unverified.

Local Linux validation for this checkpoint: the required runner passed all nine
suites / 45 Rust tests (`/tmp/1496-phone-offline/receipt.json`), its Python
validation test passed, `cargo check -p augmentagent-cli --locked` passed with
preexisting warnings, and phone Clippy passed with warnings denied. Remote
macOS/Linux workflow results and live evidence are still absent.


## Slack adapter checkpoint

The actual Slack Socket Mode → dispatcher → outbox path accepts a shared phone
service with `with_phone`. Private setup consumes input before ordinary inbox
persistence. Only authenticated routing metadata is persisted; a bounded map
holds up to 128 inputs, each at most 8 KiB, expiring after five minutes. The
Socket Mode ACK does not await provider HTTP (tested with a held create call).
Restart loses the private input deliberately and delivers an instruction to
check status/resend, rather than replaying a code or an uncertain call.

Setup/code-bearing edits and nested provider payloads are excluded too. Agent
history fetches, subscribed live ingestion, catch-up conversion and Composio
poll ingestion all exclude private phone commands, including edited messages.
The transport does not claim to delete the original message from Slack.

Phone call controls use independent command dispatch lanes; a blocked agent turn
cannot hold up stop/removal. Authenticated Slack native turns carry the trusted
phone origin. The shared queue exposed detached Slack workers during simulated
restart: aborting the surface now aborts its worker tasks, allowing the native
conversation to resume. Existing text, cancellation and restart regressions pass.

Behavioral red logs: `/tmp/1496-slack-history-red.log`,
`/tmp/1496-slack-context-red.log`, `/tmp/1496-slack-urgent-red.log`,
`/tmp/1496-slack-restart-diagnostic.log`, `/tmp/1496-slack-ingestion-red.log`,
`/tmp/1496-slack-catchup-red.log`, `/tmp/1496-slack-poll-red.log`.
`/tmp/1496-slack-private-red.log` first failed for the absent phone dependency/API.
Passing targeted results are in `/tmp/1496-slack-checkpoint-tests.log` (84 tests).
The required offline CI runner now includes all seven of those Slack suites.

This is adapter-level evidence using the actual socket/outbox code with fake
telephony and agent implementations. `serve` still must install the runtime and
service, attach the query-handler endpoint, and provide platform-specific phone
history/mirror adapters. Discord/WhatsApp and live qualification remain open;
no legacy voice removal is justified by these offline results.

Checkpoint validation: `/tmp/1496-phone-slack-offline/receipt.json` reports all
16 required suites / 129 Rust tests passing locally; the runner's Python test,
CLI `cargo check --locked`, and Slack library Clippy with warnings denied also
pass. These development logs ran against uncommitted changes; their HEAD SHA
alone does not identify the tested tree. Future runner receipts explicitly mark
`worktree_dirty`; remote clean-checkout CI remains the commit-specific evidence.


## Slack daemon and durable phone conversation checkpoint

`serve` now creates one shared phone runtime (default off, no dry-run calls) and
hands its endpoint to Slack's existing query handler and interactive surface.
The phone router uses the persisted call origin and a revocable platform
registration. Slack's phone bridge uses the same `WikiQuerier`, selection policy
and common native session runner as text. Runtime health is supervised and
shutdown drains the phone service. Missing/invalid configuration leaves phone
unavailable while other daemon surfaces continue.

`SlackPhoneIo` rechecks the owner/control binding, fetches the existing filtered
history for a cold session, and enqueues spoken turns/final answers through the
existing durable Slack delivery planner. It shares the sender wakeup and retains
the original channel/thread. A reopen/rate-limit fixture retries delivery without
rerunning the agent or duplicating transcript sends.

The CLI adapter test drives an authenticated text `call me` through the private
Unix tool endpoint, accepts a fake call, sends three finalized WebSocket prompts
through the real listener/router, checks tool injection, drains six Slack
transcript posts, then resumes text. Claude and Codex both observe the same native
session throughout. Provider transport and native model execution remain fakes;
this does not prove real speech timing or gated tool approvals.

Test-first records: `/tmp/1496-slack-delivery-{red,green}.log` and
`/tmp/1496-router-{red,green}.log` first failed on missing adapter APIs;
`/tmp/1496-cli-ci-selector-{red,green}.log` covers explicit named CLI selection
without permitting zero/ignored required cases. The full CLI test build found
nine fixture constructors missing the earlier phone endpoint field; those are
now updated. The daemon adapter passes in `/tmp/1496-daemon-slack-test.log`.

The expanded offline runner passed 20 suites / 140 tests locally, with a dirty
worktree recorded in `/tmp/1496-daemon-slack-offline/receipt.json`. It includes
the new delivery/daemon tests and existing CLI surface-conformance and Discord
voice-session regressions. Remote CI and live receipts remain outstanding.
Disable-at-start cleanup, complete stale-binding/final-duration handling,
Discord/WhatsApp installation, the production preset inventory, real approval/tool
qualification and legacy voice retirement are still required for #1496.


A follow-up supervisor regression (`/tmp/1496-daemon-cleanup-isolation-red.log`)
proved that propagating a phone cleanup error would fail the daemon task drain.
The CLI supervisor now logs the incomplete cleanup and returns without failing
other runners; the uncertain call remains marked for cleanup in the store.
The shared process drain budget is shorter than the standalone phone cleanup
window, so full graceful-stop/disable qualification remains open.

The follow-up supervisor and daemon integration tests both pass in
`/tmp/1496-daemon-cleanup-isolation-green.log` (2 tests). This is targeted
evidence after the 140-test full runner receipt above.

### Callback revocation follow-up

Signed start and acceptance callbacks now revalidate the persisted call origin
against current owner/control-chat authorization before offering acceptance or
opening Relay. Revocation requests durable cleanup and returns Hangup; signed
terminal status still records the outcome. Test-first evidence:
`/tmp/1496-revoked-callback-red.log` reproduced acceptance after unbinding;
`/tmp/1496-revoked-callback-green.log` passes all 15 service tests, including
revocation before start and between start/accept. This does not yet prove
same-owner rebind invalidation; the subsequent revision checkpoint below addresses
that gap.


### Durable authorization revision

Phone grants and call origins now use a durable per-account revision instead of
`confirmed_at_ms`. The revision survives owner removal and advances on owner or
control-conversation changes, including changes with the same timestamp.
Origin registration checks the grant revision in its write transaction and
persists an immutable snapshot. Start/accept callbacks and native phone-turn
authorization compare that snapshot with current authority. Calls lacking a
snapshot fail authorization rather than inheriting a fresh binding.

`/tmp/1496-binding-revision-red.log` reproduces a grant reviving after a same-time
unbind/rebind. The service regression now passes for both acceptance and
connected-call authorization. `/tmp/1496-binding-revision-store.log` passes all
11 call-store and 15 owner-store tests, including restart, control-chat restore,
immutable snapshots and migration preserving existing revisions. The offline
runner now requires the owner-store regression suite as well. Live callback and
streaming revocation qualification remain outstanding.

The expanded Linux offline run passes 21 suites / 158 tests; receipt:
`/tmp/1496-binding-revision-offline/receipt.json` (worktree dirty at execution).
Phone-library strict clippy passes in
`/tmp/1496-binding-revision-phone-clippy.log`. Including the store library in
strict clippy reports eight existing findings in unchanged `store.rs` (type
complexity, argument counts and doc-list indentation); see
`/tmp/1496-binding-revision-clippy.log`. No live provider calls were made.


### Discord command and privacy boundary

The authenticated Discord gateway now dispatches phone commands before attachment
fetches, conversation-history retrieval and native agent turns. The hook receives
provider message/author/channel/bot/guild IDs independently from command text.
An uninstalled adapter consumes the command with a generic unavailable response;
it does not submit number setup or verification codes to the agent. This is the
entry point for the remaining Discord runtime adapter, not working Discord calls.

Later Discord history omits private number controls and assistant replies to those
controls. The separate Discord subscription poller skips private controls before
attachment download, and its ingestion handler repeats the check before writing
an email/inbox record. Red logs:
`/tmp/1496-discord-phone-history-red.log` and
`/tmp/1496-discord-phone-ingest-red.log` reproduce the previous history/inbox
leaks; `/tmp/1496-discord-phone-boundary-red.log` records the missing command API.
Green logs run all 255 Discord broker unit tests plus the new dispatch integration
test, and all 44 offline Discord subscription tests. Two existing live digest
qualification tests remain ignored because they require native-provider login.
The required phone runner selects the new ingestion regression explicitly and
still rejects ignored or absent required cases.

Outstanding Discord work includes authenticated account/control-chat setup,
installing the service and tool endpoint, DM native-session continuity, durable
transcript delivery and actual phone calls. Legacy voice remains installed.

Inspection of `WikiQuerier::answer_turn` confirms that current Discord DM turns
fall back to the legacy answer path: shared native scheduling is gated on a guild
ID and the old voice flag. Discord phone installation must remove that dependency
for authenticated phone-enabled DMs and reuse their native session, rather than
creating a separate phone conversation.

The expanded required offline run passes all 24 suites / 415 tests, with no
ignored required cases. Receipt:
`/tmp/1496-discord-boundary-offline/receipt.json` (dirty worktree recorded).
It includes CLI daemon phone, surface-conformance and legacy voice-session
regressions. The runner's two Python validation tests also pass. No live Discord
messages or phone calls were sent during this checkpoint.


### Discord DM native-session route

The gateway now has a phone-aware text-turn wrapper. It builds a scoped origin
only when the query handler can authorize it through its installed phone tool
server. Discord identities come from the authenticated gateway: guild
conversations retain the existing guild account key; DMs use `dm:<bot-user-id>`
as their account key and the DM channel ID as their conversation key. Zero or
missing provider IDs are rejected. This conversion never creates owner bindings
or control chats; those still require authenticated setup.

For an authorized phone-enabled Discord turn, `WikiQuerier` now uses the shared
native session runner without requiring a guild or the legacy voice flag. It
checks the audit channel/account, rechecks stored owner authority after queueing,
and uses the same conversation key as `PhoneAgent`. It refuses unsupported native
providers; duplicate turn IDs retain the store's no-replay behavior.

`/tmp/1496-discord-native-red.log` reproduced DM execution without native session
context. `/tmp/1496-discord-native-green.log` exercises the real query handler,
MCP tool configuration and phone bridge with fake provider/model transports:
Claude and Codex both reuse the original session across text → phone → text,
exclude old history on resume, reject duplicate native submission and reject
wrong-channel/revoked origins. The fixture's phone mirrors are in memory; this
is not proof of durable Discord delivery or audible calls.
`/tmp/1496-discord-origin-{red,green}.log` covers gateway ID conversion and
separation of bot DM accounts.

The daemon still does not install a Discord phone tool endpoint. Durable Discord
mirrors, runtime registration, authenticated account/control-chat setup and DM
model-control continuity remain required before enabling calls on Discord.

The expanded offline receipt passes all 25 required suites / 417 tests:
`/tmp/1496-discord-native-offline/receipt.json` (dirty worktree at execution).
The new native-DM regression is required by CI, alongside existing Discord voice,
Slack daemon, session-conformance and privacy regressions. The two Python runner
validation tests pass. These checks used no live phone calls or Discord messages.


### Durable Discord transcript delivery

`phone_delivery` now enqueues complete transcript messages as atomic batches of
Discord-sized parts in the shared outbox. Each part has an immutable whole-message
hash, stable nonce, original conversation and owner-authority revision. Changed
payloads under an existing key fail the entire batch. Duplicate identical enqueue
is idempotent. The new scoped-prefix claim API prevents the phone worker from
claiming another transport's outbox work while retaining conversation order.

The delivery boundary rechecks authorization, bounds provider work to 20 seconds,
backs off explicit temporary rejection, and parks ambiguous outcomes for
reconciliation. An interrupted sending row is recovered to reconcile by the
existing daemon-start recovery contract; workers must start after that recovery.
A bounded nonce lookup only marks positively identified bot/channel messages
sent. An inconclusive lookup stays pending with a 30-second lookup backoff and
never requeues a blind send. Unknown delivery remains visible in the shared
outbox status. Agent execution is not part of delivery or reconciliation.

The Serenity transport creates messages with mention parsing disabled and nonce
uniqueness enabled. Discord's nonce uniqueness window is only a few minutes, so
it is defense in depth, not the durable retry rule:
[Discord Create Message documentation](https://docs.discord.com/developers/resources/message#create-message).
Lookup checks the most recent 100 messages; older/absent results are inconclusive.

Test-first logs: `/tmp/1496-discord-delivery-batch-red.log`,
`/tmp/1496-discord-delivery-prefix-red.log`, `/tmp/1496-discord-delivery-red.log`
and `/tmp/1496-discord-reconcile-red.log`. The green delivery suite covers atomic
multipart replay, changed payload rejection, reopen, explicit retry, unknown
outcome, positive reconciliation without a resend, same-time reauthorization,
and crash recovery with no proof of delivery. Provider requests are checked via
the serialized production builder; sends/lookups use fake transport responses.

This transport is not yet connected to the daemon or `ConversationIo`. Remaining
Discord work includes sharing filtered history, registering the phone agent and
delivery loop, account/control-chat setup and DM model-command continuity. No
live Discord delivery is claimed by these tests.

The expanded required offline run passes all 27 suites / 455 tests, including
all 35 shared delivery tests and three Discord phone-delivery tests. Receipt:
`/tmp/1496-discord-delivery-offline/receipt.json` (dirty worktree recorded).
The two Python runner validation tests pass. Real Discord API delivery and
reconciliation behavior remain live qualification requirements.


### Discord conversation IO and delivery worker

`DiscordPhoneIo` now connects `PhoneAgent` to the durable Discord outbox. It checks
the bot-specific DM account (or guild account), stored owner/control conversation,
and numeric provider IDs before fetching history or queuing a mirror. Audit
contexts preserve the originating Discord channel, guild and HTTP client so
existing approval notifications can use their normal route.

The phone history adapter shares `fetch_owner_history` with ordinary Discord
text: the owner/bot filter, bounded age/length, and private setup/reply exclusion
remain in one implementation. Phone bootstrap propagates unavailable history
instead of silently treating it as empty. Authorization is checked again after
history retrieval. The existing text path retains its empty-history fallback.

The account-scoped delivery worker handles outbox wakes and periodic retries,
uses the bounded delivery/reconciliation boundary, and observes daemon
cancellation. Cancelling during a provider send leaves the durable sending row
for startup reconciliation. Startup recovery must run before spawning workers;
the worker does not reset another running surface's delivery state.

Test-first evidence: `/tmp/1496-discord-io-red.log` and
`/tmp/1496-discord-worker-red.log`; green:
`/tmp/1496-discord-io-worker-green.log` (three tests). The real query-handler
fixture now exercises text → three phone turns → text for both Claude and Codex,
with one native session, five agent invocations and six durable mirrors delivered
through a reopened store. Wrong-bot and revoked-owner contexts fetch no additional
history and enqueue no transcript; shutdown during a send finishes within the
test's one-second deadline and recovery parks the send without resending.

Provider/model/HTTP boundaries remain scripted. This is not live Discord or
approval-tool qualification. Daemon registration, private setup routing and DM
model-command continuity remain unfinished; Discord calls are not enabled yet.

The expanded required offline run passes all 28 suites / 457 tests:
`/tmp/1496-discord-io-offline/receipt.json` (dirty worktree recorded). This includes
the shared Discord history/privacy tests, the three-turn native fixture, the new
IO/worker tests and existing Slack/voice/session regressions. Both Python runner
validation tests pass. No live phone calls or Discord messages were sent.


### Discord daemon installation

`serve` captures the existing Discord query handler plus authenticated bot HTTP
identity after gateway READY. Once the shared phone runtime starts, the daemon
registers a Discord `PhoneAgent` with that same query handler, installs its private
phone tools, recovers eligible accounts and starts the transcript worker on the
daemon cancellation token. A registration guard lasts until daemon shutdown.
One-shot commands and dry runs do not install this path. Failed Discord phone
installation leaves the existing text/approval service running.

Installation requires an explicit phone owner link, the matching Discord
allowlist identity, and a stored owner/control-chat binding. Wrong-bot DM accounts
and other owners are excluded. It does not invent bindings. Private number
commands now reach the service directly through the tool server's trusted chat
entry point, before agent history/inference. Discord DM model commands receive
the same authenticated origin and refuse changes that would leave a bound native
session; legacy guild voice model guards are preserved.

The startup audit found that Slack's global recovery could reset an active
Discord send. Both adapters now recover only their own accounts. The regression
proves that recovering Slack leaves an in-flight Discord row in `sending`.

Test-first evidence: `/tmp/1496-discord-install-red.log`,
`/tmp/1496-discord-recovery-red.log` and
`/tmp/1496-discord-model-boundary-red.log`. Targeted green logs:
`/tmp/1496-discord-install-model-green.log` and
`/tmp/1496-discord-install-private-green.log`. The installation fixture exercises
private set → spoken-code verification → normal call through the installed query
handler with fake telephony, rejects a forged owner, and proves replay creates
no second call. It explicitly completes the verification call before starting
the normal call; the earlier attempt is rejected by the one-active-call limit.
There are no real Discord HTTP sends, model calls or Twilio calls in this fixture.

Discord initial identity/control-chat provisioning remains unfinished: this
checkpoint enables the path only for preexisting trusted bindings. WhatsApp,
disable-at-start cleanup and native-session continuity with phone disabled,
streaming/late-output revocation, final provider durations, tool/approval
qualification, production preset inventory, both-OS CI and live qualification
remain required. Legacy conversational voice is still installed pending those
gates.

The required offline runner passes 29 suites / 459 tests, including all 36 shared
outbox tests, the installed Discord private setup/call fixture, native DM model
switch protection, and prior Slack/voice regressions. Receipt:
`/tmp/1496-discord-install-offline/receipt.json` (dirty worktree at execution).
Both Python runner validation tests pass. No live calls or Discord posts were
made; real deployment and live acceptance remain unqualified.

### Revocation during an active answer

The phone bridge now revalidates the persisted call authority before mirroring a
completed native answer and again before returning it for speech. The Relay
socket checks authority before dispatching queued turns, processing incoming
frames and emitting speech, plus its existing 250 ms lifecycle tick. A revoked
call cancels its worker, closes the socket and requests provider cleanup. A
correctly signed upgrade or valid setup for a revoked call also marks it for
watchdog cleanup; invalid signatures cannot change call state.

Three test-first regressions cover a same-timestamp owner rebind during a native
Claude/Codex answer, queued explicit/final speech after a rebind, and a signed
upgrade rejected after revocation. They assert no late assistant mirror or
speech frame, provider stop for the active socket, and no additional dial.
Unsigned upgrade rejection does not request a stop. Red evidence:
`/tmp/1496-late-reply-red.log`, `/tmp/1496-late-speech-red.log` and
`/tmp/1496-revoked-upgrade-red.log`.

These checks establish offline authorization boundaries, not live audible
interruption timing or recall of already delivered bytes. Initial Discord
identity/control-chat setup, WhatsApp installation, disabled-start cleanup,
provider duration accounting, tool/approval continuity and live qualification
remain required. Legacy voice remains installed until migration gates pass.

Validation: all 29 required offline suites / 462 tests pass in
`/tmp/1496-revocation-offline/receipt.json` (dirty worktree at execution). Both
Python runner checks pass. Phone all-target Clippy with `--no-deps -D warnings`
passes; including dependencies still fails on eight existing errors in unchanged
store code (`/tmp/1496-revocation-clippy.log`). No live calls or chat posts were
made for this checkpoint.

### First-time Discord private setup

Discord installation now includes the bot-specific DM account when the operator
has explicitly linked the gateway-allowlisted owner in phone configuration. It
installs the adapter and delivery worker without creating any chat authority.
The owner's first `phone number set +<country code><number>` in an authenticated
DM atomically creates the owner binding and private control conversation before
requesting verification. Status and ordinary messages never create bindings.
No destination or code passes through the model for this setup.

The store initializer requires a never-bound account, using durable authority
revision history to reject previously revoked accounts even after restart. It
cannot replace an existing owner or control conversation. Repeated setup in the
same authorized chat preserves the authority revision. Concurrent first-time
chats have exactly one winner, with a single complete owner/control binding.
Existing or revoked account recovery remains a trusted operator operation.

The installation fixture now begins with no stored binding and exercises
set → verify → call through the installed WikiQuerier and fake telephony. It
rejects forged owners, public-channel setup, another DM and post-revocation
setup without extra provider creates. Same-event setup/call retries do not dial
again. Store tests cover reopen after revocation, identity mismatch, stable
idempotence and competing initial chats. Red evidence:
`/tmp/1496-discord-bootstrap-store-red.log` (missing store API) and
`/tmp/1496-discord-bootstrap-install-red.log` (real installer rejected the empty
store). Targeted green: `/tmp/1496-discord-bootstrap-install-green.log`.

This closes the pre-populated database requirement for initial Discord DM setup.
It does not qualify live Discord/Twilio behavior or complete WhatsApp integration,
disabled-start cleanup, final duration accounting, tool/approval continuation,
remote OS checks or the required live migration gates.

All 29 required offline suites / 464 tests pass:
`/tmp/1496-discord-bootstrap-offline/receipt.json` (dirty worktree at execution).
Both Python runner validation tests and phone all-target Clippy with
`--no-deps -D warnings` pass. No live calls, Discord posts or provider credentials
were used in this checkpoint's tests.

### WhatsApp setup privacy before phone installation

The existing WhatsApp control surface previously treated phone controls as
ordinary queries. It now reserves them before the query/approval parser and
returns a generic unavailable reply while the phone adapter is uninstalled.
The live channel also drops private number controls before durable email/inbox
storage and triage when no control adapter consumes them. The same shared
command classifier protects the independent archive importer: setup entries
are omitted from searchable history and incremental wiki capture, while the
cursor advances over their original archive positions. Ordinary history keeps
its original message identities and remains importable after restart.

Test-first evidence: `/tmp/1496-wa-privacy-red.log` (five controls reached the
query handler), `/tmp/1496-wa-inbox-red.log` (private setup reached triage), and
`/tmp/1496-wa-archive-red.log` (two private entries were imported). Green targets:
`/tmp/1496-wa-lib-green.log`, `/tmp/1496-wa-privacy-green.log`, and
`/tmp/1496-wa-archive-green.log`. The archive regression checks search, wiki
capture, restart, and subsequent ordinary messages. These tests use temporary
stores and a local Unix socket; they do not contact WhatsApp or Twilio.

This checkpoint does not enable WhatsApp calling. Authenticated owner/self-chat
handling, durable outbound echo suppression, native session/tool integration,
phone command execution, daemon installation and live qualification are still
required. It prevents unavailable phone controls from falling into ordinary
model/history paths while those adapters are being built. Previously imported
history is not rewritten by this change.

Validation: all 32 required offline suites / 519 tests pass in
`/tmp/1496-wa-privacy-offline/receipt.json` (dirty worktree at execution).
Both Python runner validation tests pass. No live calls or chat messages were
sent, and live acceptance remains unqualified.

### Durable WhatsApp echo classification

The Go sidecar now records each generated outgoing message ID in a private
SQLite ledger before sending it, and supplies that same ID to whatsmeow. Records
are keyed by account/chat/message ID and survive restart and uncertain outcomes.
Every message event carries a nullable agent-outbound classification. Missing or
failed storage, and events at/before the persisted ledger creation second, are
unknown rather than owner-authored. The ledger contains no message text.

Rust decodes missing/null classification as unknown. The new self-chat predicate
requires a positively classified non-agent message, `from_me`, the exact linked
owner's personal chat and sender, nonempty ID/text, and no edit/revoke flags.
This predicate is a transport check, not owner authorization. Existing outbound
filtering remains in place until the authenticated phone adapter and replay
handling can use it safely. This does not yet enable WhatsApp calling.

Test-first evidence: `/tmp/1496-wa-echo-red.log` and
`/tmp/1496-wa-self-red.log` show the missing APIs. Go tests cover durable
reservation/reopen, account/chat scoping, duplicate rejection, private file mode,
unknown-on-storage-failure and actual event wire classification. The Rust test
covers valid self-chat and rejected echo, missing/null classification, wrong
sender/chat, group, empty input, edit/revoke and wrong linked account type.
Go race tests and vet pass; the sidecar is built locally for the offline compiled
process contract. The existing both-OS sidecar workflow runs those Go tests.
The phone workflow now also triggers on archive importer changes.

Remaining work includes using this boundary in authenticated WhatsApp command
execution, owner/control provisioning, replay protection, native sessions,
durable transcript delivery and daemon installation, plus the shared lifecycle,
provider-duration, tool/approval, remote CI and live qualification gates.

Validation: all 33 required Rust offline suites / 520 tests pass in
`/tmp/1496-wa-echo-offline/receipt.json` (dirty worktree at execution). The compiled
Go-process contract passes with `/tmp/1496-wa-sidecar`, without skipping for a
missing binary. Go `test -race` and `vet`, WhatsApp all-target Clippy with
`--no-deps -D warnings`, and both Python runner checks pass. Logs:
`/tmp/1496-wa-echo-green.log`, `/tmp/1496-wa-echo-contract.log`,
`/tmp/1496-wa-echo-vet.log`, `/tmp/1496-wa-echo-clippy.log`.
No real WhatsApp messages or phone calls were sent; remote CI and live acceptance
are not claimed by these local checks.

### WhatsApp self-chat phone commands

An installed WhatsApp control adapter can now pass verified owner self-chat
phone controls directly to the shared tool server, before generic query/triage.
Its linked identity must come from authenticated sidecar status; the account key
is `device:<linked number>`, the sender is the bare owner JID, and the control
conversation must be that owner's self-chat. Every command checks the explicit
phone owner link and stored control binding. This adapter does not provision
those bindings or infer owner links from messages.

Echo/unknown classification, wrong sender, edits/revocations and non-self input
cannot execute phone commands. The existing outbound allowlist and control
feature flag must permit execution; the channel also blocks all phone commands
in dry run. Rejected inputs do not consume legitimate provider event IDs.
Authorized private commands commit a durable receipt before executing. Receipts
contain provider identifiers and time only, never bodies, destinations or codes.
Duplicates after restart return a status-check instruction without re-executing.
An interrupted command stays consumed rather than automatically retrying an
uncertain action; the owner can check status and send a new command if needed.

Test-first evidence: `/tmp/1496-wa-command-store-red.log` and
`/tmp/1496-wa-command-red.log` (missing receipt/adapter APIs), plus
`/tmp/1496-wa-command-dry-red.log` (the event queue dialed in dry run). The green
fixture in `/tmp/1496-wa-command-green.log` uses the real channel event queue,
control handler, local fake sidecar, temporary database and shared PhoneService
with fake telephony. It exercises setup → verify → call → remove, replay after
reopen, stable receipt counts, revoked authority, disabled/allowlist/dry-run
zero-call gates, rejected echoes/forged inputs, and redacted replies. Private
setup entering the reasoner would panic the fixture.

This is library integration, not daemon installation or live WhatsApp calling.
Initial WhatsApp identity/control provisioning, ordinary native text/phone
session continuity, tool/approval continuation, durable transcript delivery and
prompt event dispatch remain required. The shared provider lifecycle, final
duration, remote OS CI and live qualification/migration gates also remain open.

Validation: all 34 required offline suites / 522 tests pass in
`/tmp/1496-wa-command-offline/receipt.json` (dirty worktree at execution).
WhatsApp all-target Clippy with `--no-deps -D warnings` and both Python runner
checks pass. No live WhatsApp messages or Twilio calls were made; the test's
provider and local sidecar responses are scripted.

### WhatsApp ordinary text and native phone session continuity

An installed self-chat adapter now routes authenticated ordinary messages through
`QueryHandler::answer_turn` with a trusted phone origin. Echo/unknown events,
wrong owners, disabled controls and revoked bindings cannot enter this route.
Dry run also excludes ordinary self-chat execution. The adapter retains
Discord's `owner_authorized = false`: phone access now relies on the transport
scope and server grant, which revalidates explicit owner/control authority,
rather than inheriting Discord-specific authorization. No scope still means no
phone tools.

The real WikiQuerier recognizes WhatsApp origins and uses the same native turn
queue, durable turn claims and provider session binding as PhoneAgent. Its
identity checks require the linked `device:<number>` account, exact owner
self-chat, matching `wa:<chat>:<event>` identity, and no Discord audit context.
Discord and WhatsApp share the lower native execution helper after their
transport checks. It snapshots authority revision and checks again after queue
wait and after the answer; the WhatsApp adapter also checks before reply chunks.

Test-first logs `/tmp/1496-wa-native-control-red.log` and
`/tmp/1496-wa-native-red.log` reproduce ignored ordinary text and missing phone
tools. `/tmp/1496-wa-native-late-red.log` reproduces a late native reply returning
after a same-timestamp rebind. Green logs:
`/tmp/1496-wa-native-control-green.log` and `/tmp/1496-wa-native-green.log`.
The CLI fixture runs real WikiQuerier/PhoneAgent with fake native providers for
both Claude and Codex: text → three phone turns → text, five invocations on the
original native ID, phone tools present, no repeated history, duplicate text
rejected, invalid event/revoked identity rejected, and an additional late answer
suppressed after rebind. The channel fixture checks scope, dry run, echo rejection
and late reply suppression. Phone conversation IO in the CLI fixture is a stub;
it is not evidence of WhatsApp transcript delivery.

Remaining WhatsApp work includes initial identity/control provisioning, durable
transcript delivery/history integration and actual daemon installation with
prompt event dispatch. Approval/tool continuation beyond phone-tool availability
and the shared lifecycle, provider duration, remote OS CI and live qualification
requirements remain open. No replacement voice stack is retired yet.

Validation: all 35 required offline suites / 523 tests pass in
`/tmp/1496-wa-native-offline/receipt.json` (dirty worktree at execution), including
the existing Discord native-session and legacy voice regressions. WhatsApp
all-target Clippy with `--no-deps -D warnings` and both Python runner checks pass.
No real model, WhatsApp or Twilio calls were made for these fixtures.

### Durable WhatsApp phone transcript IO

`WhatsappPhoneIo` now queues PhoneAgent transcript messages in the shared outbox
and wakes delivery. It checks the authenticated linked owner, explicit phone
link and stored direct self-chat binding. Its history source is injected and
bounded; history errors propagate rather than silently replacing context.
Messages are split into bounded WhatsApp parts and inserted atomically, with
stable local delivery keys and a hash of the complete message. Repeating the
same key/content is idempotent; changed content or authority cannot append a
partial replacement. Queue claims are scoped to the WhatsApp account and phone
mirror prefix.

The actual WaClient transport rechecks authority revision, linked identity,
phone grant, outbound allowlist and control flag before sending. Confirmed
NotPaired/NotConnected failures may retry with bounded backoff; BadRequest is
permanent. Lost replies, timeouts, empty IDs and other uncertain results enter
reconciliation without another send. Restart recovery also leaves interrupted
sends for reconciliation. Unknown outcomes remain visible in the outbox.

The generic reconciliation interface accepts positive delivery evidence, but the
current WhatsApp sidecar does not yet expose lookup by the local delivery key.
The production transport therefore cannot automatically resolve unknown posts;
they remain parked. Extending the sidecar protocol with durable delivery-key
lookup is required before this part is complete. Local keys are not claimed to
provide provider-side deduplication.

Test-first logs: `/tmp/1496-wa-delivery-red.log` and
`/tmp/1496-wa-mirror-red.log`. Green tests cover atomic multipart reopen/retry,
changed-content rejection, revoked binding, interrupted-send recovery, positive
fake reconciliation and inconclusive parking. A real WaClient with a local fake
sidecar checks send gates, successful posting and unknown SendFailed handling,
with no extra requests on retry. The CLI native-session fixture now uses real
WhatsappPhoneIo rather than a transcript stub and verifies six durable owner/
assistant mirror rows after three phone turns and store reopen. Its history
fixture still requires an existing native session; this does not qualify a live
history source or real WhatsApp delivery.

Daemon registration, delivery worker, initial WhatsApp owner provisioning,
filtered history, sidecar reconciliation, tool/approval continuation and the
shared lifecycle/duration/live migration gates remain unfinished.

Validation: all 36 required offline suites / 527 tests pass in
`/tmp/1496-wa-delivery-offline/receipt.json` (dirty worktree at execution).
WhatsApp all-target Clippy with `--no-deps -D warnings` and both Python runner
checks pass. No live WhatsApp messages or Twilio calls were made.

### WhatsApp delivery-key reconciliation

Phone transcript transport now supplies a stable delivery key and expected
linked account to the sidecar. The sidecar atomically reserves key/content hash/
message ID together with echo identity before the provider send. Concurrent
requests for one key allow exactly one sender. Reused keys never launch another
send, changed content is rejected, and successful provider responses are
persisted before the Unix-socket reply. A lost reply can therefore be recovered
through `lookup_delivery` using the same account/chat/key after restart.

The Rust transport checks current owner authority and WhatsApp send gates before
lookup, and resolves its outbox row only from a positive persisted confirmation.
An unknown key, unconfirmed attempt, old sidecar, failed storage or failed lookup
remains inconclusive and does not cause resend. A crash between provider success
and the ledger commit can still remain uncertain. A successful send response is
not proof that the owner read the message.

Test-first logs: `/tmp/1496-wa-key-red.log` (missing durable-key APIs) and
`/tmp/1496-wa-lookup-red.log` (the transport did not send keys or perform lookup).
Go tests cover reopen with an unfinished reservation, content conflict, wrong
provider ID, account scoping, persisted positive lookup through the real event
connection while disconnected, and concurrent reservation with one winner.
Rust's actual WaClient transport fixture resolves an initially unknown send via
lookup without increasing the number of send requests. Go `test -race` and vet
pass; WhatsApp Clippy with `--no-deps -D warnings` passes.

This replaces the prior missing-sidecar-lookup limitation. Daemon delivery
worker/registration, initial WhatsApp provisioning, filtered live history,
tool/approval continuation, shared lifecycle/final-duration work and remote/live
migration qualification remain required.

Validation: all 36 required Rust offline suites / 527 tests pass in
`/tmp/1496-wa-lookup-offline/receipt.json` (dirty worktree at execution). Go race
tests and vet, the compiled Go/Rust sidecar contract, WhatsApp all-target Clippy
with `--no-deps -D warnings`, and both Python runner tests pass. Evidence logs:
`/tmp/1496-wa-key-green.log`, `/tmp/1496-wa-key-vet.log`,
`/tmp/1496-wa-lookup-contract.log`, `/tmp/1496-wa-lookup-clippy.log`.
No live WhatsApp messages or phone calls were made. Remote OS checks and live
acceptance are still unqualified.

### WhatsApp phone delivery worker

The WhatsApp phone outbox now has an account-scoped async worker. It drains only
phone mirror rows, performs bounded sends and reconciliation, responds to wake
notifications, and polls due retries once per second when idle. Cancellation has
priority over an in-flight operation; dropping that request leaves the durable
sending row for startup reconciliation rather than requeueing it for resend.
Account recovery remains an explicit daemon startup step before the worker is
spawned, so a running sender is not reset by another worker's initialization.

Test-first evidence: `/tmp/1496-wa-worker-red.log` (missing worker API).
`/tmp/1496-wa-worker-green.log` covers cancellation while a transport is blocked,
reopen plus account recovery, positive reconciliation without another send, and
ordinary outbound rows remaining queued/untouched. The worker exits within the
fixture's one-second timeout. The real WikiQuerier/PhoneAgent fixture now drains
all six transcript rows from its three phone turns through this worker using a
fake transport, while retaining the original Claude/Codex native session.
Targeted evidence: `/tmp/1496-wa-worker-native-green.log`.

This worker is not yet started by `serve`. Actual WhatsApp startup/status and
identity provisioning, agent/history installation, prompt event dispatch,
logout cleanup, shared lifecycle/duration fixes, remote CI and live migration
qualification remain outstanding. The tests send no real chat messages or calls.

Validation: the 36-suite required offline run passes 528 tests in
`/tmp/1496-wa-worker-offline/receipt.json` (dirty worktree at execution). After
strengthening the ordinary-outbox isolation assertion, all five delivery tests
were rerun successfully in `/tmp/1496-wa-worker-green.log`. WhatsApp all-target
Clippy with `--no-deps -D warnings` and both Python runner checks pass. No live
WhatsApp messages or phone calls were made.


### WhatsApp first-time private setup

The authenticated self-chat adapter now initializes the private owner binding
on an explicit `phone number set` command, using the operator-configured owner
link and linked device identity. Ordinary messages do not create bindings.
Disabled control, dry run, denied outbound access, unknown/agent echoes and
forged senders are rejected before initialization. Previously revoked accounts
still require trusted operator reprovisioning; repeating setup does not restore
access or change an existing binding revision.

The existing end-to-end fake-sidecar command test now starts without any stored
owner binding. It failed with zero verification calls before the fix
(`/tmp/1496-wa-bootstrap-red.log`) and passes the setup, verification, calling,
replay, removal and revocation sequence afterward
(`/tmp/1496-wa-bootstrap-green.log`). Service coverage additionally rejects
unlinked, disabled, mismatched-device/sender/chat and threaded setup and checks
idempotency and revocation. WhatsApp daemon installation and live qualification
remain required; this is not evidence of deployed WhatsApp support.

Validation: all 36 required offline suites / 529 tests pass in
`/tmp/1496-wa-bootstrap-offline/receipt.json` (dirty worktree recorded).
Targeted phone/WhatsApp all-target Clippy with `--no-deps -D warnings` and both
Python runner checks pass. No live provider calls or chat posts were made.


### WhatsApp daemon installation and reconnect supervision

`serve` now installs WhatsApp phone control when the shared endpoint, wiki root,
control feature flag and explicit WhatsApp owner link are present. The sidecar
socket connection checks the peer UID before reading status or events. Startup
requires connected/paired status, a personal numeric linked identity matching the
configured owner and an allowed outbound self-chat. No sender text establishes
the linked device identity. Pairing and outbound allowlist provisioning remain
operator tasks; this installer does not implement the separate WhatsApp login
CLI.

The installed adapter shares one query handler between self-chat and PhoneAgent,
registers the WhatsApp phone router, recovers only its account's durable outbox
and starts transcript delivery. It dispatches verified self-chat messages through
bounded tasks with separate stop capacity, so blocked native turns cannot hold
up phone stop. Existing native scheduling still serializes conversational turns.
Disconnect/shutdown durably request call cleanup; logout or identity changes
also revoke stored owner authority. A five-second reconnect loop recreates the
transport without replaying requests, redialing or restoring revoked bindings.
Registration, delivery and command tasks are released on connection exit.

The initial phone history source is the imported self-chat archive, limited to
40 rows, bounded row sizes and 64 KiB output. Setup/code commands are filtered,
and other accounts/conversations are excluded. This is not live provider history;
the sidecar's `fetch_history` is still unavailable. Existing native sessions do
not reload archive history. Approval handlers are passed through when available,
but this does not prove complete WhatsApp approval-card delivery/continuation.

The installation fixture first failed for the missing installer API
(`/tmp/1496-wa-install-red.log`). The fake sidecar exercises rejection of an
unlinked status identity, an actual installed call command, stop while a query is
blocked, reconnect with no redial, logout revocation and bounded shutdown.
Additional cases validate status and private/scoped/bounded archive history.
The fixture uses fake telephony and local sockets; it sends no real WhatsApp
messages or calls. This integration does not close live qualification, approval
continuation, provider lifecycle/limit gaps or legacy voice retirement.

Validation: all 37 required offline suites / 532 tests pass in
`/tmp/1496-wa-install-offline/receipt.json` (dirty worktree recorded). The final
installer/history target passes in `/tmp/1496-wa-install-complete-tests.log`.
The compiled Go process/Rust wire contract passes without skipping in
`/tmp/1496-wa-install-sidecar-contract.log`. WhatsApp all-target Clippy with
`--no-deps -D warnings` and both Python runner checks pass. CLI Clippy completes
with warnings outside the new module; this is not a warning-free workspace
claim. No live chat messages or provider calls were made.

### Immediate Twilio stop requests and CI inventory corrections

Twilio cleanup now posts a terminal status before any provider status lookup.
The persisted answer timestamp selects cancellation for unanswered calls and
completion for answered calls. A definite HTTP 400 / Twilio 21220 invalid-state
response permits one attempt with the other terminal status, handling a call
advancing during cancellation. Timeouts, redirects, server errors and other
rejections stay uncertain and do not trigger an immediate second mutation.
Only after both terminal mutations are rejected does a GET reconcile an
already-ended call; the SID must match and the status must be terminal.
Each stop-related HTTP request has a five-second timeout. This removes the
preliminary read from the deadline path; it does not guarantee provider/network
latency or close the outstanding final-duration and startup/shutdown gaps.

Provider references: [Call resource](https://www.twilio.com/docs/voice/api/call-resource)
and [21220 invalid call state](https://www.twilio.com/docs/api/errors/21220).
The regression first failed when a failed preliminary GET prevented the POST
(`/tmp/1496-hangup-post-red.log`). The transport tests cover immediate POST,
answer-state races, uncertain outcomes, and terminal reconciliation. The existing
watchdog test also checks that persisted answer state reaches the provider while
retaining its one-second cross-call deadline assertion.

Draft PR #1497's initial CI exposed missing inventory entries for `phone-tool`
and the `attach_tools` options wrapper, plus a secret-scanner false positive on
a synthetic verification-key fixture. Both inventories now describe the actual
capability and reference its conformance tests without claiming live parity.
The fixture is an obvious all-zero test placeholder; no scanner rule was relaxed.
The capability inventory suite is now mandatory in the phone offline runner.
The same pinned release scanner reports zero findings after the fixture change.

Validation: all 38 required offline suites / 545 tests pass in
`/tmp/1496-hangup-offline/receipt.json` (dirty worktree recorded). The full phone
crate suite and strict targeted Clippy pass; the eight macOS inventory tests and
two runner checks pass. The pinned release scanner reports zero findings on the
updated release tree in `/tmp/1496-secret-scan/final.log`. Remote checks need a
new run on the pushed commit; old-head failures are not described as passing.

### Provider duration and late budget updates

Signed terminal callbacks now persist immutable provider `CallDuration` seconds
and the signed event `Timestamp`. Late callbacks can finalize usage after local
cleanup; exact replays are idempotent and conflicting durations are rejected.
Daily accounting allocates the reported interval across UTC day boundaries,
rather than assigning usage to the day a delayed callback arrives. Calls that
may have connected but lack final usage retain their full duration reservation.
The watchdog also rechecks the current daily budget, so late usage from an older
call can shorten an active call's remaining allowance.

Store regressions cover restart, additive migration, missing answer callbacks,
late/replayed/conflicting usage, invalid values and midnight allocation. Service
regressions cover signature validation, malformed final usage and the shortened
active-call deadline. This does not implement REST reconciliation for missing
final callbacks; those calls remain conservatively reserved.

### Cross-platform and notification CI corrections

Phone transcript delivery is registered as an origin-bound reply in the
notification inventory, and the required offline runner includes all seventeen
notification registry/routing tests. The preset permission test compares the
canonical wiki path, accounting for macOS `/var` aliases. The Codex bridge keeps
Linux pidfd lifetime tracking and uses its own parent relationship on systems
without pidfd support. A subprocess regression removes pidfd support and proves
that parent exit terminates the bridge even while its input pipe stays open.
These local checks do not replace the required macOS CI or live audio receipts.

### Phone shutdown inside the daemon drain budget

The phone watchdog now drains for at most 16 seconds, and all runtime tasks
share one 18-second shutdown deadline. The daemon statically requires that
deadline to remain below its 20-second process drain budget. This replaces the
previous 35-second watchdog / per-task 40-second waits, which could be cut off
by daemon exit before reporting failure. A stalled-provider regression first
failed at the 19-second test deadline, then passed with an error result, a
durable nonterminal stop request and rejection of new starts. Both runtime
integration tests pass (`/tmp/1496-drain-green.log`); the failing baseline is
`/tmp/1496-drain-red.log`. This proves bounded local shutdown reporting, not
provider-side hangup when the provider is unreachable. Disable-at-start cleanup
and the live graceful-stop receipt remain outstanding.

### Disabled or unavailable startup cleanup

The daemon now schedules cleanup when normal phone startup returns no endpoint,
including disabled configuration, missing wiki configuration and startup errors.
Stop intent is persisted before provider configuration is read; missing SIDs are
marked uncertain. The stop-only Twilio client needs account credentials but no
caller number, verification key, owner links or public ingress. Its create method
rejects locally. Cleanup reuses the shared watchdog, persisted retry limits and
bounded drain without constructing agent tools or a listener. Missing credentials
or failed cleanup produce a generic notice while unrelated daemon tasks continue.

The runtime regression verifies cleanup, no dialing and idempotent restart. The
transport regression verifies credential validation and local dial rejection.
The daemon regression covers dry-run nonmutation, missing credentials and durable
uncertain-call stop requests. Full live disable/restart receipts are still absent.

### macOS canonical paths and shared tool permissions

The macOS acceptance job progressed past bridge startup and exposed permitted
file/attachment reads denied by `/var` versus `/private/var` spelling, plus the
scope guard's reliance on GNU `readlink -m`. The bridge now normalizes only
verified root-owned macOS `/tmp`, `/var` and `/etc` aliases; model-controlled
symlinks still go through descriptor-based no-follow checks. Single-file Read
allowances normalize both their configured directory and incoming path without
granting Glob/Grep access. The shell guard uses Python canonicalization when
GNU `-m` is unavailable, preserves protected requested names through system
alias normalization, and compares attachment carve-outs using their `/tmp`
spelling while other roots remain canonical.

Local regression coverage simulates trusted alias spelling, checks rejection
of untrusted aliases and user symlinks, and runs the real guard without GNU
`-m`. The Linux capability inventory passes all nine tests; all 57 shell scope
guard cases pass. Actual macOS CI remains required before declaring parity.

### Deferred history at the shared conversation queue

Phone now resolves initial history after acquiring the same conversation queue
used by text and after rechecking the native binding. If an earlier queued text
turn established a native session, phone resumes it without fetching history.
An unavailable history source therefore cannot block that native resume. Loading
initial history is cancellable before the turn claim and model dispatch, and
phone authority is checked again before fetching. The owner transcript mirror
occurs after loading history, preventing the current utterance from entering its
own initial-history prompt. Existing text callers retain their supplied-history
API through the shared implementation.

All ten surface-turn tests and three phone-agent tests pass locally, including
Claude/Codex queued resume without a history fetch, cancellation during a pending
history load without a turn claim, native continuity and late authority changes.
The required offline runner includes both new regression cases. Full live tool
and approval continuity receipts remain outstanding.
