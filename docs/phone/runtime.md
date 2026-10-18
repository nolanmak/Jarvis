# Phone runtime and operator configuration

Implementation status: `serve` now installs the shared phone runtime and Slack
adapter when phone is enabled, configuration is complete, `--wiki-dir` is set,
and the daemon is not in dry run. Slack must also have an installed app and a
bound owner. The text handler and phone bridge share the same query handler,
native session scheduler and durable Slack outbox. The debug synthetic Slack
handler does not enable real phone commands.

Discord and WhatsApp adapters are not yet installed. Public TLS/provider setup,
actual audio, approvals during a phone call and the full live host matrix still
need qualification. This is not a declaration of production readiness.

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
  {"platform":"slack","account":"T-example","surface_owner":"U-example","owner":"owner-main"},
  {"platform":"discord","account":"guild-example","surface_owner":"user-example","owner":"owner-main"}
]
```

The mapping does not create authenticated owner bindings or private control
conversations. Those must already exist in the surface's trusted setup. Never
link accounts by display name or by finding the same phone number. Account IDs
must match the surface adapter's canonical `SurfaceAccountRef` exactly.

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
private tool endpoint on Slack's existing query handler. Authenticated text turns
carry a trusted `PhoneOrigin`; the phone router resolves the persisted origin
and selects the registered platform adapter. Registration ends with that
surface's handler. A missing adapter fails closed rather than selecting another
conversation.

Slack speech transcripts and answers are enqueued on its existing outbox with
stable idempotency keys and a shared sender wakeup. Delivery retries do not
rerun the native turn or dial another call. The same outbox is recovered on
restart. The daemon supervises phone runtime health and drains it on shutdown.

Startup marks preexisting active calls stopped **before** handing the runtime
back to command adapters. It never redials them. The watchdog scans every 250 ms
while provider hangups run concurrently. All cleanup paths share per-call
in-flight guards and the store's three-attempt backoff. One slow call does not
block another call's deadline. Provider requests are bounded; the provider-side
maximum duration remains the last bound if the daemon dies.

`shutdown()` immediately rejects new starts, requests stops and drains cleanup.
Dropping the handle initiates the same cancellation. Cleanup has a bounded
35-second drain window; unresolved records remain durably pending and shutdown
reports failure, rather than claiming hangup succeeded. A missing provider SID
remains uncertain and cannot be automatically redialed. Runtime task failure
cancels the sibling listener/watchdog. `is_running()` supports daemon supervision;
`shutdown()` provides the final task result. The daemon logs phone failures without
propagating them into unrelated text/approval runners. Its existing shared
20-second process drain budget can end before the standalone runtime’s
35-second cleanup window; pending records and provider-side caps remain the
recovery mechanism. A full graceful-stop qualification is still required.

Disable-at-start handling still needs daemon integration: turning the flag off
must reconcile existing calls while leaving text and approvals usable. Do not
claim the configuration parser's `None` result alone performs that cleanup.

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
