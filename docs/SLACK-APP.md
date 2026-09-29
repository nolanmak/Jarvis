# Slack app: install, verify, rotate, remove

Issue [#1284](https://github.com/nolanmak/Jarvis/issues/1284), part of the
Slack parity epic [#1281](https://github.com/nolanmak/Jarvis/issues/1281).
Transport decisions (Socket Mode, scopes, tokens) are in
[`SLACK-TRANSPORT.md`](SLACK-TRANSPORT.md).

Jarvis talks to Slack in real time through a first-party **Slack app over
Socket Mode**. The app needs no public URL. This page covers creating the
app and managing its credentials with `augmentagent slack app …`; section 5
covers what `serve` does with it (#1287).

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

## 4. Bind the owner (#1286)

Only the bound owner can start agent turns, press owner cards or run
`/jarvis`. Authority is the exact workspace + **member ID** pair; display
names, usernames and emails are never used.

```sh
# Your member ID: Slack profile > "..." > "Copy member ID"
augmentagent slack app owner bind --user U0123ABCD
# Optional private control channel (invite the app first: /invite @Jarvis)
augmentagent slack app owner control set --channel C0123ABCD
augmentagent slack app owner show            # local, no network
augmentagent slack app owner control remove
augmentagent slack app owner unbind
```

| Command | What it does |
| --- | --- |
| `owner bind --user U… [--team T…] [--json]` | Live check with the stored bot token: `auth.test` still answers for this workspace (and gives the Enterprise Grid ID), and `users.info` shows an active, full member of it. Guests (single- or multi-channel), bots and app users (including this app's bot), deactivated accounts, people from connected organizations and members of other teams are refused. Then records the owner and the owner's DM with the app (`conversations.open`). Binding someone else replaces the previous owner and clears the control channel. Nothing is written unless every check passes. |
| `owner show [--team T…] [--json]` | Local state: owner, DM, control channel, the app's bot identity, number of audited rejections. |
| `owner control set --channel C… [--team T…] [--json]` | Live check with `conversations.info`: a private channel, not shared with another organization, not archived, with the app as a member. Replaces any previous control channel. |
| `owner control remove [--team T…] [--json]` | Stop using the control channel; the DM keeps working. |
| `owner unbind [--team T…] [--json]` | Remove the binding and its control conversations. Nobody has owner authority until you bind again. Idempotent; works with `--team` even after `slack app remove`. |

**DM rule.** Recording the DM needs the `im:write` scope (in the manifest
since #1286). If `conversations.open` fails, for example on an app installed
before that scope was added, bind still succeeds and reports the DM as not
recorded. Until a later bind records it, any DM with the app **from the
owner** is accepted; a DM from anyone else is still rejected.

The binding lives in the shared database (`surface_owner_bindings`,
`surface_control_conversations`), not in the credential store, so it
behaves the same on macOS and Linux. Rejected input is audited in
`surface_auth_rejections` (identifiers and a reason code only, never message
text). `serve` enforces the binding on every event (#1287), re-reading it
each time, so an unbind takes effect at once.

## 5. Run it in `serve` (#1287)

`augmentagent serve` starts the interactive surface when an app is installed
**and** an owner is bound for the same workspace. Nothing else is needed:
no Discord token or IDs, no WhatsApp state, no Composio key. It is
independent of Composio ingestion and the Slack digest, which keep their own
cadence. Configuration is read at startup, so restart the daemon
(`augmentagent service --unit daemon restart`) after `install`, `rotate` or
`owner bind`.

| `AUGMENTAGENT_SLACK_INTERACTIVE` | Effect |
| --- | --- |
| unset or `auto` | Run when installed and bound; otherwise report `not_configured` |
| `1`, `true`, `on`, `yes` | Run; missing setup is reported as `misconfigured` (an error in `doctor`) |
| `0`, `false`, `off`, `no` | Do not run; report `disabled`. The credential store is not read |

What it does:

- One Socket Mode link per app. Each envelope is written to the durable
  inbox before it is acknowledged, and the dispatcher is woken at once, so
  an owner message starts a turn immediately; no poll or triage timer is
  involved. A redelivered message is acknowledged and never a second turn.
- Owner messages in the DM with the app or the control channel, and
  `/jarvis` commands, run through the same agent as Discord (wiki, memory,
  tools, skills, audit, provider fallback), which needs `serve --wiki-dir`;
  without it the owner is told how to turn queries on. Answers go through
  the durable outbox and the #1294 delivery path (mrkdwn, splitting,
  reconcile). A DM is answered in the DM; a control-channel message is
  answered in a thread under it.
- Conversations (#1288): the DM, each DM thread and each control-channel
  thread keep their own agent session, so a follow-up continues where the
  last answer left off and separate threads never mix. A message sent while
  that conversation is still working waits its turn. Reply `cancel` (or
  `stop`) in the same thread, or in the DM, to stop the running request;
  the status line shown while it works says so. After a restart, a request
  that was cut off is reported and not re-run. Files you attach (images,
  text, PDF/DOCX) are handed to the agent read-only for that one request.
- Anyone else gets the fixed rejection: back in their DM with the app, or
  as an ephemeral message in a channel. It never reaches the reasoner.
  The same applies to their clicks on approval cards (#1289).
- `serve` is a dry run unless started with `--dry-run false`: turns still
  run, but every send is recorded in the outbox as sent with a `dry-run:`
  provider ID and no Slack method is called.
- On SIGINT a turn in progress is abandoned and its event returned to the
  inbox. If the process ends any other way (SIGTERM from `launchctl` or
  `systemctl`, a crash), the claim is recovered at the next start. Either
  way the event is handled once more; if its answer was already queued,
  that answer is sent and the turn is not run again.
- A Slack failure never stops `serve`, and a Discord or WhatsApp
  configuration error no longer stops Slack.

`augmentagent status` reports it under `interactive.slack`, separately from
`channels.slack` (Composio ingestion): `state` is one of `not_configured`,
`disabled`, `misconfigured`, `connecting`, `connected`, `reconnecting`,
`disconnected` or `stopped`, with `last_event_unix`, `last_send_unix`, and
`detail`/`recovery` when something needs doing. Only a `connected` listener
whose daemon is still reporting is `healthy`; a report older than 60 s reads
as `disconnected`. `doctor` has the matching `interactive.slack` check.

The surface runs inside the existing daemon process, so the existing
launchd and systemd service files start it; no unit, job, socket or path is
added. Under launchd the daemon reads the tokens from the login Keychain,
which needs the user's login session; whether a launchd-run daemon can read
an item written from a terminal is still unverified (#1246), so check
`status` after the first start on a Mac.

For tests and local QA only, a **debug build** runs owner turns through the
real harness with a fake agent instead of the reasoner when
`AUGMENTAGENT_TEST_SLACK_TURN_REPLY` is set (release builds ignore it). The
fake joins the conversation's native session and answers
`<value> <first line> (session <id>, turn <n>)`, plus
`read <file> (<bytes> bytes)` per attachment it could open; a message
containing `slow` takes 15 s, so queueing, `cancel` and restart can be
seen. `AUGMENTAGENT_SLACK_TEST_FILE_HOSTS` (loopback `host:port` only) lets
it download attachments from a local fake.
`crates/augmentagent-cli/tests/slack_serve_cli.rs` runs `serve` that way
against a local fake Slack.

## 6. Approvals on Slack (#1289)

With the app installed and an owner bound, `serve` posts approval cards to
Slack: your DM with the app, or your bound control channel with
`AUGMENTAGENT_SLACK_APPROVAL_CHANNEL=control`. Every card has the draft,
**Approve & Send**, **Revise** (a form), **Skip**, **Quick refine…**, and
**Provide missing info** when the draft needs a detail from you. They make
the same decisions as the Discord card, through the same code.

- The card changes in place: once decided it says what happened (sent,
  skipped, superseded and why) instead of offering buttons. A decision on
  Discord updates the Slack card and a decision on Slack updates the Discord
  card; a second click anywhere is told "Already sent." (or the reason).
- Every card prints a short reference, e.g. `3f2a9c1b`, and the text
  commands that work without buttons: `approve 3f2a9c1b`, `skip 3f2a9c1b`,
  `revise 3f2a9c1b <what to change>`, `refine 3f2a9c1b shorter`,
  `recompose 3f2a9c1b`. `approvals` lists what is pending. Other messages
  (including "send the report…") still go to the agent.
- If a click reaches the daemon late (the Mac was asleep), the Revise form
  cannot open any more; you get a message with the text command instead.
- Routing: `AUGMENTAGENT_APPROVAL_SURFACES=slack` (Slack only, no Discord
  token needed), `discord`, or `discord,slack`; unset sends cards to every
  configured surface. The approvals need `COMPOSIO_API_KEY` for Gmail and
  calendar cards, and a connected Composio Slack workspace to send Slack
  contact replies. Scheduling a send (#1291) is still Discord-only.

For local QA only, a **debug build** sends Composio Slack calls to a
loopback fake when `AUGMENTAGENT_TEST_COMPOSIO_BASE=http://127.0.0.1:<port>`
is set (release builds and non-loopback values ignore it).

## 7. Replying to and writing to Slack contacts (#1290)

Contact messages are always sent **as you**, through the Composio Slack
connection (`augmentagent slack persist-auth`), never as the app. Every
Slack card says where the message goes (**Goes to** `#general · in thread
…`, `DM with Alice Example`) and who sends it (**Sends as** your Slack user
id). If the connection has no user id, Approve refuses to send.

- A drafted reply goes back to the conversation it answers: in the thread
  the message was in, under the message in a channel, top level in a DM.
- To write a new message, reply `compose <person or #channel>: <message>`
  in your DM with the app (for example `compose Alice Example: lunch
  Thursday?`). People are looked up in the wiki's people pages (their
  `identities: slack:` id); channels must be subscribed. If the name
  matches more than one person you are asked which; an unknown name is
  refused. Nothing is sent until you approve the card. The agent (or a
  shell) can do the same with `augmentagent slack compose --to … --text …`
  (`--dry-run` only resolves).
- If a send fails after you approve, the card says why and offers **Retry
  send** (or `approve <ref>`). A send whose outcome is unknown is looked for
  in the conversation before it is sent again; if it cannot be checked you
  are asked to look first.

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
