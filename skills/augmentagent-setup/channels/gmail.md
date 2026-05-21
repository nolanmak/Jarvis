# Gmail Setup

## Category

callback OAuth

## When to use

User wants AugmentAgent to read, draft, or reply in their Gmail inbox, or
status JSON reports `channels.gmail.configured = false` and the user wants
it on.

## Prereqs

- `COMPOSIO_API_KEY` set in `.env` (Gmail is a Composio-backed channel; the
  daemon refuses to enrol an account without it).
- Dashboard sidecar installed and running. The OAuth callback lives on the
  dashboard, not the daemon. Verify with `augmentagent status --json` and
  check `dashboard.active = true` and `dashboard.reachable = true`.
- A logged-in Google account in the same browser session the user will open
  the dashboard URL from.

## Steps

1. Confirm the dashboard is up. Run:
   ```
   augmentagent status --json
   ```
   Read `dashboard.active`, `dashboard.reachable`, `dashboard.port`. If
   either flag is false, install the dashboard first (see
   `components/systemd-units.md`) and stop this flow.
2. Build the start URL from the reported port:
   ```
   http://localhost:<dashboard.port>/oauth/gmail/start
   ```
   The default port is 3000.
3. Use AskUserQuestion to tell the user to open that URL in a browser,
   complete Google's consent screen, and wait for the dashboard's "Gmail
   connected" page. The dashboard handles the callback at
   `/oauth/gmail/callback` internally and persists the account to sqlite.
4. After the user confirms the consent flow finished, re-run
   `augmentagent status --json` and read `channels.gmail.configured` and
   `channels.gmail.accounts`. `accounts` should increment by one.

This channel has no `augmentagent gmail login` subcommand. The dashboard
URL is the only entry point today.

## Validate

```
augmentagent status --channel gmail --json
augmentagent gmail accounts --json
```

The first prints the gmail block of the status snapshot; `configured`
should be `true` and `accounts` should be at least `1`. The second lists
every connected account by email address. If the list is empty after the
consent flow, the dashboard's callback failed; consult Common errors.

## Common errors and fixes

- Consent screen 400 with "redirect_uri_mismatch". The dashboard's
  callback URL is hard-coded to `http://localhost:<DASHBOARD_PORT>/oauth/gmail/callback`.
  Either the OAuth client in Google Cloud Console is missing that exact
  redirect URI, or `DASHBOARD_PORT` differs between the start URL and the
  callback registration. Fix in the Cloud Console, restart the dashboard.
- "Composio API key not configured" on the callback page. The dashboard's
  callback handler reads `COMPOSIO_API_KEY` from the daemon's environment.
  Set it in `.env`, run `augmentagent service restart`, retry.
- Callback page hangs at "exchanging code". Composio's auth-discovery call
  is timing out; pull dashboard logs via
  `augmentagent logs --unit augmentagent-dashboard.service` and surface
  the last 50 lines verbatim.
- `gmail accounts` is empty after a green callback page. The dashboard
  wrote the account but the daemon has not picked it up yet; run
  `augmentagent service restart` and re-check.

## Disarm / undo

Gmail is on-by-default once an account is connected (no arming gate). To
disconnect an account:

```
augmentagent gmail accounts --json
```

Find the account id, then revoke via Google Account settings
(`https://myaccount.google.com/permissions`) and delete the row through
the dashboard's account-management UI. There is no CLI delete verb today;
manual removal is the supported path.
