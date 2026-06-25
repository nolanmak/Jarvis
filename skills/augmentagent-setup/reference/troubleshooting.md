# Setup Skill Troubleshooting

Quick lookup table the skill consults when `augmentagent status --json`
fails outright, or when a channel reports `configured=false` or a
non-empty `needs[]` without an obvious cause. Start small; this file
will grow as Phases 2 and 3 land.

## Symptom to cause to fix

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `augmentagent status` exits with "command not found" or "No such file" | The release binary has not been built or is not on PATH. | Run `cargo build --release -p augmentagent-cli`, then re-run with the full path `./target/release/augmentagent status --json`. If the user wants it on PATH, symlink into `~/.cargo/bin/`. |
| `status --json` returns valid JSON with `summary: "daemon_down"` and `daemon.active=false` | The main daemon is not running. | Run `augmentagent service start --unit daemon` (or `systemctl --user start augmentagent.service`). Re-check status. If it fails immediately, pull `augmentagent logs --unit augmentagent.service` and surface the last 50 lines verbatim. |
| `dashboard.active=false` and `dashboard.reachable=false` | The dashboard sidecar is not running (no `installed` flag exists; absence is inferred from both being false). | Tell the user the dashboard is optional in Phase 1. If they want it, run `./scripts/install-dashboard.sh` and re-run status. Do not flag this as a fault. |
| A channel reports `armed=false` (gate off) or a non-empty `needs[]` for creds the user expects to be set | `.env` is missing the line / the gate was never set, or the daemon was started before the config was changed. | Apply the value with `augmentagent env set <KEY> <VALUE>` (or `augmentagent channel <name> arm` for the arming gate), point them at the matching block in `.env.example`, then run `augmentagent service restart` so the daemon re-reads its config. |
| Channel validation fails with a keyring error (e.g. "Cannot autolaunch D-Bus", "secret service not available") | The D-Bus session has no unlocked secret service. Common on SSH or freshly-rebooted headless boxes. | Have the user log into a graphical session once to unlock the keyring, or wrap the daemon start in `dbus-run-session` for fully headless flows. Re-run channel validate. |
| Instagram or Twitter validate fails with "browser sidecar timeout" | The browser sidecar needs a display or a headed launch profile and there is none. | If the user is on pure SSH with no DISPLAY, this is expected; tell them to run the validation from a graphical session, or to enable the sidecar's headless profile if their channel build supports it. Do not retry. |
| `service restart` succeeds but the unit drops back to `inactive` within seconds | A unit dependency is failing (typically a sidecar or the database). | Run `systemctl --user status augmentagent.service --no-pager` and surface verbatim. Look for "Failed to start" lines in the dependency chain. Route the user to the failing sub-unit's logs. |
| `status --json` hangs or takes more than ten seconds | The CLI is trying to reach a live channel during status collection. | Should not happen with the Phase 1 aggregator; if it does, file a bug against issue #1. Interrupt with Ctrl+C and report the stderr. |
| `auto-update` looks stale (binary built more than a week ago) | The auto-updater unit is not active, or has not picked up new commits. | Tell the user to run `scripts/check-for-updates.sh` once manually and check that `augmentagent-update.timer` is enabled via `systemctl --user list-timers`. |
| The skill itself emits an emoji or an emdash | The skill output is being post-processed somewhere outside the skill, or the model ignored the writing-style rules. | Regenerate the message. If it repeats, file a bug against issue #5. |

## Doctor findings

`augmentagent doctor --json` emits one `{name, severity, message,
suggested_cmd}` object per check (see `crates/augmentagent-cli/src/doctor.rs`).
Each section below maps one finding name to its meaning and the fix the
skill should offer (always behind AskUserQuestion). Names are stable; the
skill matches on them verbatim.

## sqlite_open

