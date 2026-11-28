# Spec: Jarvis web dashboard v2 (chat-first, Discord alternative)

Status: audited draft (rev 3) · Owner: nolanmak · Date: 2026-11-27
Audit: rev 1 went through spec-interrogation Passes A (fresh eyes), B (conversation drift) and C
(40 seams, 9 auditors). Rev 2 resolves the findings. See §11 for the audit log.

## 1. Problem

The owner talks to Jarvis mostly through Discord. The web dashboard (`src/dashboard.ts`, EJS
views) is an admin panel, and its `POST /api/ask` is not the brain that answers in Discord:

- `/api/ask` spawns a one-shot `augmentagent wiki ask --stdin` (`src/dashboardQuery.ts:8-74`). It has
  no session, no history, no model selection and fewer tools than the Discord path.
- `POST /api/v1/actions/:id` changes `actions.status` with a SQL compare-and-swap
  (`src/apiV1.ts:209-255`). The PWA `/queue` Approve posts `status: "sent"`
  (`views/queue.ejs:73`). The row says sent, and nothing was sent.
- `/queue` and `/api/push/*` are mounted outside `requireAuth` (`src/apiV1.ts:347`).
- Production binds `0.0.0.0:3000` (`AUGMENTAGENT_BIND_HOST=0.0.0.0` in the Legion `.env`), which
  reaches the whole LAN. The code's default is loopback.
- `jarvis.shipsystems.ai` (cloudflared → `127.0.0.1:3000`) sits behind Cloudflare Access at the
  edge. The app never checks the Access token.

Goal: one web app, usable from any device, that is a better place than Discord to talk to Jarvis.
It runs real Jarvis turns with session continuity and real approvals, behind serious auth. UX
direction: T3 Code (thread sidebar, chat in the middle, a context rail, a keyboard-first palette).

## 2. Decisions

- **D1. Web chat is a new surface on the existing turn pipeline.** A web turn runs through
  `run_surface_turn_with_history` (`crates/augmentagent-channel-core/src/surface_turn.rs`) with
  `SurfaceConversationRef::new(account="owner", conversation_id=<thread_id>, thread_id=None)` on
  platform `"web"`. `web_surface.rs` calls `run_surface_turn_with_history` **directly** with its
  own closures. Its `answer` closure is `QueryHandler::answer(&audit, &prompt)` on the daemon's
  `WikiQuerier`, the same call Discord's `answer_turn` ends in. Web does **not** reuse
  `WikiQuerier::answer_turn`, `ConversationScheduler`, `local_phone.rs` or
  `phone_runtime::native_text_turn`. Those are Discord-, CLI- or phone-specific, and they carry
  transcript mirroring, phone-tool grants and owner-binding gates that web doesn't want. The
  Discord-only native-session gate (`main.rs:12246`) therefore doesn't apply. *Why:* session
  continuity, idempotent turn claims, cancel and restart handling live in `run_surface_turn*` and are
  covered by `surface_conformance.rs`. Calling it directly is the smallest correct path (KISS)
  and touches no existing surface. Authority: `AuditCtx` with `owner_authorized = true`, `session_id =
  "web:<thread_id>:<turn_id>"`, no Discord http/channel. Approval-history context is treated as
  private (owner-authenticated, D6). Phone/dial tools aren't granted to web turns in v1 (§9). See T1.
- **D2. Node talks to the daemon over a new private Unix socket.** The security checks are copied
  from `local_phone_transport.rs`: the socket file is mode 0600, the parent dir is private, and the
  peer UID is checked. The framing is the same as there: **one newline-terminated JSON request, one
  newline-terminated JSON response, one request per connection**, **256 KiB** max per frame (a
  32 KiB text JSON-escaped is at most 192 KiB; an oversize request frame → `text_too_large`), and a
  10 s read timeout for the request. It is a separate socket with its own versioned ops (§5.2), because the
  CLI `Request` is `deny_unknown_fields` and pinned to `"local"`. *Why:* the daemon has no general
  HTTP API, and a Unix socket can't be reached from the network at all.
- **D3. The database is the only source of truth for web chat. The daemon is the only writer.**
  Tables are in §5.1. Node opens its own **read-only** SQLite connection for reads and sends every
  write as a socket op. *Why:* Discord text is never stored, so web needs its own record. One writer
  means no second home for any fact.
- **D4. Submitting is fast and running happens in the background. The browser learns about
  changes by refetching.** `turn.submit` validates the turn, records it and returns right away. The
  daemon runs it in the background. Node watches `web_turns.updated_at` and tells SSE clients
  "thread X changed". The browser then refetches the thread. No turn result travels over the socket
  or SSE. *Why:* every failure is then visible before the HTTP response (validation, offline). Node
  restarts, daemon restarts and slow turns all recover the same way: re-read the database. There is
  no token streaming in v1 (§8).
