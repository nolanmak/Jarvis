# X / Twitter Protocol — Internal Web API Client

> **Status: SPEC FROM PUBLIC KNOWLEDGE — REQUIRES LIVE OPERATOR VALIDATION.**
>
> Unlike `docs/discord-protocol.md` (validated live 2026-04-21) and the
> LinkedIn capture, this document was **not** produced from a live `/intercept`
> proxy session — true X capture needs an operator browser session logged into
> a real X account, which the spike (#14) did not have. Every endpoint,
> `queryId`, header, and JSON shape below is reconstructed from public
> reverse-engineering knowledge (twikit / twscrape / the-convocation
> twitter-openapi community work). **Each section flagged
> `REQUIRES LIVE OPERATOR VALIDATION` must be confirmed against a live session
> before the channel is trusted in non-dry-run mode.**
>
> The Rust client in `crates/augmentagent-channel-twitter/` implements against
> this doc. Re-harvest + update when X ships a web deploy (symptoms: sudden
> 400s on GraphQL ops, 403s despite a valid session, rotated `queryId`).

This doc has six sections, matching the spike scope (#14):

1. Auth
2. User-timeline GraphQL (`UserTweets`)
3. `CreateTweet` (post + reply)
4. DM inbox (`inbox_initial_state.json`)
5. DM send (`new2.json`)
6. Pagination + rate limits

---

## 1. Auth — `REQUIRES LIVE OPERATOR VALIDATION`

X's web app authenticates internal calls with a **cookie session bundle plus
a CSRF echo header and a static public Bearer**:

| Piece | Where | Notes |
|---|---|---|
| `auth_token` cookie | browser cookie jar (`x.com`) | The session bearer. Treat as a password. Rotated on logout / password change. |
| `ct0` cookie | browser cookie jar | CSRF token. |
| `x-csrf-token` header | request header | A **verbatim echo of the `ct0` cookie**. Mismatch / absence => HTTP 403. |
| `authorization` header | static | `Bearer <public web token>`. App-level, *not* per-user; shipped in `main.<hash>.js`. Stable for years. Default baked into `auth.rs::DEFAULT_PUBLIC_BEARER`; override via `AUGMENTAGENT_TWITTER_BEARER`. |
| `x-client-transaction-id` header | per-request | Anti-automation header X added in 2023. Derived client-side from a per-page-load animation/key in the X bundle. **Full derivation REQUIRES LIVE OPERATOR VALIDATION** — the client currently sends a plausible opaque value rather than omitting it (omission is a harder reject on newer deploys). |

Additional headers the web client always sends (the client sets these):

```
x-twitter-auth-type:        OAuth2Session
x-twitter-active-user:      yes
x-twitter-client-language:  en
content-type:               application/json
referer:                    https://x.com/
origin:                     https://x.com
```

Harvest the cookies from a real logged-in browser session via
`scripts/twitter-harvest.sh` (mirrors `linkedin-harvest.sh`):
`auth_token`, `ct0`, plus your numeric `user_id` and `@screen_name`. Stored in
the OS keychain at `augmentagent/twitter/default` with a legacy
`twitter-auth.json` file fallback (same migration path as LinkedIn).

> **VALIDATION TODO:** confirm whether `x-client-transaction-id` is *required*
> on `UserTweets` / `CreateTweet` / DM endpoints in the current deploy, and if
> so capture the real derivation. Confirm the public Bearer is still accepted.

---

## 2. User-timeline GraphQL — `UserTweets` — `REQUIRES LIVE OPERATOR VALIDATION`

`GET https://x.com/i/api/graphql/<queryId>/UserTweets`

Query string carries two URL-encoded JSON blobs:

- `variables` — `{ "userId": "<id>", "count": 20, "includePromotedContent": false, "withQuickPromoteEligibilityTweetFields": false, "withVoice": true, "withV2Timeline": true }`
- `features` — a boolean map (see `api.rs::graphql_features`).

### queryId fragility — `REQUIRES LIVE OPERATOR VALIDATION`

The `<queryId>` is a rotating hash X regenerates on web deploys (~every 2-6
weeks). A stale id => HTTP 404/400. Recovery chain (no redeploy needed),
implemented in `client.rs::resolve_query_id` and `api.rs::query_id_for`:

1. `twitter_query_ids` store table (operation → cached id), populated from a capture
2. `AUGMENTAGENT_TWITTER_USER_TWEETS_QUERY_ID` / `..._CREATE_TWEET_QUERY_ID` env override
3. static default constant in `api.rs` (`DEFAULT_USER_TWEETS_QUERY_ID`)

> **VALIDATION TODO:** the static defaults are best-guess shapes. Capture the
> live `queryId` for `UserTweets` + `CreateTweet` and either cache them in
> `twitter_query_ids` or update the constants.

### Response shape (partial)

`data.user.result.timeline_v2.timeline.instructions[].entries[]`, where each
tweet entry has `content.itemContent.tweet_results.result` with:

- `rest_id` — tweet id
- `legacy.full_text`, `legacy.created_at` (legacy format
  `Wed Oct 10 20:19:24 +0000 2018`), `legacy.conversation_id_str`, `legacy.id_str`
- `core.user_results.result.legacy.{name,screen_name}` — author

The parser (`api.rs::parse_user_tweets`) walks defensively for any object with
both `legacy` + `rest_id`, so minor instruction-nesting changes don't break it.
The exact nesting still `REQUIRES LIVE OPERATOR VALIDATION`.

---

## 3. `CreateTweet` (post + reply) — `REQUIRES LIVE OPERATOR VALIDATION`

`POST https://x.com/i/api/graphql/<queryId>/CreateTweet`

Body:

```json
{
  "variables": {
    "tweet_text": "<text>",
    "reply": { "in_reply_to_tweet_id": "<parent_tweet_id>", "exclude_reply_user_ids": [] },
    "dark_request": false,
    "media": { "media_entities": [], "possibly_sensitive": false },
    "semantic_annotation_ids": []
  },
  "features": { ...same boolean map as UserTweets... },
  "queryId": "<queryId>"
}
```

- **Reply**: include the `reply.in_reply_to_tweet_id` block. **Original tweet**:
  omit `reply` (the channel currently routes both through the same op; an
  empty parent id is treated as "original").
- Success response carries the new tweet's `rest_id` (extracted via a
  recursive `rest_id` field search).
- `features` is the #1 cause of HTTP 400 if stale.

> **VALIDATION TODO:** confirm the `features` map matches the current deploy;
> confirm an original (non-reply) tweet is accepted with `reply` omitted vs.
> needing a different variables shape.

---

## 4. DM inbox — `inbox_initial_state.json` — `REQUIRES LIVE OPERATOR VALIDATION`

`GET https://x.com/i/api/1.1/dm/inbox_initial_state.json`

Query params used:
`nsfw_filtering_enabled=false&filter_low_quality=false&include_quality=all&dm_secret_conversations_enabled=false`
(+ `max_id=<cursor>` for pagination, see §6).

Response (partial): `inbox_initial_state` with:

- `users` — id → `{ name, screen_name, ... }` map
- `entries[]` — each with `message.id`, `message.conversation_id`, and
  `message.message_data.{sender_id, conversation_id, text, time}` (`time` is
  epoch-ms as a string)

Parser: `api.rs::parse_dm_inbox`. Non-message entries
(`trust_conversation`, etc.) and incomplete entries are skipped, not fatal.

> **VALIDATION TODO:** `inbox_initial_state.json` returns only the *initial*
> page; large/older inboxes need `dm/user_updates.json` or
> `dm/conversation/<id>.json` for full history. The exact pagination cursor
> field on this endpoint must be confirmed live (§6).

---

## 5. DM send — `new2.json` — `REQUIRES LIVE OPERATOR VALIDATION`

`POST https://x.com/i/api/1.1/dm/new2.json`

Body:

```json
{
  "conversation_id": "<conversation_id>",
  "recipient_ids": false,
  "request_id": "<fresh uuid v4>",
  "text": "<message>",
  "cards_platform": "Web-12",
  "include_cards": 1,
  "include_quote_count": true,
  "dm_users": false
}
```

- `conversation_id` is the sorted participant-pair id (`123-456`) — this is
  the `thread_id` carried on the `Email` row, so the approver sends to the
  right thread.
- `request_id` is a client dedup id; a fresh UUID v4 per send.
- Success response carries the new event `id` (recursive field search).

> **VALIDATION TODO:** confirm `new2.json` (vs. the older `new.json`) is the
> current send endpoint and the body shape matches; confirm whether a
> separate `conversation_id` creation step is needed for a brand-new thread
> (v1 scope: existing conversations only).

---

## 6. Pagination + rate limits — `REQUIRES LIVE OPERATOR VALIDATION`

### Pagination

- **`UserTweets`**: cursor-based via timeline `entries` of type
  `TimelineTimelineCursor` (Top/Bottom). The channel polls a small `count`
  per friend and filters client-side by `since_id` (highest seen `rest_id`)
  — no deep backfill in v1.
- **DM inbox**: `inbox_initial_state.json` is page-1 only; the client passes
  an opaque `max_id` cursor for older pages. The exact cursor field
  `REQUIRES LIVE OPERATOR VALIDATION`.

### Rate limits

X returns standard rate-limit headers on internal endpoints:

```
x-rate-limit-limit:      <int — requests allowed in the window>
x-rate-limit-remaining:  <int — remaining>
x-rate-limit-reset:      <unix seconds — window reset>
```

On overrun: **HTTP 429**. On session invalidation: **HTTP 401/403** (the
client maps both to `TwitterError::AuthExpired` and the channel halts polling
with "re-run `augmentagent twitter login`").

Two layers of throttle protect the account:

1. **Channel cadence** — feed poll every 2h ± 20min jitter; DM inbox poll
   floored at ≥30min (`channel.rs`).
2. **Outbound caps** — the shared `RateGovernor` (#83) holds the X soft caps
   (`Platform::Twitter`: 100 replies/day, 20 posts/day, 30 DMs/day, etc., in
   `governor/limits.rs`). The posting client (`client.rs`) adds a **hard
   15-posts/day quota preflight** off the `twitter_post_log` table — stricter
   than the soft cap so the user owns most of their own posting budget.

> **VALIDATION TODO:** confirm exact `x-rate-limit-*` header names on the
> GraphQL + 1.1 DM endpoints and the per-endpoint window sizes.

---

## Ban / detection risk

X automation detection is aggressive. Practices baked in:

- Feed poll ≥ 2h with ±20min jitter; DM poll floored at 30min.
- Honor 401/403 as terminal auth failure — halt + tell the user to re-login.
- Mirror the public Bearer + browser `user-agent` from a real session;
  re-harvest when 400/403 anomaly symptoms appear.
- No tweet or DM is auto-sent — every reply goes through Discord approval.
- Hard 15-posts/day agent ceiling on top of the #83 governor soft caps.

Risk accepted per the project's "unofficial API" posture. Losing a primary
X account is not appealable. **Do not enable non-dry-run posting until the
`REQUIRES LIVE OPERATOR VALIDATION` items above are cleared against a live
session.**