What it means. The doctor opened the sqlite database at `$AUGMENTAGENT_DB`
(default `data.db` relative to the daemon's cwd) and ran `PRAGMA
integrity_check`. An `error` severity means either the file is missing or
sqlite reported corruption. On a truly fresh box where the dashboard has
never been started, this finding lands as `error` simply because the db
file does not exist yet.

Fix. If the user has never run the installer, the suggested fix is the
first-run bootstrap: `augmentagent install dashboard` initialises the db
on first start. For an existing install with a corrupted db, run `sqlite3
"$AUGMENTAGENT_DB" 'PRAGMA integrity_check;'` to confirm the corruption,
then take a backup and restore from the most recent `~/.config/augmentagent`
snapshot the auto-updater keeps. Do not delete the db without copying it
aside first.

## sqlite_migrated

What it means. The db opened cleanly but at least one of the core tables
(`actions`, `config`, `channel_subscriptions`) is missing. This usually
indicates the daemon was started against an empty db and crashed before
running migrations, or a tenant slug was renamed and the daemon is now
opening a stale file.

Fix. `augmentagent service restart --unit daemon` triggers a fresh
migration pass on the next start. If the unit refuses to come up, pull
`augmentagent logs --unit augmentagent.service` and surface the last 50
lines verbatim; the migration failure prints to journald.

## keyring_reachable

What it means. The doctor ran `secret-tool lookup augmentagent _probe` to
confirm libsecret is reachable. An `error` severity means the
`secret-tool` binary is not on `$PATH` (the `libsecret-tools` Debian
package is missing). A `warn` severity means the call timed out or
spawned with a non-NotFound error; the keyring daemon may be locked or
unreachable from this session.

Fix. For the missing binary: `apt-get install -y libsecret-tools` (the
suggested_cmd the doctor emits). For a locked keyring on a headless box,
either log into a graphical session once to unlock it, or wrap the daemon
start in `dbus-run-session` so the daemon owns its own session bus.

## dashboard_reachable

What it means. The doctor probed
`http://127.0.0.1:${DASHBOARD_PORT:-3000}/api/v1/stats` and
`/api/v1/health`. An `error` means neither endpoint answered within 2
seconds (with the `AUGMENTAGENT_API_KEY` header when set). The dashboard
unit is either not running, listening on a different port, or wedged.

Fix. The doctor's suggested_cmd is `augmentagent service start --unit
dashboard`. Run it via AskUserQuestion, then re-check. If the unit fails
to come up, `augmentagent logs --unit augmentagent-dashboard.service` and
surface the tail. Confirm `$DASHBOARD_PORT` matches what the dashboard is
actually bound to (default 3000); a mismatched port is the most common
silent cause.

## claude_cli_in_path

What it means. The doctor ran `which $CLAUDE_CLI` (default `claude`) and
got nothing. The Claude Code CLI is missing or the `CLAUDE_CLI` env var
points at a non-existent binary. The agent's self-improvement and
chat-relay paths break without it.

Fix. Install Claude Code per `https://docs.claude.com/claude-code/install`,
or set `CLAUDE_CLI=/absolute/path/to/claude` in `.env` (then run
`augmentagent service restart` so the daemon picks up the change).

## python3_in_path

What it means. The doctor ran `which python3` and got nothing. The browser
sidecar (for instagram and twitter cookie-harvest fallbacks) and several
installer scripts shell out to `python3`.

Fix. `apt-get install -y python3`. Re-run `augmentagent doctor --json`
after install to confirm.

## node_in_path

What it means. The doctor ran `which node` and got nothing. The dashboard
runs on Node, so this finding usually appears alongside a failed
`dashboard_reachable` check.

Fix. Install Node (LTS preferred). The doctor's suggested_cmd is `nvm
install --lts`; for a system-wide install, follow the Debian / Ubuntu
NodeSource setup. Re-run the dashboard installer after Node lands:
`augmentagent install dashboard`.

## rust_binary_freshness

What it means. The doctor located the `augmentagent` release binary (via
`which augmentagent`, fallback to `target/release/augmentagent`) and read
its mtime. A `warn` severity means the binary is older than 7 days; the
auto-updater either is not running or has not picked up new commits.

Fix. The suggested_cmd is `scripts/check-for-updates.sh`. Run it once
manually; if it pulls and rebuilds cleanly, also confirm the timer is
enabled with `systemctl --user list-timers augmentagent-update.timer`. If
the timer is inactive, re-run `augmentagent install autoupdate`.

## dashboard_build_present

What it means. The doctor walked up from the resolved binary path
looking for `dist/dashboard-server.js`. A `warn` means the file is
missing; the dashboard's compiled bundle has not been built (or the
install layout puts it somewhere the doctor cannot find).

Fix. The suggested_cmd is `augmentagent install dashboard`, which the
dashboard installer rebuilds the bundle as part of its run. On slim
Rust-only deploys (no Node tree) this finding is expected; treat it as
informational unless the user has explicitly enabled the dashboard.

## env_file_present

What it means. The doctor checked for `.env` in the cwd and
`~/.config/augmentagent/.env`. A `warn` severity means neither exists.
The daemon can run on env-only configuration (the sqlite `config` table
wins anyway), but most installs keep a `.env` for the canonical env vars.

Fix. The suggested_cmd is `cp .env.example .env && $EDITOR .env`. Tell
the user which env vars are missing from the install (read `core_keys`
in the status JSON for the canonical list) and offer to set them via
`augmentagent env set <KEY> <VALUE>` instead of editing the file.

## socialapi

What it means. The doctor probes the SocialAPI.ai integration (#245): is
`SOCIALAPI_API_KEY` set (sqlite `config.socialapi_api_key`, env-var
fallback), and is there at least one active account in the local
`socialapi_accounts` registry. This check runs unconditionally, not
behind `--deep`. An `ok` means the key is set and at least one account
is active; a `warn` means the key is set but no accounts are connected,
or no key is set at all (the integration is optional, so it never
errors).

Fix. The suggested_cmd is `augmentagent socialapi connect` (or connect
via the dashboard). For the no-key case, set the key with `augmentagent
env set SOCIALAPI_API_KEY <key>` first, then connect an account.

## composio_api (--deep)

What it means. The `--deep` doctor pings
`https://backend.composio.dev/api/v1/client/auth/client_info` with the
operator's `COMPOSIO_API_KEY`. An empty key is silently ok; a 2xx is ok;
a 401 / 403 is `warn` (key recognised, scope wrong); anything else is
`error` (Composio unreachable or returned an unexpected status).

Fix. For the 401 / 403 case, re-issue the key at
`https://app.composio.dev` and run `augmentagent env set COMPOSIO_API_KEY
<new-key>`. For a generic outage, the doctor's `error` finding includes
the HTTP status; surface it verbatim and tell the user to retry once
Composio's status page reports green.

## channel.NAME.validate (--deep)

What it means. One finding per configured channel. `ok` when the channel
is configured + armed + has no missing fields. `warn` when configured
but not armed (the channel is dark) or when armed with non-empty
`needs[]`.

Fix. For "not armed", the suggested_cmd is `augmentagent channel <name>
arm`; see `components/env-file.md` for the arming semantics and the
restart cycle. For "missing fields", re-enter the channel's sub-flow
with `/setup --fix <name>`; the suggested_cmd is `augmentagent setup
harvest <name>`.

## Notes

- Always surface CLI stderr verbatim. The user often greps for an exact
  error string; paraphrasing breaks that.
- One fix at a time. Apply, re-run doctor or status, then decide.
  Stacking fixes hides which one worked.
- Repair branch is read-mostly except for `suggested_cmd` runs and the
  per-channel sub-flow, both gated by AskUserQuestion. Never run
  destructive flags from this table without an explicit confirmation
  that quotes the command back.
- The doctor is the source of truth for "what is wrong on this host".
  The symptom table at the top of this file is the fallback when the
  doctor itself cannot run (binary missing, sqlite gone, etc.).
