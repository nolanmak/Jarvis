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
`phone number set +<country code><number>` establishes the initial private
control chat and requests number verification. Ordinary messages and status
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
configured through private chat verification and stored under the canonical
owner; it is not supplied by a `call_owner` argument or an environment fallback.

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