- **D5. Web approvals go through the real handler.** Node sends `action.resolve` over the socket.
  The daemon builds its own `SyncingActionHandler::new("web", approver.clone(), surfaces.clone())`
  around the **same** `ReplyApprover` and surface set that `serve` gives Discord (`main.rs:16934`).
  Origin `"web"` makes `redraw_except` redraw the Discord **and** Slack cards (`sync.rs:79`) and
  records the decision as coming from the web. The action really executes. Exactly-once is
  `Store::try_resolve_action`. *Why:* the current SQL flip records outcomes that never happened.
- **D6. One public door: Cloudflare Access, checked again in the app.** When
  `AUGMENTAGENT_CF_ACCESS_AUD` is set, `requireAuth` (the existing chokepoint) admits a request
  only if it has (a) a valid `Cf-Access-Jwt-Assertion` header or (b) a valid API key
  (`Authorization: Bearer` / `x-api-key`, for machine clients such as
  `crates/augmentagent-cli/src/setup/oauth.rs` polling), or (c) the existing loopback admit
  (`security.ts:149-167`), which is unchanged. It already refuses tunnel traffic, because that
  traffic carries a non-loopback Host and `X-Forwarded-For`. That keeps the Legion's local browser
  and the `setup oauth` browser step (`/oauth/<p>/start` on `127.0.0.1`) working. The cookie login
  (`/login`) is off in Access mode. **Routes exempt from `requireAuth` today stay
  exactly as they are:** the `webhooks.ts` router (provider webhooks, each with its own
  secret/HMAC), `/api/webhook/github`, `/api/discord/creds` and `/oauth/*/callback`
  (`dashboard.ts:113-132`). The JWT check is: RS256; key from
  `https://<team-domain>/cdn-cgi/access/certs` by `kid`; `aud` contains the configured AUD;
  `iss` = `https://<team-domain>`; `exp`/`nbf` with 60 s skew; `email` ∈ allowlist. Node's
  built-in `crypto` only, no new dependency. *Why:* the edge decides who reaches the tunnel. The
  app check makes the same decision for anything that reaches `127.0.0.1:3000` some other way, such
  as LAN traffic before D8, a misrouted tunnel ingress or a future second route.
- **D7. Trust model: network callers are untrusted. Processes running as the Jarvis user are
  trusted.** Any process with the Jarvis UID can already read `.env`, the API key, the bot tokens and
  `data.db`. D6 and D2 don't defend against such a process and don't claim to (R1). That's why
  loopback admit stays in Access mode (D6 (c)).
- **D8. Bind loopback, enforced at startup.** `getBindHost()` throws unless the host is loopback
  (`127.0.0.0/8`, `::1`, `localhost`; unset means the default `127.0.0.1`) or
  `AUGMENTAGENT_ALLOW_PUBLIC_BIND=1` is set. Both entrypoints call `getBindHost()`, so both are
  covered. *Why:* cloudflared reaches the app over loopback. The throw turns a config rule into a
  loud failure. **Precondition:** the Legion `.env` is flipped to `127.0.0.1` before item 1 merges
  (O2a), because main auto-deploys.
- **D9. Frontend: EJS + htmx + Tailwind, server-rendered fragments, no new npm dependency, no
  bundler.** htmx 2.0.4 and its SSE extension are vendored into `public/vendor/` by downloading
  the pinned release files with their SRI hashes recorded in a comment. They are not added to
  `package.json`. The CSP drops `https://unpkg.com`. Testable logic lives in **server-side** pure
  modules (`src/web/*.logic.ts`) with `node:test` tests. The browser gets HTML fragments and has no
  client-side logic beyond htmx attributes and a short `public/web.js` (scope below).
  `public/web.js` also mints `turn_id`s (D12), filters SSE events by thread, and reloads the page
  on any 401 or a repeated SSE failure (expired Access session, §5.4). Writes from the UI are
  htmx POSTs to fragment routes that return HTML (§5.3).
  *Why:* KISS. Autoship refuses diffs touching `package-lock.json`, so a React/Vite stack would take
  every UI issue out of autoship. Reversible: the JSON API (§5.3) is kept for a later client.
- **D10. Discord stays, and the surfaces are separate.** Web threads and Discord channels are
  separate conversations. A web thread does not see Discord chat, and Discord doesn't see web
  threads. What they share is Jarvis's long-term state: wiki, memory, the `actions` queue. The
  approvals rail shows every pending action, including ones drafted in Discord. Resolving one on
  the web redraws its Discord card (D5).
- **D11. Single owner.** Every email in `AUGMENTAGENT_CF_ACCESS_EMAILS` is the owner. The variable
  is a list so the owner can use more than one address. There are no roles.
- **D12. The browser mints `turn_id`** (UUIDv7, `crypto.randomUUID()`-based v7 helper in
  `public/web.js`). A retried POST with the same `turn_id` returns the existing turn. The composer
  keeps its `turn_id` until it gets a 2xx, or a 4xx other than 409 `turn_conflict`. It mints a new
  one after a success or when the owner edits the text. *Why:* the network hop from a phone is the
  one most likely to repeat.

