#!/bin/bash
# Register the AugmentAgent Rust daemon as a user-level service so it:
#   - auto-starts on user login
#   - auto-restarts if it crashes
#   - survives terminal close
#
# Cross-platform: writes a launchd plist on macOS, or a systemd user unit
# on Linux. Idempotent — re-running reloads with the current config.
#
# Usage: ./scripts/install-autostart.sh
#
# Resource limits (#902, from the 2026-08-31 OOM post-mortem #897): the Linux
# unit carries MemoryHigh/MemoryMax/MemorySwapMax/TasksMax/LimitNOFILE/
# OOMPolicy so a runaway fan-out is contained INSIDE its own cgroup instead of
# the kernel's global OOM killer taking the desktop session down with it (that
# is what happened when ~300 `claude -p` children fanned out at once). With
# OOMPolicy=continue the kernel kills the offending child and the daemon keeps
# running — `kill` would turn one OOM'd child into a full daemon restart, the
# very restart storm #903 exists to prevent. MemoryHigh throttles the
# in-cgroup `cargo build` (self-improve gate) before MemoryMax bites; the
# ceilings leave a release build room (it peaks at several GB) while still
# leaving the desktop ~5 GB on a 15 GB laptop. Override per host at install:
#   AUGMENTAGENT_UNIT_MEMORY_HIGH (8G)  AUGMENTAGENT_UNIT_MEMORY_MAX (10G)
#   AUGMENTAGENT_UNIT_TASKS_MAX (2048)  AUGMENTAGENT_UNIT_NOFILE (4096)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent"

log() { printf '\033[1;36m[install-autostart]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-autostart ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ -x "$REPO_ROOT/scripts/run-rs.sh" ] || die "run-rs.sh not executable — run chmod +x scripts/*.sh"
[ -x "$REPO_ROOT/target/release/augmentagent" ] \
  || die "release binary missing — run: cargo build --release -p augmentagent-cli"

install_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local LOG_DIR="$HOME/Library/Logs/augmentagent"
  mkdir -p "$LOG_DIR"
  mkdir -p "$(dirname "$PLIST")"

  # Build PATH that works for a graphical login (launchd inherits /usr/bin:/bin by default).
  # Include the usual suspects so `claude`, `cargo`, `node`, Homebrew, etc. are found.
  local LAUNCH_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

  log "Writing plist: $PLIST"
  cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>

    <key>WorkingDirectory</key>
    <string>$REPO_ROOT</string>

    <key>ProgramArguments</key>
    <array>
        <string>$REPO_ROOT/scripts/run-rs.sh</string>
        <string>--wiki-dir</string>
        <string>./wiki</string>
        <string>serve</string>
        <string>--dry-run</string>
        <string>false</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$LAUNCH_PATH</string>
        <key>RUST_LOG</key>
        <string>info</string>
        <key>HOME</key>
        <string>$HOME</string>
    </dict>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
        <key>Crashed</key>
        <true/>
    </dict>

    <key>ThrottleInterval</key>
    <integer>10</integer>

    <key>StandardOutPath</key>
    <string>$LOG_DIR/stdout.log</string>

    <key>StandardErrorPath</key>
    <string>$LOG_DIR/stderr.log</string>
</dict>
</plist>
PLIST_EOF

  local UID_NUM
  UID_NUM="$(id -u)"
  local DOMAIN="gui/$UID_NUM"

  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    log "Previous agent found — bootout first"
    launchctl bootout "$DOMAIN/$LABEL" || true
    sleep 1
  fi

  log "Bootstrapping agent"
  launchctl bootstrap "$DOMAIN" "$PLIST"
  launchctl enable "$DOMAIN/$LABEL"
  launchctl kickstart -k "$DOMAIN/$LABEL"

  log "Installed."
  log "  Label:     $LABEL"
  log "  Plist:     $PLIST"
  log "  Logs:      $LOG_DIR/"
  log "  Status:    launchctl print $DOMAIN/$LABEL | head -30"
  log "  Uninstall: ./scripts/uninstall-autostart.sh"
}

install_linux() {
  local UNIT_NAME="augmentagent.service"
  local UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  local UNIT="$UNIT_DIR/$UNIT_NAME"
  local LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
  mkdir -p "$UNIT_DIR" "$LOG_DIR"

  command -v systemctl >/dev/null 2>&1 \
    || die "systemctl not found — this installer needs systemd"

  # PATH a systemd --user service inherits is minimal; pre-seed the usual
  # spots so cargo/node/etc are findable from the script.
  local SERVICE_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

  # #902 — cgroup + rlimit ceilings (see the header). Overridable per host.
  local MEMORY_HIGH="${AUGMENTAGENT_UNIT_MEMORY_HIGH:-8G}"
  local MEMORY_MAX="${AUGMENTAGENT_UNIT_MEMORY_MAX:-10G}"
  local TASKS_MAX="${AUGMENTAGENT_UNIT_TASKS_MAX:-2048}"
  local NOFILE="${AUGMENTAGENT_UNIT_NOFILE:-4096}"

  log "Writing unit: $UNIT"
  cat > "$UNIT" <<UNIT_EOF
[Unit]
Description=AugmentAgent Rust daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$REPO_ROOT
ExecStart=$REPO_ROOT/scripts/run-rs.sh --wiki-dir ./wiki serve --dry-run false
Environment=PATH=$SERVICE_PATH
Environment=RUST_LOG=info
# Deft channel still inert until a token is stored via 'augmentagent deft login'
Environment=AUGMENTAGENT_DEFT_ENABLED=1
Restart=on-failure
RestartSec=10
# #902 — die alone: a fan-out is killed inside this cgroup, not by the
# kernel's global OOM killer picking off the desktop session.
MemoryHigh=$MEMORY_HIGH
MemoryMax=$MEMORY_MAX
MemorySwapMax=0
TasksMax=$TASKS_MAX
LimitNOFILE=$NOFILE
OOMPolicy=continue
StandardOutput=append:$LOG_DIR/stdout.log
StandardError=append:$LOG_DIR/stderr.log

[Install]
WantedBy=default.target
UNIT_EOF

  log "Reloading systemd --user"
  systemctl --user daemon-reload

  log "Enabling + (re)starting $UNIT_NAME"
  systemctl --user enable "$UNIT_NAME" >/dev/null
  systemctl --user restart "$UNIT_NAME"

  if ! loginctl show-user "$USER" 2>/dev/null | grep -q "Linger=yes"; then
    log "NOTE: linger is OFF — daemon will only run while you're logged in."
    log "      To run it 24/7 even after logout: sudo loginctl enable-linger $USER"
  fi

  log "Installed."
  log "  Unit:      $UNIT"
  log "  Logs:      $LOG_DIR/  (also: journalctl --user -u $UNIT_NAME -f)"
  log "  Status:    systemctl --user status $UNIT_NAME"
  log "  Uninstall: ./scripts/uninstall-autostart.sh"
}

case "$(uname -s)" in
  Darwin) install_macos ;;
  Linux)  install_linux ;;
  *)      die "unsupported platform: $(uname -s)" ;;
esac
