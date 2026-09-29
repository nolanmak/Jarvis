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
  `download_file` is implemented (#1293, see "Inbound files" below).

## Outbound delivery (#1294)

Code: `crates/augmentagent-channel-slack/src/delivery/` (`mrkdwn.rs`,
`split.rs`, `plan.rs`, `progress.rs`) and `HttpSlackWebApi::upload_file` in
`transport/web.rs`. The interactive surface in `serve` (#1287) delivers its
answers through it; the operator path is `augmentagent slack deliver`.

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
  fields"; sending it as a JSON object in a JSON body is **unverified**.
- Failure classes (dispatcher in `plan.rs`): rate limit / transient Slack
  error / HTTP 5xx → retried after `max(backoff, Retry-After)`; upload failed
  before completion → retried whole; timeout or lost connection on a post or
  completion → `reconcile` (never resent blindly); other Slack errors, HTTP
  4xx, bad file → `dead_letter`.
- **A turn with a hole is closed.** When a text or file part of a turn is
  dead-lettered or abandoned, every part of that turn not yet sent is
  abandoned and one notice (`turn:<id>:notice`, sent once, same
  conversation/thread) says ":warning: This answer could not be delivered in
  full. N of M parts arrived; the rest was not sent." It runs right after a
  part dead-letters and again at the start of every drain, so a restart in
  between still sends the notice exactly once.
- **[docs]** `chat.postMessage` allows "1 message per second to a specific
  channel" with bursts; the dispatcher does not pace itself and relies on
  429/`Retry-After`. Effective behaviour for a 10-part answer is
  **unverified** live.

### Reconciling a send whose outcome was lost

`delivery/reconcile.rs` (`SlackSendReconciler`, also usable as the store's
`SendReconciler`); the dispatcher runs it before every claim.

- Sources: https://docs.slack.dev/reference/methods/conversations.history,
  https://docs.slack.dev/reference/methods/conversations.replies (read
  2026-09-29). **[docs]** both take `channel`, `cursor`,
  `include_all_metadata` ("Return all metadata associated with this
  message", default false), `inclusive`, `latest`, `limit` and `oldest`
  ("Only messages after this Unix timestamp"); `conversations.replies` also
  takes `ts` (the thread's parent). Form-encoded and JSON are accepted; sent
  form-encoded. History returns "the most recent messages … first"; replies
  returns the parent first, then replies in order. Paging via
  `has_more` + `response_metadata.next_cursor`. Scopes: the `*:history`
  scopes already in the manifest. Rate limit: Tier 3 ("50+ per minute") for
  Marketplace and internal apps; non-Marketplace commercially distributed
  apps get 1 request per minute with `limit` ≤ 15 (not our case: Jarvis is a
  single-workspace internal app).
- That history messages carry the `metadata` object
  (`{event_type, event_payload}`) we set when `include_all_metadata=true`
  is **unverified** live; the fetched pages do not show the field in the
  response example.
- Query: `conversations.replies` for a part in a thread, otherwise
  `conversations.history`; `oldest` = the send's last claim time minus 60 s
  (host clock skew); `include_all_metadata=true`; 200 messages per page, at
  most 5 pages.
- Found (event type `augmentagent_delivery` and the part's key) → `sent`
  with that `ts`. All pages read, key absent, and the claim is at least 30 s
  old (settle window) → requeued for exactly one resend. Key absent inside
  the settle window → looked up again when it ends (not counted as a
  failure). Lookup error or page budget exhausted → failed lookup, retried
  after 5 s, 10 s, 20 s … (cap 5 min); after 5 failed lookups the part is
  dead-lettered, which closes the turn with the notice above, so a
  conversation is never held forever.
- Uploads: the `files.completeUploadExternal` result is not recorded when
  its reply is lost, and the file-share message carries none of our
  metadata, so an uncertain upload is treated as not delivered once the
  settle window has passed and uploaded again. Worst case: one duplicate
  file, never a missing one. `chat.update` parts are idempotent and simply
  redone.

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

## Inbound files (#1293)

Code: `HttpSlackWebApi::download_file` in `transport/web.rs`, the pipeline
in `crates/augmentagent-channel-slack/src/inbound.rs`, the shared policy in
`crates/augmentagent-docs/src/inbound.rs` (Discord's rules, moved there
unchanged and used by both surfaces) and the bounded converter lookup in
`crates/augmentagent-docs/src/lib.rs`. Not wired into `serve` yet
(#1287/#1288); the operator path is `augmentagent slack files fetch
--channel C… --ts TS [--json]`.

### Download

Sources: https://docs.slack.dev/reference/objects/file-object and
https://docs.slack.dev/messaging/working-with-files (read 2026-09-29).

- **[docs]** `url_private` and `url_private_download` "require an
  authorization header of the form: Authorization: Bearer A_VALID_TOKEN"
  with at least `files:read` (already in the manifest). Both example URLs
  are `https://files.slack.com/files-pri/…`. `url_private_download` is
  preferred, `url_private` is the fallback.
- **Host allow-list:** `SLACK_FILE_HOSTS = ["files.slack.com"]`, https on
  the default port only. Any other host (GovSlack, Enterprise Grid, a CDN
  Slack might redirect to) is **unverified** and refused until observed
  live; the list is one constant. Subdomain and suffix tricks, credentials
  in the URL and other ports are refused
  (`only_documented_slack_file_hosts_are_allowed_by_default`).
- **Redirects** are followed by hand (reqwest's automatic redirects are
  off for downloads), at most 3 hops. Every hop must be on the allow-list,
  otherwise the download fails with `FileHostRefused` before anything is
  sent there. The bot token is sent only to the host the caller named; a
  hop to a different allow-listed host is fetched without it. Whether Slack
  redirects file downloads at all is **unverified**.
- **Size cap while streaming:** a `Content-Length` over the cap is refused
  before the body; otherwise bytes are counted as they arrive and the
  transfer stops just past the cap. The destination is created exclusively
  with mode 0600 and removed on every error, timeout and cancellation.
- **Sign-in page:** an HTML answer for a file whose Slack `mimetype` is not
  HTML is refused (`DownloadRejected`, hint: `files:read`). That Slack
  answers a bad or under-scoped token this way is commonly reported and
  **unverified** here.
- Bounded by `DownloadLimits::transfer_timeout` (120 s, whole transfer
  including redirects) and the client's cancel token; 429 is retried after
  `Retry-After` like other calls. The rate-limit tier for file downloads is
  not documented on the fetched pages (**unverified**).
- Test hosts: `AUGMENTAGENT_SLACK_TEST_FILE_HOSTS` adds loopback
  `host:port` entries (plain http allowed for exactly those); anything else
  in it is an error. It exists only for the local mock in tests and QA.

### Pipeline

`inbound::prepare_inbound(api, text, files, &InboundOptions, &cancel)`
returns an `InboundMessage` with the same pieces Discord feeds the
reasoner: `prompt` from the shared `build_prompt` (`IMAGE:` markers, text
path list with `TRUNCATED`/OCR notes), `images`, `text_files`, plus
`accepted`, `rejected` and `rejection_notice()` (the shared "⚠️ skipped: …"
line) and `starts_turn()`.

- Types and limits are Discord's: images (any `image/*`), text/code by MIME
  or extension allow-list, PDF/DOCX/DOC through pdftotext/pandoc (+ OCR for
  scanned PDFs when `MISTRAL_API_KEY` is set), credential formats
  (`.env`, `.pem`, …) refused, 8 MiB cap for text/documents, text truncated
  to 1 MiB. Slack also caps images at 20 MiB (`MAX_IMAGE_BYTES`) because the
  download is streamed on our side; Discord does not apply that cap.
- Refused before any download: unsupported/credential types, a declared
  size over the cap, deleted (`mode: tombstone`), plan-hidden
  (`hidden_by_limit`), external files, Slack Connect files that need
  `files.info` (`file_access: check_file_info`), a missing download link,
  and more than 10 files per message. Download and conversion failures are
  reported per file ("couldn't download: timed out", "couldn't read the
  document: pdftotext is not installed …"); the other files still arrive.
- An attachment-only message (`subtype: file_share`, empty text) starts a
  turn; a message whose files were all refused and that has no text only
  gets the notice (`starts_turn() == false`).
- **Storage:** `<state dir>/slack-inbound/msg-XXXX/` (the shared
  `state_dir()`: `$XDG_STATE_HOME/augmentagent` or
  `~/.local/state/augmentagent`, same rule on Linux and macOS, never
  `/tmp`). The root is created 0700 and refused if it is a symlink or owned
  by another user; each message gets its own 0700 directory; files are
  0600. The directory is removed when the `InboundMessage` is dropped or
  `cleanup()` is called, on failure, and on cancellation. Retention beyond
  the turn is #995 and must change every surface together.
- **File names** come from `sanitize_filename`, a pure function (same
  result on both hosts): last path component only, `[A-Za-z0-9._-]` kept,
  everything else `_`, no leading dots, bounded length, and a two-digit
  per-message index prefix so names that differ only by case or Unicode
  normalisation never collide on a case-insensitive APFS volume. The
  original name is kept for the owner-facing summary.
- **Converters** are resolved by `augmentagent_docs::resolve_tool`, the
  same path Discord's pipeline now uses: the process `PATH` (set by
  `scripts/lib/launchd-install.sh` and the systemd units), then
  `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin` for a launchd
  job whose plist has no `PATH`. A missing tool is an immediate error with
  an install hint; a converter is killed after 120 s (`kill_on_drop`, also
  on cancellation).

### Remaining work for serve (#1287/#1288)

- Call `prepare_inbound` for owner `message`/`file_share` events after the
  owner check (#1286), post `rejection_notice()` in the thread, and skip the
  reasoner when `!starts_turn()`.
- The wiki-ask scope guard (`scripts/aa-wiki-scope-guard.sh` and its Codex
  mirror in `codex_tools.rs`) allows the reasoner's `Read` only on Discord's
  `/tmp/aa-{img,txt,doc}-*` names; a read-only carve-out for
  `<state dir>/slack-inbound/msg-*/NN-*` is needed when the dispatcher runs
  a Claude turn on these files.

## Voice clips and spoken replies (#1297)

Asynchronous voice only: a recorded clip in, an uploaded audio reply out,
in the same conversation and thread as typed turns. This is **not** live
voice; that stays an open blocker ([`SLACK-LIVE-VOICE.md`](SLACK-LIVE-VOICE.md),
#1298). Code: `crates/augmentagent-channel-slack/src/voice/`. Not wired into
`serve` yet (#1288).

### What Slack sends (file object, read 2026-09-29)

- Clips recorded in Slack carry `subtype: slack_audio` / `slack_video`,
  `media_display_type` and `duration_ms` **[docs]**; they arrive as
  `files[]` on a `file_share` message like any upload. The documented audio
  filetypes are `m4a`, `mp3`, `mp4`, `wav`, `ogg`, `webm` **[docs]**. Which
  MIME each client uses for a clip (`audio/webm` on desktop, `audio/mp4` on
  mobile are the expectation) is **unverified** against a live workspace;
  detection therefore uses the MIME type, then the clip subtype, then the
  Slack `filetype`/extension.
- Slack's own `transcription` field is not used: it depends on the
  workspace's plan and region and is not the shared speech stack.

### Inbound

`inbound::prepare_inbound_with_voice(api, text, files, opts, Some(&VoiceInbound), cancel)`:

- decodes `audio/webm|ogg|opus|mp4|m4a|x-m4a|aac|mpeg|mp3|wav|x-wav|wave|flac`
  and `video/mp4|webm|quicktime`; any other `audio/*`/`video/*` is skipped
  before download with "unsupported audio format (…); send m4a, mp3, wav,
  ogg, webm, flac or mp4";
- limits: 25 MiB (declared and enforced while streaming), 10 minutes
  (declared `duration_ms`, then the decoded length), 3 clips per message,
  `ffmpeg` 60 s, transcription 120 s; each limit is an owner-facing reason;
- downloads into the same private per-message directory as #1293, decodes
  to 16 kHz mono 16-bit WAV (the format the Discord sidecar streams to both
  vendors) and deletes the clip and the WAV as soon as the transcript is in;
- the transcript joins the typed text in `turn_text()` (and the prompt), and
  `transcript_notice()` is the line shown back to the owner
  ("🎙️ Transcript of clip.webm (0:04): …"). An empty transcript, a decode
  failure or a provider failure is a "skipped" reason telling the owner to
  resend or type; the turn starts only if something else is left.
- `prepare_inbound` (no speech stack) keeps #1293's behaviour: clips are
  "unsupported". Owner-only: call it only for input `owner::admit`
  dispatched (`tests/voice_clips.rs::non_owner_audio_never_reaches_the_speech_provider`).

### Speech providers: the seam, not a second stack

The Deepgram/ElevenLabs clients, provider selection
(`AUGMENTAGENT_DISCORD_STT_PROVIDER` / `_TTS_PROVIDER`, `DEEPGRAM_API_KEY`,
`ELEVENLABS_API_KEY`, `ELEVENLABS_VOICE_ID`) and the credit fallback live
only in the TypeScript sidecar (`sidecars/discord-voice`), bound to a live
Discord voice session over private IPC; nothing in Rust calls them and they
have no request/response mode for a recorded file. The Rust seam is
`voice::speech`:

- `SpeechToText` / `TextToSpeech` traits over a finished file or text;
- `SttStack` / `TtsStack` with the sidecar's rule: switch to the alternate
  vendor only on confirmed credit exhaustion (`402` or `quota_exceeded`,
  `ProviderError.exhausted`); any other failure is reported as is;
- `TranscriberStt` plugs in the existing Rust transcriber
  (`augmentagent_channel_voice::Transcriber`, whisper.cpp under
  `vendor/whisper`, used for Telegram voice memos). This is what `slack voice
  transcribe` uses in a release build.
- No TTS provider has a Rust file adapter yet, so spoken replies need one
  before they work outside tests: either a request/response `synthesize`
  op on the sidecar's IPC or a Rust port of its two TTS calls, reusing its
  env names and fallback. Tracked as remaining work on #1297.

`voice::fake::{ScriptedStt, ScriptedTts}` are offline fakes for tests; the
CLI selects them only in a debug build through
`AUGMENTAGENT_TEST_SLACK_SPEECH` (`ok:TEXT`, `fail:CODE`, `exhausted:TEXT`,
`exhausted-all`); release builds ignore it.

### Spoken replies

`voice::reply::enqueue_spoken_answer(store, conversation, &SpokenAnswer { turn_id, markdown, files, mode }, Some(&tts), &opts, now_ms)`:

- `ReplyMode` is per turn and defaults to `Text` (`"spoken"`/`"voice"`
  parse to `Spoken`); the harness sets it when the owner asks for a spoken
  answer;
- the text mirror is always the full answer (`turn:<id>:text:<n>`); the
  audio is the turn's first file (`turn:<id>:file:0`, WAV named
  `spoken-reply.wav`, raw PCM from a provider gets a WAV header), then any
  generated files;
- the speech text drops Markdown markers, code blocks and URLs and is cut
  at 12 000 characters (the sidecar's limit) with "The rest is in the text
  reply.";
- a restart of the same turn finds `turn:<id>:file:0` in the outbox and
  neither synthesises nor sends again;
- a TTS failure, timeout (30 s) or missing provider still delivers the text,
  with "_Spoken reply unavailable: <provider> failed (…)._";
- the audio is stored under `<state dir>/slack-voice-replies/<turn>-<hash>/`
  (0700/0600) until uploaded; `release_spoken_audio` removes it once the
  upload is sent, dead-lettered or abandoned. Whether Slack plays an
  uploaded WAV inline on every client is **unverified**.

### Host dependencies

| host | install | resolved under the service manager |
| --- | --- | --- |
| macOS Apple Silicon | `brew install ffmpeg` → `/opt/homebrew/bin/ffmpeg` | launchd `PATH` from `scripts/lib/launchd-install.sh`, else the `/opt/homebrew/bin` fallback |
| macOS Intel | `brew install ffmpeg` → `/usr/local/bin/ffmpeg` | same, `/usr/local/bin` fallback |
| Debian/Ubuntu | `apt install ffmpeg` → `/usr/bin/ffmpeg` | systemd `PATH`, else `/usr/bin` |
| Fedora | `dnf install ffmpeg-free` (or `ffmpeg` from RPM Fusion) → `/usr/bin/ffmpeg`; **unverified** here | same |

`ffmpeg` is resolved with `augmentagent_docs::resolve_tool`, like the
#1293 converters, and killed at its timeout or on cancellation. A missing
binary is the owner-facing reason "ffmpeg is not installed or not on the
service PATH (…); install it (`brew install ffmpeg` on macOS, `apt install
ffmpeg` or `dnf install ffmpeg` on Linux)". The local speech-to-text path
additionally needs whisper.cpp and its model (`scripts/build-whisper.sh`,
`vendor/whisper/`), resolved from the working directory as for Telegram
voice memos. CI uses a fake `ffmpeg` and scripted providers on both hosts.
Real launchd/systemd qualification of the audio dependencies is #1255.

### Remaining work (#1288 harness, #1297)

- Call `prepare_inbound_with_voice` instead of `prepare_inbound` for owner
  input, post `transcript_notice()` and `rejection_notice()` in the thread,
  and run `turn_text()` as the turn in the existing conversation.
- Decide how the owner requests a spoken reply (a per-turn control or a
  phrase) and set `ReplyMode::Spoken`; call `enqueue_spoken_answer` instead
  of `enqueue_answer`, then `release_spoken_audio` after the drain.
- Add a TTS file adapter (see above) and, for the daemon, configure the STT
  provider selection instead of the working-directory whisper default.

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
  a non-Slack download host refused without a request, recording fake.
- `tests/transport_upload.rs` (#1294): each upload step against a mock
  Slack; step 1 fails, transfer cut off midway, upload host 5xx, completion
  fails, size/empty/missing file, stalled transfer timeout and cancel, rate
  limits, insecure upload URL, no token on the upload host.
- `tests/delivery_format.rs`, `tests/delivery_outbox.rs`,
  `tests/delivery_progress.rs` (#1294): conversion table and mention
  neutralisation, splitting invariants, restart between parts, crash
  mid-send reconciled (found / not found / lookup errors / budget
  exhausted / uploads), dead-lettered or abandoned part closing the turn
  with one notice (also across a restart), rate limit mid-answer, upload
  failing midway,
  paused-clock progress throttling. `augmentagent-cli/tests/slack_deliver_cli.rs`
  runs `slack deliver` end to end.
- `tests/transport_download.rs` (#1293): bearer token only to allow-listed
  hosts, same-host / other-allowed-host / foreign-host redirects, redirect
  loops, `Content-Length` and streaming size caps, sign-in page, 429 retry
  and cap, HTTP errors redacted, stalled transfer timeout, cancellation,
  existing destination never overwritten, no partial file left behind.
- `tests/inbound_files.rs` (#1293): image + text + PDF + DOCX in one
  message reaching the Discord-shaped prompt, attachment-only message,
  rejections before download (oversize, unsupported, credential formats,
  deleted/external/Slack Connect/no link), a file larger than declared,
  truncation, missing and stuck converters, case-colliding and hostile
  names, too many files, download timeout and cancellation cleanup,
  private permissions, symlinked root refused.
  `augmentagent-cli/tests/slack_files_cli.rs` runs `slack files fetch` end
  to end; `augmentagent-docs/tests/converter_bounds.rs` pins converter
  lookup under a launchd-style PATH and the timeout.
- `tests/voice_clips.rs` and `tests/voice_replies.rs` (#1297): clip
  detection, Slack clip fields, transcript into turn text and the owner
  line, typed text plus images plus a clip, unsupported codec before
  download, undecodable audio, missing and stuck `ffmpeg`, size/duration/
  count limits, empty transcript, credit-exhaustion fallback and
  non-exhaustion failure, STT timeout and cancellation cleanup, non-owner
  audio never reaching the provider through `admit`; spoken reply as audio
  plus text mirror delivered once across a restart, text mode, TTS failure
  and missing provider notes, PCM to WAV, speakable text, audio retention.
  `augmentagent-cli/tests/slack_voice_cli.rs` runs `slack voice transcribe`
  and `slack voice speak` end to end.
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

`admit()` is the gate `serve` (#1287) puts in front of the turn handler: owner
input goes to an `OwnerInputSink`, rejections are audited before it returns.
Field names used for team and sharing checks come from the Events API,
interactivity and slash-command reference pages; like the rest of this file
they are unconfirmed against a live workspace (item 11 below).

## Interactive surface in `serve` (#1287)

Code: `crates/augmentagent-channel-slack/src/interactive.rs`, wired by
`crates/augmentagent-cli/src/slack_serve.rs`. Operator view:
[`SLACK-APP.md`](SLACK-APP.md) section 5.

- **Ack after persist.** The Socket Mode sink records every envelope with
  `record_inbound_event` (messages keyed `<channel>:<ts>`, mentions
  `mention:<channel>:<ts>`, everything else by `stable_id()`) and only then
  lets the client acknowledge it. A store failure leaves it unacknowledged.
  Duplicates are acknowledged and dropped.
- **Dispatcher.** Woken by each new row (fallback poll 5 s), it claims
  Slack's events only (`claim_next_inbound_event_for`) and runs up to four
  at once: the store never hands out a second event of a conversation or
  thread that has one claimed, so a slow turn in the DM does not hold up
  the control channel or a rejection, and one conversation stays in order.
  Each event: re-parse the
  stored frame, reloads the owner bindings and runs `owner::admit`. Owner
  input goes to a `SlackTurnHandler`; rejections are replied to (DM through
  the outbox, channel via `chat.postEphemeral`, which history cannot
  reconcile) and never reach the handler; ignored events are settled.
- **Replies.** `delivery::enqueue_answer` with the event ID as the turn ID,
  then `SlackOutboxDispatcher::drain` per workspace (dry run: sends are
  claimed and marked sent with `dry-run:<id>`). A failed turn posts one
  generic notice; the error stays in the log.
- **Restart and shutdown.** `recover_surface_delivery` runs at start.
  Shutdown during a turn drops the handler future and releases the event.
  A replayed event (`attempt > 1`) whose answer parts already exist is
  settled without calling the handler.
- **Health.** Listener states, last event and last send are written to
  `surface_listener_health` on every change and every 15 s. `status` treats
  a live state older than 60 s as `disconnected`.
- **Turn handler seam.** `SlackTurnHandler::handle_turn(&SlackTurn) ->
  Option<SlackTurnReply>` is text in, Markdown out. `serve` plugs in
  `QueryTurnHandler`, an adapter over the Discord crate's `QueryHandler`
  (`WikiQuerier::answer`, one shared-reasoner call) with an `AuditCtx`
  carrying a `slack:<account>:<event>` session ID, no Discord http, channel
  or guild, and `owner_authorized: false` (the WhatsApp precedent: that flag
  grants Discord-owner tools). #1288 replaces the adapter with the full
  harness (sessions, history, tools and permissions, follow-ups, progress).

Tests: `tests/interactive_surface.rs` (the real client, sink, dispatcher and
outbox over an in-memory WebSocket with `RecordingSlackWebApi`, a fake turn
handler and an hour-long fallback poll) and
`augmentagent-cli/tests/slack_serve_cli.rs` (the built binary, Slack only,
against a mock Web API and a local WebSocket).

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
12. #1294: whether history/replies return our `metadata` with
    `include_all_metadata=true`, and how quickly a just-posted message is
    visible there (the 30 s settle window assumes well under that); the
    unit of the 4,000-character text limit; `link_names` default;
    `metadata` accepted as a JSON object; the upload URL working without
    an `Authorization` header; the per-file size limit; a real 10-part
    answer and a PDF/PNG upload landing in a thread in order; a launchd-run
    daemon reading generated files from the shared state/temp location
    (#1256).