## 3. Glossary

- **thread**: a web conversation, a row in `web_threads`. It fills the `conversation_id` slot of
  `SurfaceConversationRef`, and the `thread_id` slot is always `None`. It has nothing to do with
  Discord threads or with that `thread_id` slot.
- **turn**: one owner message and Jarvis's handling of it. Its `turn_id` is passed to the pipeline as
  the surface turn ref `web:<turn_id>`.
- **turn status**: `queued | running | done | failed | cancelled`. Separate from `actions.status`,
  and the two are never coupled.
- **pending action**: a row in `actions` with `status = 'pending'`.
- **Access**: Cloudflare Access. "Access JWT" means the `Cf-Access-Jwt-Assertion` header. The
  `CF_Authorization` cookie is ignored.

## 4. Engineering rules

- KISS: no new runtime dependencies (npm or cargo) in v1. SRP: one module per responsibility, as
  named in §5. TDD: each issue lands its failing test first, and its exit is a named test passing in CI.
- Each child issue stays under autoship's 600-line diff cap. Items 6 and 12 are pre-split (6a/6b,
  12a/12b). Items 1, 2 and 4 change the auth chokepoint and are labelled `autoship-manual`, which
  means a human-run session builds them, for review rather than because a path is refused.
- New Rust test targets that CI must run need adding to the workflow and `scripts/ci-contract.json`
  (protected path, owner PR).

## 5. Design

### 5.1 Storage (`crates/augmentagent-store/src/web_chat.rs`, daemon-owned)

```
web_threads(id TEXT PK,                -- UUIDv7
            title TEXT NOT NULL,       -- 1..120 chars; default 'New thread'
            title_set INTEGER NOT NULL DEFAULT 0,  -- 1 once renamed or auto-titled
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,   -- last activity; drives sidebar sort
            changed_at INTEGER NOT NULL,   -- any change incl. pin/archive; drives the watcher
            pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0)
-- all timestamps: epoch milliseconds
web_chat_messages(seq INTEGER PK AUTOINCREMENT, thread_id TEXT NOT NULL REFERENCES web_threads(id),
                  turn_id TEXT NOT NULL, role TEXT NOT NULL CHECK(role IN ('owner','jarvis')),
                  text TEXT NOT NULL, created_at INTEGER NOT NULL, UNIQUE(turn_id, role))
web_turns(turn_id TEXT PK, thread_id TEXT NOT NULL REFERENCES web_threads(id),
          text_sha256 TEXT NOT NULL,
          status TEXT NOT NULL CHECK(status IN ('queued','running','done','failed','cancelled')),
          error_code TEXT NULL,   -- turn error code, see §5.2.1 "Turn error codes"
          error TEXT NULL,        -- human text
          created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER NULL)
CREATE INDEX web_turns_updated ON web_turns(updated_at);
```

- **Failures are not messages.** A failed or cancelled turn has an owner message, no `jarvis`
  message, and its reason in `web_turns.error_code/error`. The UI renders it from the turn row.
  One home for the fact.
- **Titles:** `thread.create` without a title stores `'New thread'` with `title_set=0`. The first
  `turn.submit` on a thread with `title_set=0` sets the title to the first 60 chars of the text
  (whitespace collapsed) and `title_set=1`. `thread.rename` sets `title_set=1`.
- On a thread, `updated_at` is bumped by `turn.submit`, every turn status change and
  `thread.rename`. `changed_at` is bumped by all of those, plus pin, archive and reset.
- **Only the runner writes the `jarvis` message**, in the same transaction as `done`, after
  `run_surface_turn_with_history` returns. The web surface never mirrors transcript lines anywhere
  else (no `local_chat` writes).
- **Fallback history** (used by the pipeline only for a new session or a non-native provider):
  loaded **inside** the per-conversation queue, through `run_surface_turn_with_history`'s loader.
  It holds the thread's most recent `owner`/`jarvis` messages, oldest first, labelled by role, and
  **excludes the current turn's own owner row**. It stops at 50 messages or 16 KiB, whichever is hit
  first. A single message over 16 KiB is truncated to 16 KiB with a trailing `…`.
- **Restart sweep** (daemon start, before the socket opens): every `web_turns` row in `queued` or
  `running` becomes `failed` with `error_code='interrupted'`. For each of them whose surface turn
  state is `Pending`, the claim is also resolved as interrupted via `resolve_surface_turn`. Rows
  that were never claimed are only marked failed, so the sweep never errors on them. Exit test:
  re-send after a simulated restart succeeds (item 6a).
- Retention: forever in v1. Archive can be undone. Nothing is purged.

### 5.2 Daemon web surface

