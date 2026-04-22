# Discord Protocol — User-Token REST Client

Status: validated end-to-end against the live Discord API on **2026-04-21**.
API version captured: `v9`. Client build number: `532435`.

This document is the reference the Rust client in
`crates/augmentagent-channel-discord-dm/` implements against. Re-harvest + update
this doc when Discord ships a new client build (roughly monthly; symptoms:
anomalous 401s, rejected requests despite valid token).

---

## Auth

Discord user accounts authenticate with a **raw user token** in the
`authorization` header. Note: **no** `Bearer` / `Bot` prefix. Bot tokens use
`Bot <token>`; user tokens don't.

```
authorization: <YOUR_DISCORD_USER_TOKEN>
```

The token's first `.`-separated segment is a base64url encoding of the numeric
user id. The rest is opaque — treat the whole thing as a secret bytestring.

### Required request headers

These three together were enough for every endpoint we tested:

```
authorization:     <user-token>
x-super-properties: <base64 JSON client fingerprint — see below>
user-agent:        <matches the fingerprint's `browser_user_agent`>
```

### `x-super-properties` fingerprint

Base64-encoded JSON. Field names are stable across Discord versions; the
`client_build_number` changes per web release. Example decode:

```json
{
  "os": "Mac OS X",
  "browser": "Chrome",
  "device": "",
  "system_locale": "en-US",
  "has_client_mods": false,
  "browser_user_agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36",
  "browser_version": "147.0.0.0",
  "os_version": "10.15.7",
  "referrer": "https://www.google.com/",
  "referring_domain": "www.google.com",
  "search_engine": "google",
  "release_channel": "stable",
  "client_build_number": 532435,
  "client_event_source": null,
  "client_launch_id": "<uuid>",
  "launch_signature": "<uuid>",
  "client_heartbeat_session_id": "<uuid>",
  "client_app_state": "focused"
}
```

Harvest from a real browser session — don't hand-fabricate. The `browser_user_agent`
inside the JSON MUST match the `user-agent` request header, or Discord flags
the session as anomalous.

### Optional headers (belt-and-suspenders)

The real web client always sends these; Discord doesn't reject requests without
them but sending them reduces fingerprint risk:

```
x-discord-locale:       en-US
x-discord-timezone:     America/New_York
x-installation-id:      <opaque per-install value, observed `<snowflake>.<base64>` shape>
x-debug-options:        bugReporterEnabled
```

Cookies (`__dcfduid`, `__sdcfduid`, `_ga*`, `cf_clearance`, `_cfuvid`, etc.) are
likewise not enforced but the web client always sends them. Optional for our
use.

---

## Endpoints

Base URL: `https://discord.com/api/v9`

### `GET /users/@me`

Fetch the authenticated user. Primary use: validate a token after harvest.

Response (excerpt):

```json
{
  "id": "<YOUR_USER_ID>",
  "username": "<your-username>",
  "global_name": "Nolan",
  "email": "<you>@example.com",
  "verified": true,
  ...
}
```

### `GET /users/@me/channels`

Lists DM channels (both 1:1 and group DMs).

Response: JSON array of `DmChannel` objects.

```json
[
  {
    "id": "1256240630137487451",
    "type": 1,                    // 1 = DM, 3 = group DM
    "recipients": [
      { "id": "...", "username": "thefool363", "global_name": "The Fool(Tony Siu)" }
    ],
    "last_message_id": "1496276571848446072"
  }
]
```

### `GET /users/@me/guilds`

Lists guilds (servers) the user is in.

```json
[
  { "id": "894703368411422790", "name": "Code & Coffee", ... }
]
```

### `GET /guilds/{guild_id}/channels`

Lists channels in a guild. Filter to `type == 0` for text channels (the only
kind this project reads from).

```json
[
  { "id": "894703368411422794", "name": "general", "type": 0, ... },
  { "id": "...", "name": "Voice Lobby", "type": 2, ... }
]
```

### `GET /channels/{channel_id}/messages`

Read messages from a channel. Works uniformly for DMs and guild channels — the
same endpoint, same response shape. This is the polling primitive.

Query params:
- `limit=N` (1..100, default 50) — newest N messages, returned newest-first
- `before=<message_id>` — messages older than this id (for backfill pagination)
- `after=<message_id>` — messages newer than this id (for polling-since-last-seen)

