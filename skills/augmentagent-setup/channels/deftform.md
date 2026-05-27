# Deftform Setup

## Category

token paste

## When to use

User wants AugmentAgent to react to Deftform submissions (the C&C path
documented in `docs/deft-protocol.md`). Deftform is a form builder; the
agent receives submissions via webhook and can also poll the public REST
API.

## Prereqs

- A Deftform account with at least one form.
- A Deftform API token (Bearer auth). Issued in the Deftform workspace
  settings, scope: read submissions. The user pastes this into `.env`.
- The daemon's dashboard host must be reachable from Deftform's outbound
  webhook IPs (i.e. a public URL, not pure localhost). The webhook path
  is `POST https://<host>/webhooks/deft/<AUGMENTAGENT_DEFT_WEBHOOK_SECRET>`.
- Two env vars in `.env`:
  - `AUGMENTAGENT_DEFT_ENABLED=true` (the arming gate; without this the
    channel is inert).
  - `AUGMENTAGENT_DEFT_WEBHOOK_SECRET=<hex>` (carried in the URL path;
    Deftform fires the webhook at the exact path).

## Steps

1. AskUserQuestion: confirm the user has a Deftform account, has issued
   a workspace API token, and knows the public hostname their dashboard
   is reachable on.
2. Generate the webhook secret. Tell the user to run on the daemon
   host:
   ```
   openssl rand -hex 32
   ```
   and paste the output as `AUGMENTAGENT_DEFT_WEBHOOK_SECRET` in `.env`.
3. Tell the user to also set `AUGMENTAGENT_DEFT_ENABLED=true` in `.env`.
   The skill is read-only on `.env`; the user edits by hand.
4. Restart the daemon so it re-reads `.env`:
   ```
   augmentagent service restart
   ```
   Confirm with AskUserQuestion first.
5. In the Deftform portal, the user registers the webhook URL:
   ```
   https://<host>/webhooks/deft/<the-secret-from-step-2>
   ```
   The form-level webhook fires on every submission.
6. (Optional) For the poll path, persist the API token into the keychain
   via the `augmentagent deft login` subcommand documented in the
   **Login** section below.

## Validate

```
augmentagent status --channel deft --json     # if surfaced; else status --json
```

The deft channel is not in the canonical channels list in
`reference/status-schema.md` today. Use the global `status --json` and
check whether the daemon logs report "[deft] webhook ready" on startup:

```
augmentagent logs --unit augmentagent.service
```

The first real-form submission is the live validation; capture it per
`docs/deft-protocol.md` §4 and confirm the envelope parses.

## Common errors and fixes

- Deftform reports "webhook timeout". The dashboard host is not
  publicly reachable from Deftform's outbound IPs. Either expose the
  dashboard port through a tunnel (cloudflared, ngrok) and use the
  tunnel URL in the webhook registration, or run on a public-facing
  host.
- "401 Unauthorized" on the webhook side. The secret in the URL path
  does not match `AUGMENTAGENT_DEFT_WEBHOOK_SECRET` in `.env`. Re-copy
  the value, restart the daemon.
- Submissions stop firing after a Deftform plan change. Deftform's
  webhook feature may be plan-gated; check the workspace billing page.
- "deft channel inert" in daemon logs. `AUGMENTAGENT_DEFT_ENABLED` is
  empty or false. Set it true and restart.

## Disarm / undo

```
# Edit .env, set AUGMENTAGENT_DEFT_ENABLED=false
augmentagent service restart
```

Also delete the webhook registration in the Deftform portal so Deftform
stops posting to the dead URL. No CLI verb for either side today; both
are manual.

## Login (CLI)

The systemd unit installed by `scripts/install-autostart.sh` pins
`Environment=AUGMENTAGENT_DEFT_ENABLED=1` so the channel is **armed** at
the OS level. The crate stays inert (no network I/O) until a workspace
token is also persisted to the Linux keyring. Three operator verbs:

- `augmentagent deft login [--token <bearer>] [--base-url <url>]`
  Validates the workspace token via `GET /workspace` **before** writing
  anything. On success, persists `DeftAuth` to
  `augmentagent/deft/<workspace_id>` under the Linux Secret Service. If
  `--token` is omitted the CLI prompts on stdin so the secret never
  lands in shell history. Issue a token at
  <https://deftform.com/settings/api>.

- `augmentagent deft status [--workspace-id <id>] [--offline] [--json]`
  Reports whether a token is in the keyring for the named workspace,
  and (unless `--offline`) re-runs `whoami()` to confirm reachability.
  `--workspace-id` falls back to the `AUGMENTAGENT_DEFT_WORKSPACE_ID`
  env var. The `--json` form is what the dashboard's status panel
  consumes.

- `augmentagent deft logout [--workspace-id <id>]`
  Deletes the keychain slot. Idempotent — running it twice is fine.

Linux Secret Service can't enumerate slots, so every `status`/`logout`
invocation must name the workspace (either via `--workspace-id` or
`AUGMENTAGENT_DEFT_WORKSPACE_ID`). `login` learns the workspace id from
the server during the `whoami()` probe and prints it on success — copy
that into `.env` if you want subsequent `status`/`logout` calls to find
the slot without a flag.