Modules (SRP): `crates/augmentagent-cli/src/web_surface_transport.rs` (socket, framing, peer
checks), `web_surface.rs` (op dispatch), `web_turn_runner.rs` (runner, cancel registry,
semaphore). Storage is in `web_chat.rs` (§5.1).

- Path: `AUGMENTAGENT_WEB_SOCKET`, **required, absolute, at most 100 bytes**. If it is unset, the web
  surface is disabled and the daemon logs that once. The Legion sets it in `.env` to
  `/home/nolan-makatche/.local/state/augmentagent/web.sock`, and the daemon and the dashboard both
  read that same variable.
- Request: `{"v":1,"op":"<name>", ...}`. Response: `{"ok":true, ...}` or
  `{"ok":false,"code":"<code>","error":"<text>"}`.
- Ops:

| op | args | result | notes |
|---|---|---|---|
| `thread.create` | `title?` | `{thread}` | |
| `thread.rename` | `thread_id, title` | `{thread}` | title 1..120 chars after trim |
| `thread.set` | `thread_id, pinned?, archived?` | `{thread}` | idempotent |
| `thread.reset_session` | `thread_id` | `{thread}` | calls the existing `Store::reset_surface_conversation` (`surface_commands.rs:59`), which resolves unresolved claims and drops the binding in one transaction, so the next turn starts fresh with fallback history. Refused with `thread_busy` while the thread has a non-terminal turn |
| `turn.submit` | `thread_id, turn_id, text` | `{turn_id, status}` | see below |
| `turn.cancel` | `turn_id` | `{turn_id, status}` | see below |
| `action.resolve` | `action_id, verb, note?` | `{outcome}` | §5.2.2 |

#### 5.2.1 Turns

- `turn.submit`, synchronous and fast, in one transaction:
  1. If `turn_id` exists: the same thread and `text_sha256` → return the stored `{turn_id,status}`
     (idempotent, even if the thread was archived since). Anything different → `turn_conflict`.
  2. Validate. Unknown thread → `thread_not_found`. Archived → `thread_archived`. More than 32 KiB of
     UTF-8 bytes → `text_too_large`. Empty after trim → `bad_request`.
  3. If the thread already has 3 non-terminal turns → `thread_busy`.
  4. Insert the owner message, a `web_turns` row with `queued`, the auto-title (§5.1) and the
     thread `updated_at` bump.
  5. Register a `CancellationToken` in the in-memory registry `turn_id → token`, spawn the runner
     and return.
- **Runner:** spawns `run_surface_turn_with_history` with the turn ref `web:<turn_id>`, the
  registry's cancel token and the §5.1 history loader. The per-conversation queue therefore runs
  first, and at most one turn per thread is ever past it. **Inside** the `answer` closure (so past
  the queue), it acquires a global `tokio::Semaphore` with **3 permits** (concurrent web turns
  across all threads). It then compares-and-swaps `queued → running` (if the swap fails, the turn
  was cancelled, so it returns early) and calls `QueryHandler::answer`. The `select` closure
  returns `Ok(None)`, which the core resolves to Claude. There is no web model picker in v1 (§8).
  Exactly one terminal write follows: `jarvis` message + `done` (one transaction), or `failed` with
  a turn error code, or `cancelled`. The registry entry is removed on any terminal state. The daemon
  finishes turns whatever happens to any socket connection.
- `turn.cancel`: always triggers the token if one is registered. It also compares-and-swaps
  `queued → cancelled`. If the turn is `running`, the runner writes `cancelled` when the token fires.
  Terminal → no-op, returns the current status. No row → `turn_not_found` (404).
- **Turn error codes** (`web_turns.error_code`; separate from the socket codes below):
  `interrupted` (restart sweep, or `ReasonerError::Interrupted` / host signal), `uncertain_session`
  (decided by a **store probe** after the error, true when the surface conversation is marked
  uncertain or has an unresolved claim, never by matching error strings), `provider_error`
  (any other answer error), `internal` (anything else).
- **Error codes → HTTP** (Node maps them in one table in `src/web/errors.ts`):

| code | HTTP |
|---|---|
| `bad_request` | 400 |
| `thread_not_found`, `action_not_found`, `turn_not_found` | 404 |
| `thread_archived`, `turn_conflict`, `already_resolved` | 409 |
| `text_too_large` | 413 |
| `thread_busy` | 429 |
| `unsupported_version`, `internal`, anything unknown | 500 |
| socket missing / ECONNREFUSED / connect timeout (2 s) → `jarvis_offline` | 503 |
| `AUGMENTAGENT_WEB_SOCKET` unset in Node → `web_disabled` | 503 |
| response timeout after connect → `jarvis_timeout` | 504 |

#### 5.2.2 Approvals

- `action.resolve {action_id, verb: "approve"|"skip"|"revise", note?}`. `revise` requires a
  non-empty `note`. Schedule, send-now, cancel-schedule and recipient override aren't offered on
  the web in v1. They stay in Discord (§9).
