# Setup Skill Troubleshooting

Quick lookup table the skill consults when `augmentagent status --json`
fails outright, or when a channel reports `validated=false` without an
obvious cause. Start small; this file will grow as Phases 2 and 3 land.

## Symptom to cause to fix

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `augmentagent status` exits with "command not found" or "No such file" | The release binary has not been built or is not on PATH. | Run `cargo build --release -p augmentagent-cli`, then re-run with the full path `./target/release/augmentagent status --json`. If the user wants it on PATH, symlink into `~/.cargo/bin/`. |
| `status --json` returns valid JSON with `state: "repair"` and `services[].active=false` for `augmentagent.service` | The main daemon is not running. | Run `systemctl --user start augmentagent.service`. Re-check status. If it fails immediately, pull `augmentagent logs --unit augmentagent.service` and surface the last 50 lines verbatim. |
| `dashboard.running=false` and `dashboard.installed=false` | The dashboard sidecar was never installed. | Tell the user the dashboard is optional in Phase 1. If they want it, run `./scripts/install-dashboard.sh` and re-run status. Do not flag this as a fault. |
| Any channel reports `gates.<VAR>="unset"` for a var the user expects to be set | `.env` is missing the line, or the daemon was started before `.env` was edited. | Tell the user which env var to add, point them at the matching block in `.env.example`, then run `augmentagent service restart` so the daemon re-reads `.env`. |
| Channel validation fails with a keyring error (e.g. "Cannot autolaunch D-Bus", "secret service not available") | The D-Bus session has no unlocked secret service. Common on SSH or freshly-rebooted headless boxes. | Have the user log into a graphical session once to unlock the keyring, or wrap the daemon start in `dbus-run-session` for fully headless flows. Re-run channel validate. |
| Instagram or Twitter validate fails with "browser sidecar timeout" | The browser sidecar needs a display or a headed launch profile and there is none. | If the user is on pure SSH with no DISPLAY, this is expected; tell them to run the validation from a graphical session, or to enable the sidecar's headless profile if their channel build supports it. Do not retry. |
| `service restart` succeeds but the unit drops back to `inactive` within seconds | A unit dependency is failing (typically a sidecar or the database). | Run `systemctl --user status augmentagent.service --no-pager` and surface verbatim. Look for "Failed to start" lines in the dependency chain. Route the user to the failing sub-unit's logs. |
| `status --json` hangs or takes more than ten seconds | The CLI is trying to reach a live channel during status collection. | Should not happen with the Phase 1 aggregator; if it does, file a bug against issue #1. Interrupt with Ctrl+C and report the stderr. |
| `auto-update` looks stale (binary built more than a week ago) | The auto-updater unit is not active, or has not picked up new commits. | Tell the user to run `scripts/check-for-updates.sh` once manually and check that `augmentagent-autoupdate.timer` is enabled via `systemctl --user list-timers`. |
| The skill itself emits an emoji or an emdash | The skill output is being post-processed somewhere outside the skill, or the model ignored the writing-style rules. | Regenerate the message. If it repeats, file a bug against issue #5. |

## Notes

- Always surface CLI stderr verbatim. The user often greps for an exact
  error string; paraphrasing breaks that.
- One fix at a time. Apply, re-run status, then decide. Stacking fixes
  hides which one worked.
- Repair branch is read-mostly. Never run destructive flags from this
  table without an explicit AskUserQuestion confirmation that quotes the
  command back.

This file is intentionally short for Phase 1. Phase 3 (issue #11) will
add the `augmentagent doctor` command, at which point most of the rows
above move into structured diagnostics and this file becomes the index
to the doctor's checks.
