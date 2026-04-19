#!/bin/bash
# Register a daily 10:00-local-time job that composes a morning digest of
# inbox activity and posts it to DISCORD_CHANNEL_ID.
#
# Cross-platform: launchd on macOS (StartCalendarInterval), systemd user
# timer on Linux (OnCalendar). Idempotent — rerunning replaces the prior
# registration.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent.digest"
HOUR="${AUGMENTAGENT_DIGEST_HOUR:-10}"
MINUTE="${AUGMENTAGENT_DIGEST_MINUTE:-0}"

log() { printf '\033[1;36m[install-digest]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-digest ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ -x "$REPO_ROOT/target/release/augmentagent" ] \
  || die "release binary missing — run: cargo build --release -p augmentagent-cli"

install_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local LOG_DIR="$HOME/Library/Logs/augmentagent"
  mkdir -p "$LOG_DIR"
  mkdir -p "$(dirname "$PLIST")"

  local LAUNCH_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

  log "Writing plist: $PLIST (daily at ${HOUR}:$(printf '%02d' "$MINUTE"))"
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
        <string>$REPO_ROOT/scripts/daily-digest.sh</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$LAUNCH_PATH</string>
        <key>HOME</key>
        <string>$HOME</string>
    </dict>

    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>$HOUR</integer>
        <key>Minute</key>
        <integer>$MINUTE</integer>
    </dict>

    <key>StandardOutPath</key>
    <string>$LOG_DIR/digest.stdout.log</string>

    <key>StandardErrorPath</key>
    <string>$LOG_DIR/digest.stderr.log</string>
</dict>
</plist>
PLIST_EOF

  local UID_NUM
  UID_NUM="$(id -u)"
  local DOMAIN="gui/$UID_NUM"

  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    log "Previous digest agent found — bootout first"
    launchctl bootout "$DOMAIN/$LABEL" || true
    sleep 1
  fi

  log "Bootstrapping digest agent"
  launchctl bootstrap "$DOMAIN" "$PLIST"
  launchctl enable "$DOMAIN/$LABEL"

  log "Installed."
  log "  Label:     $LABEL"
  log "  Fires:     every day at ${HOUR}:$(printf '%02d' "$MINUTE") local time"
  log "  Logs:      $LOG_DIR/digest.stdout.log + $LOG_DIR/digest.stderr.log"
  log "  Uninstall: ./scripts/uninstall-digest.sh"
  log ""
  log "Test it right now (without waiting for 10am):"
  log "  ./scripts/daily-digest.sh"
}

install_linux() {
  local SERVICE_NAME="augmentagent-digest.service"
  local TIMER_NAME="augmentagent-digest.timer"
  local UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  local SERVICE="$UNIT_DIR/$SERVICE_NAME"
  local TIMER="$UNIT_DIR/$TIMER_NAME"
  local LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
  mkdir -p "$UNIT_DIR" "$LOG_DIR"

  command -v systemctl >/dev/null 2>&1 \
    || die "systemctl not found — this installer needs systemd"

  local SERVICE_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
  local ONCAL
  ONCAL="$(printf '*-*-* %02d:%02d:00' "$HOUR" "$MINUTE")"

  log "Writing service: $SERVICE"
  cat > "$SERVICE" <<UNIT_EOF
[Unit]
Description=AugmentAgent daily digest
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
WorkingDirectory=$REPO_ROOT
ExecStart=$REPO_ROOT/scripts/daily-digest.sh
Environment=PATH=$SERVICE_PATH
StandardOutput=append:$LOG_DIR/digest.stdout.log
StandardError=append:$LOG_DIR/digest.stderr.log
UNIT_EOF

  log "Writing timer: $TIMER (OnCalendar=$ONCAL)"
  cat > "$TIMER" <<TIMER_EOF
[Unit]
Description=Trigger the AugmentAgent daily digest at ${HOUR}:$(printf '%02d' "$MINUTE")

[Timer]
OnCalendar=$ONCAL
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
    log "NOTE: linger is OFF — digest only fires while you're logged in."
    log "      Run 24/7: sudo loginctl enable-linger $USER"
  fi

  log "Installed."
  log "  Service:   $SERVICE"
  log "  Timer:     $TIMER (daily at ${HOUR}:$(printf '%02d' "$MINUTE"))"
  log "  Logs:      $LOG_DIR/digest.stdout.log  (also: journalctl --user -u $SERVICE_NAME)"
  log "  Status:    systemctl --user list-timers $TIMER_NAME"
  log "  Uninstall: ./scripts/uninstall-digest.sh"
  log ""
  log "Test it right now:  ./scripts/daily-digest.sh"
}

case "$(uname -s)" in
  Darwin) install_macos ;;
  Linux)  install_linux ;;
  *)      die "unsupported platform: $(uname -s)" ;;
esac