- The result is the handler's `ApprovalActionOutcome`, mapped by one function
  (`web_outcome_json`) to `{"kind":"<variant snake_case>","summary":"<one human line>"}` plus
  **only** these extra fields: `manual_send_ready` → `text`, `link`; `scheduled` → `local`;
  `failed` → `message`. Every other variant carries just `kind` and `summary` (`revised` included).
  `AlreadyResolved` → `{"ok":false,"code":"already_resolved"}`. `NotFound` →
  `{"ok":false,"code":"action_not_found"}`. The UI renders `summary` for every kind. It also
  special-cases `failed` (red), and `manual_send_ready`, which shows **"Not sent: send this
  yourself"** with the `text` in a copy box and the `link`, so the owner is never led to believe it
  was sent. A test covers every variant.
- Socket response timeout for this op in Node: 60 s.

### 5.3 Node backend (`src/web/`)

- `daemonClient.ts`: the only module that opens the socket. One function per op, connect timeout
  2 s, response timeout 10 s (60 s for `action.resolve`). It throws `DaemonError(code)` for every
  failure, using the codes in §5.2.1. Before connecting it checks that the path is a socket owned
  by this UID.
- `errors.ts`: the code → HTTP table, and the only place it lives.
- `readDb.ts`: opens a read-only `better-sqlite3` connection on the same DB file the dashboard
  already resolves. A missing `web_*` table is read as empty, because the dashboard may start
  before the daemon has migrated.
- `changeWatcher.ts`: polls every 1 s with
  `SELECT thread_id, max(updated_at) FROM web_turns WHERE updated_at >= ? GROUP BY thread_id`
  (cursor = the last max seen) and `SELECT max(changed_at) FROM web_threads WHERE changed_at >= ?`.
  It emits `changed {thread_id}` and `threads_changed`. `>=` re-emits the boundary row. Duplicates
  are harmless because a refetch is idempotent, and nothing written in the same millisecond is
  missed. It starts at "now" on boot and always runs. Browsers don't depend on its history,
  because they refetch on every SSE (re)connect (§5.4).
- `auth` (in `src/security.ts`, items 1/4): D6/D8. `src/web/cfAccess.ts` holds the pure
  `verifyAccessJwt(token, {aud, teamDomain, allowEmails, now, keys})` and the JWKS cache. The cache
  refetches on an unknown `kid`, at most once per 60 s, and holds keys for 1 h. If the JWKS fetch
  fails and no cached key matches, the request gets 503 (fail closed). If only some of
  `AUGMENTAGENT_CF_ACCESS_{AUD,TEAM_DOMAIN,EMAILS}` are set, the dashboard refuses to start.
  `TEAM_DOMAIN` is the full host, e.g. `royal-voice-826d.cloudflareaccess.com`. A rejected
  **top-level navigation** (`Sec-Fetch-Mode: navigate`) gets 302 to
  `https://<team-domain>/cdn-cgi/access/login/<host>`. Every other rejected request gets 401 JSON:
  API calls, htmx requests (`HX-Request`) and SSE (`Accept: text/event-stream`). `web.js` reloads
  the page on a 401, and the reload then goes through the Access login.
- JSON routes (`src/web/routes.ts`, all behind `requireAuth`):
  - `GET /api/web/threads?archived=0|1` → `{threads:[...]}`, pinned first, then `updated_at` desc
  - `POST /api/web/threads {title?}` → 201 `{thread}`
  - `PATCH /api/web/threads/:id {title?, pinned?, archived?}` → rename and/or `thread.set`.
    `POST /api/web/threads/:id/reset-session` → `thread.reset_session`
  - `GET /api/web/threads/:id/messages?before=<seq>&limit=<≤200, default 100>` →
    `{messages:[{seq,turn_id,role,text,created_at}], turns:[{turn_id,status,error_code,error,created_at,finished_at}]}`.
    `turns` covers every turn referenced by the returned messages, plus all non-terminal turns of
    the thread.
  - `POST /api/web/threads/:id/turns {turn_id, text}` → `turn.submit` → 202 `{turn_id,status}`
    or the mapped error. Node rejects text over 32 KiB itself with 413 before calling the socket.
  - `POST /api/web/turns/:id/cancel` → `turn.cancel`
  - `GET /api/web/events` → SSE: `changed` `{thread_id}` and `threads_changed`, heartbeat every 25 s
  - `GET /api/web/approvals` → `{actions:[...]}` (`status='pending'`, newest first, max 100)
  - `POST /api/web/approvals/:id {verb, note?}` → `action.resolve`
- Fragment routes (`src/web/fragments.ts`, HTML for htmx). They call the **same handler
  functions** as the JSON routes and render HTML instead of JSON. Errors render as an inline error
  fragment with the HTTP status from `errors.ts`.
  - GET: `/web/threads` (sidebar), `/web/threads/:id/log`, `/web/approvals`, `/web/palette?q=`
  - POST: `/web/threads` (new → returns the sidebar plus `HX-Push-Url`),
    `/web/threads/:id/turns` (returns the log), `/web/turns/:id/cancel` (log),
    `/web/threads/:id/{rename,set,reset-session}` (sidebar or log), `/web/approvals/:id` (rail)

