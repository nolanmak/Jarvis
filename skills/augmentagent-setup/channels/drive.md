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

For CLI-managed sign-in, run:

```sh
node scripts/connect-google.mjs start --toolkit googledrive --email owner@example.com
# Or generate separate links for several accounts:
node scripts/connect-google.mjs start --toolkit googledrive --count 4
node scripts/connect-google.mjs finish
node scripts/connect-google.mjs status
```

Open each printed link and consent with the intended Google account, then run
`finish`. The CLI verifies the ACTIVE connection and provider email before
saving it. `--email` rejects accidental sign-in to a different address. Each
attempt has separate persisted state; no dashboard callback is required.
The same CLI supports `--toolkit gmail` for a new mailbox.

## Validate

```
augmentagent status --channel gdrive --json
augmentagent gdrive accounts --json
```

The first prints the gdrive block; `configured` should be `true` and
`accounts` at least `1`. The second lists connected Drive accounts. If
empty, the callback wrote no row; consult Common errors.

## Common errors and fixes

- Callback redirects with `googledrive=error`. Only ACTIVE connections matching
  the pending user and Drive toolkit are persisted. The callback also checks
  the Drive profile through Composio. Retry the connect URL; inspect dashboard
  logs if the connection or profile cannot be verified. Project-wide account
  discovery is intentionally not used.
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

## On-demand agent tools

The interactive agent can use `gdrive accounts`, `gdrive search`, `gdrive get`,
and `gdrive read` without enabling change-feed notifications. Example:

```sh
augmentagent gdrive search --query "trashed = false and name contains 'plan'"
augmentagent gdrive read --file-id FILE_ID
```

Use `--account` for multiple accounts. Search accepts Google Drive v3 query
syntax and returns JSON with pagination. Read supports UTF-8 text files and
Docs/Slides text exports; Sheets CSV exports cover the first sheet only.
Binary files are discoverable but not text-readable. The tool reports content
truncation; `--max-chars` can raise the output limit up to 200000.

API contracts: https://docs.composio.dev/toolkits/googledrive and
https://docs.composio.dev/docs/tools-direct/executing-tools.
