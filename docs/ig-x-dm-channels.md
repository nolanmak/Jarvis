# Instagram & X (Twitter) DM Channels — Decision Doc (#46)

> **SUPERSEDED — DO NOT TREAT AS THE AS-BUILT DESIGN.**
>
> This decision doc recommended an **official-API-only, no-sidecar** path for
> both platforms. The project ultimately reversed both recommendations and
> shipped the **unofficial** channels this doc rejected:
> - **Instagram** ships `augmentagent-channel-instagram` — the private web API
>   (`https://www.instagram.com/api/v1`, `sessionid`-cookie auth), **with DM
>   send**, plus a **logged-in Chromium browser sidecar** (Playwright/CDP) for
>   feed posting. It does **not** use the Meta Messaging API.
> - **X / Twitter** ships `augmentagent-channel-twitter` — the scraped GraphQL /
>   private endpoints (`x.com`, `auth_token`/`ct0` cookies), **with DM send**.
>   It does **not** use `api.twitter.com/2`.
>
> Ban-risk is managed via the governor caps and approval-gating, not by
> avoiding the unofficial transport. For the as-built protocols see
> **`docs/instagram-protocol.md`** and **`docs/twitter-protocol.md`**. The
> sections below are retained only as the historical decision record.

Status: **research / decision** — no code in this PR (`Refs #46 — research doc`).
Audience: whoever picks up the IG-DM (#17–#19, #50, #76) and X-DM (#14–#16,
#79) implementation tracks. This doc fixes the *approach* so those tracks
don't relitigate the official-vs-unofficial question per platform.

---

## 1. The core tension

Both Instagram and X expose DMs, but the only *officially sanctioned* access
is gated:

| | Official API | Unofficial / scraped |
|---|---|---|
| **Instagram** | Messaging API requires an **Instagram Business/Creator account linked to a Facebook Page**, a Meta app in Advanced Access, and App Review. Personal-account DMs are **not** available officially at all. | `instagrapi` / private mobile API. Full personal-DM access. |
| **X / Twitter** | DM endpoints exist on **API v2** but require a **paid tier** (Basic $200/mo for meaningful DM access; Free tier cannot read/most-write DMs). | Scraped GraphQL/private endpoints (the same family `augmentagent-channel-linkedin`'s Voyager client uses for LinkedIn). |

The decision is therefore **per platform**, and it is primarily a
**risk vs. cost** trade, not a technical one.

---

## 2. Instagram — recommendation: **official only, gated, Phase-2**

### Ban risk of the unofficial path is unacceptable here

`instagrapi` drives the private mobile API with the user's *personal*
credentials. Meta actively fingerprints automation on Instagram (device
attestation, behavioral heuristics, login-location anomalies). The realistic
failure mode is **the user's personal Instagram account getting
checkpointed or permanently banned** — not a rate-limit, an *account loss*.
For a personal-assistant product whose entire value is acting on behalf of
the user, torching their primary social identity is a catastrophic,
non-recoverable outcome. This is categorically worse than the LinkedIn
Voyager path we already ship: LinkedIn's enforcement is softer (mostly
rate-limit / temporary restriction) and the account is professional, not
personal-social.

### Recommended path

1. **Do not ship an instagrapi-backed IG-DM channel.**
2. Implement the **Instagram Messaging API** path only, behind an explicit
   onboarding gate that tells the user the hard requirements:
   - convert to a Business/Creator account,
   - link a Facebook Page,
   - connect via the dashboard OAuth bootstrap (same shape as the new
     Reddit `/api/reddit/auth` → `/api/reddit/callback` flow in #48).
3. Treat IG-DM as **Phase 2** of the IG track. Phase 1 (#17–#19, #76) can
   land the read-only / feed-engagement pieces (which have a less
   ban-sensitive surface) without DMs.
4. If a user genuinely cannot meet the Business-account requirement, IG-DM
   is simply **unavailable** for them — an honest "not supported" beats a
   feature that bricks their account.

### Webhook shape

Meta Messaging delivers via webhook (same model as the #49 dev-tool
webhooks). Reuse the `/webhooks/*` HMAC-verification pattern
(`X-Hub-Signature-256`, identical algorithm to the Linear crate's
`hmac_sha256_hex` — already pinned by the RFC 4231 vector). No polling
fallback is needed; Meta webhooks are reliable when the app is in good
standing.

---

## 3. X / Twitter — recommendation: **official paid API, behind a cost gate**

### Unofficial scraping is viable but degrading

The scraped-GraphQL approach is the same technique
`augmentagent-channel-linkedin` uses successfully, and X's DM private
endpoints are reachable the same way. The blocker is **trajectory**: since
2023 X has aggressively broken unofficial clients, rotated GraphQL query IDs
(we *already* maintain a `twitter_query_ids` cache for exactly this reason —
see the #79 work), and tightened auth-flow detection. An unofficial X-DM
channel is a permanent maintenance treadmill with unpredictable multi-week
outages, and carries account-suspension risk (lower than IG personal-ban,
but real).

### Cost reality

Official X API v2 DM access needs **Basic ($200/mo)** at minimum. That is a
real per-user cost that the product cannot silently absorb.

### Recommended path

1. Implement the **official API v2** DM channel.
2. Put it behind an explicit **cost gate**: it is **off by default** and the
   onboarding copy states the $200/mo X requirement plainly. The user
   supplies their own X API bearer token (their billing relationship, not
   ours) — mirror the BYO-token pattern the GitHub PAT and Reddit
   installed-app flows already use.
3. Do **not** build the scraped X-DM path. The `twitter_query_ids` cache and
   any Voyager-style scraping stay scoped to the *read/engagement* X track
   (#14–#16, #79) where an outage is a degraded feature, not a missed
   personal message the user was relying on.

---

## 4. Phased rollout (both platforms)

| Phase | Instagram | X |
|---|---|---|
| **P1 (no DM)** | read/feed engagement (#17–#19, #76) | timeline read + engagement (#14–#16, #79) |
| **P2 (DM, gated)** | Messaging API, Business-account gate | API v2 DM, $200/mo cost gate, BYO token |
| **P3** | — | — (no unofficial DM path, by decision) |

DMs are explicitly *last*, gated, and official-only on both. The product
ships value (P1) without ever putting a personal account at ban risk.

---

## 5. Shared sidecar-runner factoring

Neither recommended path needs a credential-driving sidecar (unlike WhatsApp's
whatsmeow Go sidecar) — both are HTTP/webhook against official APIs, so they
fit the **direct-`reqwest` + Trigger** shape the new Reddit / Linear / Notion /
Calendly channels already use. Concretely, when these land they should:

- implement `augmentagent_channel_core::Trigger` (webhook-fed for IG,
  poll-or-stream for X), exactly like `LinearChannel` / `RedditChannel`;
- reuse the `/webhooks/*` HMAC verifier (the `hmac_sha256_hex` primitive in
  `augmentagent-channel-linear`, RFC-4231-pinned) for IG's
  `X-Hub-Signature-256`;
- reuse the dashboard OAuth-callback bootstrap pattern
  (`/api/reddit/auth` → CLI `exchange` → keyring via `augmentagent-auth`)
  for both IG (Meta OAuth) and X (token paste);
- surface decisions through the new `ApprovalSurface` seam (#45) so IG/X DMs
  get the same approval UX as every other channel with no per-platform UI.

**A `sidecar-runner` abstraction is *not* warranted for IG/X.** Factoring a
shared sidecar runner only pays off if ≥2 channels actually need a
credential-driving subprocess. Today only WhatsApp does. Revisit a shared
`SidecarRunner` trait *iff* a future platform forces an unofficial
subprocess path — IG/X under this decision do not.

> **SUPERSEDED:** the as-built Instagram channel drives a logged-in Chromium
> browser sidecar (Playwright/CDP) for feed posting — see
> `docs/instagram-protocol.md` "Browser posting" and
> `crates/augmentagent-browser-client` + `sidecars/browser`. So WhatsApp is no
> longer the *only* channel using a credential-driving subprocess, and the
> "no sidecar needed for IG" premise above no longer holds.

---

## 6. One-line summary

- **Instagram DM:** official Messaging API only, Business-account gated,
  Phase 2. Unofficial `instagrapi` is **rejected** — personal-account ban
  risk is non-recoverable and disqualifying.
- **X DM:** official API v2 only, behind a visible $200/mo cost gate with
  BYO token. Unofficial scraping **rejected for DMs** (maintenance treadmill
  + suspension risk); scraping stays scoped to read/engagement tracks.
- Both fit the existing direct-`reqwest`/Trigger + webhook + OAuth-callback +
  `ApprovalSurface` machinery; **no shared sidecar-runner needed**.

> **SUPERSEDED:** see the banner at the top of this doc. The shipped channels
> are unofficial (cookie-based IG/X with DM send) and the Instagram path does
> use a browser sidecar for feed posting. As-built: `docs/instagram-protocol.md`,
> `docs/twitter-protocol.md`.