### 5.4 UI (`views/web/*.ejs`)

- `GET /` renders the new app in place of today's redirect. Legacy pages keep their URLs and go
  under a "More" menu.
- Layout: left sidebar = threads (title search over loaded threads, pinned, recent, an archived
  toggle, ⌘N new); centre = message log + composer; right rail (collapsible) = pending approvals.
  Below 768 px: one pane at a time, with a bottom bar (Threads · Chat · Approvals).
- Live updates: the page holds one SSE connection (`hx-ext="sse"`). The log element refetches its
  fragment on `sse:changed` when the thread id matches (filtered in `public/web.js`). The sidebar
  refetches on `sse:threads_changed`. **On every SSE open or reopen (`htmx:sseOpen`) the log and the
  sidebar refetch**, so changes missed while a phone slept show up. After 3 consecutive SSE errors,
  or on any 401, `web.js` reloads the page. The approvals rail refetches every 20 s and after
  any resolve.
- Composer: Enter sends, Shift+Enter adds a newline. While a POST is pending the composer is disabled.
  It keeps its text on any error, and the error is shown inline using the code's human message. A
  turn row shows a status pill (queued/running/failed/cancelled) and Cancel while it is
  queued or running. A failed turn shows its `error`, and also "Reset session" when
  `error_code = uncertain_session`.
- Markdown: answers are rendered server-side by `src/web/markdown.logic.ts`. All text is
  HTML-escaped first. Then a whitelist is applied: paragraphs, fenced code, inline code, lists,
  bold/italic, and links with `http(s)` only and `rel="noopener noreferrer" target="_blank"`. An
  `ATTACH:` line renders as "Attachment, available in Discord" (§8). Approval previews are
  rendered as escaped plain text, not markdown.
- Approval card (rail only, §9): the action kind, recipient/target, an escaped preview, and Approve /
  Skip / Revise (Revise opens a note field). After resolving it shows the outcome `summary` and
  refetches.
- ⌘K palette: a server-side fragment `/web/palette?q=` ranks actions (new thread, open approvals,
  legacy pages) and thread titles. The ranking is in `src/web/palette.logic.ts`.
- Theme: dark by default, and the system light theme is respected. It is visually inspired by T3
  Code's density and type. No T3 assets or branding are copied.
- PWA: add `public/icon-192.png` and `public/icon-512.png`. The manifest gets `start_url: "/"` and
  is linked with `crossorigin="use-credentials"` (so it loads behind Access). The service worker's
  cache version is bumped, and it becomes network-first for HTML and never caches redirects. No push (§8).

## 6. Build order (one child issue each; the exit is the named test)

Phase 0, security. Ship first. Each item is independent except where noted:
1. **Loopback bind guard** (D8). Precondition: O2a is done. `autoship-manual`. Exit:
   `test/bindGuard.test.js` (loopback set accepted, `0.0.0.0` refused, override accepted, unset → 127.0.0.1).
2. **Authenticate PWA/push routes + service worker caching**: `/queue` and `/api/push/*` go under
   `requireAuth`. The SW changes are as in §5.4. `test/pushSubscribe.test.js` is updated to
   authenticate. `autoship-manual`. Exit: `test/pwaAuth.test.js` (unauthenticated → 401/redirect).
3. **Stop the false outcomes on the legacy route:** `POST /api/v1/actions/:id` accepts only
   `status: "pending"`, meaning field edits that keep the action pending, such as the
   `draft-preview` recipient edit. Any other target status gets 409 `{error:"use_real_approval"}`.
   No status is ever changed by SQL from Node again. `queue.ejs` loses Approve, Skip and Revise
   **for good**: its end state is a read-only list with a link to the web rail (or Discord). Exit:
   `test/apiV1.parity.test.js` extended.
4. **Access JWT in `requireAuth`** (D6, §5.3 auth). `autoship-manual`. Exit:
   `test/cfAccess.test.js` uses a locally generated RSA key and JWKS (team domain
   `test.cloudflareaccess.com`, AUD `test-aud`) and covers: valid token; wrong aud/iss; expired;
   nbf in the future; unknown kid → refresh → ok; email not allowed; missing header; API key still
   admitted; loopback admit still works for a loopback peer with no forwarded headers and refuses
   tunnel-shaped requests; cookie login refused; navigate → 302, htmx/SSE/API → 401; partial
   config refuses to start; JWKS
   down → 503; and the existing exempt routes still reachable without a token, with the GitHub
   webhook still enforcing its HMAC.

