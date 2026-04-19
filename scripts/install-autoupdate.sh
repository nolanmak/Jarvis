#!/bin/bash
# Register a periodic job that checks GitHub every 5 minutes for new commits
# on main, then pulls + rebuilds + restarts the daemon if there are.
#
# Cross-platform: launchd on macOS, systemd user .timer on Linux.
# Paired with install-autostart.sh (which runs the daemon itself). They're
# separate jobs so updater failures don't take the daemon down.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent.updater"
INTERVAL_SECS=300

log() { printf '\033[1;36m[install-autoupdate]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-autoupdate ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ -x "$REPO_ROOT/scripts/check-for-updates.sh" ] \
  || die "check-for-updates.sh not executable"

install_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local LOG_DIR="$HOME/Library/Logs/augmentagent"
  mkdir -p "$LOG_DIR"
  mkdir -p "$(dirname "$PLIST")"

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
        <string>$REPO_ROOT/scripts/check-for-updates.sh</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$LAUNCH_PATH</string>
        <key>HOME</key>
        <string>$HOME</string>
    </dict>

    <key>StartInterval</key>
    <integer>$INTERVAL_SECS</integer>

    <key>RunAtLoad</key>
    <true/>

    <key>StandardOutPath</key>
    <string>$LOG_DIR/update.stdout.log</string>

    <key>StandardErrorPath</key>
    <string>$LOG_DIR/update.stderr.log</string>
</dict>
</plist>
PLIST_EOF

  local UID_NUM
  UID_NUM="$(id -u)"
  local DOMAIN="gui/$UID_NUM"

  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    log "Previous updater found — bootout first"
    launchctl bootout "$DOMAIN/$LABEL" || true
    sleep 1
  fi

  log "Bootstrapping updater (runs every ${INTERVAL_SECS}s)"
  launchctl bootstrap "$DOMAIN" "$PLIST"
  launchctl enable "$DOMAIN/$LABEL"

  log "Installed."
  log "  Label:     $LABEL"
  log "  Interval:  ${INTERVAL_SECS}s"
  log "  Logs:      $LOG_DIR/update.log"
  log "  Uninstall: ./scripts/uninstall-autoupdate.sh"
}

install_linux() {
  local SERVICE_NAME="augmentagent-update.service"
  local TIMER_NAME="augmentagent-update.timer"
  local UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  local SERVICE="$UNIT_DIR/$SERVICE_NAME"
  local TIMER="$UNIT_DIR/$TIMER_NAME"
  local LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
  mkdir -p "$UNIT_DIR" "$LOG_DIR"

  command -v systemctl >/dev/null 2>&1 \
    || die "systemctl not found — this installer needs systemd"

  local SERVICE_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

  log "Writing service: $SERVICE"
  cat > "$SERVICE" <<UNIT_EOF
[Unit]
Description=AugmentAgent auto-updater (one-shot pull/rebuild/restart)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
WorkingDirectory=$REPO_ROOT
ExecStart=$REPO_ROOT/scripts/check-for-updates.sh
Environment=PATH=$SERVICE_PATH
StandardOutput=append:$LOG_DIR/update.stdout.log
StandardError=append:$LOG_DIR/update.stderr.log
UNIT_EOF

  log "Writing timer: $TIMER"
  cat > "$TIMER" <<TIMER_EOF
[Unit]
Description=Run AugmentAgent auto-updater every ${INTERVAL_SECS}s

[Timer]
OnBootSec=2min
OnUnitActiveSec=${INTERVAL_SECS}s
Unit=$SERVICE_NAME
Persistent=true

[Install]
WantedBy=timers.target
TIMER_EOF

  log "Reloading systemd --user"
  systemctl --user daemon-reload

  log "Enabling + starting $TIMER_NAME"
  systemctl --user enable "$TIMER_NAME" >/dev/null
  systemctl --user restart "$TIMER_NAME"

  if ! loginctl show-user "$USER" 2>/dev/null | grep -q "Linger=yes"; then
    log "NOTE: linger is OFF — updater only runs while you're logged in."
    log "      To poll 24/7: sudo loginctl enable-linger $USER"
  fi

  log "Installed."
  log "  Service:   $SERVICE"
  log "  Timer:     $TIMER (every ${INTERVAL_SECS}s)"
  log "  Logs:      $LOG_DIR/update.log  (also: journalctl --user -u $SERVICE_NAME)"
  log "  Status:    systemctl --user list-timers $TIMER_NAME"
  log "  Uninstall: ./scripts/uninstall-autoupdate.sh"
}

case "$(uname -s)" in
  Darwin) install_macos ;;
  Linux)  install_linux ;;
  *)      die "unsupported platform: $(uname -s)" ;;
esac