Snowflake IDs are time-sortable, so `after=` is the natural "what's new since
the last tick" query.

Response: JSON array of `Message` objects.

```json
[
  {
    "id": "1496276571848446072",
    "channel_id": "1256240630137487451",
    "author": {
      "id": "<YOUR_USER_ID>",
      "username": "<your-username>",
      "global_name": "Nolan",
      "bot": false
    },
    "content": "hello world test!",
    "timestamp": "2026-04-21T22:28:54.203000+00:00",
    "edited_timestamp": null,
    "mentions": [],
    "mention_roles": [],
    "attachments": [],
    "embeds": [],
    "referenced_message": null,
    "type": 0
  }
]
```

### `POST /channels/{channel_id}/messages`

Send a message. Works for DMs and guild channels (guild channels require
member-level permission; DMs always work if the recipient hasn't blocked you).

Body:

```json
{
  "content": "hello world test!",
  "nonce": "1496261698577760256",
  "tts": false,
  "flags": 0
}
```

- `content` — plain text, up to 2000 characters
- `nonce` — client-generated snowflake-shaped dedup id; use a fresh nanosecond
  timestamp cast to string per send
- `tts` — always `false` (text-to-speech; we never want this)
- `flags` — always `0` for plain text (no special flags)

The web client also sends `mobile_network_type: "unknown"`; optional and not
required per observation.

Response: the created `Message` object (same shape as the read endpoint).

---

## Rate limiting

Per-route buckets. Each response may carry:

```
x-ratelimit-limit:        <int — requests allowed in this bucket>
x-ratelimit-remaining:    <int — remaining in current window>
x-ratelimit-reset-after:  <float seconds until bucket resets>
x-ratelimit-bucket:       <opaque bucket id>
```

On overrun Discord returns HTTP 429 with a JSON body:

```json
{
  "message": "You are being rate limited.",
  "retry_after": 2.315,
  "global": false
}
```

**Honor `retry_after` rigorously.** Not honoring it is the fastest way for a
selfbot to get flagged. The Rust client must:

1. Delay the next request by `retry_after` seconds when 429 is returned.
2. Track `x-ratelimit-remaining == 0` + `x-ratelimit-reset-after` proactively
   to avoid triggering 429 in the first place.
3. Exponentially back off transient 5xx responses.
4. Treat 401 as terminal auth failure — halt polling and surface
   "re-run `augmentagent discord login`" to the user.

### Observed cadence safety

During validation we issued ~8 requests in under a minute with zero
rate-limit-hits. Our planned polling cadence (4h per channel, dozens of
channels) is orders of magnitude below any documented cap.

---

## Message conversion (Rust → `Email`)

When a Discord `Message` is upserted into the shared `emails` table:

| `emails` column | Source | Notes |
|---|---|---|
| `messageId` | `message.id` | Snowflake string |
| `threadId` | `message.channel_id` | Stable per DM pair / guild channel — used as the reply target on approval |
| `fromEmail` | `"{author.global_name \|\| author.username} <discord:{author.id}>"` | `<discord:...>` tag so `IdentityIndex::lookup("discord", id)` resolves |
| `subject` | `""` | Discord DMs have no subject |
| `body` | `message.content` | Plain text |
| `receivedAt` | `message.timestamp` | ISO8601 |
| `accountEntityId` | `"discord:{my_user_id}"` | Prefix-scoped per channel conventions |
| `platform` | `"discord"` | From issue #3 migration |
| `kind` | `"dm"` for `Priority`-mode subs, `"digest_item"` for `Digest`/`StoreOnly` | From issue #3 migration |

---

## Ban / detection risk

Selfbot detection is real and more aggressive on Discord than on LinkedIn.
Practices baked into this project:

- Poll cadence ≥ 4 hours per channel with random jitter
- Honor every `x-ratelimit-*` response and 429 `retry_after` exactly
- Mirror `x-super-properties` from an actual browser session; re-harvest when
  anomaly symptoms appear
- Token rotation (happens on password change, session expiry, or Discord
  anomaly detection) surfaces as 401. The daemon halts polling and tells the
  user to re-run `augmentagent discord login`.

Risk accepted per the project's "unofficial API" posture. Losing a primary
Discord account is not appealable.
