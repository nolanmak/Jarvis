# systemd Units

Reference for every user-mode systemd unit the AugmentAgent stack ships.
The skill consults this file when a triage branch needs to install,
check, or restart a unit. Every install/uninstall verb wraps the
matching `scripts/install-<name>.sh` (or unit-template copy for the
browser sidecar) via the `augmentagent install <component>` shim from
issue #6.

## Units at a glance

| Unit                                    | Component name       | Role                                                                 |
| --------------------------------------- | -------------------- | -------------------------------------------------------------------- |
| `augmentagent.service`                  | `autostart`          | The Rust daemon. Polls every channel, drives drafts, owns sqlite.    |
| `augmentagent-dashboard.service`        | `dashboard`          | The web dashboard on `$DASHBOARD_PORT`. Hosts OAuth callbacks.       |
| `augmentagent-update.timer`             | `autoupdate`         | 5-minute timer that runs `scripts/check-for-updates.sh`.             |
| `augmentagent-update.service`           | (paired with timer)  | The one-shot unit the `autoupdate` timer triggers.                   |
| `augmentagent-digest.timer`             | `digest`             | Daily digest at `AUGMENTAGENT_DIGEST_HOUR:_MINUTE`.                  |
| `augmentagent-digest.service`           | (paired with timer)  | The one-shot unit the `digest` timer triggers.                       |
| `augmentagent-xvfb.service`             | `browser-sidecar`    | Headless X server for the headed Chromium profile.                   |
| `augmentagent-chromium.service`         | `browser-sidecar`    | Headed Chromium pointed at the Xvfb display.                         |
| `augmentagent-browser-sidecar.service`  | `browser-sidecar`    | Python sidecar that drives the Chromium instance.                    |
| `augmentagent-<tenant>.service`         | `tenant --name <n>`  | Multi-tenant per-Discord-server isolated daemon.                     |

## Install

Use the CLI shim, not the raw scripts; the shim resolves the scripts
dir, surfaces stderr cleanly, and supports `--json` for the skill to
parse the receipt.

```
augmentagent install autostart
augmentagent install dashboard
augmentagent install autoupdate
augmentagent install digest
augmentagent install browser-sidecar
augmentagent install tenant --name <slug>
```

Add `--rebuild` to run `cargo build --release -p augmentagent-cli && npm
run build` before the install. Add `--json` to suppress live output and
emit a single summary `{component, action, succeeded, stdout_tail,
stderr_tail}`. Always confirm via AskUserQuestion before installing;
these touch systemd unit files.

The browser sidecar variant is unique: it copies three `.service` files
from `systemd/` to `~/.config/systemd/user/`, runs `daemon-reload`, then
`enable --now` on all three. The dashboard install variant has no
upstream uninstall script.

## Check

Read-only status for any unit:

```
systemctl --user status <unit>.service --no-pager
systemctl --user status <unit>.timer --no-pager
systemctl --user list-timers
```

For the timers, also confirm they last ran recently:

```
systemctl --user list-timers augmentagent-update.timer augmentagent-digest.timer
```

For the daemon's overall view, prefer the canonical status command:

```
augmentagent status --json
```

It populates `daemon.active`, `dashboard.active`, `updater.timer_active`
without needing `systemctl` directly.

## Restart

```
augmentagent service restart                 # main augmentagent.service
augmentagent service restart --unit <name>   # sidecar
```

Always confirm via AskUserQuestion; restarts drop in-flight approval
windows. After restart, re-run `augmentagent status --json` and verify
the unit came back `active`.

## Uninstall

```
augmentagent uninstall autostart
augmentagent uninstall autoupdate
augmentagent uninstall digest
augmentagent uninstall browser-sidecar
augmentagent uninstall tenant --name <slug>
```

The dashboard has no upstream uninstall script; the CLI surfaces the
manual removal incantation:

```
systemctl --user disable --now augmentagent-dashboard.service
rm ~/.config/systemd/user/augmentagent-dashboard.service
systemctl --user daemon-reload
```

Uninstall is destructive. Always confirm via AskUserQuestion quoting
the exact command back.

## Common pitfalls

- The whole stack runs as user-mode systemd. `--user` is required on
  every `systemctl` invocation; without it you address PID 1 and the
  units do not exist there.
- Unit dependencies are not declared between daemon and sidecars; a
  failing browser-sidecar does not bring the daemon down. Each unit is
  inspected independently.
- The auto-update timer triggers `check-for-updates.sh`, which can pull
  new commits and rebuild. Never run a destructive `--purge` flag while
  the updater is in flight; check `systemctl --user list-timers` and
  wait for the next ActiveExitTimestamp before doing anything heavy.
