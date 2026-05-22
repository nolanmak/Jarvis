# Reddit Setup

## Category

callback OAuth

## When to use

User wants AugmentAgent to read their Reddit inbox and reply to messages,
or status JSON reports `channels.reddit.configured = false` and the user
wants Reddit on.

## Prereqs

- A Reddit "installed app" or "web app" client registered at
  `https://www.reddit.com/prefs/apps`. The client must list the dashboard
  callback URL `http://localhost:<DASHBOARD_PORT>/api/reddit/callback`
  (default port 3000) as its redirect URI.
- `REDDIT_CLIENT_ID` set in `.env`. The dashboard reads this directly;
  without it the auth URL returns 503.
- Optional: `REDDIT_REDIRECT_URI` override if the user is fronting the
  dashboard behind a reverse proxy.
- Dashboard sidecar installed and running. Verify via
  `augmentagent status --json`.

## Steps

1. Confirm dashboard reachable and Reddit client id set:
   ```
   augmentagent status --json
   ```
   Read `dashboard.active`, `dashboard.reachable`, `dashboard.port`.
2. AskUserQuestion: confirm the user has registered the Reddit app and
   the redirect URI matches
   `http://localhost:<DASHBOARD_PORT>/api/reddit/callback`.
3. Build the start URL from the reported port:
   ```
   http://localhost:<dashboard.port>/oauth/reddit/start
   ```
   The legacy `/api/reddit/auth` path is still served as an alias for
   apps registered against the original URI; new flows should use the
   canonical `/oauth/reddit/start` form so they match Gmail/Drive/Slack.
4. Tell the user to open that URL, sign in to Reddit, and click Allow.
   The callback at `/oauth/reddit/callback` (or the legacy
   `/api/reddit/callback`, both routed to the same handler) finishes the
   exchange and the dashboard prints "Reddit connected".
5. Alternative headless flow (if the user cannot use a browser on the
   daemon host): generate the URL with the CLI and exchange the code
   manually:
   ```
   augmentagent reddit auth-url --client-id <id> --redirect-uri <uri> --state <random>
   augmentagent reddit exchange --client-id <id> --code <code> --redirect-uri <uri>
   ```
   `exchange` persists the creds to the keychain and prints `{"ok":true}`.

## Validate

```
augmentagent status --channel reddit --json
```

`configured` should be `true`. There is no Reddit `--validate` op today;
the next poll cycle is the live confirmation. Run one manually if you
want immediate signal:

```
augmentagent channel reddit poll-once --dry-run true
```

(Reddit may not expose `poll-once` via the router today; if the CLI
reports "does not support op", fall back to the daemon's natural cadence
and check `last_poll_unix` on the next `status --json`.)

## Common errors and fixes

- 503 "REDDIT_CLIENT_ID not configured" on the auth URL. Set it in
  `.env`, run `augmentagent service restart`, retry.
- "invalid_grant" on the callback. The redirect URI registered at Reddit
  does not match what the dashboard sent. Fix the app's redirect URI at
  `https://www.reddit.com/prefs/apps` and retry.
- Token refresh failures after a long idle. Reddit's refresh tokens are
  long-lived but the client must request `duration=permanent` on the
  initial auth, which the dashboard does. If refresh fails, re-run the
  start URL to re-consent.
- "401 Unauthorized" on the next poll. The persisted creds were lost
  from the keychain (gnome-keyring restart on a headless host). Re-run
  the auth URL.

## Disarm / undo

Reddit has no arming gate. To revoke:

1. Have the user revoke the app at
   `https://www.reddit.com/prefs/apps` (Allowed apps list).
2. Delete the keychain row manually. There is no CLI delete verb today;
   manual removal is the supported path.
