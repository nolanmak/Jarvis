#!/bin/bash
# Tear down the periodic auto-update job and remove its unit file.
# Cross-platform: launchd plist on macOS, systemd user .timer + .service on Linux.
#
# Usage: ./scripts/uninstall-autoupdate.sh

set -euo pipefail

LABEL="com.nolanmak.augmentagent.updater"

log() { printf '\033[1;36m[uninstall-autoupdate]\033[0m %s\n' "$*" >&2; }

uninstall_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local DOMAIN="gui/$(id -u)"

  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    log "Booting out $DOMAIN/$LABEL"
    launchctl bootout "$DOMAIN/$LABEL" || true
  else
    log "Updater not loaded"
  fi

  if [ -f "$PLIST" ]; then
    log "Removing $PLIST"
    rm "$PLIST"
  else
    log "No plist at $PLIST"
  fi

  log "Done. Auto-update disabled."
}

uninstall_linux() {
  local SERVICE_NAME="augmentagent-update.service"
  local TIMER_NAME="augmentagent-update.timer"
  local UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  local SERVICE="$UNIT_DIR/$SERVICE_NAME"
  local TIMER="$UNIT_DIR/$TIMER_NAME"

  if ! command -v systemctl >/dev/null 2>&1; then
    log "systemctl not found — nothing to do"
    return
  fi

  if systemctl --user list-unit-files "$TIMER_NAME" 2>/dev/null | grep -q "$TIMER_NAME"; then
    log "Stopping + disabling $TIMER_NAME"
    systemctl --user disable --now "$TIMER_NAME" || true
  else
    log "Timer $TIMER_NAME not loaded"
  fi

  for f in "$TIMER" "$SERVICE"; do
    if [ -f "$f" ]; then
      log "Removing $f"
      rm "$f"
    fi
  done

  systemctl --user daemon-reload
  log "Done. Auto-update disabled."
}

case "$(uname -s)" in
  Darwin) uninstall_macos ;;
  Linux)  uninstall_linux ;;
  *)      log "unsupported platform: $(uname -s) — nothing to do" ;;
esac