Phase 1, daemon (Rust):
5. **`web_chat.rs` store** (§5.1), including idempotent submit, auto-title and the restart sweep.
   Exit: `cargo test -p augmentagent-store web_chat`.
6a. **Web turn runner** (`web_turn_runner.rs`: D1, §5.2.1, no socket yet). Depends on 5. Answer T1
   first. Exit: a web `ConformanceAdapter` passing `native_session_conformance`, plus tests for
   re-send-after-restart, cancel-while-queued (the CAS race), cancel-while-running, `thread_busy`,
   the semaphore cap (one busy thread doesn't block others), `uncertain_session` via the store
   probe, and reset_session refused while busy. If it exceeds 600 lines, split it into runner and
   conformance/tests PRs.
6b. **Socket transport + thread/turn ops.** Depends on 6a. Exit: a transport test (wrong UID,
   oversize frame, bad version, unknown op → `bad_request`) plus a round-trip for each op.
7. **`action.resolve`** (§5.2.2). Depends on 6b. Exit: concurrent resolve from the web op and a
   simulated Discord button gives exactly one success and one `already_resolved`. A web-origin
   resolve calls `redraw_cards` on the Discord surface. `web_outcome_json` is tested for every
   `ApprovalActionOutcome` variant.

Phase 2, Node:
8. **`daemonClient.ts` + `errors.ts`** against a fake socket server. Exit:
   `test/daemonClient.test.js` (every code in the §5.2.1 table, including offline, timeout and
   disabled).
9. **`readDb.ts` + `changeWatcher.ts` + JSON routes + SSE.** Depends on 8. Exit:
   `test/webRoutes.test.js` (submit → 202 → watcher emits `changed`; retry with the same `turn_id`
   → same result; 413/404/409/503 mapping; missing tables → empty lists; approvals proxy).

Phase 3, UI. Each item depends on 9:
10. **Vendor htmx 2.0.4 + `htmx-ext-sse` (latest 2.x at implementation time, version and SRI
    recorded in `public/vendor/VERSIONS.md`), repoint `views/partials/header.ejs` to the vendored
    copy, drop unpkg from CSP.** Exit: a CSP header test, plus a test that the vendored files match
    the recorded hashes.
11. **`markdown.logic.ts` + `palette.logic.ts`.** Exit: unit tests (XSS corpus: script tags, `on*`
    attributes, `javascript:` links, nested fences, `ATTACH:` lines).
12a. **App shell + sidebar + fragments + responsive layout.** Exit: render tests for `/` and
     `/web/threads`.
12b. **Message log + composer + turn pills + cancel/reset.** Exit: render tests with fixture
     threads covering each turn status.
13. **Approvals rail.** Depends on 7. Exit: render test with fixture actions, plus an outcome
    summary rendering test.
14. **PWA icons + manifest.** Exit: manifest test (icons exist, `start_url` `/`, credentials link).

Ops (owner/human, not autoship):
- **O1. Cloudflare Access** for `jarvis.shipsystems.ai`:
  - Export the current application and policies into `docs/ops/cloudflare-access.md`, including
    any bypass rules for webhook paths.
  - Policy: allow only the owner email(s). Require a phishing-resistant second factor where the
    plan supports it; otherwise Google login with Google 2-Step Verification enforced.
  - Session at most 24 h.
  - Record the AUD tag and team domain in that doc.
  - Never sign in to Access from the agent-driven Chrome profile on the Legion.
- **O2a (before item 1 merges):** Legion `.env` `AUGMENTAGENT_BIND_HOST=127.0.0.1`, then
  `systemctl --user restart augmentagent-dashboard`. Verify the public URL still serves.
- **O2b (after item 4 deploys):** set `AUGMENTAGENT_CF_ACCESS_AUD`,
  `AUGMENTAGENT_CF_ACCESS_TEAM_DOMAIN`, `AUGMENTAGENT_CF_ACCESS_EMAILS` and restart. **Until O2b, the
  in-app check is off** (D6 is not in force). That is tracked as an epic checklist item, not left
  implicit.
- **O2c (before item 6b deploys):** set `AUGMENTAGENT_WEB_SOCKET` in `.env` and restart the daemon
  and dashboard. Auto-deploys don't touch `.env`.
- **O3.** Limit gnome-remote-desktop (3389) and RustDesk (21118) to Tailscale or loopback. Today
  both listen on all interfaces. This is a separate issue.

## 7. Accepted risks

- **R1. Processes with the Jarvis UID are trusted** (D7). They can use `web.sock`, including
  `action.resolve approve`, and they can already read every credential. Compensating control: every
  web turn and resolve is audited with its id. Ends when: agents run under a separate UID or sandbox.
  At that point `web.sock`'s peer check will exclude them with no protocol change.
- **R2. Edge identity is Cloudflare plus Google.** Compromising either account grants access.
  Controls: MFA on both, short Access sessions, and the app re-checking the email allowlist.
- **R3. Access JWT replay.** A captured token is valid until it expires (≤ 24 h). Controls: short
  sessions, and the agent-Chrome rule in O1.

## 8. Deferrals (each with trigger and carrying cost)

- **Token/tool-progress streaming.** Trigger: the owner finds waits painful. Cost: a progress
  column or table, plus the watcher emitting it. The D4 refetch model is unchanged.
- **Web model picker.** Trigger: the owner wants per-thread models. Cost: a `model` column on
  `web_threads` passed as `select`.
- **Web Push.** Trigger: v1 is in daily use. Needs the `web-push` dependency, so a lockfile change
  and a human PR.
- **Attachments** (upload and `ATTACH:` download). Trigger: the first need. Cost: a files table and
  an authed download route. Raw text is already stored, so no backfill.
- **Inline approval cards in threads.** Trigger: after item 13. Cost: `actions.source_turn_id` and
  plumbing through the drafting tools. Until then cards live in the rail only.
- **Tailscale Serve route.** Trigger: a Cloudflare outage or a latency complaint. It must not trust
  `Tailscale-User-*` headers on any listener cloudflared can reach.
- **ACP adapter so T3 Code's clients can drive Jarvis.** Trigger: wanting T3's mobile apps. Cost:
  an adapter over the §5.2 ops.
- **Voice in the browser.**

## 9. Non-goals (v1)

- Importing or mirroring Discord history. Cross-surface chat context.
- Schedule / send-now / cancel-schedule / recipient override on the web (Discord keeps them).
- Phone/dial tools in web turns.
- Restoring Approve on legacy `/queue` (item 3 removes it for good).
- Changing `POST /api/ask`. It stays as it is for curl/script callers, and the new UI doesn't use it.
- Multiple users, roles, sharing. Replacing Discord. React/Vite or any bundler.
- Redesigning legacy pages.

## 10. Open threads

- **T1 (load-bearing, blocks 6a).** `QueryHandler::answer` is the call Discord ends in. Which
  `AuditCtx` fields does it, or the tools it builds, read beyond `owner_authorized` and
  `session_id`? Discord http/channel, guild, owner-binding or `private` checks are the candidates.
  What does each do when absent (`AuditCtx::empty()` plus `owner_authorized`)? Owner: the item 6a
  implementer, before writing the runner. Deliverable: a test asserting that a web-shaped
  `AuditCtx` produces the same tool list and owner rules as a Discord owner turn, minus the
  Discord-posting tools and phone tools, which must be listed in the PR. If approval-history
  context needs a `private` flag, set it true for web.
- **T2 (blocks O1 doc, not code).** Which paths does the edge Access policy already bypass for
  webhooks? The in-app exemptions are fixed by D6 either way. This only decides whether provider
  webhooks reach the app through `jarvis.shipsystems.ai` at all. Owner: O1.

## 11. Audit log

- Rev 1 → rev 2 (2026-11-27). Resolved:
  - Blocking `turn.run` + 202 replaced by submit/refetch (D4), which fixes the invisible errors,
    timeouts, restart gaps and oversized frames.
  - Browser-minted `turn_id` (D12).
  - Restart sweep resolves surface claims, plus `thread.reset_session` (no more stuck "uncertain"
    threads).
  - Explicit driver and no Discord gate (D1).
  - Real concurrency cap.
  - The Access check moved into `requireAuth`, with existing exemptions kept, API keys kept, and
    the fail-closed and partial-config rules (D6).
  - The trust model is stated honestly (D7, R1, R3).
  - Bind precondition ordering (O2a).
  - Socket path required and the same in both processes, framing corrected (D2).
  - Error code table. Approval outcome contract.
  - Legacy Approve removed for good (item 3).
  - Cut: inline cards, `/api/ask` changes, schedule verbs.
  - Server-side palette (no browser logic).
  - Manifest and SW behind Access.
- Rev 2 → rev 3 (2026-11-27), after a second Pass A:
  - Separate `"web"`-origin `SyncingActionHandler` so Discord cards redraw.
  - Direct `run_surface_turn_with_history` call instead of generalising the CLI/phone driver. This
    removes the owner-binding gate, the mirror double-write and the five driver-signature changes.
  - Semaphore taken inside the queue.
  - `reset_session` = `reset_surface_conversation`, refused while busy.
  - Loopback admit kept in Access mode (fixes `setup oauth` and local use).
  - Sweep resolves only Pending claims.
  - Turn error-code table with a store probe for uncertainty.
  - CAS for cancel versus running.
  - 401 for htmx/SSE, with reload. Refetch on SSE open.
  - POST fragment routes. `turn_id` retention rule.
  - Outcome field map (manual_send_ready shown as not sent).
  - Legacy queue made read-only (no SQL skip).
  - Epoch ms, `>=` watcher cursor, `changed_at`.
  - 256 KiB frames. Idempotency checked before validation. History truncation rule.
  - Vendored htmx versions pinned.
