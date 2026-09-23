# General browser research

Jarvis delegates interactive lookups to an independent `gpt-6-astra` worker
using the existing Codex login. The worker uses scoped MCP browser operations,
rendered text and screenshots. It attaches to the owner's existing Chrome
through the shared NewsletterBuddy bridge; it never launches a replacement
profile. No NewsletterBuddy research run is needed.

## Install on the Linux host

1. Run `npm ci` in this directory. Codex must already be signed in with access
   to `gpt-6-astra`; the worker does not silently substitute another model.
2. Set `NEWSLETTER_CHROME_SHARED_BRIDGE=true` for the newsletter browser
   worker (NewsletterBuddy PR #29). Both workers must use the same bridge
   directory and lock directory. Leave the bridge itself running to retain
   Chrome's consented upstream connection.
3. Link `systemd/augmentagent-computer-use.service` as a user unit with
   `systemctl --user link /absolute/repo/systemd/augmentagent-computer-use.service`. Adjust
   its absolute paths if this installation uses a different layout. Enable
   and start it. It creates an owner-only state directory, credential and Unix
   socket. Jarvis resolves its client path through the shared state-directory
   resolver; `JARVIS_COMPUTER_STATE` and `JARVIS_COMPUTER_SOCKET` override it.
4. Deploy the updated Jarvis binary. Authorized Discord conversations and
   direct local `wiki ask` invocations receive `computer_task`. Non-owner
   correspondents do not receive the tool. The parent cannot provide the
   trusted conversation identity or read the service credential through it.

Existing Chrome debugging consent is browser-enforced. If the bridge is not
ready, enable the existing-session debugging setting in
`chrome://inspect/#remote-debugging` and accept Chrome's connection dialog.
Do not restart Chrome, copy its profile, export cookies or reconnect directly
from each worker. A bridge disconnection can require fresh browser consent.

## Behavior and recovery

`start` accepts a complete goal and exact relevant hostnames. Flight lookups
also accept `flight` with displayed origin/destination city names and ISO
`departureDates`, enabling evidence checks. Identical starts in the same
trusted event reuse a task; distinct searches get distinct operation keys.
`status`, `cancel` and `resume` take `taskId`. Status polls wait at most 20
seconds and are read-only in the shared handoff harness, so they are never
replayed as stale mutation receipts. Polling returns bounded source summaries,
not the worker's full browser history.

The service persists task state atomically and holds a kernel singleton
lease. Tasks have a 60-action and five-minute wall-time budget; resumes retain
consumed actions, elapsed time and the original model. Cancellation fences
the old worker, kills its model subprocess group and closes only its task tab.
A browser lock is retained when cleanup cannot be established. Inspect the
selected bridge and lock owner before clearing an abandoned lock; do not
delete locks held by another worker. Operator input in a task tab pauses it.
Resume requires a new trusted owner event after the obstruction is resolved.
Restarted tasks report `needs_action` instead of claiming success.

Task text and metadata are private, with 24-hour retention for inactive tasks.
Screenshots are sent to the configured model in memory; only their hashes are
persisted. Evidence excludes HTTP headers/cookies, redacts email identifiers
and strips credential query parameters. Successful XHR/fetch response bodies
from assigned hosts are captured as bounded, sanitized `networkResponses`
(path only, no headers or query string) so fares are read off the wire rather
than only from painted text. Current native Codex transport reports
token usage when its turn finishes; action/time limits are enforced during
execution, while token usage is accounting, not a hard monetary cap.

The browser adapter allows navigation and interaction on assigned hosts.
Public subresources are fetched using validated, pinned DNS addresses and
normal TLS verification; redirects are checked again. Task documents cannot
start service workers, and task WebSockets/downloads/popups are blocked. New
task tabs bypass existing service workers. HTML sandbox policy prevents popup creation; task frames disable WebRTC and WebTransport. No global proxy or browser setting
is changed. Local/private destinations and arbitrary script execution are
unavailable through the tool.

Search POSTs require reviewed endpoint/RPC rules. Google Flights shopping,
booking-details retrieval and observed airport-search RPCs are supported;
unreviewed requests are denied with sanitized diagnostics. A denied request
does not imply every permitted navigation alternative will fail. Consequential
button labels and known mutation routes are refused, including keyboard
activation. These checks cannot prove the semantics of an arbitrary website's
GET endpoint; this is browser research, not a universal read-only guarantee
for hostile sites. Purchases, bookings, messages and account changes are not
supported by this worker.

## Model updates

`model-policy.json` defaults to `auto-validated` with `current: gpt-6-astra`.
The daily check intersects fresh native provider model metadata (a unique
visible image-capable flagship) with official OpenAI guidance. Ambiguity,
stale metadata, access failure or failed tests retains the current model and
records a reason. A candidate must pass deterministic browser/policy/lifecycle
tests and an actual-model flight/event fixture before promotion. The old
selection and validation revision are preserved. Running/resumed tasks keep
their snapshotted model.

To pin, set `mode` to `pinned` and `pin` to the validated model ID in the
owner-only policy file. To roll back, pin the stored `previous` ID. The service
reloads this policy on its next check (within one minute); task models already
chosen do not change. Upgrades never enable more tools or switch accounts.

## Verification

`npm test` runs deterministic tests using temporary Chrome profiles and
synthetic content. `LIVE_COMPUTER_TEST=1 npm test` additionally invokes the
configured Astra login against the synthetic flight and event fixture.
Never use personal browser profiles or raw authenticated captures as fixtures.
The updater installs dependencies and restarts an installed browser worker when its source changes.

Production QA is separate and must verify observed fares and dates, a second
general lookup, and cancellation/recovery. See Jarvis issue #1168 for the
acceptance matrix and recorded limitations.
