# LinkedIn DM Channel

The daemon polls LinkedIn every 4 hours, runs each inbound DM through the same triage → draft → Discord approval flow as Gmail, and sends via the voyager API when you approve. Outbound messages you send yourself are filtered out so you don't triage your own replies.

## One-time setup

> **The auth file contains your LinkedIn session cookie. Treat it like a password.** Anyone with `linkedin-auth.json` can read and send DMs as you until you log out on linkedin.com (which rotates `li_at`). The `.gitignore` in this repo already excludes `linkedin-auth.json` and `linkedin-cookies.json` so a stray `git add` can't leak it.

### Option A — auto-extract from Claude Intercept (easiest)

If you've already run `/intercept` and browsed `linkedin.com/messaging/` through the proxy, your cookies are already sitting in the intercept capture DB. One command pulls them out:

```sh
LINKEDIN_SSH_TARGET=nolan-makatche@100.91.92.24 \
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
LINKEDIN_SSH_TARGET=nolan-makatche@100.91.92.24 ./scripts/linkedin-harvest.sh
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

## Tuning

- `AUGMENTAGENT_LINKEDIN_POLL_SECS=14400` — override the default 4h poll interval (min 60)
- `AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID=messengerConversations.xxx` — override if LinkedIn rotates the queryId (error text includes the current one)
- `AUGMENTAGENT_LINKEDIN_AUTH=/path/to/file.json` — use a non-default cookie path

## Risks

- **Account**: LinkedIn's ToS discourages automated access. Conservative poll cadence + jitter keeps the footprint small, but there's always non-zero risk your account gets flagged. Don't send dozens of replies per day; don't run this on an account you can't afford to lose.
- **Cookie theft**: if `linkedin-auth.json` leaks (git commit, backup, shared drive), the attacker has full DM read + send until you log out of linkedin.com. Rotate by logging out + re-harvesting.
- **queryId rotation**: LinkedIn can change the `messengerConversations.xxx` id any deploy. When that happens, re-capture it via Chrome network tab and export `AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID`.
