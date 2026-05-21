# Google Drive Setup

## Category

callback OAuth

## When to use

User wants AugmentAgent to read or write files in their Google Drive, or
status JSON reports `channels.gdrive.configured = false` and the user
wants Drive on.

## Prereqs

- `COMPOSIO_API_KEY` set in `.env` (Drive is a Composio-backed channel
  with the same auth-discovery flow as Gmail).
- Dashboard sidecar installed and running. The OAuth callback lives on
  the dashboard at `/oauth/googledrive/callback`. Verify via
  `augmentagent status --json` (`dashboard.active`, `dashboard.reachable`).
- A logged-in Google account in the same browser session the user will
  open the dashboard URL from. Same Google account can serve both Gmail
  and Drive; the OAuth scopes differ so each provider is consented
  separately.

## Steps

1. Confirm dashboard reachable:
   ```
   augmentagent status --json
   ```
   Read `dashboard.active`, `dashboard.reachable`, `dashboard.port`.
2. Build the start URL from the reported port:
   ```
   http://localhost:<dashboard.port>/oauth/googledrive/start
   ```
   Note the path segment is `googledrive`, not `gdrive` or `drive`. The
   skill must match what `src/dashboard.ts` registers.
3. AskUserQuestion: open that URL, complete Google's consent screen,
   wait for the dashboard's "Drive connected" page. The dashboard's
   callback at `/oauth/googledrive/callback` runs the Composio retrieve
   loop and writes the account row to sqlite.
4. After consent, re-run `augmentagent status --json` and check
   `channels.gdrive.configured` and `channels.gdrive.accounts`.

This channel has no `augmentagent gdrive login` subcommand. The dashboard
URL is the only entry point today.

## Validate

```
augmentagent status --channel gdrive --json
augmentagent gdrive accounts --json
```

The first prints the gdrive block; `configured` should be `true` and
`accounts` at least `1`. The second lists connected Drive accounts. If
empty, the callback wrote no row; consult Common errors.

## Common errors and fixes

- "Discovery failed" on the callback page. The dashboard's gdrive
  callback waits up to a fixed number of retries for Composio to surface
  the connected-account record (see the retry loop in `src/dashboard.ts`
  around `/oauth/googledrive/callback`). If discovery fails, re-running
  the start URL usually clears it; if not, pull dashboard logs via
  `augmentagent logs --unit augmentagent-dashboard.service`.
- "redirect_uri_mismatch" at Google. The redirect must match
  `http://localhost:<DASHBOARD_PORT>/oauth/googledrive/callback` exactly.
  Fix in Google Cloud Console.
- Drive shows configured but polls return zero items. Confirm the
  consented account is the one you expect; run `gdrive accounts --json`
  and verify the email. Composio scopes Drive per account.
- Token refresh failures after weeks of use. Composio handles refresh
  server-side; if it fails the dashboard reports the account as stale.
  Re-run the start URL to re-consent.

## Disarm / undo

Drive has no arming gate (on-by-default once an account is connected).
To disconnect:

```
augmentagent gdrive accounts --json
```

Find the account id, revoke at
`https://myaccount.google.com/permissions`, and remove the row via the
dashboard's UI. No CLI delete verb today.
