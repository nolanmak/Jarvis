#!/bin/bash
# Register a recurring job that runs one Google Calendar -> wiki Meeting log
# poll cycle (#82). The calendar channel is deliberately NOT spawned by
# `serve` — an external timer drives it (#376).
#
# Cross-platform: launchd on macOS (StartInterval), systemd user timer on
# Linux (OnCalendar). Idempotent — rerunning replaces the prior
# registration. Polls every 30 minutes by default.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent.calendar"
INTERVAL_MIN="${AUGMENTAGENT_CALENDAR_INTERVAL_MIN:-30}"

log() { printf '\033[1;36m[install-calendar]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-calendar ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ -x "$REPO_ROOT/target/release/augmentagent" ] \
  || die "release binary missing — run: cargo build --release -p augmentagent-cli"

case "$INTERVAL_MIN" in
  ''|*[!0-9]*) die "AUGMENTAGENT_CALENDAR_INTERVAL_MIN must be a positive integer (got '$INTERVAL_MIN')" ;;
esac
[ "$INTERVAL_MIN" -ge 1 ] && [ "$INTERVAL_MIN" -le 60 ] \
  || die "AUGMENTAGENT_CALENDAR_INTERVAL_MIN must be 1-60 (got '$INTERVAL_MIN')"

install_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local LOG_DIR="$HOME/Library/Logs/augmentagent"
  mkdir -p "$LOG_DIR"
  mkdir -p "$(dirname "$PLIST")"

  local LAUNCH_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

  log "Writing plist: $PLIST (every ${INTERVAL_MIN} min)"
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
        <string>$REPO_ROOT/scripts/calendar-poll.sh</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$LAUNCH_PATH</string>
        <key>HOME</key>
        <string>$HOME</string>
    </dict>

    <key>StartInterval</key>
    <integer>$((INTERVAL_MIN * 60))</integer>

    <key>StandardOutPath</key>
    <string>$LOG_DIR/calendar.stdout.log</string>

    <key>StandardErrorPath</key>
    <string>$LOG_DIR/calendar.stderr.log</string>
</dict>
</plist>
PLIST_EOF

  local UID_NUM
  UID_NUM="$(id -u)"
  local DOMAIN="gui/$UID_NUM"

  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    log "Previous calendar agent found — bootout first"
    launchctl bootout "$DOMAIN/$LABEL" || true
    sleep 1
  fi

  log "Bootstrapping calendar agent"
  launchctl bootstrap "$DOMAIN" "$PLIST"
  launchctl enable "$DOMAIN/$LABEL"

  log "Installed."
  log "  Label:     $LABEL"
  log "  Fires:     every ${INTERVAL_MIN} min"
  log "  Logs:      $LOG_DIR/calendar.stdout.log + $LOG_DIR/calendar.stderr.log"
  log "  Uninstall: ./scripts/uninstall-calendar.sh"
  log ""
  log "Test it right now:  ./scripts/calendar-poll.sh"
}

install_linux() {
  local SERVICE_NAME="augmentagent-calendar.service"
  local TIMER_NAME="augmentagent-calendar.timer"
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
Description=AugmentAgent calendar ingest (one poll cycle)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
WorkingDirectory=$REPO_ROOT
ExecStart=$REPO_ROOT/scripts/calendar-poll.sh
Environment=PATH=$SERVICE_PATH
StandardOutput=append:$LOG_DIR/calendar.stdout.log
StandardError=append:$LOG_DIR/calendar.stderr.log
UNIT_EOF

  log "Writing timer: $TIMER (every ${INTERVAL_MIN} min)"
  cat > "$TIMER" <<TIMER_EOF
[Unit]
Description=Trigger AugmentAgent calendar ingest every ${INTERVAL_MIN} min

[Timer]
OnCalendar=*:0/${INTERVAL_MIN}
Persistent=true
Unit=$SERVICE_NAME

[Install]
WantedBy=timers.target
TIMER_EOF

  log "Reloading systemd --user"
  systemctl --user daemon-reload

  log "Enabling + starting $TIMER_NAME"
  systemctl --user enable "$TIMER_NAME" >/dev/null
  systemctl --user restart "$TIMER_NAME"

  if ! loginctl show-user "$USER" 2>/dev/null | grep -q "Linger=yes"; then
    log "NOTE: linger is OFF — calendar only polls while you're logged in."
    log "      Run 24/7: sudo loginctl enable-linger $USER"
  fi

  log "Installed."
  log "  Service:   $SERVICE"
  log "  Timer:     $TIMER (every ${INTERVAL_MIN} min)"
  log "  Logs:      $LOG_DIR/calendar.stdout.log  (also: journalctl --user -u $SERVICE_NAME)"
  log "  Status:    systemctl --user list-timers $TIMER_NAME"
  log "  Uninstall: ./scripts/uninstall-calendar.sh"
  log ""
  log "Test it right now:  ./scripts/calendar-poll.sh"
}

case "$(uname -s)" in
  Darwin) install_macos ;;
  Linux)  install_linux ;;
  *)      die "unsupported platform: $(uname -s)" ;;
esac
