# LinkedIn DM Channel

The daemon polls LinkedIn every 4 hours, runs each inbound DM through the same triage → draft → Discord approval flow as Gmail, and sends via the voyager API when you approve. Outbound messages you send yourself are filtered out so you don't triage your own replies.

## One-time setup

> **The auth file contains your LinkedIn session cookie. Treat it like a password.** Anyone with `linkedin-auth.json` can read and send DMs as you until you log out on linkedin.com (which rotates `li_at`). The `.gitignore` in this repo already excludes `linkedin-auth.json` and `linkedin-cookies.json` so a stray `git add` can't leak it.

### Option A — auto-extract from Claude Intercept (easiest)

If you've already run `/intercept` and browsed `linkedin.com/messaging/` through the proxy, your cookies are already sitting in the intercept capture DB. One command pulls them out:

```sh
LINKEDIN_SSH_TARGET=<user>@<host> \
  ./scripts/linkedin-harvest-from-intercept.sh
```

No prompts, no devtools. The script reads `captures.db`, extracts `li_at` / `JSESSIONID` / `bcookie` from the most recent voyager request's cookie header, pulls your `member_urn` from the URL, writes `linkedin-auth.json`, ships it to the Linux host, and runs `augmentagent linkedin login` there. Total: one command.

Without `LINKEDIN_SSH_TARGET` it just writes the JSON locally and prints the scp+ssh commands to run by hand.

