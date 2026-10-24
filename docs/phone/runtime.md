# Phone runtime and operator configuration

Implementation status: `serve` now installs the shared phone runtime and Slack
adapter when phone is enabled, configuration is complete, `--wiki-dir` is set,
and the daemon is not in dry run. Slack must also have an installed app and a
bound owner. The text handler and phone bridge share the same query handler,
native session scheduler and durable Slack outbox. The debug synthetic Slack
handler does not enable real phone commands.

The daemon also installs Discord after authenticated gateway READY when the
allowlisted owner has an explicit phone link for the bot DM account (or an
already-authorized guild control chat). It reuses Discord's existing query
handler and starts the durable transcript worker. In the configured owner's DM,
`my number is …` establishes the initial private control chat and saves the
owner-supplied destination. `phone number set +<country code><number>` remains
available for optional code verification. Ordinary messages and status
commands do not create bindings. Existing bindings are never replaced; revoked
accounts require trusted operator reprovisioning and cannot be restored by chat.
WhatsApp now installs when phone is enabled, an explicit WhatsApp owner link is
configured, and `AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED=1`. The paired sidecar
must run as the daemon user and report a connected personal device matching the
owner link. Its self-chat must already be in the outbound allowlist. The socket
uses the channel default, overridable with `AUGMENTAGENT_WA_SOCK`. The existing
sidecar must be running; this adapter does not pair or provision devices.

The WhatsApp supervisor reconnects after sidecar failures without redialing.
It marks active calls for shared watchdog cleanup on disconnect or shutdown;
logout or a changed linked identity also revokes the stored owner binding.
First-time number setup can establish a never-before-bound private control chat;
revoked bindings require trusted operator reprovisioning. Long-running text turns
cannot occupy the separate stop-command task capacity. Phone transcript delivery
uses the durable outbox, and reconnect recovers only this WhatsApp account.
For an unbound native session, phone history uses a bounded, filtered imported
self-chat archive. The sidecar's live history fetch remains unavailable.

Public TLS/provider setup, actual audio, approvals during a phone call and the
full live host matrix still need qualification. This is not a declaration of
production readiness.

## Configuration contract

