# [Twitter] /intercept spike: reverse X web client (timeline, post, DM)

Tracking issue: [#100](https://github.com/nolanmak/MyAgentAssistant/issues/100)
(migrated from `nolanmak/AugmentAgent#14`).

## Problem

X / Twitter charges $200/mo for the Basic API (timeline + posting only) and
gates DM read/send behind the ~$5k/mo Enterprise tier. Same predicament we
faced with LinkedIn and WhatsApp: the only viable path for an agent that
needs all three surfaces (timeline read, reply post, DM read+send) is to
reverse-engineer the X web client and replay its internal endpoints from a
captured logged-in browser session.

The spike's goal was to produce a protocol spec covering six things — auth,
user timeline, `CreateTweet`, DM inbox, DM send, pagination + rate limits —
plus a Rust type sketch for the session bundle. The intent was always a
docs-first deliverable; implementation lives in follow-up issues (friend-post
reply automation and DM channel).

## Findings

The spike was **substantively completed and superseded by implementation**.
Current repo state on `origin/main` (commit `7f91986`):

- **`docs/twitter-protocol.md`** (390 lines) — committed, covers all six
  required sections in the order the issue prescribed:
  1. Auth (cookie bundle: `auth_token` + `ct0`, `x-csrf-token` echo header,
     static public Bearer baked into `auth.rs::DEFAULT_PUBLIC_BEARER`,
     `x-client-transaction-id` anti-automation header)
  2. `UserTweets` GraphQL (rotating `queryId`, `variables` / `features`
     blobs, queryId-recovery chain in `client.rs::resolve_query_id` +
     `api.rs::query_id_for`)
  3. `CreateTweet` GraphQL (reply via `in_reply_to_tweet_id`)
  4. DM inbox via `/i/api/1.1/dm/inbox_initial_state.json`
  5. DM send via `/i/api/1.1/dm/new2.json`
  6. Pagination + observed rate limits
- **`crates/augmentagent-channel-twitter/`** — a working Rust client crate
  exists (`api.rs`, `auth.rs`, `channel.rs`, `client.rs`, `types.rs`,
  `validate.rs`, `lib.rs`). The session-bundle struct lives in `auth.rs` /
  `types.rs`. Keychain entry: `augmentagent/twitter/default` with a
  `twitter-auth.json` fallback (same LinkedIn-style migration path).
- **`scripts/twitter-harvest.sh`** — operator-facing helper to lift cookies
  from a real logged-in browser session.

What the doc itself flags as **incomplete**: every section is marked
`REQUIRES LIVE OPERATOR VALIDATION`. The original `/intercept` capture
against a real X session never happened — the doc was reconstructed from
public reverse-engineering knowledge (twikit, twscrape, the-convocation
twitter-openapi). The Rust client is implemented against this reconstructed
spec, so the first time a real session is harvested and run end-to-end is
also the first time the spec is validated.

The biggest fragility vector called out in the doc: the GraphQL `queryId` is
a rotating hash X regenerates on web deploys roughly every 2–6 weeks. The
client has a runtime recovery chain rather than a baked-in constant, which
is the right call.

What was **not** delivered vs the original acceptance criteria:

- "A throwaway `curl`/Node script proves we can auth + hit each endpoint
  from captured cookies" — not on disk anywhere I can find. The harvest
  script exists but a per-endpoint smoke probe does not.

## Recommendation

**Close as done — substantively delivered, with one explicit caveat.** The
issue's primary deliverable (`docs/twitter-protocol.md` + session-bundle
struct) shipped, and the channel crate is already built on top of it. The
spike outgrew its docs-only scope: implementation landed inline rather than
as a follow-up, which is fine but means the "no live validation" risk is now
load-bearing for actual traffic.

Do not pursue a fresh `/intercept` capture as part of this issue. Instead
treat live validation as the first task whenever the Twitter channel moves
out of dry-run, and capture the per-endpoint findings as deltas to
`docs/twitter-protocol.md`.

## Follow-ups

(For the orchestrator / triager to file separately — not filed by Scribe.)

- File: "Live-validate `docs/twitter-protocol.md` against a real harvested
  X session." Acceptance: each of the six sections has the
  `REQUIRES LIVE OPERATOR VALIDATION` banner removed or replaced with a
  dated capture note. Include the `x-client-transaction-id` derivation.
- File: "Twitter friend-post reply automation" (implementation issue that
  consumes the timeline + `CreateTweet` halves of the spec).
- File: "Twitter DM channel" (implementation issue that consumes the DM
  inbox + DM send halves; coordinate with `docs/ig-x-dm-channels.md`).
- File: "queryId staleness watchdog" — observability for the ~2–6 week
  X-redeploy cycle so we notice 400/404 spikes before users do.
- File: "Capture-script regression harness" — a small `scripts/` probe
  that runs the six endpoints against a known-good harvested session and
  asserts the response envelope matches the spec's JSON shapes. Replaces
  the missing acceptance-criterion smoke script.

## Confidence: high
