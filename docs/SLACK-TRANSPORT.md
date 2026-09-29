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
  `im:write` (`conversations.open` for the owner's DM, #1286; from the
  method's reference page, not yet exercised live),
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
- `upload_file` is implemented (#1294, see "Outbound delivery" below).
  `download_file` still returns `WebApiError::Unsupported`; #1293 owns it.

## Outbound delivery (#1294)

Code: `crates/augmentagent-channel-slack/src/delivery/` (`mrkdwn.rs`,
`split.rs`, `plan.rs`, `progress.rs`) and `HttpSlackWebApi::upload_file` in
`transport/web.rs`. Not wired into `serve` yet (#1287/#1288); the operator
path is `augmentagent slack deliver`.

### Formatting and mentions

Source: https://docs.slack.dev/messaging/formatting-message-text (read
2026-09-29).

- **[docs]** `&`, `<`, `>` must be sent as `&amp;`, `&lt;`, `&gt;`. The
  converter escapes them everywhere, code included, so model text can never
  form a `<…>` control sequence: `<!channel>`, `<!here>`, `<!everyone>`,
  `<@U…>`, `<#C…>` and `<!subteam^…>` render as text.
- **[docs]** "Plain text `@channel` does not trigger notifications; the
  special syntax `<!channel>` is required." Defence in depth anyway: a word
  joiner (U+2060) is inserted after the `@` of `@channel`/`@here`/`@everyone`,
  and every post sends `link_names: false`. The `chat.postMessage` page
  describes `link_names` as "Find and link user groups"; its default is
  **unverified**, hence the explicit `false`.
- **[docs]** mrkdwn has `*bold*`, `_italic_`, `~strike~`, `` `code` ``,
  ```` ``` ```` blocks, `<url|text>` links, `>` quotes and no headings. The
  converter maps Markdown onto these (table in `mrkdwn.rs`); headings become a
  bold line and pipe tables a code block. Links are only built for
  `http(s)`/`mailto` URLs without `<`, `>`, `|` or whitespace.
- Code is verbatim apart from the entity escaping (which Slack renders back).
  Known limitation: a literal ```` ``` ```` inside a `~~~`-fenced block ends
  Slack's code block early.

### Message size and splitting

- **[docs]** `chat.postMessage`: keep `text` to 4,000 characters; Slack
  truncates above 40,000. `chat.update`: `text` "cannot exceed 4,000
  characters". Sources: https://docs.slack.dev/reference/methods/chat.postMessage,
  https://docs.slack.dev/reference/methods/chat.update (2026-09-29).
- Whether "characters" means Unicode scalars, UTF-16 units or bytes is
  **unverified**. Parts default to 3,500 scalars (`DEFAULT_PART_CHARS`), so
  even a part of only 4-byte characters (14,000 bytes) stays far below the
  40,000 truncation point.
- Splits prefer blank lines, then lines, then spaces, never inside an
  entity, a `<…>` link or an inline code span. A cut inside a code block
  closes the fence and reopens it in the next part. The parts' ranges tile
  the input exactly (property test over 400 generated inputs).
- Blocks are not used for answers (text only), so the 50-block limit does
  not apply.

### Multi-part delivery on the outbox

- One outbox entry per part, keyed `turn:<turn_id>:text:<n>` /
  `turn:<turn_id>:file:<n>`, all in the turn's conversation (thread
  included). Re-planning the same turn enqueues nothing new; the outbox's
  per-conversation ordering sends what is left, in order.
- Every post carries message metadata `{"event_type":
  "augmentagent_delivery", "event_payload": {"idempotency_key": …}}` so a
  send whose outcome was lost can be found in history. **[docs]** the
  `metadata` argument is "JSON object with event_type and event_payload
  fields"; sending it as a JSON object in a JSON body is **unverified**. The
  Slack `SendReconciler` that looks it up (`conversations.history` /
  `conversations.replies`) is not implemented yet; until it is, a part in
  `reconcile` holds the rest of its conversation and is visible in status.
- Failure classes (dispatcher in `plan.rs`): rate limit / transient Slack
  error / HTTP 5xx → retried after `max(backoff, Retry-After)`; upload failed
  before completion → retried whole; timeout or lost connection on a post or
  completion → `reconcile` (never resent blindly); other Slack errors, HTTP
  4xx, bad file → `dead_letter`.
- **[docs]** `chat.postMessage` allows "1 message per second to a specific
  channel" with bursts; the dispatcher does not pace itself and relies on
  429/`Retry-After`. Effective behaviour for a 10-part answer is
  **unverified** live.

### File upload

Sources: https://docs.slack.dev/reference/methods/files.getUploadURLExternal,
https://docs.slack.dev/reference/methods/files.completeUploadExternal,
https://docs.slack.dev/messaging/working-with-files (all read 2026-09-29).

1. `files.getUploadURLExternal` **[docs]**: `filename` and `length` (bytes)
   required, `alt_txt` (max 1,000 characters) and `snippet_type` optional;
   form-encoded or JSON; returns `upload_url` and `file_id`; scope
   `files:write`; Tier 4 ("100+ per minute"). Sent form-encoded.
2. POST the bytes to `upload_url` **[docs]**: "Files can be sent as raw
   bytes or can be multipart form encoded"; the example sends
   `Content-Type: application/octet-stream`. Sent raw, streamed from disk,
   with `Content-Length`, **without** the bot token (the docs do not say the
   URL needs one; **unverified**). The URL must be https (plain http only on
   loopback, for tests) and is never logged or echoed in errors. Any 2xx is
   success; the example body is `OK - <bytes>`.
3. `files.completeUploadExternal` **[docs]**: `files` = `[{id, title}]`
   required; `channel_id`, `thread_ts`, `initial_comment` optional; JSON
   accepted; Tier 4. "If not called, the uploaded file and associated
   metadata will be discarded", and it "may only be invoked once per
   upload". A failure in steps 1–2 is therefore reported as
   `WebApiError::UploadIncomplete` and retried from step 1 without risk of a
   duplicate.

- Size limit: the fetched method pages state none. `UploadLimits::max_bytes`
  defaults to 1 GiB (Slack's commonly cited per-file limit, **unverified**);
  larger files are refused before any request. Transfer timeout 300 s
  (`UploadLimits::transfer_timeout`), cancellable with the client's token.
- Slack derives the file type from `filename`; no content type is sent
  beyond `application/octet-stream` (**unverified** for every type).
- How long `upload_url` stays valid is not documented (**unverified**).

### Progress

`ProgressMessage` posts (or adopts) a status message and edits it with
`chat.update` at most once per `min_interval` (default 3 s) per message,
newest text wins, identical text is not re-sent, a rate-limited edit waits
`max(interval, Retry-After)`. **[docs]** `chat.update` is Tier 3 ("50+ per
minute"), per method per workspace. Progress edits are best effort and not
written to the outbox.

## Install-time checks (#1284)

- `auth.test` (bot token) returns `team_id`, `team`, `user_id`, `bot_id`
  **[docs]**. Granted scopes are read from the `x-oauth-scopes` response
  header, which Slack documents on Web API responses; that it is present on
  `auth.test` for bot tokens is **unverified**. When it is absent the CLI
  reports scopes as unknown instead of failing.
- The app-level token is checked with `apps.connections.open`. The returned
  one-time URL is dropped immediately; only its `app_id` query parameter is
  kept. Whether an unused ticket has any side effect is **unverified**.
- The manifest is `docs/slack-app-manifest.json`; its scope list must equal
  `REQUIRED_BOT_SCOPES` (`tests/app_manifest.rs`). `commands` is added to
  the scopes above for the `/jarvis` slash command.

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
  `augmentagent-auth` under `augmentagent/slack-app/<team_id>` (#1284,
  [`SLACK-APP.md`](SLACK-APP.md)).

## Tests (all offline, `cargo test -p augmentagent-channel-slack`)

- `tests/transport_events.rs`: envelope parsing, typed events, unknown
  kinds, malformed frames, token redaction.
- `tests/transport_web_api.rs`: bearer + JSON bodies, form-encoded lookups,
  `ok:false` errors, 429/`Retry-After` retry and caps, timeout, cancellation
  (including during a rate-limit wait), token never in Debug/errors,
  deferred file transfer, recording fake.
- `tests/transport_upload.rs` (#1294): each upload step against a mock
  Slack; step 1 fails, transfer cut off midway, upload host 5xx, completion
  fails, size/empty/missing file, stalled transfer timeout and cancel, rate
  limits, insecure upload URL, no token on the upload host.
- `tests/delivery_format.rs`, `tests/delivery_outbox.rs`,
  `tests/delivery_progress.rs` (#1294): conversion table and mention
  neutralisation, splitting invariants, restart between parts, crash
  mid-send reconciled, rate limit mid-answer, upload failing midway,
  paused-clock progress throttling. `augmentagent-cli/tests/slack_deliver_cli.rs`
  runs `slack deliver` end to end.
- `tests/transport_socket.rs`: ack only after hand-off, rejected and slow
  hand-offs not acked, redelivery with stable id, unknown/malformed frames
  keep the link, response payloads, refresh drain then reconnect, forced
  close with bounded jitter, transient connect backoff bounds, dead-socket
  heartbeat, healthy ping/pong, wall-clock jump, `link_disabled` and fatal
  auth stop, real `SlackConnector` over loopback (`apps.connections.open`
  mock + WebSocket handshake), secrets never in errors, plus the ignored live
  probe.

## Owner authority (#1286)

`augmentagent_channel_slack::owner` decides, per parsed envelope, whether
input is the bound owner's (`Owner`), never a turn (`Ignore`) or refused
(`Reject`). The binding is the shared store's `surface_owner_bindings` row for
the workspace account (`team:T…` or `enterprise:E…/team:T…`) plus
`surface_control_conversations` (the owner's DM with the app and, optionally,
one private control channel). Rejections go to `surface_auth_rejections`
with identifiers and a reason code only, never message text.

- Authority is the exact `(workspace, user ID)` pair. Display names,
  usernames, profile fields and emails are never read.
- The envelope's team must be bound and its enterprise must match exactly;
  a message or click whose acting user's team (`user_team`, `source_team`,
  `team`, `user.team_id`) is another team is rejected, so Slack Connect users
  and the owner's ID from another workspace carry no authority.
- The app's own posts, other bots, workflows and integrations, edits and
  unfurls (`message_changed`), deletions, hidden and system subtypes are
  ignored before any identity check, so the agent never answers itself.
- Messages start a turn only in a control conversation; chatter elsewhere is
  ignored. Non-owners in a control conversation or DM with the app are
  rejected. An externally shared control channel is rejected even for the
  owner.
- Interactions and slash commands are authorized by the acting user, not the
  channel.
- Every rejected user sees the same short text; no reply is offered for an
  unbound workspace, an enterprise mismatch or a payload without an actor.

`admit()` is the gate `serve` (#1287) puts in front of the harness: owner
input goes to an `OwnerInputSink`, rejections are audited before it returns.
Field names used for team and sharing checks come from the Events API,
interactivity and slash-command reference pages; like the rest of this file
they are unconfirmed against a live workspace (item 11 below).

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
9. `x-oauth-scopes` on `auth.test` for a bot token, and the manifest being
   accepted as-is by Slack's "From a manifest" flow (#1284).
10. A launchd-run daemon reading the `slack-app` Keychain item written from
    a terminal (#1246).
11. Owner authority inputs (#1286): `enterprise_id` / `enterprise.id`,
    `user_team` / `source_team` on Slack Connect messages, `user.team_id` on
    interactions, `is_ext_shared_channel` on Events API callbacks, and whether
    the app receives both `message` and `app_mention` for one post in a
    control channel.
12. #1294: the unit of the 4,000-character text limit; `link_names` default;
    `metadata` accepted as a JSON object; the upload URL working without
    an `Authorization` header; the per-file size limit; a real 10-part
    answer and a PDF/PNG upload landing in a thread in order; a launchd-run
    daemon reading generated files from the shared state/temp location
    (#1256).
