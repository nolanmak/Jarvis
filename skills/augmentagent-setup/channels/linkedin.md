# LinkedIn Setup

## Category

cookie harvest

## When to use

User wants AugmentAgent to read their LinkedIn DMs, post to their feed,
or engage with friend posts. Status JSON reports
`channels.linkedin.configured = false` and the user wants LinkedIn on.

## Prereqs

- A logged-in LinkedIn session at `https://www.linkedin.com/messaging/`.
- Chrome devtools (Application -> Cookies; Network for the `member_urn`
  lookup).
- For feed posting, the arming gate `AUGMENTAGENT_LINKEDIN_POST_CONFIRM`
  must be set to `yes` to clear the first-three-posts confirmation guard
  (see `docs/LINKEDIN.md`).

## Steps

The LinkedIn schema exposes two methods. Read the schema first to decide
which to use:

```
augmentagent setup harvest linkedin --non-interactive --json --creds-out /tmp/linkedin-creds-$$.json
```

Methods returned:

- `devtools_cookies`: paste four values from devtools (`member_urn`,
  `li_at`, `JSESSIONID`, `bcookie`). Use this if the user has no prior
  `/intercept` run.
- `browser_intercept`: auto-extracts from a prior `/intercept` capture
  database. Zero fields; the script reads cookies + URN directly. Use
  this only if `/intercept` has captured logged-in linkedin.com traffic
  already; the user can override the DB path with `CAPTURES_DB=<path>`.

### Devtools cookies path (default)

1. Echo `methods[0].doc_steps` verbatim.
2. AskUserQuestion per field. Mask `li_at`, `JSESSIONID`, `bcookie`
   (`secret = true`). Paste `JSESSIONID` WITH surrounding quotes (e.g.
   `"ajax:0103..."`); the daemon expects them.
3. Write the values as JSON to the `expected_creds_path` printed by the
   schema. Mode 0600.
4. Run:
   ```
   augmentagent linkedin login --cookies-json /tmp/linkedin-creds-$$.json
   ```
5. Delete the temp file:
   ```
   rm /tmp/linkedin-creds-$$.json
   ```

### Browser-intercept path (advanced)

```
bash scripts/linkedin-harvest-from-intercept.sh
```

Runs without prompts. Reads cookies + URN from `captures.db`. The skill
should only pick this when the user confirms `/intercept` has captured
linkedin.com traffic in the recent past.

### Arm if posting is desired

```
augmentagent channel linkedin arm
augmentagent service restart
```

Then set `AUGMENTAGENT_LINKEDIN_POST_CONFIRM=yes` in `.env` to clear the
first-three-posts guard. The skill is read-only on `.env`; tell the user
to edit the file by hand.

## Validate

```
augmentagent status --channel linkedin --json
augmentagent channel linkedin recent
```

`status` should show `configured = true`. `recent` lists the last few
items the channel has processed; on a fresh login this is the cleanest
live-credential probe today. See `docs/LINKEDIN.md` for the full
runbook including cookie-expiry notes.

## Common errors and fixes

- 401 on `recent`. `li_at` expired (typical at 1 year, sooner if the
  user logged out elsewhere). Re-harvest from a fresh browser session.
- "Invalid JSESSIONID format" on login. Cookie was pasted without the
  surrounding quotes. LinkedIn's JSESSIONID always has literal quotes in
  the cookie value column; include them.
- "missing member_urn" on send. The URN is the `urn:li:fsd_profile:...`
  string; on a fresh session, devtools may not show it until the user
  loads the messaging page once. Re-load messaging, retry the Network
  capture.
- Feed posts blocked with "post confirm required". Arming flipped but
  `AUGMENTAGENT_LINKEDIN_POST_CONFIRM` is still unset. Edit `.env`, set
  it to `yes`, run `augmentagent service restart`.

## Disarm / undo

```
augmentagent channel linkedin disarm
augmentagent service restart
```

To remove credentials, delete the keychain slot manually
(`augmentagent/linkedin/<member_urn>`).