Skip to [Step 3](#step-3--smoke-test) once done.

### Option B — manual cookie paste

If you don't have intercept captures (fresh install, different machine, etc.), use this path instead.

#### Step 1 — grab four values from Chrome devtools

Open https://www.linkedin.com/messaging/ in Chrome (logged in). Open devtools → **Application** tab → **Storage** → **Cookies** → `https://www.linkedin.com`.

Copy the **Value** column for each of:

- `li_at` — your session token (starts with `AQEF...`)
- `JSESSIONID` — **copy with the surrounding double quotes**, e.g. `"ajax:0103540587890015905"` (the quotes are part of the cookie value)
- `bcookie` — starts with `v=2&...`

You also need your own member URN. Easiest way:

1. On `linkedin.com/messaging/`, open devtools → **Network** tab → reload the page
2. Click any request to `voyager/api/*`
3. In the request URL or body, find a string like `urn:li:fsd_profile:ACoAA...` — copy the whole URN, that's yours

#### Step 2 — run the harvest script

On your Mac (or wherever Chrome is logged in):

```sh
# Option A: script writes JSON locally, prints scp/ssh commands for you to run
./scripts/linkedin-harvest.sh

# Option B: script writes + ships + installs in one shot
LINKEDIN_SSH_TARGET=<user>@<host> ./scripts/linkedin-harvest.sh
```

Paste each of the four values when prompted. The script:

1. Writes `linkedin-auth.json` (chmod 600) with the values formatted correctly
2. If `LINKEDIN_SSH_TARGET` is set, `scp`s it to the Linux host, runs `augmentagent linkedin login` remotely, and deletes the temp file
3. Otherwise prints the exact two commands to run yourself

The `login` command probes voyager once before persisting — bad cookies fail fast here instead of three hours later at the first poll.

### Step 3 — smoke test

On the Linux host:

```sh
cd ~/AugmentAgent
./target/release/augmentagent linkedin recent
```

Should print your 15 most recent DM threads with peer name + last-message snippet. If that works, the daemon will start polling LinkedIn every 4h on its own after the next auto-update cycle.

## When cookies expire

LinkedIn invalidates `li_at` on:

- Logout anywhere (including mobile)
- Password change
- Periodic idle expiry (weeks to months)
- Suspicious-activity flag

When it happens you'll see this in the daemon logs:

```
WARN linkedin auth expired — run `augmentagent linkedin login`
```

The channel stops polling until you re-harvest. Gmail keeps running unaffected. To fix: repeat steps 1-3.

## Where the file lives on disk

`default_auth_path()` resolves in this order:

1. `$AUGMENTAGENT_LINKEDIN_AUTH` env override
2. `/Volumes/augmentagent/linkedin-auth.json` (macOS encrypted vault if mounted)
3. `<repo_root>/linkedin-auth.json`

On the Linux daemon host, the usual case is #3 — lives in `~/AugmentAgent/linkedin-auth.json`.

## What `augmentagent linkedin login` actually does

1. Parses the JSON file at `--cookies-json <path>`
2. Validates shape: non-empty `member_urn`, cookies include `li_at` and `JSESSIONID`
3. Probes voyager by fetching your recent conversations — fails fast if cookies are bad
4. Writes the validated JSON to `default_auth_path()` with `harvested_at_ms` stamped

If step 3 fails, nothing is persisted and the existing auth file (if any) is untouched.

## Friend-post engagement (#13)

Alongside DMs, the daemon watches the feeds of people you mark "close" and
drafts a supportive comment for each fresh post — sent to Discord for
approval, never auto-posted.

- Mark a contact: add `close: true` to the front-matter of their
  `people/<slug>.md` wiki page. The page must also carry a `linkedin:`
  identity (`identities.linkedin: urn:li:fsd_profile:...`).
- Cadence: every 6h + jitter (independent of the 4h DM poll).
- Daily cap: 5 engagements/day by default, enforced durably via the
  `linkedin_action_log` table (survives restarts; never double-comments the
  same post).
- Approve a card → the comment is posted via Voyager and the engagement is
  logged against the cap. Rubric lives in `skills/linkedin-triage/SKILL.md`.

## Posting to your feed (#51 / #77)

`augmentagent linkedin post --text "..." [--image path]... [--visibility public|connections] [--dry-run true]`

Voyager-only **text** + **N images** via `contentcreation/normShares`. Repeat
`--image` for a multi-image post; argument order is display order.
`--dry-run true` prints the exact request body without sending. Guards:

- Rolling-24h cap of **3 posts/day** (preflight; defers with a clear error).
  This counts *posts*, not images.
- First 3 lifetime posts require `AUGMENTAGENT_LINKEDIN_POST_CONFIRM=yes` —
  a second-confirmation guard for the highest-blast-radius action.
- Per-post image cap, default 9, overridable with
  `AUGMENTAGENT_LINKEDIN_MAX_IMAGES`. Checked *before* any upload, so an
  over-cap post costs zero network calls instead of leaving orphaned assets.
- All images are read from disk before the first register call, so a missing
  file fails before any upload rather than half-way through.
- The CLI is a manual/test path. The daemon posts through the standard
  Discord approval pipeline.

> **Multi-image is not capture-verified.** The single-image body shape was
> reverse-engineered from live captures; nothing in this repo captures a 2+
> image post. `media` was always a JSON array on the wire, so N entries is the
> natural extension, but two things remain unconfirmed: whether LinkedIn
> accepts N `ShareImage` entries in one `media` array, and whether the
> register step needs a `mediaUploadType` other than `FEEDSHARE_IMAGE` for a
> multi-image asset. Both would fail as an opaque HTTP 400. Confirm with one
> real multi-image post captured through the intercept proxy before relying
> on this.

Uploads run **sequentially**, not concurrently: each register burns a fresh
`x-li-page-instance`, and a parallel burst of registers reads as automated on
the highest-blast-radius surface here.

Deferred to Phase 2 (`Refs #51`): video, polls, articles, scheduling,
browser fallback.

## Tuning

- `AUGMENTAGENT_LINKEDIN_POLL_SECS=14400` — override the default 4h DM poll interval (min 60)
- `AUGMENTAGENT_LINKEDIN_FEED_POLL_SECS=21600` — override the default 6h feed-engagement poll
- `AUGMENTAGENT_LINKEDIN_MAX_ENGAGEMENTS=5` — override the daily friend-post engagement cap
- `AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID=messengerConversations.xxx` — override if LinkedIn rotates the DM queryId (error text includes the current one)
- `AUGMENTAGENT_LINKEDIN_FEED_QUERY_ID=...` — override if LinkedIn rotates the profile-updates feed queryId
- `AUGMENTAGENT_LINKEDIN_MEDIA_UPLOAD_PATH=/voyager/api/...` — override if LinkedIn renames the media-upload register endpoint
- `AUGMENTAGENT_LINKEDIN_CLIENT_VERSION=1.13.x` — override the `x-li-track` clientVersion sent on content-creation calls
- `AUGMENTAGENT_LINKEDIN_AUTH=/path/to/file.json` — use a non-default cookie path

## Risks

- **Account**: LinkedIn's ToS discourages automated access. Conservative poll cadence + jitter keeps the footprint small, but there's always non-zero risk your account gets flagged. Don't send dozens of replies per day; don't run this on an account you can't afford to lose.
- **Cookie theft**: if `linkedin-auth.json` leaks (git commit, backup, shared drive), the attacker has full DM read + send until you log out of linkedin.com. Rotate by logging out + re-harvesting.
- **queryId rotation**: LinkedIn can change the `messengerConversations.xxx` id any deploy. When that happens, re-capture it via Chrome network tab and export `AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID`.

## Archive reads and backfill

`linkedin recent-dms --limit 40 --json` follows mailbox pages and includes group
conversations and conversations whose newest message is yours. `--category`
selects the mailbox category (default `INBOX`); `--max-pages` bounds requests.
A limit of zero performs no requests. A page budget of zero returns a resumable
partial result. Limits above the mailbox size stop at server exhaustion.

The JSON contract is version 2: an object with `schema_version`, `account_id`, `conversations`,
`category`, `pages`, `complete`, and `next_cursor`, replacing the old flat DM
array. Each conversation contains its stable `urn`, `title`, `group` flag,
`participants`, and source `last_activity_ms`. These are conversation records,
not message snippets or proof of complete message history. Agent integrations
must read `conversations` and inspect `complete` instead of counting text lines.

Pass the returned `next_cursor` unchanged with the same category to resume.
Continuation retains seen IDs across overlapping pages and is bound to the
signed-in account. If a partially consumed page changes order or membership,
the command reports that the listing must restart; it does not silently skip
messages. A continuation is read state, not authentication material.

```sh
augmentagent linkedin recent-dms --limit 40 --category PRIMARY_INBOX --json
augmentagent linkedin recent-dms --limit 40 --category PRIMARY_INBOX --cursor '<returned-cursor>' --json
augmentagent linkedin thread '<conversation-urn>' --max-pages 10 --json
augmentagent linkedin backfill --max-pages 100
augmentagent linkedin backfill --dry-run --max-pages 100
augmentagent linkedin archive-status --category PRIMARY_INBOX
```

A rejected continuation (HTTP 400 or 410 on a request carrying a cursor) gets
one automatic restart per mailbox or thread reconciliation. The restart keeps
stored messages and deduplicates replayed IDs. Its allowance is durable across
process restarts; a repeated rejection records an error and defers retries
instead of restarting indefinitely. This is a conservative recovery policy,
not a claim that every 400 response means an expired LinkedIn cursor. A first-page
failure, authentication error, or throttle does not trigger this recovery.
Conflicting copies of one message ID, or an explicit message conversation ID
that disagrees with the requested thread, fail the page instead of choosing an
arbitrary copy. Identical copies are deduplicated. Cancellation leaves committed
checkpoints intact; a started read still consumes the invocation's request budget,
while durable page counters advance only after a successful transaction.

Authentication, redirect, and rate-limit failures pause archival reads for the
whole account, including category discovery. The pause survives restarts and
explicit category changes; `Retry-After` controls throttling, with a 60-second
backoff for session failures. Inventory failures also persist a backoff.
Search/status coverage includes `accounts` with the last error and retry time,
even when discovery failed before any category was recorded. Successful reads
begun after the pause clear its error; older concurrent requests cannot clear
a newer pause.

Backfill defaults to all categories exposed by the account's mailbox inventory.
It stores group and owner messages without drafting, raising cards or sending.
`--dry-run` performs the same bounded live reads and indexing in a fresh in-memory
database. It does not open or migrate the configured database, advance durable
checkpoints, migrate credentials, or repair credential-file permissions. Its
counts describe a fresh traversal, not the number of new rows relative to an
existing archive. JSON identifies `dry_run`, `persistent_writes`, the
`all_accessible_history` scope, request `pages`, and completion. Category discovery
consumes one request from `--max-pages`; a zero budget performs no requests or
credential reads and reports incomplete coverage. Missing credentials fail
without creating database or credential files.
Repeat bounded invocations to resume durable progress; `--refresh` reconciles
mailbox heads while preserving imported history. Each invocation with a positive page budget indexes at most 1,000 queued
entries, checking cancellation between batches. Remaining LinkedIn index work
keeps coverage incomplete; another bounded invocation resumes it. A zero-page
budget performs no indexing. Completion requires mailbox
and thread exhaustion and a healthy search index. A successful request alone
does not establish complete coverage.

For stored messages, use `search_messages` with `with:<name-or-stable-identity>`
and `in:linkedin`, then `read_conversation_thread`. Membership includes silent
participants; `from:` restricts actual authorship. Equal display names return
ambiguity. Continue stored thread reads using `next_offset`, `next_body_offset`,
`account_id`, and `snapshot_max_rowid` to reconstruct full Unicode text. Inspect
the returned coverage state and observation time before describing a thread as
complete or an introduction as unanswered.

### Live ingestion and approval

Production polling uses the same paginated archive reader as backfill. Each
poll has a 100-request budget, including category discovery, and resumes
unfinished histories before admitting their messages for triage. Completed
mailbox scopes are reconciled again on later polls to find changed threads.

The first live poll persists an activation timestamp. Historical messages stay
archive-only, including after restart; newly received messages are eligible
based on their source timestamps. A separate seen-ID ledger handles equal-time
arrivals. Completed direct threads queue one candidate per burst and pass their
stored, source-ordered history to triage and drafting. Pending candidates survive
restart, and a durable claim prevents concurrent workers from triaging the same
candidate. Claims renew while a worker runs, release on normal exit, and expire
after an hour without renewal following a crash.

Group messages and owner-latest threads are stored and indexed without entering
triage. Direct eligibility requires two known participants, including the owner
and the actual sender. Reclassification retires undispatched candidates.
Authoritative conversation metadata is checked before archived candidates enter
triage, and the send API checks direct membership again before an approved reply
is posted. Approval atomically claims the pending action before attempting a
send, so simultaneous clicks or a replay after restart cannot send that action
twice. A stale direct-message card is refused if current membership identifies
a group or is unknown. The read and backfill commands never grant group-send
permission. A `linkedin dm` invocation without `--post` only prints the supplied
text; it neither generates a draft nor needs credentials. Direct membership is
validated before any approval card is persisted and again before sending.

### Search coverage and account identity

Both `search_messages` and the memory tool's `search_conversation_history`
return `account_id` with hits. Pass that identity with `thread_id` when reading
stored threads. Search responses include `linkedin_coverage` whenever LinkedIn
is in scope, including zero-hit results. Its scope is explicitly
`known_categories_only`; recorded categories do not prove that every current
mailbox category has been discovered. A successful category discovery persists
its `inventory.categories` and `inventory.observed_at_ms` on the account, and
creates incomplete progress records even when no request budget remains for
those categories. The inventory is an observation, not a guarantee that today's
API category set is unchanged. Previously stored categories and messages remain
available when absent from a later inventory. Per-category results expose last success,
last error, retry time, failed thread count, and search-index health.

`archive-status` reads persisted state without requiring working cookies. Its
version-1 response contains `category`, `coverage`, and `threads`, covering
recorded accounts in the requested category. Each thread reports `history_state`
(`unknown`, `partial`, `failed`, or `complete_at_observation`), direct/group kind,
source-time bounds, counts by direction and text/non-text kind, last success/error,
retry time, continuation presence, and separate index-health counts. History
exhaustion does not imply that indexing is finished. Opaque cursor values and
message bodies are omitted.

`mailbox_pages` and each thread's `pages` count durable page commits, including
replayed pages during reconciliation. `committed` is cumulative since tracking
began; `current_cycle` resets on an explicit mailbox refresh or newly observed
thread activity. Rejected transactions and a concurrent importer's losing page
do not count. A rejected-cursor restart retains the cycle count. Null counters
mean no page receipt was recorded; they are not invented from message totals.
The coverage `scope` is `all_accessible_history`, with source-date bounds describing
the stored rows rather than claiming that missing upstream history was recovered. A missing progress record is `not_synced`, not proof
that the database contains no older message rows. Legacy LinkedIn rows use their
stored `receivedAt` source timestamp when parseable, while import time remains
separately labeled. Their history coverage stays unverified until reconciled.
