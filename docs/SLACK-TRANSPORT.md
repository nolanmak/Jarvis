# Slack transport decision record

Issue [#1283](https://github.com/nolanmak/Jarvis/issues/1283), part of the
Slack parity epic [#1281](https://github.com/nolanmak/Jarvis/issues/1281).
Baseline inspected: `ba3a742f770c5864e669340499d2bfddb129676a`.

## Decision

Jarvis talks to Slack as a **first-party Slack app over Socket Mode**, with
the client running **inside the `augmentagent` daemon process** (no sidecar).
Web API calls go through a typed, fakeable trait. The existing
Composio-backed ingestion (`api.rs`, `channel.rs`) is left unchanged for now
and reconciled later in #1296.

Code: `crates/augmentagent-channel-slack/src/transport/`
(`socket.rs`, `event.rs`, `web.rs`, `token.rs`, `backoff.rs`).

## Evidence status

Every statement about Slack's behaviour below was read from Slack's public
documentation on **2026-09-29** through an automated fetch and summary. None
has been verified against a live workspace yet. The live probe
(`live_socket_mode_hello_roundtrip`, opt-in, `#[ignore]`) exists but has not
been run. Treat each item marked **[docs]** as a hypothesis until the
"Remaining live verification" list is worked through in #1284/#1300.

## Why Socket Mode, in the daemon

| Option | Inbound port / public URL | Runs on a laptop Mac behind NAT | Extra process | Verdict |
| --- | --- | --- | --- | --- |
| Socket Mode (outbound WebSocket) | none | yes | no | **chosen** |
| Events API over HTTP | public HTTPS endpoint required | only with a tunnel/relay | tunnel | rejected: needs a public URL and a third party |
| Composio managed toolkit (current) | none | yes | no | rejected for real-time: polling only, no interactions, slash commands, modals, file events or message updates in its toolkit as used in `api.rs` |
| Sidecar process (e.g. Bolt in Node) | none | yes | yes | rejected: adds a managed service on both hosts (systemd + launchd), a Unix socket under macOS path limits and a second credential holder, for no capability the Rust client lacks |

Socket Mode limitations that matter for us and are acceptable:

- **[docs]** "Apps using Socket Mode are not currently allowed in the public
  Slack Marketplace." Jarvis is a single-workspace owner app; not relevant.
- **[docs]** "Socket Mode is only available for apps using granular
  permissions." New apps use granular permissions by default.

Source: https://docs.slack.dev/apis/events-api/using-socket-mode (checked
2026-09-29).

## Tokens and scopes

- **App-level token** (`xapp-…`): used only for `apps.connections.open`.
  **[docs]** The method page says "No scopes required" and that the token
  "must be passed in the HTTP `Authorization` header". The Socket Mode guide
  says the app-level token "allows your app … to generate a WebSocket URL".
  The `connections:write` scope is what the Slack UI attaches when creating
  an app-level token for Socket Mode; the fetched pages did not state it
  explicitly, so it is **unverified**.
  Source: https://docs.slack.dev/reference/methods/apps.connections.open
  (2026-09-29).
- **Bot token** (`xoxb-…`): used for every Web API call. Scopes needed by the
  epic, per method (all **[docs]**, 2026-09-29):
  `chat:write` (`chat.postMessage`, `chat.update`, `chat.delete`,
  `chat.postEphemeral`), `reactions:write`, `users:read` (`users.info`),
  `channels:read`/`groups:read`/`im:read`/`mpim:read` (`conversations.info`),
  `files:write`/`files:read` (#1293/#1294). `views.open` and `views.update`
  are listed with "No scopes required". Event subscriptions need the matching
  `*:history` scopes (`channels:history`, `groups:history`, `im:history`,
  `mpim:history`) plus `app_mentions:read`. Exact per-workspace behaviour is
  **unverified**.
- Both tokens are wrapped in `AppLevelToken` / `BotToken`, which redact in
  `Debug` and never implement `Display`. The one-time `wss://…?ticket=…` URL
  is treated as a secret and redacted from every error string.

## Socket Mode protocol as implemented

- **Connect:** `POST {api_base}/apps.connections.open` with
  `Authorization: Bearer <app token>`; response `{ "ok": true, "url":
  "wss://…/link/?ticket=…&app_id=…" }` **[docs]**. The client refuses a
  non-`wss://` URL unless `allow_insecure_ws(true)` (tests only).
  **[docs]** rate limit tier for `apps.connections.open`: Tier 3, "50+ per
  minute". Our reconnect backoff (1 s … 60 s, jittered) stays far below this.
- **Hello:** `{"type":"hello","connection_info":{"app_id":…},
  "num_connections":…,"debug_info":{"host":…,"approximate_connection_time":
  3600,…}}` **[docs]**. Parsed into `event::Hello`; sets
  `ConnectionState::Connected`.
- **Envelopes:** `{"envelope_id":…,"type":"events_api"|"interactive"|
  "slash_commands","accepts_response_payload":bool,"payload":{…}}`
  **[docs]**. `retry_attempt` / `retry_reason` are accepted if present; the
  fetched Socket Mode page does **not** document them, they are known from
  Slack's official SDK sources and are **unverified**.
- **Acknowledgement:** send `{"envelope_id": "<id>"}` back, optionally with
  `"payload"` when `accepts_response_payload` is true **[docs]**. The client
  acknowledges **only after** the `SlackEventSink` has accepted the delivery
  (test `envelope_is_acknowledged_only_after_the_sink_accepts_it`). Rejected
  or timed-out hand-offs are never acknowledged.
- **Ack deadline:** the Socket Mode page fetched does not state a deadline.
  The Events API page says HTTP endpoints must respond "within three
  seconds"; whether the same window applies to Socket Mode acks is
  **unverified**. `SocketModeConfig::handoff_timeout` defaults to 2.5 s so
  the ack normally leaves inside three seconds.
- **Redelivery:** **[docs, HTTP Events API]** up to three retries: "nearly
  immediately", "after 1 minute", "after 5 minutes", with
  `x-slack-retry-num` and `x-slack-retry-reason`. Whether Socket Mode retries
  on the same schedule, and whether a retry keeps the same `envelope_id`, is
  **unverified**. The client surfaces every delivery with a stable id
  (`event_id` for Events API callbacks, documented as "globally unique across
  all workspaces"; `trigger_id` for interactions and slash commands) and a
  `seen_before` hint from an in-memory ring; durable dedupe is #1285.
- **Disconnect frames:** `{"type":"disconnect","reason":"warning"|
  "refresh_requested"|"link_disabled","debug_info":{"host":…}}` **[docs]**.
  `warning` and `refresh_requested` start a drain: in-flight hand-offs are
  given `drain_timeout` (3 s) to finish so their acks go out on the old link,
  then the client closes and reconnects with backoff. `link_disabled` stops
  the client with a fatal error (an operator has to re-enable Socket Mode).
- **Connection lifetime:** **[docs]** "you'll need to handle connection
  refreshes once every few hours"; `approximate_connection_time` (seconds)
  estimates it. **[docs]** up to 10 concurrent connections are allowed; the
  client opens exactly one.
- **Failure conditions:** **[docs, HTTP Events API]** subscriptions are
  disabled when more than 95% of deliveries fail within 60 minutes. Whether
  unacknowledged Socket Mode envelopes count is **unverified**; the client
  keeps acks flowing for unknown event types and malformed payloads it can
  still identify, so only genuine consumer failures go unacknowledged.
- **Event volume:** **[docs]** "30,000 per workspace/team per app per 60
  minutes"; excess triggers `app_rate_limited` events. Surfaced as
  `SlackEvent::Unknown { kind: "app_rate_limited", .. }` until a consumer
  needs more.
- **Ping/pong:** not mentioned on the fetched page. The client sends a
  WebSocket ping every `ping_interval` (15 s) and treats the link as dead
  when no frame of any kind arrives for `dead_after` (45 s).

## Web API as implemented

- **[docs]** Tokens go in `Authorization: Bearer …`; "Most write methods
  allow arguments with application/json attributes" and `chat.postMessage`
  accepts JSON bodies. Write methods send `application/json; charset=utf-8`.
  `users.info` and `conversations.info` are sent form-encoded because read
  methods take URL-encoded arguments; **unverified** whether they also accept
  JSON. Source: https://docs.slack.dev/apis/web-api/ (2026-09-29).
- **[docs]** Rate limiting: `HTTP 429 Too Many Requests` with a
  `Retry-After` header in seconds; limits are "per API method per
  workspace/team per app". Tiers: 1 = "1+ per minute", 2 = "20+", 3 = "50+",
  4 = "100+", plus "special". `chat.postMessage` is special: "no more than
  one message per second per channel" with burst allowance. Source:
  https://docs.slack.dev/apis/web-api/rate-limits (2026-09-29). The client
  waits `Retry-After` (cancellable) and retries up to `max_attempts` (3),
  unless `Retry-After` exceeds `max_retry_after` (30 s), in which case it
  returns `WebApiError::RateLimited` at once. Per-channel pacing is left to
  the outbox in #1285.
- **[docs]** Size limits: `chat.postMessage` text "limit … to 4,000
  characters", "Slack will truncate messages containing more than 40,000
  characters"; "up to 50 blocks in each message, and 100 blocks in modals or
  Home tabs"; `views.open` returns `view_too_large` above 250 kB. Sources:
  https://docs.slack.dev/reference/methods/chat.postMessage,
  https://docs.slack.dev/reference/block-kit/blocks,
  https://docs.slack.dev/reference/methods/views.open (2026-09-29).
  Chunking long answers is #1294's job; the client does not truncate.
- **[docs]** `views.open` needs a `trigger_id` that expires quickly
  (`expired_trigger_id`); the three-second window commonly cited was not on
  the fetched page and is **unverified**. Acks with a response payload for
  slash commands and interactions are the safe path and are supported.
- `chat.postMessage` returns `{ok, channel, ts, message}` **[docs]**;
  `PostedMessage { channel, ts }` is the message reference used everywhere.
- Every call is bounded by `request_timeout` (15 s) and by the
  `CancellationToken` bound with `HttpSlackWebApi::scoped`.
- `upload_file` / `download_file` are declared on `SlackWebApi` and return
  `WebApiError::Unsupported` in `HttpSlackWebApi`; #1293 and #1294 implement
  them (Slack's `files.getUploadURLExternal` /
  `files.completeUploadExternal` flow, **unverified**).

## macOS and Linux implications

- TLS: `tokio-tungstenite 0.21` with `rustls-tls-webpki-roots`, the exact
  crate version and TLS backend the workspace already resolves for serenity
  and the Discord voice bridge. No native-tls, no system OpenSSL. `reqwest`
  is the workspace build (`rustls-tls`).
- Sleep/wake: the monotonic clock (`CLOCK_UPTIME_RAW` on macOS,
  `CLOCK_MONOTONIC` on Linux) does not advance while the host sleeps, so a
  timer alone cannot notice a multi-hour suspension. On every ping tick the
  client compares wall-clock progress with monotonic progress and reconnects
  immediately when the wall clock jumped by more than `suspend_threshold`
  (60 s) (`wall_clock_jump_after_sleep_forces_an_immediate_reconnect`). If
  that heuristic misses (e.g. the clock is also stepped), the heartbeat
  reconnects within `dead_after + ping_interval` of awake time
  (`heartbeat_detects_a_dead_socket_without_tcp_timeout`). Neither path waits
  for a TCP timeout. Real-Mac confirmation is #1300.
- Network change: a new default route makes the old socket silent; the same
  heartbeat path covers it.
- No sidecar, so no Unix socket path, launchd job or systemd unit is added
  by this issue. The daemon's existing service files (#1245/#1299) cover it.
- No paths are used by the transport; credentials come from
  `augmentagent-auth` in #1284.

## Tests (all offline, `cargo test -p augmentagent-channel-slack`)

- `tests/transport_events.rs`: envelope parsing, typed events, unknown
  kinds, malformed frames, token redaction.
- `tests/transport_web_api.rs`: bearer + JSON bodies, form-encoded lookups,
  `ok:false` errors, 429/`Retry-After` retry and caps, timeout, cancellation
  (including during a rate-limit wait), token never in Debug/errors,
  deferred file transfer, recording fake.
- `tests/transport_socket.rs`: ack only after hand-off, rejected and slow
  hand-offs not acked, redelivery with stable id, unknown/malformed frames
  keep the link, response payloads, refresh drain then reconnect, forced
  close with bounded jitter, transient connect backoff bounds, dead-socket
  heartbeat, healthy ping/pong, wall-clock jump, `link_disabled` and fatal
  auth stop, real `SlackConnector` over loopback (`apps.connections.open`
  mock + WebSocket handshake), secrets never in errors, plus the ignored live
  probe.

## Remaining live verification (owner, test workspace)

1. App-level token scope (`connections:write`) and `apps.connections.open`
   response shape, including `app_id` in the URL.
2. Whether `retry_attempt` / `retry_reason` appear on Socket Mode envelopes,
   the retry schedule, and whether a retry reuses the `envelope_id`.
3. The acknowledgement deadline for Socket Mode and what happens to a late
   ack.
4. Whether Slack sends WebSocket pings itself, and how quickly a silent link
   is closed server-side.
5. `disconnect` timing: how long the old link stays writable after
   `warning` / `refresh_requested`.
6. Whether `users.info` / `conversations.info` accept JSON bodies.
7. Effective per-channel `chat.postMessage` limit and `Retry-After` values.
8. Real Mac sleep/wake and Wi-Fi change with the daemon under launchd
   (#1300), and a Linux host under systemd.
