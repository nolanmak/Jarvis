# Slack runbook: install, verify, operate, upgrade, roll back, remove

Owner runbook for the interactive Slack surface on macOS and Linux
([#1299](https://github.com/nolanmak/Jarvis/issues/1299), part of the Slack
parity epic [#1281](https://github.com/nolanmak/Jarvis/issues/1281)). Command
reference: [`SLACK-APP.md`](SLACK-APP.md). Transport and limits:
[`SLACK-TRANSPORT.md`](SLACK-TRANSPORT.md). Live voice:
[`SLACK-LIVE-VOICE.md`](SLACK-LIVE-VOICE.md).

Every `augmentagent` command on this page is checked by
`crates/augmentagent-cli/tests/runbook_commands.rs`, which parses each one
against the real CLI, so the page cannot drift from the binary. Commands
assume `augmentagent` is the release binary on your `PATH` (for example
`export PATH="$PWD/target/release:$PATH"` from the checkout) and that you run
them from the checkout, where the daemon also runs and reads `.env`.

## What is qualified

| Host | State |
| --- | --- |
| macOS, Apple Silicon | Pending real-host acceptance (#1300). CI runs every Slack test on `macos-latest`. |
| macOS, Intel | Pending, qualified separately from Apple Silicon (#1300). Not supported until recorded. |
| Linux (systemd) | Pending real-host acceptance (#1300). Credentials persist since [#1325](https://github.com/nolanmak/Jarvis/issues/1325) in an owner-only file store (`credential_backend: private-file`); CI proves a second process reads what the first stored. After upgrading from an earlier build, re-run `slack app install` (see Linux specifics). |

A CI or mocked pass never qualifies a host. The acceptance script in
section 11 is how a host gets qualified.

## How it runs

The Slack surface runs **inside the existing daemon** (`augmentagent serve`)
over Socket Mode: outbound connections only, no public URL, no new service,
unit, launchd job, socket or port. The daemon is installed once with
`augmentagent install autostart` (a launchd LaunchAgent on macOS, a
`systemd --user` unit on Linux), which sets the checkout as the working
directory, so every setting below goes in the checkout's `.env` and is read
at the next daemon start. The installers pass no Slack setting or token
into the service definition, and do not need to.

| Setting (`.env`) | Effect |
| --- | --- |
| `AUGMENTAGENT_SLACK_INTERACTIVE` unset or `auto` | Run when an app is installed and an owner is bound |
| `AUGMENTAGENT_SLACK_INTERACTIVE=1` | Run; missing setup is an error in `status`/`doctor` |
| `AUGMENTAGENT_SLACK_INTERACTIVE=0` | Do not run (`disabled`); credentials are not read |
| `AUGMENTAGENT_APPROVAL_SURFACES` unset or `auto` | Approval cards go to every configured surface (Discord and/or Slack) |
| `AUGMENTAGENT_APPROVAL_SURFACES=slack` (or `discord`, `discord,slack`) | Approval cards go only to the named surfaces; with `slack` alone the Discord broker is not started and a Discord token without `DISCORD_CHANNEL_ID` is not an issue |
| `AUGMENTAGENT_SLACK_APPROVAL_CHANNEL` unset or `dm`, or `control` | Where Slack approval cards are posted: the owner DM (default) or the control channel (section 4) |
| `AUGMENTAGENT_NOTIFY_SURFACES` unset or `auto` | Proactive notifications (digests, research results, calendar reminders, tool audit notices, health alerts, review results) go to every configured surface |
| `AUGMENTAGENT_NOTIFY_SURFACES=slack` (or `discord`, `discord,slack`) | Notifications go only to the named surfaces; Slack alone needs no Discord credential |
| `AUGMENTAGENT_NOTIFY_SURFACES_<CLASS>` (`DIGEST`, `RESEARCH`, `REMINDER`, `AUDIT`, `HEALTH`, `REVIEW`) | The same, for one class; overrides `AUGMENTAGENT_NOTIFY_SURFACES` |
| `AUGMENTAGENT_SLACK_NOTIFY_CHANNEL` unset or `dm`, or `control` | Where Slack notifications are posted: the owner DM (default) or the control channel |
| `AUGMENTAGENT_NOTIFY_LATE_AFTER_SECS` (default `300`) | A notification delivered later than this after it was due is marked late |

**Notifications (#1295).** Slack notifications are queued on the durable
outbox and sent by the daemon, so a scheduled one-shot (`digest --post true`,
`research --post true`, `calendar poll-once`, `autopr-health --notify`) queues
them even while the daemon is stopped or the Mac sleeps. When the daemon
runs again they go out one about every second (never a burst), each marked
late when it waited longer than the threshold, and a producer that runs
twice posts once. Discord keeps its destinations (`DISCORD_CHANNEL_ID`,
`DISCORD_WEBHOOK_URL`, the channel a request came from); a failure on one
surface never stops the other and is logged. Slack failures show in
`status` as outbox retries and dead letters.

Never set `AUGMENTAGENT_INSECURE_CREDENTIAL_DIR` or
`AUGMENTAGENT_SLACK_API_BASE` outside tests: the first stores every
credential as plaintext files, the second points the daemon at a fake Slack.
`status` and `doctor` flag the plaintext store as an error, both for the
command you run and for the running daemon.

## 1. Prerequisites

Build and install the daemon:

```sh
cargo build --release -p augmentagent-cli
augmentagent install autostart
augmentagent service --unit daemon status
```

To register the service without it ever sending (a first run on a new
host), install it as a dry run and reinstall without the variable later:

```sh
AUGMENTAGENT_AUTOSTART_DRY_RUN=true augmentagent install autostart
```

Tools the Slack features call, resolved on the service `PATH`:

| Host | Install | Used for |
| --- | --- | --- |
| macOS, Apple Silicon | `brew install ffmpeg poppler pandoc` (under `/opt/homebrew/bin`) | voice clips (`ffmpeg`), PDF (`pdftotext` from poppler), DOCX (`pandoc`) |
| macOS, Intel | `brew install ffmpeg poppler pandoc` (under `/usr/local/bin`) | same |
| Debian/Ubuntu | `sudo apt install ffmpeg poppler-utils pandoc` | same |

The launchd installer puts both Homebrew prefixes on the service `PATH`.
Voice transcription also needs whisper.cpp (`scripts/build-whisper.sh`).

## 2. Create the Slack app from the manifest

```sh
augmentagent slack app manifest > slack-app-manifest.json
```

At api.slack.com/apps choose **Create New App > From a manifest**, pick the
workspace and paste the file. Then:

1. **Basic Information > App-Level Tokens > Generate Token and Scopes** with
   the `connections:write` scope. Copy the app-level token (`xapp-…`).
2. **Install App**. Copy the **Bot User OAuth Token** (`xoxb-…`).

Keep token rotation off (the manifest does): the daemon cannot refresh
expiring bot tokens.

## 3. Install the tokens

Paste both tokens, one per line in any order, then press Ctrl-D. Tokens are
never accepted as arguments.

```sh
augmentagent slack app install --stdin
augmentagent slack app status
augmentagent slack app verify
```

Install checks both tokens live and stores nothing unless every check
passes, including the required bot scopes. `status` is local (no network);
`verify` is a live check that refreshes the stored scopes.

On macOS the tokens go to the login Keychain. On Linux they go to
owner-only files under `credentials` in the state directory (#1325).

## 4. Bind the owner and choose the DM or a control channel

Your member ID is in your Slack profile under **... > Copy member ID**.

```sh
augmentagent slack app owner bind --user U0123ABCD
augmentagent slack app owner show
```

Binding records your DM with the app; the DM is always the owner channel.
To also use a private control channel, create it, invite the app
(`/invite @Jarvis`) and set it:

```sh
augmentagent slack app owner control set --channel C0123ABCD
```

To go back to the DM only:

```sh
augmentagent slack app owner control remove
```

Then start the surface:

```sh
augmentagent service --unit daemon restart
```

## 5. Verify

```sh
augmentagent status
augmentagent status --json
augmentagent doctor
augmentagent doctor --deep
```

`interactive.slack` in `status` is the live Socket Mode surface; it is
separate from `channels.slack`, which is Composio ingestion. The fields that
matter:

| Field | Healthy | Meaning |
| --- | --- | --- |
| `state` / `healthy` | `connected` / `true` | Only a connected listener whose daemon is still running and reported in the last 60 s is healthy. |
| `app_installed`, `owner_bound` | `true`, `true` | The two setup steps; `detail` and `recovery` name the first one missing. |
| `credentials` | `usable` | `missing` (none stored), `present` (stored, not yet proven readable by the daemon), `unreadable` (this shell cannot read the credential store; `app_installed` is then `null`), `usable` (the daemon connected with them). Never `usable` without that proof. |
| `last_event_unix`, `last_send_unix` | recent | Last inbound event and last accepted send. |
| `reconnects` | small | Times the running daemon lost and re-opened the link. |
| `delivery.slack` | zeros except in-flight work | Backlog, retries, sends awaiting reconcile, dead letters. |
| `credentials` (top level) | `macos-keychain`, persistent | The credential backend of the command you ran. |
| `daemon_report` | `running: true`, not insecure | What the running daemon started with. |
| `config_issues` | empty | Each problem has `detail` and `recovery`. |

`doctor` turns the same facts into findings with a suggested command:
`interactive.slack`, `interactive.slack.credentials`, `surface_delivery`,
`credential_backend`, one `config.<id>` per configuration issue, and with
`--deep`, `slack_app.scopes` (granted versus required scopes, read from the
stored install).

### Common failures

| What you see | Cause | Fix |
| --- | --- | --- |
| `detail: no interactive Slack app is installed` | Section 3 not done, or the tokens are in another credential store | `augmentagent slack app install --stdin` |
| `detail: the Slack app is installed but no owner is bound` | Section 4 not done | `augmentagent slack app owner bind --user U0123ABCD`, then restart |
| `state: disconnected`, "has not reported this surface" | Daemon not running, or started before the install | `augmentagent service --unit daemon restart` |
| `state: disconnected`, "no heartbeat" | Daemon stopped or stuck | Restart; then read the logs (section 6) |
| `state: disconnected`, token rejected | App-level token revoked | Section 7, then restart |
| `state: reconnecting` for minutes | Network, or Slack is down | Check the network and slack-status.com; the daemon retries on its own |
| `credentials: unreadable`, `app_installed: null`, `config.credentials.unreadable` | This shell cannot read the credential store: on macOS an SSH or remote session, a locked login Keychain, or no login session | Run `augmentagent doctor --keychain-probe` from your logged-in session, unlock the login Keychain, rerun `augmentagent status` |
| `credentials: present` long after a restart (macOS) | The daemon cannot read the Keychain item | `augmentagent doctor --keychain-probe`, unlock the login Keychain, restart |
| `config.discord.approval_broker` | `DISCORD_BOT_TOKEN` set without a numeric `DISCORD_CHANNEL_ID`; serve runs without Discord approvals | Set `DISCORD_CHANNEL_ID` in `.env` (or remove the token), restart |
| `config.credentials.insecure_file_store` or `config.daemon.insecure_file_store` | `AUGMENTAGENT_INSECURE_CREDENTIAL_DIR` is set | Remove it from `.env` and the shell, reinstall the tokens, restart |
| `credential_backend: unavailable` (Linux) | Neither `HOME` nor an absolute `XDG_STATE_HOME` is set, so there is nowhere to keep credentials | Run the CLI and the daemon with `HOME` set (systemd user units set it), re-run `augmentagent slack app install`, restart |
| `credential_backend: keyring-mock` | A build older than #1325 on Linux, or an OS with no supported store | Upgrade, then re-run `augmentagent slack app install` |
| `slack_app.scopes` error | The app lacks a required scope | Add it in the app settings, reinstall the app, `augmentagent slack app rotate --stdin`, restart |
| `surface_delivery` warning | Dead letters or sends awaiting reconcile | `augmentagent status --json` shows which surface; the daemon reconciles on its own |

## 6. Logs

Both installers send the daemon's output to the same files,
`~/.local/state/augmentagent/stdout.log` and `stderr.log` (or under
`$XDG_STATE_HOME/augmentagent/`).

```sh
augmentagent logs --unit daemon --lines 200
augmentagent logs --unit daemon -f
```

On macOS `augmentagent logs` tails those files. On Linux it reads the
systemd journal, which holds only the unit's start/stop lines because the
unit appends its output to the files; read them directly:

```sh
tail -n 200 ~/.local/state/augmentagent/stderr.log
```

Slack lines start with `slack interactive:` and carry the durable event
`seq` of the turn; the workspace and channel are in the database row that
`seq` names (the turn line itself does not name them yet, #1288). Tokens,
socket URLs and message text are never logged, including at trace level for
the Slack, store and credential crates;
`crates/augmentagent-cli/tests/slack_ops_cli.rs` scans the output of a full
flow (install, bind, serve, an owner turn, a rejected stranger, a reconnect,
status, doctor) for synthetic tokens and message bodies.

## 7. Rotate tokens

Generate the new token(s) at api.slack.com/apps, paste them, then restart:

```sh
augmentagent slack app rotate --stdin
augmentagent slack app verify
augmentagent service --unit daemon restart
```

The new tokens must belong to the same workspace; on any failure the old
ones stay. After a leak, also regenerate the token at Slack: rotating
locally does not revoke the old one.

## 8. Upgrade

The updater (`augmentagent install autoupdate`) pulls, rebuilds and restarts
on its own; a failed build keeps the old binary running. By hand:

```sh
git pull --ff-only
cargo build --release -p augmentagent-cli
augmentagent service --unit daemon restart
augmentagent status --json
```

The store migrates itself on start. Slack migrations only add tables and
columns with defaults, and pending inbound events, queued and in-flight
sends, the owner binding, control channel and listener health all survive;
`crates/augmentagent-channel-slack/tests/upgrade_state.rs` upgrades a
database written by the previous release and checks each of them. The
install record in the Keychain/keyring is not touched by an upgrade.

## 9. Roll back

**Rule: a Slack schema change is additive only, so an older binary runs on a
newer database.** It ignores tables and columns it does not know, and its own
migration is a no-op. There is no down-migration and none is needed.

```sh
git checkout <previous-release-commit>
cargo build --release -p augmentagent-cli
augmentagent service --unit daemon restart
augmentagent status --json
```

What to expect after a rollback: pending work and configuration are kept;
fields the older binary does not produce (for example `reconnects`,
`daemon_report`) are absent from its `status`; a `daemon_report` written by
the newer binary stays in the database until the newer binary starts again.
`upgrade_state.rs` also plays the older binary's migration and writes against
an upgraded database. Run one daemon per database at a time: stop before
swapping binaries (`restart` does).

## 10. Remove

```sh
augmentagent slack app owner unbind
augmentagent slack app remove
augmentagent service --unit daemon restart
augmentagent status
```

`remove` deletes the stored tokens but does not revoke them: uninstall the
app from the workspace at api.slack.com/apps too. Composio ingestion is not
affected. To keep the install but stop the surface, set
`AUGMENTAGENT_SLACK_INTERACTIVE=0` in `.env` and restart.

## macOS specifics

- **Login session.** The daemon is a LaunchAgent in your GUI session: it
  starts when you log in (`RunAtLoad`), restarts after a crash, and does not
  run before the first login after a reboot. The login Keychain is unlocked
  by that login. With no one logged in, nothing answers on Slack.
- **Keychain access from launchd is unverified** (#1246). A token stored from
  a terminal is not proven readable by the LaunchAgent. `augmentagent doctor
  --keychain-probe` proves Keychain write/read/delete from the terminal (it
  may show a Keychain prompt); the proof for the daemon itself is
  `interactive.slack.credentials: usable` in `status` after a restart. A
  rebuilt binary can trigger a new Keychain "allow access" prompt, which the
  LaunchAgent cannot answer. Running `augmentagent slack app verify` once
  from a terminal after rebuilding and choosing **Always Allow** should cover
  the daemon too (same binary path), but that is unverified until #1246;
  `credentials: usable` after the restart is the check.
- **Sleep and wake.** While the Mac sleeps nothing is answered. On wake the
  Socket Mode client notices the suspension (wall clock jumped more than 60 s
  past the monotonic clock) and reconnects at once; `status` shows
  `reconnecting` and then `connected`, and `reconnects` goes up by one. Slack
  retries an undelivered event only a few times, so messages sent during a
  long sleep depend on catch-up (#1285/#1296). Real multi-hour sleep
  behavior is recorded by #1300. To keep a desktop Mac answering, stop it
  sleeping on power (System Settings > Energy).
- **Startup at login.** `augmentagent install autostart` writes
  `~/Library/LaunchAgents/com.nolanmak.augmentagent.plist` with the service
  `PATH`, validates it before replacing a working one, and bootstraps it.
  `augmentagent uninstall autostart` removes it.
- **Apple Silicon and Intel** differ only in the Homebrew prefix above and
  are qualified separately (#1300).

## Linux specifics

- The daemon is the `systemd --user` unit `augmentagent.service`. Enable
  lingering so it runs without an open login session:
  `sudo loginctl enable-linger "$USER"`.
- `augmentagent service --unit daemon restart` wraps `systemctl --user`.
  The daemon's own output is in `~/.local/state/augmentagent/*.log`, not the
  journal (section 6).
- **Credential store (#1325).** Credentials are owner-only files under
  `credentials` in the state directory (`$XDG_STATE_HOME/augmentagent`, by
  default under `~/.local/state`): the directory is `0700`, each file `0600`,
  writes are atomic and a damaged file is reported, never used. It needs no
  D-Bus session or unlocked keyring, so the headless `systemd --user` daemon
  reads what the CLI stored, provided both run as the same user with the
  same `HOME`. The files are not encrypted; they are protected like
  `~/.ssh` keys, by ownership and mode. `augmentagent doctor` reports
  `credential_backend: private-file (persistent)`.
- **Upgrading from a build before #1325.** Earlier Linux builds kept
  credentials in the keyring library: the released binary used the D-Bus
  Secret Service (gnome-keyring or KWallet), which works only where a
  session bus and an unlocked keyring exist, and a build of the auth crate
  alone used an in-memory store that lost them when the command exited.
  A credential still in the Secret Service is copied into the file store the
  first time a command or the daemon reads it from a session where the
  keyring is reachable and unlocked; it stays in the keyring, so rolling
  back still finds it. Where the keyring was never reachable (a headless
  host, or a store that never persisted), there is nothing to migrate:
  after upgrading, re-run `augmentagent slack app install` and any other
  channel login, then restart the daemon.
- Do not work around a credential problem with
  `AUGMENTAGENT_INSECURE_CREDENTIAL_DIR`; `doctor` treats that as an error.

## 11. Owner acceptance script (#1300)

Run on each host type separately (Linux, macOS Apple Silicon, macOS Intel)
with a test workspace. Record the result of every step with the commit,
`sw_vers` or `/etc/os-release`, and `uname -m`. A step marked with an issue
number depends on that issue; until it lands, record the observed result as
pending, not as a pass or a fail.

**What is left.** From the checkout, `augmentagent slack parity report`
prints the executable parity matrix (`docs/slack-parity-matrix.json`): one
row per epic capability with its owning issues, named tests, status
(`supported`, `blocked` with its issue, or `unverified-live`) and per-host
acceptance, followed by every open blocker. `augmentagent slack parity report
--json` gives the same as JSON. It exits non-zero when the matrix no longer
matches the source tree or the Slack capability tables. Each matrix row lists
the steps below that exercise it (`acceptance_steps`). When a host passes
them, fill that row's `host_evidence` for the host with the full commit, OS
version, `uname -m`, the commands run, an artifact path, `"result": "pass"`
and the date; the check rejects anything incomplete. A CI or mocked pass is
never host evidence.

1. **Baseline.** Run `git rev-parse HEAD`, `uname -m`, then
   `augmentagent doctor --json`. Expect `credential_backend` ok and
   persistent (`macos-keychain` or, on Linux, `private-file`), and no
   `config.*` errors.
2. **Nothing installed.** `augmentagent status --json`. Expect
   `interactive.slack.state: not_configured`, `detail: no interactive Slack
   app is installed`, `credentials: missing`.
3. **Install.** Create the app (section 2) and run
   `augmentagent slack app install --stdin`. Expect the workspace, bot user
   and "all 16 required scopes granted". `augmentagent status --json` now
   shows `detail: the Slack app is installed but no owner is bound`,
   `app_installed: true`, `credentials: present`.
4. **Bind.** `augmentagent slack app owner bind --user <your member ID>`.
   Expect the owner and DM recorded. `status` shows `state: disconnected`
   ("has not reported") until the daemon restarts.
5. **Start.** `augmentagent service --unit daemon restart`. Within 60 s
   expect `state: connected`, `healthy: true`, `credentials: usable`,
   `reconnects: 0`, `daemon_report.running: true`, and
   `augmentagent doctor` with `interactive.slack` and
   `interactive.slack.credentials` ok.
6. **Text turn.** DM the app "What is on my calendar today?". Expect an
   answer in the DM within seconds, `last_event_unix` and `last_send_unix`
   set, and `delivery.slack` back to zero backlog.
7. **Follow-up in a thread** (#1288). Reply in the answer's thread. Expect an
   answer that uses the earlier turn.
8. **Control channel.** Create a private channel, `/invite @Jarvis`, run
   `augmentagent slack app owner control set --channel <channel ID>`, post a
   question there. Expect the answer in a thread under your message.
9. **Stranger.** From a second test account, DM the app. Expect the fixed
   rejection only, no answer, and a higher rejection count in
   `augmentagent slack app owner show`.
10. **Slash command.** Run `/jarvis what is due today?`. Expect an answer;
    the full command set is #1292.
11. **Approvals** (#1289), **contact send** (#1290), **scheduling** (#1291),
    **commands** (#1292): run each owner workflow from the epic matrix and
    expect the Discord behavior.
12. **Files in and out** (#1293/#1294). Send a PDF and ask for a summary;
    ask for a generated file. Expect the summary and an uploaded file.
13. **Notifications** (#1295). Trigger a reminder. Expect it in Slack.
14. **Voice clip** (#1297). Send a voice clip. Expect a transcript and an
    answer. **Live voice** (#1298): record the result or the blocker in
    `SLACK-LIVE-VOICE.md`.
15. **Restart mid-turn.** Ask a long question and run
    `augmentagent service --unit daemon restart` before the answer. Expect
    exactly one answer after the restart.
16. **Network loss.** Turn networking off for two minutes, send a DM, turn it
    back on. Expect `state: reconnecting` (not healthy) during the outage,
    then `connected` with `reconnects` up by one, and the DM answered once.
17. **Sleep/wake** (macOS). Sleep the Mac for at least an hour, wake it,
    DM the app. Expect `connected` within a minute of wake and one answer.
18. **Slack only.** Remove every `DISCORD_*` and WhatsApp setting from
    `.env` and restart. Expect Slack to keep answering and no Discord
    issue in `config_issues`.
19. **Discord misconfigured beside Slack.** Put back only
    `DISCORD_BOT_TOKEN` (no `DISCORD_CHANNEL_ID`) and restart. Expect
    `config_issues` to list `discord.approval_broker` with its recovery
    (from both `cli` and `daemon`), `doctor` to warn
    `config.discord.approval_broker`, and Slack to keep answering. Restore
    `.env` and restart.
20. **Rotate.** `augmentagent slack app rotate --stdin` with a new app-level
    token, restart. Expect `connected` and answers.
21. **Upgrade and roll back.** Note the commit, upgrade (section 8), confirm
    step 6, roll back (section 9), confirm step 6 again. Expect no lost or
    duplicated answer and the owner binding intact.
22. **Logs are clean.** Run
    `grep -c -E 'xox[abp]-|xapp-' ~/.local/state/augmentagent/*.log`.
    Expect `0` for every file. Search the same files for a phrase from step
    6's question; expect no match.
23. **Remove.** Section 10. Expect `detail: no interactive Slack app is
    installed` and no answers from the app.
24. **Soak** (#1300). Leave the host running for 24 hours with the
    reconnect, restart and (macOS) sleep/wake events above. Expect no lost
    turn, duplicate answer or answer in the wrong conversation.
