# Slack app: install, verify, rotate, remove

Issue [#1284](https://github.com/nolanmak/Jarvis/issues/1284), part of the
Slack parity epic [#1281](https://github.com/nolanmak/Jarvis/issues/1281).
Transport decisions (Socket Mode, scopes, tokens) are in
[`SLACK-TRANSPORT.md`](SLACK-TRANSPORT.md).

Jarvis talks to Slack in real time through a first-party **Slack app over
Socket Mode**. The app needs no public URL. This page covers creating the
app and managing its credentials with `augmentagent slack app …`. Wiring
the app into `serve` is #1287; until then an installed app is stored and
verified but not yet listened to.

## 1. Create the app from the manifest

```sh
augmentagent slack app manifest > slack-app-manifest.json
```

The same file is checked in as [`slack-app-manifest.json`](slack-app-manifest.json).
At api.slack.com/apps choose **Create New App → From a manifest**, pick the
workspace, and paste the JSON. The manifest:

- enables **Socket Mode** and **interactivity** with no request URLs;
- subscribes to `app_mention`, `message.im`, `message.channels`,
  `message.groups` and `message.mpim`;
- declares the `/jarvis` slash command;
- requests exactly the bot scopes in `REQUIRED_BOT_SCOPES`
  (`crates/augmentagent-channel-slack/src/app.rs`), no user scopes. A test
  (`tests/app_manifest.rs`) fails if the manifest and the code drift;
- turns **token rotation off**, because the daemon cannot refresh expiring
  bot tokens yet.

Then:

1. **Basic Information → App-Level Tokens → Generate Token and Scopes**, add
   the `connections:write` scope. Copy the app-level token (begins `xapp`).
2. **Install App** (or **OAuth & Permissions → Install to Workspace**).
   Copy the **Bot User OAuth Token** (begins `xoxb`).

## 2. Install

Tokens are never accepted as command-line arguments (they would end up in
shell history and the process list). Use one of:

```sh
# stdin, one token per line, any order (paste, then Ctrl-D)
augmentagent slack app install --stdin

# files (one token each)
augmentagent slack app install --app-token-file ~/app.token --bot-token-file ~/bot.token

# environment
AUGMENTAGENT_SLACK_APP_TOKEN=… AUGMENTAGENT_SLACK_BOT_TOKEN=… augmentagent slack app install
```

Install checks the tokens by prefix first (a bot token where the
app-level token belongs is rejected before any network call), then live:
`auth.test` with the bot token and `apps.connections.open` with the
app-level token. It reports the workspace, bot user, app id and granted
scopes, and names any missing required scope. Nothing is stored unless every
check passes. Running install again for the same workspace replaces the
tokens (reinstall).

Every command takes `--json`. Failures exit 1 and print a `recovery:` line
(or `{"ok":false,"error":…,"message":…,"recovery":…}` with `--json`).

## 3. Day-to-day commands

| Command | What it does |
| --- | --- |
| `slack app status [--team T…] [--json]` | Local state only: installed workspaces, bot user, scopes, timestamps, whether a Composio connection exists too. |
| `slack app verify [--team T…] [--json]` | Live check of the stored tokens; refreshes stored scopes and `verified_at`. Changes nothing on failure. |
| `slack app rotate [--team T…] --stdin` | Replace the app-level token, the bot token or both. The new tokens are verified and must belong to the same workspace, or the old ones stay. |
| `slack app remove [--team T…]` (alias `disconnect`) | Delete the stored app credentials. Idempotent. Does **not** revoke the tokens at Slack. |

`--team` can be omitted when exactly one workspace is installed.

To revoke at Slack (for example after a leak), regenerate the app-level
token and reinstall the app (or uninstall it from the workspace), then
`slack app rotate` or `slack app remove`.

## Where credentials live

| Connection | Credential slot | Index | Managed by |
| --- | --- | --- | --- |
| Composio ingestion (polling) | `augmentagent/slack/<team_id>` | `slack_workspaces` table | `slack login`, `persist-auth`, `remove-workspace`, `reset` |
| Interactive app (Socket Mode) | `augmentagent/slack-app/<team_id>` | `augmentagent/slack-app/_installs` (team ids only) | `slack app …` |

Slots live in the macOS Keychain, or the platform keyring on Linux, through
`augmentagent-auth`. The app slot holds both tokens plus non-secret
metadata (bot user, scopes, timestamps). Tokens are never written to the
database, printed, or logged; the CLI tests assert this for stdout, stderr
with debug logging, and the database file.

**Daemon access is unverified.** A Keychain item written from a terminal is
not proven readable by a launchd-run daemon (#1246). On macOS run
`augmentagent doctor --keychain-probe`; status reports
`daemon_credential_access: "unverified"` until that is proven on a real Mac.

## When both connections exist for one workspace

They are independent: each has its own slot, and no command touches the
other's.

- `slack app remove` leaves Composio ingestion running; `slack
  remove-workspace` and `slack reset` leave the app installed.
- `slack app status` shows `composio_connected` per workspace so the overlap
  is visible.
- Both can run at once. Composio keeps polling the conversations it is
  subscribed to; the app receives real-time events. Reconciling the two
  streams so a message is not handled twice is #1296. Until then, prefer the
  app for conversations with Jarvis and keep Composio subscriptions for
  contact ingestion.

## Testing and local QA

Two environment variables let the real binary run without Slack or the
Keychain. Both are for tests and local QA only:

- `AUGMENTAGENT_SLACK_API_BASE` — Web API base URL. Must be `https://`, or
  `http://` on `localhost`/`127.0.0.1`/`[::1]`. The CLI logs a warning when
  it is set.
- `AUGMENTAGENT_INSECURE_CREDENTIAL_DIR` — store **all** `augmentagent-auth`
  credentials as plaintext files (dir `0700`, files `0600`) under this
  directory instead of the Keychain/keyring. Never point it at real
  secrets.

`crates/augmentagent-cli/tests/slack_app_cli.rs` drives every command this
way against a mock Slack.