Keep provider credentials and the verification key in the existing private
service environment (`.env` or the host's secret mechanism). Never put them in
chat, tool arguments, repository files or diagnostic output. The destination is
configured through authenticated owner chat and stored under the canonical
owner; it is not supplied by a `call_owner` argument or an environment fallback.

In a linked Discord control chat, `call me` asks for a missing destination.
Reply with the number or `my number is …`; a 10-digit number defaults to US +1,
while other national formats prompt for an explicit international country code.
The pending call intent lasts 15 minutes in that same conversation. A combined
`my number is …` followed by `call me`, or `call me at …`, saves and calls at once.
Number entry is intercepted before model/history processing and replies do not
echo the full number. This flow records `owner_supplied` provenance; it does not
claim a verification code was completed. Owner binding, call acceptance, rate
limits and duplicate-call protections still apply. Private DM entry is optional.

| Variable | Meaning |
| --- | --- |
| `AUGMENTAGENT_PHONE_ENABLED` | `1` enables configuration; absent or `0` defaults off. Other values fail configuration. |
| `TWILIO_ACCOUNT_SID` | Twilio account SID. |
| `TWILIO_AUTH_TOKEN` | Twilio API authentication and callback-signature secret. |
| `TWILIO_PHONE_NUMBER` | Operator's voice-capable Twilio caller number, in E.164 format. This is **not** the owner's destination. |
| `AUGMENTAGENT_PHONE_PUBLIC_URL` | Stable public HTTPS origin, e.g. `https://phone.example.test`; no path, query, credentials or nonstandard port. |
| `AUGMENTAGENT_PHONE_BIND` | Local proxy upstream, default `127.0.0.1:8878`; must be a loopback IP with a nonzero port. |
| `AUGMENTAGENT_PHONE_VERIFICATION_KEY` | Separate secret of at least 32 bytes, used for keyed verification-code hashes. Keep stable across restarts; rotation invalidates outstanding challenges. |
| `AUGMENTAGENT_PHONE_OWNER_LINKS` | JSON array mapping authenticated surface identities to a canonical owner; 1–64 explicit links, at most 32 KiB. |
| `AUGMENTAGENT_PHONE_STT_PROVIDER` | ConversationRelay `transcriptionProvider`: `Deepgram` (default) or `Google`. Exact, case-sensitive; anything else fails configuration. |
| `AUGMENTAGENT_PHONE_STT_MODEL` | ConversationRelay `speechModel`. Default `nova-3-general` for Deepgram, `telephony` for Google. Must match `^[A-Za-z0-9._-]{1,96}$`. |
| `AUGMENTAGENT_PHONE_TTS_PROVIDER` | ConversationRelay `ttsProvider`: `ElevenLabs` (default), `Amazon` or `Google`. Exact, case-sensitive; anything else fails configuration. |
| `AUGMENTAGENT_PHONE_TTS_VOICE` | ConversationRelay `voice`. Default `UgBBYS2sOqTuMpoF3BR0-flash_v2_5` for ElevenLabs (voice ID with model suffix; a `-<speed>_<stability>_<similarity>` suffix is allowed), `Joanna` for Amazon; required for Google. Must match `^[A-Za-z0-9._-]{1,96}$`. |
| `AUGMENTAGENT_PHONE_DEBUG_EVENTS` | `1` adds `debug="debugging tokens-played speaker-events"` to the ConversationRelay TwiML so #1551 can record when tokens were actually played; `0` disables it. Absent defaults to on for debug (non-production) builds and off for release builds. Other values fail configuration. |
| `AUGMENTAGENT_PHONE_FAST_LAYER` | `1` answers call utterances with the fast conversational layer (#1546); absent or `0` (default) keeps the native agent answering every utterance unchanged. Other values fail configuration. |
| `AUGMENTAGENT_PHONE_FAST_PROVIDER` | Fast model provider when the layer is on: `anthropic` (default, `claude-haiku-5-5`, key `ANTHROPIC_API_KEY`) or `cerebras` (fast-tier Cerebras model, key `CEREBRAS_API_KEY`). Keys load per request from the service keyring, then the service environment. Other values fail configuration. |
| `AUGMENTAGENT_PHONE_MAX_TOOL_STEPS` | Tool calls the fast model may make per owner utterance, `1`–`8`, default `3`. Further calls get a typed refusal. Other values fail configuration. |
| `AUGMENTAGENT_PHONE_THINKING_SOUND_URL` | Optional audio played while a fast-layer tool runs longer than 700 ms with nothing spoken. Must be `https` on the same origin as `AUGMENTAGENT_PHONE_PUBLIC_URL`, without credentials; anything else fails configuration, even while the layer is off. Unset (default) never plays a sound. |
| `AUGMENTAGENT_PHONE_SILENCE_CHECKIN_1_SECS` | Seconds of silence before the first check-in ("Still there?"), default `12` (#1556). |
| `AUGMENTAGENT_PHONE_SILENCE_CHECKIN_2_SECS` | Seconds of silence before the second check-in ("I'll hang up in a bit if you're done."), default `25`. |
| `AUGMENTAGENT_PHONE_SILENCE_END_SECS` | Seconds of silence before the goodbye line and `end`, default `45`. Each of the three silence values must be a whole number in 5–300 and they must be strictly increasing; otherwise configuration fails with a typed `Silence` error naming the variable (never its value). |

Synthetic link example (replace identifiers with actual authenticated bindings):

```json
[
  {"platform":"slack","account":"team:T00000001","surface_owner":"U00000001","owner":"owner-main"},
  {"platform":"discord","account":"dm:100000000000000001","surface_owner":"100000000000000002","owner":"owner-main"}
]
```

The mapping does not create authenticated owner bindings or private control
conversations. Those must already exist in the surface's trusted setup. Never
link accounts by display name or by finding the same phone number. Account IDs
must match the surface adapter's canonical `SurfaceAccountRef` exactly. Discord DM
accounts are `dm:<bot-user-id>`; guild accounts are the numeric guild ID. The
Discord sender must match `DISCORD_ALLOWED_USER_ID`. Stored owner/control-chat
bindings must match the mapping; the daemon does not infer or overwrite them.
Restart after changing configured identity links or adding a new account.

## Listener and proxy contract

Terminate TLS at a stable reverse proxy/tunnel on the same host as the loopback
listener. Forward HTTP POST callbacks and WebSocket upgrades only for:

- `/phone/start/{call_id}`
- `/phone/accept/{call_id}`
- `/phone/status/{call_id}`
- `/phone/relay/{call_id}`

Preserve the path, form fields and `X-Twilio-Signature`; support long-lived WSS
connections. Signature verification uses the configured public origin, never
untrusted forwarded-host headers. The general agent/MCP endpoint is a separate
private Unix socket and must not be exposed by the proxy. Unknown paths return
404. An unsigned request to a real callback route must fail without changing
call state; it is not a successful provider-connectivity check.

The deployment runbook still needs the selected proxy's exact configuration,
certificate/renewal checks and actual externally signed HTTP/WSS receipts.
Provider setup must confirm a voice-capable caller number, permitted destination,
account verification restrictions and ConversationRelay enablement. The
successful earlier one-way test call is not evidence for Relay operation.

## Runtime integration and shutdown

The daemon loads `config::Settings`, builds it against the shared store, binds
the loopback listener, and calls `runtime::PhoneRuntime::start`. It installs the
private tool endpoint on the existing Slack and eligible Discord query handlers. Authenticated text turns
carry a trusted `PhoneOrigin`; the phone router resolves the persisted origin
and selects the registered platform adapter. Registration ends with that
surface's handler or daemon registration guard. A missing adapter fails closed rather than selecting another
conversation.

Slack speech transcripts and answers are enqueued on its existing outbox with
stable idempotency keys and a shared sender wakeup. Delivery retries do not
rerun the native turn or dial another call. The same outbox is recovered on
restart. The daemon supervises phone runtime health and drains it on shutdown.

Discord uses the shared outbox for transcripts, with account-scoped startup
recovery and its own cancellation-aware worker. Slack recovery is account-scoped
as well, so starting either surface does not reset the other's active sends.
The authenticated Discord command path handles private number set/verify/status/
remove before history/model inference. A normal call remains blocked until a
pending verification call has ended. Existing native DM bindings reject model
switches that would move the conversation into a different session.

Startup marks preexisting active calls stopped **before** handing the runtime
back to command adapters. It never redials them. The watchdog scans every 250 ms
while provider hangups run concurrently. All cleanup paths share per-call
in-flight guards and the store's three-attempt backoff. One slow call does not
block another call's deadline. Provider requests are bounded; the provider-side
maximum duration remains the last bound if the daemon dies.

`shutdown()` immediately rejects new starts, requests stops and drains cleanup.
Dropping the handle initiates the same cancellation. Cleanup has a bounded
16-second drain window; unresolved records remain durably pending and shutdown
reports failure, rather than claiming hangup succeeded. A missing provider SID
remains uncertain and cannot be automatically redialed. Runtime task failure
cancels the sibling listener/watchdog. `is_running()` supports daemon supervision;
`shutdown()` provides the final task result. The daemon logs phone failures without
propagating them into unrelated text/approval runners. All phone tasks share one 18-second shutdown deadline, inside the daemon’s
20-second process drain budget. A compile-time assertion preserves that ordering.
Pending records and provider-side caps remain the recovery mechanism. A full graceful-stop qualification is still required.

When normal phone startup is disabled or unavailable (including missing wiki
configuration or a listener/configuration error), the daemon marks persisted
active calls for cleanup and runs a bounded stop-only worker. This path requires
only `TWILIO_ACCOUNT_SID` and `TWILIO_AUTH_TOKEN`; it does not open ingress or a
tool socket, require a caller number or verification key, or enable dialing.
Missing credentials leave stop requests durable and emit a configuration notice;
unknown call SIDs remain uncertain. Text and approvals remain available. Dry run
performs neither cleanup mutations nor provider requests. Live disable/restart
qualification is still required.

## Offline validation

```sh
python3 scripts/phone-offline-tests.py
cargo check -p augmentagent-cli --locked
cargo clippy -p augmentagent-phone --all-targets --no-deps --locked -- -D warnings
```

The runner produces `target/phone-offline/receipt.json` and per-suite logs. It
rejects missing targets, zero/insufficient executed tests, skipped required tests
and nonzero exits. Entire integration targets must be unfiltered; explicitly
named CLI test modules may filter only unrelated tests in the shared binary. `.github/workflows/phone.yml` runs the required targets on
Linux and macOS. A workflow definition is not a passing CI run; neither offline
CI nor local sockets substitute for the issue's live acceptance matrix.

## Live qualification receipt format

No live receipt is included in the repository. Offline fixtures are synthetic
and must never be submitted as qualification. After opt-in owner participation,
keep a JSON receipt and its consented WAV recordings together in a private
directory outside the repository. Do not include destinations, verification
codes, credentials or raw private conversation text in the JSON. Run:

```sh
python3 scripts/phone-live-receipt.py /private/evidence/receipt.json
```

Version 1 requires these top-level fields:

- `version: 1`, `kind: "phone-live"`, `commit`: tested 40-character Git SHA.
- `owner_audio_consent: true`, `number_setup_verified: true`: observed live
  private setup, verification and subsequent call, not an operator assumption.
- `measurement`: `{ "method": "recorded_audio", "description": "..." }`.
  Explain capture placement, annotation method and clock alignment. The audio
  must include the owner's speech and audible received playback. Server text
  send/receive times do not measure end-to-end audible latency.
- `calls`: the measured call records described below.
- `approval`: `call_index` (zero-based), `session_id`, `pending_observed: true`,
  `executed_before_resolution: false`, `resolution: "approved"`, and
  `executed_after_resolution: true`, backed by the existing approval system's
  native event trace. Review that trace alongside the receipt.

Each call records `host` (`linux` or `macos`), `architecture` (`x86_64` or
`arm64`), `provider` (`claude` or `codex`), `origin` (`discord`, `slack` or
`whatsapp`), `voice`, `provider_settings` (including
`transport: "conversationrelay"`), `outcome: "completed"`, and
`hangup_confirmed: true`. The `sessions` array contains the native session IDs
for text before the call, every measured phone turn, and text after the call.
All must be the same. `read_only_tool` records `completed: true` and its
`session_id`; verify the invocation and result in the native event trace.

`audio` contains `path` (relative WAV path within the receipt directory) and
`sha256`. `turns` contains at least three objects with `owner_end_ms`,
`response_start_ms`, and Boolean `used_tools`. `interruptions` contains at least
one object with `owner_start_ms` and `playback_stop_ms`. All offsets refer to
the associated audio file, in milliseconds. Hashes, complete WAV frame data,
nonnegative finite offsets and audio bounds are checked. Reusing the same
recorded response sample does not add coverage.

The matrix requires both native providers on Linux and on macOS ARM and Intel,
with all three chat origins represented. At least 20 no-tool turns must yield
nearest-rank p95 ≤5000 ms. At least five interruption trials must each stop
audible playback within 1000 ms. Every call must include an interruption,
read-only tool and confirmed hangup; at least one call demonstrates approval
gating. These are structural/timing checks, not proof that annotations or
declared tool events are truthful. Review the audio and native traces before
accepting AC10, and retain the separate migration/rollback evidence for AC11–12.

## Missing final usage callbacks

Terminal calls with a known provider SID and no finalized usage are reconciled
through authenticated, read-only Twilio Call-resource requests. Completed-call
seconds and end time are applied through the same immutable validation as signed
callbacks. A provider-confirmed never-answered call releases its reservation only
when local state also has no answer evidence. Contradictory or incomplete results
leave the conservative reservation intact.

Reconciliation uses three persisted attempts with backoff, including across
restart, at most eight jobs alongside cleanup, and five-second request limits.
It never creates a call or requests another hangup. Existing signed usage wins
over a late REST response. A missing SID or exhausted attempts still requires
operator investigation; absence of final usage is not treated as zero cost.
The normal watchdog handles these reads; disabled-start cleanup can reconcile
calls it is retiring, but does not start solely to refresh old accounting.

Provider field semantics: [Twilio Call resource](https://www.twilio.com/docs/voice/api/call-resource).

## Inspecting stalled maintenance

The owner-authorized `phone_status` tool returns a `maintenance` object with
`provider_sid_known`, `cleanup_attempts`, `usage_attempts`, and
`final_usage_recorded`. This is a read-only snapshot: asking for status neither
retries provider requests nor resets durable retry limits. It contains no phone
number or verification code. The ordinary `phone status` command adds actionable notices for uncertain calls
without provider confirmation, issued cleanup retries, and exhausted final-usage
checks. Ask the agent for detailed call status to inspect the counters. Notices
do not claim the last reserved attempt has finished.

Attempt counters count reserved attempts, including a request still in flight.
Three means no further automatic attempts remain, not that the last attempt has
finished. For a nonterminal call with cleanup exhausted, investigate the call in
the provider console and terminate it there if necessary; allow signed callbacks
to reconcile local state. If the provider SID is unknown after an uncertain
create, do not infer that the call never existed and do not clear the active slot
to redial. Investigate provider request/callback evidence first. Terminal calls
without final usage retain conservative budget accounting even after usage
retries are exhausted. For a known provider ID, use the explicit recovery command below after
investigation. Unknown-ID association remains unresolved; no retry-reset or
manual active-slot clearing command is provided.

## Explicit provider reconciliation

An operator with access to the daemon database can request one bounded,
authenticated provider read even after automatic retries have been used:

```sh
augmentagent --db /path/to/daemon.db phone-reconcile --call-id <local-call-id>
```

Use the local call ID from phone status. The command reads the provider ID
already bound in the database; it does not accept a replacement provider ID or
a phone number. It needs `TWILIO_ACCOUNT_SID` and `TWILIO_AUTH_TOKEN`, but works
with phone dialing disabled and requires no caller number or public callback URL.
It issues no dial or hangup request and does not reset automatic retry counters.

A confirmed completed call supplies final usage and closes a still-active local
record. A confirmed never-answered call releases its reservation only when the
same database transaction observes no local answer evidence. Signed final usage
that arrives during the read takes precedence. The command prints `finalized`
or `already_finalized` as a JSON string and exits successfully for those outcomes.

Unknown provider IDs, nonterminal provider responses, and contradictory answer
evidence print a descriptive outcome and exit unsuccessfully without clearing
the active-call restriction. Provider failures/timeouts also leave that
restriction intact. A still-active provider call must be ended separately in the
provider console; this command only reconciles authoritative final evidence.
Do not guess a provider ID or delete local rows to enable another call.

## Native tool/session probe without telephone calls

To exercise the installed Claude and Codex CLIs through text → three phone
adapter turns → hangup → text, using disposable native sessions and fake
telephony:

```sh
cargo build --locked -p augmentagent-cli --bin augmentagent
AUGMENTAGENT_PHONE_PROBE_BIN="$PWD/target/debug/augmentagent" \
AUGMENTAGENT_GH_DISABLE=1 cargo test --locked -p augmentagent-channel-core \
  --test phone_native_live -- --ignored --test-threads=1
```

This opt-in probe makes paid model requests. Each turn must read a newly changed
synthetic file whose value is absent from the prompt, and all five turns must
retain one native session ID. The initial owner prompt must invoke `call_owner`
through the real MCP facade, return its persisted call ID and cause exactly one
fake provider create. It requires working native CLI authentication and a
freshly built CLI binary selected by `AUGMENTAGENT_PHONE_PROBE_BIN`.
No Twilio requests, real phone calls, or chat sends occur. CI compiles the probe
without running it. Passing it establishes native tool/session compatibility
through the shared phone adapter, not audible performance, live ingress,
approval gating, or production transport qualification.


## Persistent local CLI conversation

Configure `AUGMENTAGENT_PHONE_LOCAL_SOCKET` in the daemon and CLI environments
with an absolute Unix socket path in a directory owned by the daemon user and
mode 0700. The daemon creates the socket mode 0600; both processes must run as
the same OS user. The path must fit the host's Unix socket length limit. The
socket's `.lock` sibling is retained to serialize startup and stale recovery.

Add an explicit owner link alongside your chat links, replacing `1000` with
`id -u` and `owner-main` with the same canonical owner used by your other links:

```json
{"platform":"cli","account":"uid:1000","surface_owner":"uid:1000","owner":"owner-main"}
```

With phone enabled, valid provider/TLS settings, and `serve --wiki-dir` running,
restart the daemon after adding this configuration. It initializes the local
private conversation once. Revoked bindings require trusted operator recovery;
restart does not restore them. No CLI listener is started unless the socket is
configured and the exact OS-user owner link exists.

Use `augmentagent wiki ask --daemon --stdin` and enter the prompt through stdin
(then EOF). This uses the daemon's wiki and the persistent `local` conversation,
so later invocations and accepted phone turns reuse the same native session.
Number setup/verification/status/removal commands work through this path and
bypass the model and transcript. `phone history` returns bounded recent text and
phone transcripts from durable local storage. Calls continue in the daemon after
the client exits. A lost response is not automatically retried; inspect phone
status before requesting another action. `--daemon` cannot be combined with
`--post`. Ordinary one-shot `wiki ask` retains its previous behavior.

The local adapter passed a synthetic Claude-provider session test through signed
loopback WebSocket ingress, three accepted phone turns and a local listener
restart. This is not an installed native CLI or live telephone qualification
receipt for the local origin.


### Recovering a missing provider ID

An operator can supply a candidate Twilio SID from the provider console:

```sh
augmentagent phone-reconcile --call-id <local-call-id> --provider-id <candidate-call-sid>
```

Use the original `AUGMENTAGENT_PHONE_PUBLIC_URL` for that call alongside the
Twilio account credentials. The command first requires terminal provider state,
then reads up to 100 call events and requires a POST to the exact local call's
start, acceptance or status URL, with matching account and call IDs. Only then
may the store bind the previously unknown SID and run normal final-usage
reconciliation. A concurrent different SID or another call's existing SID cannot
be replaced. The command performs no creates, hangups or retry-counter resets.

Twilio documents that [call events](https://www.twilio.com/docs/voice/api/call-event-resource)
become available 15 minutes after the call ends and include the application's
request URL, method and parameters. The event body is bounded to 1 MiB and never
printed or logged, because it can contain private numbers and verification
content. This path does not guess from destination/time similarity and does not
follow event-provided URLs or pagination links. If the first page has no exact
match, events are unavailable, the origin changed, or evidence conflicts, the
call stays unresolved. `unverified_provider_id` is not permission to redial.
Provider-event availability and this recovery path still require live
qualification; absence of evidence cannot establish that a call was never made.


### Database upgrade and rollback fixture

Run `python3 scripts/phone-upgrade-test.py` to exercise a temporary synthetic
database through pre-phone Store → current Store → pre-phone Store → current
Store. The baseline is pinned to `4caf3942414a3f499fcd2b4be539ee788cea1a65`;
the driver fetches that commit if absent, creates an isolated temporary worktree,
and uses separate build directories for old and current sources. It requires
Git, Cargo and access to dependencies. It does not open the deployed database
or contact a phone provider.

Logs and `receipt.json` are written to `target/phone-upgrade/` (override with
`--output`). Check `passed`, all four phase results, `commit`, and
`worktree_dirty` before citing a receipt. This checks retained sessions,
history, pending turns and approvals, including writes by the rolled-back Store.
It does not test installer/service rollback or authorize legacy voice removal.


## Conversational pickup and response recovery (#1526, #1527)

An owner-requested call now connects on the signed answer callback without a
keypress. Failure announcements use Polly.Joanna via plain `<Say>`. Optional
number-verification calls remain separate.

### Speech providers (#1545)

ConversationRelay TwiML pins the speech providers explicitly instead of relying
on Twilio account defaults. With no overrides it emits
`transcriptionProvider="Deepgram" speechModel="nova-3-general"
ttsProvider="ElevenLabs" voice="UgBBYS2sOqTuMpoF3BR0-flash_v2_5"
language="en-US" elevenlabsTextNormalization="on"`; the normalization attribute
is only present for ElevenLabs. The four `AUGMENTAGENT_PHONE_STT_*` /
`AUGMENTAGENT_PHONE_TTS_*` variables above are read once at startup and
validated to a conservative alphabet, so no value can break out of an attribute.
Chat text, tool arguments and model output never reach these attributes; a
source-level test holds `relay_twiml` to startup configuration only.

`hints` biases recognition toward the owner's first name (the stored
`first_name` profile fact, read by `Settings::build` at daemon startup; see
post-call memory below) and the fixed nouns `Jarvis,
Discord, WhatsApp, Slack, Linear, Notion`, XML-escaped and capped at 500
characters.

If Twilio rejects the provider, model or voice combination it sends an `error`
frame before the first prompt and closes the socket. The relay records that as
diagnostic category `setup_provider_rejected` (with the numeric code when one
is present) without logging the description, which can include the rejected
value, and no agent turn is created.

For a final transcript whose agent answer is still pending, the relay queues a
short acknowledgment after 250 ms (100 ms polling; tested within 500 ms) and one
progress notice after 10 seconds. These are server enqueue timings, not claims
about audible latency. A final answer or interruption removes pending notices.
After 120 seconds the agent receives cancellation, with up to five seconds to
clean up, then a spoken failure allows the next request without redialing or
replaying tools. Interrupted audio is invalidated immediately; chat persistence
runs separately and cannot close the socket on a slow/failing mirror.

Phone turns use the selected native Claude/Codex session directly, bypassing the
optional model gateway. Text routing, session identity and approval policy are
unchanged. Provider validation error 64107 is logged by numeric code and leaves
the authenticated relay open; invalid setup/identity and malformed frames still
fail closed. Diagnostics exclude raw transcripts and provider descriptions.

Offline regressions cover direct pickup, explicit voice, blocked-agent feedback,
slow interruption persistence, stale audio suppression, bounded cancellation,
recoverable provider errors and scoped native routing. Live audible greeting,
lookup completion and interruption behavior must be recorded separately; passing
these tests does not establish those live results.

## Fast conversational layer (#1546)

With `AUGMENTAGENT_PHONE_FAST_LAYER=1`, each final `prompt` is answered by a
small streaming model instead of a full native turn. It receives a fixed
persona prompt, the last 20 phone turns of this call, the last 10 lines of the
originating chat (bounded) and task updates from this call. Tokens are spoken
at sentence boundaries as they stream. The runtime acknowledgment (#1526)
becomes a fallback after 1.5 s without speech instead of 250 ms.

The model has exactly two tools, `dispatch_task(description)` and
`lookup_fast(query)`. Call authority (`authorize_call`) is re-checked before
every tool; any other tool name, a revoked call or a fourth call in one
utterance gets a typed refusal and the turn still ends with `last:true`.

- `dispatch_task` returns at once with a `task_id` (`{turn_id}:dispatch`). The
  work runs as a native surface turn on the originating conversation, with the
  same session, approvals and phone MCP grant (without call speech) as a text
  turn. Its result is queued once to the chat outbox under the task key; a
  replay after restart is refused by the durable turn claim and the outbox
  key. If the call is still live and idle, a short summary is also spoken as a
  system turn; after hangup nothing is spoken.
- A dispatched turn may report progress (`augmentagent_phone::fast::report_progress`):
  at most one event per 5 s and 6 per task, 140 characters each. The call
  speaks at most one progress line per 15 s. Progress is never posted to chat.
- `lookup_fast` scans the wiki's Markdown files read-only and is cut off at
  1.5 s.

Fast-layer turns are recorded with `provider = fast`; a turn that dispatched
work records outcome `dispatched`; tool time is added to `tool_ms`. Logs carry
only fixed category codes (for example `http_4xx`, `scripted_failure`), never
model text, tool arguments, chat text or keys.

## Per-turn latency metrics (#1551)

Every accepted turn (answered, interrupted, failed or dispatched) produces one
`tracing` event with target `phone.turn` and one row in the additive SQLite
table `phone_turn_metrics`. Fields are millisecond offsets from receipt of the
final `prompt` frame: `queue_wait_ms` (until the agent worker picked the turn
up), `t_first_token_ms` (first agent output), `t_first_speech_ms` (first
agent speech frame written to the socket; automatic "Got it" acknowledgments are
not counted), `t_last_speech_ms` (frame with `last:true` written), `tool_ms`
(sum reported through `SpeechSender::note_tool_time`; null when nothing was
reported), plus `provider` (`fast`|`native`, currently always `native`),
`outcome` and the build's Git commit SHA. A turn is recorded when the next
prompt arrives, on interruption, or when the connection ends, so a finished
answer that the owner talks over is stored as `interrupted`; a turn still pending
when the socket closes is stored as `failed`. The record contains no speech text,
numbers or identifiers beyond `call_id`/`turn_id`.

With `AUGMENTAGENT_PHONE_DEBUG_EVENTS=1` the relay also parses Twilio `info`
frames: a `tokensPlayed` event stamps `t_first_played_ms`/`t_last_played_ms`
for the turn currently being spoken and `audible_delta_ms =
t_first_played_ms − t_first_speech_ms`. Other `info` names and unknown frame
types are categorized as ignorable while debugging; they never count as invalid
frames and their bodies are never logged. With debug off those three fields are
null and every other field is unchanged. Twilio does not publish the exact
`info` payload schema; only the `name` field is read.

```sh
augmentagent --db /path/to/daemon.db phone metrics --last 10
```

prints nearest-rank p50/p95 per field over the last N calls (N most recently
recorded `call_id`s). Null fields are excluded from their sample set, so a
column's `samples` can be smaller than the turn count. The live acceptance
criteria (AC5, AC8 and the epic gate) require pasting this output with the
commit SHA after a real owner call; the offline suites only prove the record
shape, the ±5 ms paused-clock fixture and the hand-computed percentiles.

## Post-call memory (#1552)

When a call that reached a ConversationRelay session ends (signed
`relay-ended` callback, or a terminal status callback if that never arrives),
one durable job is queued in the additive table `phone_call_summary_jobs`,
keyed by call ID. Duplicate callbacks and restarts never add a second job;
calls that never reached the relay and verification calls get none.

The phone runtime checks the queue every 5 s. Each attempt takes a lease in
the store first (10 min, doubling; at most 3 attempts), so a crash mid-run
replays the job once after restart and a finished job is never rerun. One
attempt runs the originating conversation's heavy agent once (Claude CLI,
quality tier, 5 min limit) over that chat's bounded history, which already
contains the call's mirrored `Phone — you:` / `Phone — Jarvis:` lines. The run
has **no tools**: no allowed tools, a strict empty MCP configuration (host MCP
servers are not loaded), built-in tools denied and no phone grant, so it
cannot dial, message, dispatch or read files. It returns JSON; the phone crate
validates it and then:

- writes a summary note (at most 800 characters) to
  `<wiki>/phone/calls/<YYYY-MM-DD>-<first 8 of call ID>.md` (write then rename);
- upserts at most 10 facts (`key`, `value`, `confidence`, `source_call_id`)
  into the additive `owner_profile` table, keeping the agent's order;
- marks the job done in the same transaction as the facts.

Output that does not parse leaves the job pending for its next bounded attempt.
Logs carry only category codes, never summary or fact text.

Phone prompts then carry the owner context (data, not instructions): the
owner's first name, at most 1,200 characters of profile facts (most recently
learned first, cut only between whole facts; absent when there are none) and
the first line of each of the last 3 call summaries. The fast layer gets it in
its system prompt every turn; the native path prepends it to the first turn of
each call. A new first name reaches the recognition `hints` after the next
daemon restart.

Facts are owner-visible and deletable from the owner's **private control
chat** (same authorization as `phone number …`; other chats are refused, and
the text never reaches history or the model):

- `profile` (or `profile show`) lists the stored facts, most recent first,
  within 1,800 bytes (one message on every surface; the rest is counted);
- `profile forget <key>` deletes one; the next assembled prompt omits it.
  Forgetting `first_name` also removes it from prompts at once; only the
  recognition `hints` keep the startup copy until the next restart.

## Silence and idle policy (#1556)

A connected call with nobody talking no longer runs to the provider time
limit. The relay keeps a silence clock that starts at the latest of: any owner
`prompt` (partial or final) or `interrupt`, a closing `text` frame
(`last:true`) written by Jarvis, and a tokens-played event for that speech
(with debug events on). With the defaults:

| Silence | Frame |
| --- | --- |
| 12 s | `text` "Still there?" (`interruptible:true`, `preemptible:true`) |
| 25 s | `text` "I'll hang up in a bit if you're done." (same flags) |
| 45 s | fixed goodbye `text`, then `{"type":"end","handoffData":"{\"reason\":\"silence\"}"}` |

Check-ins and the goodbye never restart the clock, and neither does a
tokens-played event for a check-in. While an owner turn is still being
answered the clock is held. A spoken progress line of a dispatched task (#1546)
suspends silence handling for 15 s; the task's spoken result (or failure
notice) releases it and restarts the clock, and an expired hold restarts the
clock where it lapsed. Owner speech over a check-in is handled by the normal
interruption rules and restarts the clock.

When the relay ends for silence it persists `end_reason = silence`
(`phone_call_end_reasons`) before requesting the stop, then enqueues one
message in the originating chat ("Phone call ended after 45 seconds of
silence.") under the idempotency key `{call_id}:silence_end`. Delivery is a
leased job (three attempts, 60 s doubling lease) that the runtime's memory
worker retries after a failure or restart; the outbox rejects a replayed key,
so the message is never duplicated.
