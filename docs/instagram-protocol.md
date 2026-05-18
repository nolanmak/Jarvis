# Instagram Protocol — Private Web API + Browser Posting

Status: **SPEC PRODUCED FROM PUBLIC KNOWLEDGE — REQUIRES LIVE OPERATOR
VALIDATION.** No live `/intercept` capture was possible for this issue (#17):
live capture needs a logged-in operator browser session this swarm agent does
not have. Every endpoint, header, and rate observation below is reconstructed
from public reverse-engineering write-ups (instagrapi, instauto, the
`mgp25/Instagram-API` lineage, and the Elfsight / Fameviso limit guides cited
in `crates/augmentagent-channel-core/src/governor/limits.rs`). Treat the wire
details as **hypotheses to confirm**, not validated fact.

Mirror of `docs/discord-protocol.md` structure. Re-harvest + re-validate this
doc the first time an operator runs a live `/intercept` session on
instagram.com (symptoms of drift: anomalous 400/401, `feedback_required`
bodies, `checkpoint_required` redirects).

---

## Auth

Instagram's private web API authenticates with **session cookies** harvested
from a logged-in `www.instagram.com` browser session — not OAuth, not a
bearer token. The load-bearing values:

| Value | Source | Notes |
|---|---|---|
| `sessionid` | Cookie | The session secret. URL-encoded; contains `<ds_user_id>%3A...%3A...`. Treat the whole thing as opaque. |
| `csrftoken` | Cookie | Echoed back in the `x-csrftoken` request header on every write (POST). |
| `ds_user_id` | Cookie | Numeric account id. Also the prefix of `sessionid`. |
| `mid` | Cookie | Machine id. Sent back as `x-mid`. Stable per browser install. |
| `ig_did` | Cookie | Device id (UUID). Long-lived. |
| `rur` | Cookie | Region routing hint. Optional but the web client always sends it. |

### Required request headers

Observed-necessary set for the private web (`/api/v1/...`) endpoints:

```
cookie:            sessionid=...; csrftoken=...; ds_user_id=...; mid=...; ig_did=...
x-ig-app-id:       936619743392459          # the web app id; stable for years
x-csrftoken:       <value of csrftoken cookie>
x-requested-with:  XMLHttpRequest
x-asbd-id:         129477                   # anti-scraping bot-detection id; rotates rarely
x-ig-www-claim:    0                        # then echo the x-ig-set-www-claim response header back on subsequent calls
user-agent:        <a real Chrome UA matching the harvest browser>
referer:           https://www.instagram.com/
origin:            https://www.instagram.com
```

`x-mid` is sent as a header on some endpoints in addition to the `mid`
cookie; mirror the cookie value.

`x-ig-app-id: 936619743392459` is the **web** app id and is the single most
load-bearing non-cookie header — omitting it yields HTML (logged-out shell)
instead of JSON. The mobile app id (`567067343352427`) behaves differently
and is out of scope.

`x-asbd-id` and `x-ig-www-claim` are anti-automation fingerprint headers.
`x-ig-www-claim` starts at `0`; the server returns `x-ig-set-www-claim` and
the real client echoes that value on subsequent requests within the session.
We replicate that round-trip.

Harvest from a real browser session — do not hand-fabricate. The
`user-agent` request header MUST match the browser the cookies were harvested
from or Instagram escalates to `checkpoint_required`.

---

## Endpoints

Base URL: `https://www.instagram.com/api/v1`

### `GET /api/v1/users/web_profile_info/?username=<u>`

Resolve a username → numeric user id + profile metadata. Primary use:
validate a session after harvest, and resolve `instagram:<handle>` wiki
identities to the `user_id` the feed endpoint needs.

Response (excerpt): `data.user.id`, `data.user.full_name`,
`data.user.edge_owner_to_timeline_media`.

### `GET /api/v1/direct_v2/inbox/?persistentBadging=true&...`

The DM inbox. Returns threads newest-activity-first with the last few items
of each inline. Pagination via `cursor` (the response's
`inbox.oldest_cursor`) passed back as `?cursor=<c>`.

Response shape (partial; unknown fields ignored):

```json
{
  "inbox": {
    "threads": [
      {
        "thread_id": "340282...",
        "users": [{ "pk": "123", "username": "tony", "full_name": "Tony Siu" }],
        "items": [
          {
            "item_id": "289...",
            "user_id": 123,
            "timestamp": 1715900000000000,
            "item_type": "text",
            "text": "hey, free thursday?"
          }
        ]
      }
    ],
    "oldest_cursor": "169..."
  },
  "viewer": { "pk": "456" }
}
```

`timestamp` is **microseconds** since epoch (divide by 1000 for ms).
`item_type` other than `text` (`media`, `clip`, `raven_media`, `voice_media`,
`reel_share`, `link`, `story_share`) carries no usable `text` — see
"Media-only DM handling" below.

### `POST /api/v1/direct_v2/threads/<thread_id>/items/text/`

Send a text reply on an existing thread. Form-encoded body:

```
text=<utf8 text>
&_uuid=<ig_did UUID>
&action=send_item
&client_context=<random UUID — dedup key, like Discord nonce>
```

Header `x-csrftoken` required. Response carries `payload.item_id` on success;
a `{"status":"fail"}` body or HTTP 400 with `feedback_required` /
`spam` means the action was throttled or blocked (see Rate caps).

We do **not** implement new-thread creation, group send, or media send in v1
— text reply to an existing 1:1 thread only, mirroring the LinkedIn channel's
scope.

### `GET /api/v1/feed/user/<user_id>/?count=12`

A specific user's media feed (their posts), newest-first. Pagination via the
response's `next_max_id` passed back as `?max_id=<id>`. Used by the
friend-post engagement trigger (#19) to find recent posts of `close:true`
wiki contacts.

Response (partial): `items[].id` (the media id, shape `<pk>_<userpk>`),
`items[].code` (the shortcode for the public URL),
`items[].caption.text`, `items[].taken_at` (seconds), `items[].media_type`
(1 = image, 2 = video, 8 = carousel).

### `POST /api/v1/web/comments/<media_id>/add/`

Comment on a post. Form-encoded body:

```
comment_text=<utf8 text>
&_uuid=<ig_did UUID>
```

Header `x-csrftoken` required. Response `id` on success; HTTP 400 with
`{"message":"feedback_required","spam":true}` is the block signal.
**Every comment goes through Discord approval** — there is no auto-post path
(#19).

### Browser posting (feed image) — `crates/augmentagent-browser-client`

Instagram's feed-create endpoint (`/api/v1/media/configure/`) requires a
multi-step rupload handshake with signed parameters that rotate aggressively
and are the highest ban-risk surface. Per #50/#76 we therefore drive a real
logged-in Chromium via the browser sidecar (Playwright/CDP) instead of the
private upload API:

1. `navigate https://www.instagram.com/`
2. Click the **New post / Create** entry (selector registry, layered).
3. CDP `setFileInputFiles` on the hidden `<input type=file>` (file chooser
   never opens — this is why CDP, not a real OS dialog).
4. Click **Next** past the crop step (×2: crop → filter → caption).
5. Fill the caption contenteditable.
6. **Share is gated by Discord approval** — the sidecar stops before the
   final Share click until the approval handler fires.

Reels, carousels, and stories are deferred (`Refs #76 — deferred`).

---

## Rate limiting & ban / detection risk

Instagram has **no published API rate-limit headers** for the private web
endpoints (unlike Discord's `x-ratelimit-*`). Throttling is signalled
in-band by an HTTP 400 with one of these JSON bodies:

```json
{ "message": "feedback_required", "spam": true, "feedback_title": "..." }
{ "message": "checkpoint_required", "checkpoint_url": "..." }
{ "message": "challenge_required" }
{ "status": "fail", "lock": true }
```

`feedback_required` = the action was rate-limited / soft-blocked; back off
hard. `checkpoint_required` / `challenge_required` = the account itself is
flagged; this is terminal until a human clears it in the app — halt the
channel and alert loudly.

### Observed community rate caps (from the governor's cited sources)

These are the soft-limits the `RATE_TABLE` Instagram rows are derived from
(roughly 30% of the community-reported platform soft-limit; see
`governor/limits.rs`):

| Action | Community soft-limit | Our governor cap (day) | min gap |
|---|---|---|---|
| Feed post | ~25/day combined feed+story | **2/day** | n/a (hard daily quota) |
| Like | 300-500/day, ~20/hr | 60/day, 15/hr, 5/5min | 30s |
| Comment | 12-14/hr, 350-400s spacing | 30/day, 10/hr, 3/5min | 60s |
| Follow | 200/day, 20/hr | 20/day, 5/hr | 300s |
| DM | 100/day to followers, 30-40/day cold | 10/day, 3/hr | 60s |

DM polling cadence: **≥ 30 min per inbox poll with ±10 min jitter**
(#18). Feed-engagement scan cadence: **4h ± 30 min jitter, ≤ 3 comments/day**
(#19). These are channel-level cadences enforced on top of the governor's
per-action caps.

On any `feedback_required` / HTTP 429 / suspicious body the DM channel
**pauses itself for 1h** (logged loudly) and records a governor halt;
`checkpoint_required` / `challenge_required` records a longer halt and
surfaces a re-login alert. The daemon must never crash on a rate-limit — a
throttle is an expected steady-state event, not an error.

### Detection-avoidance practices baked in

- Poll cadence ≥ 30 min (DM) / 4h (feed) with random jitter.
- Every text-generative action (comment, DM reply, post caption) goes
  through Discord approval; no auto-post.
- Browser posting kept behind `INSTAGRAM_REAL_ACCOUNT_ENABLED=false` by
  default; even when enabled the final Share is approval-gated.
- Mirror `x-ig-app-id` / `x-asbd-id` / `user-agent` from a real harvested
  session; re-harvest when anomaly symptoms appear.
- Hard daily quota on posts (governor cap = 2/day) independent of approval.

Risk accepted per the project's "unofficial API" posture, same as the
Discord channel. Losing the primary Instagram account is not appealable.

---

## Message conversion (Rust → `Email`)

When an Instagram DM is upserted into the shared `emails` table (mirrors the
LinkedIn channel's repurposing of the generic `Email` row):

| `emails` column | Source | Notes |
|---|---|---|
| `messageId` | `item.item_id` | DM item id |
| `threadId` | `thread.thread_id` | Reply target on approval |
| `fromEmail` | `"{full_name \|\| username} <instagram:{user_pk}>"` | `<instagram:...>` tag so `IdentityIndex::lookup("instagram", pk)` resolves |
| `subject` | `"[Instagram DM from <name>]"` | Discord card title |
| `body` | `item.text` | Plain text (media-only items routed to a flag card) |
| `receivedAt` | RFC3339 from `timestamp / 1000` | µs → ms → RFC3339 |
| `accountEntityId` | `"instagram:{ds_user_id}"` | Prefix-scoped per channel conventions; lets the approver route the send |
| `platform` | `"instagram"` | |
| `kind` | `"dm"` (DM) / `"post_engagement"` (feed) | Matches WorkItem kinds |
