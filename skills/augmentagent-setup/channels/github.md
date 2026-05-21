# GitHub Setup

## Category

token paste

## When to use

User wants AugmentAgent to read their GitHub notifications, watch repo
events, or accept push webhooks for the auto-update flow. Status JSON
reports `channels.github.configured = false` and the user wants GitHub
on.

## Prereqs

- A GitHub personal access token (PAT) with the scopes the user needs.
  At minimum `repo` (private repo events) and `notifications`. The PAT
  is pasted by the user; the skill never asks for the user's GitHub
  password.
- The user's GitHub login (e.g. `nolanmak`). The CLI cross-checks the
  PAT's `GET /user` response against the hint and uses the
  server-reported login on conflict.
- For the auto-update webhook: `GITHUB_WEBHOOK_SECRET` set in `.env`
  (generate with `openssl rand -hex 32`). The dashboard exposes
  `POST /api/webhook/github` and verifies the signature.

## Steps

1. AskUserQuestion: tell the user to create a fine-grained PAT at
   `https://github.com/settings/tokens` with the scopes above. Mask the
   PAT in the AskUserQuestion prompt (treat as secret).
2. Run the login command, passing the PAT and the user's login as
   separate flags:
   ```
   augmentagent github login --token <PAT> --login <user>
   ```
   The CLI calls `GET /user`, surfaces the resolved login, and persists
   to the keychain slot `augmentagent/github/<resolved-login>`.
3. (Optional) Subscribe the agent to a repo's events:
   ```
   augmentagent github subscribe --repo <owner/name> --mode <mode>
   ```
   Repos must be `owner/name`; the CLI rejects bare names.
4. (Optional) For the auto-update webhook, tell the user to:
   - Set `GITHUB_WEBHOOK_SECRET` in `.env` (any 32-byte hex string).
   - Run `augmentagent service restart` so the dashboard re-reads it.
   - In the GitHub repo settings, add a webhook pointing at
     `http://<host>:<DASHBOARD_PORT>/api/webhook/github` with the same
     secret, content type `application/json`, "Just the push event".
   The skill is read-only on `.env`; do not edit it.

## Validate

```
augmentagent status --channel github --json
augmentagent github subscriptions --json
```

`configured` should be `true`. `subscriptions --json` lists active repo
subscriptions if the user ran `subscribe`. There is no separate validate
op today; the keychain round-trip on login is the live check.

To smoke the webhook side: push to a subscribed repo and watch the
dashboard logs:
```
augmentagent logs --unit augmentagent-dashboard.service
```
You should see "[webhook] GitHub signature verified" and an update
trigger.

## Common errors and fixes

- "401 Bad credentials" on login. PAT typo, or the PAT is fine-grained
  and missing the `user:read` scope. Re-generate, retry.
- "--login does not match server-reported login" warning. The CLI uses
  the server's login; the warning is informational. If the user is sure
  the hint is right, they have a different account logged in to that
  PAT; re-issue the PAT from the correct account.
- Webhook returns 401 "Invalid GitHub signature". `GITHUB_WEBHOOK_SECRET`
  in `.env` and the secret pasted into the GitHub webhook UI differ.
  Generate a fresh one, set both sides to the same value, restart the
  dashboard.
- "no github auth in keychain" on a later command. Keychain row was
  evicted (gnome-keyring restart, or a stale `--login` slot index).
  Re-run `augmentagent github login --token <PAT> --login <user>` to
  repopulate.

## Disarm / undo

GitHub has no arming gate. To revoke:

1. Tell the user to delete the PAT at
   `https://github.com/settings/tokens`.
2. Delete the keychain slot manually
   (`augmentagent/github/<resolved-login>`); no CLI delete verb today.
3. To stop event polling for a repo:
   ```
   augmentagent github unsubscribe --id <id>
   ```
