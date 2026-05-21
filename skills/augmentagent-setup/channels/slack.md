# Slack Setup

## Category

callback OAuth

## When to use

User wants AugmentAgent to read or post in their Slack workspaces, or
status JSON reports `channels.slack.configured = false` and the user
wants Slack on.

## Prereqs

- `COMPOSIO_API_KEY` set in `.env`. Slack OAuth goes through Composio's
  auth flow; without it the start URL returns 503.
- Dashboard sidecar installed and running. The OAuth callback lives on
  the dashboard at `/oauth/slack/callback`. Verify via
  `augmentagent status --json`.
- A logged-in Slack session in the same browser the user will use for
  the consent screen, for the workspace the user wants to enrol.

## Steps

1. Confirm dashboard reachable:
   ```
   augmentagent status --json
   ```
   Read `dashboard.active`, `dashboard.reachable`, `dashboard.port`.
2. Build the start URL from the reported port:
   ```
   http://localhost:<dashboard.port>/oauth/slack/start
   ```
3. AskUserQuestion: open that URL, pick the Slack workspace, complete
   the consent screen. The dashboard's `/oauth/slack/callback` handler
   retrieves the connected-account record from Composio and persists the
   workspace via `augmentagent slack persist-auth` semantics. The
   workspace appears in the sqlite `slack_workspaces` table once the
   retrieve loop succeeds.
4. After consent, list workspaces:
   ```
   augmentagent slack workspaces --json
   ```
   The new workspace's `team_id` should appear.

If the user already holds a Composio entity id and connection id (from a
prior connect outside the dashboard), they can also persist manually:

```
augmentagent slack persist-auth --entity-id <id> --connection-id <id> --composio-api-key <key>
```

Prefer the dashboard start URL for first-time setup.

## Validate

```
augmentagent status --channel slack --json
augmentagent slack workspaces --json
augmentagent slack list-conversations --team-id <id> --limit 5 --json
```

Status `configured` should be `true`. `workspaces --json` should list the
enrolled team. `list-conversations` is the live-credential probe; if it
returns a channel list, the token is good.

## Common errors and fixes

- 503 "Composio API key not configured" on `/oauth/slack/start`. Set
  `COMPOSIO_API_KEY` in `.env`, run `augmentagent service restart`,
  retry the URL.
- Consent screen "this app isn't approved by Slack". Expected on the
  first install; the dashboard's Slack app is workspace-scoped and only
  the workspace admin can approve it. Have an admin run the start URL
  once, then other accounts can re-use the same workspace row.
- `workspaces --json` empty after a green callback page. The Composio
  retrieve loop returned the connection late. Wait 30 seconds, re-run
  status, or retry the start URL.
- `list-conversations` returns "missing_scope". The Slack app needs the
  `channels:read` / `groups:read` scopes; re-consent through the start
  URL after the workspace admin adds those scopes.

## Disarm / undo

Slack has no arming gate (on-by-default once a workspace is enrolled).
To remove a workspace:

```
augmentagent slack remove-workspace --team-id <id>
```

This drops the sqlite row. To wipe every Slack workspace at once:

```
augmentagent slack reset --confirm
```

Destructive; always confirm via AskUserQuestion quoting the command back
to the user before running.
