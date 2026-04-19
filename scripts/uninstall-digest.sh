#!/bin/bash
# Tear down the daily digest scheduler.
# Cross-platform: launchd on macOS, systemd user timer on Linux.

set -euo pipefail

LABEL="com.nolanmak.augmentagent.digest"

log() { printf '\033[1;36m[uninstall-digest]\033[0m %s\n' "$*" >&2; }

uninstall_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local DOMAIN="gui/$(id -u)"

  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    log "Booting out $DOMAIN/$LABEL"
    launchctl bootout "$DOMAIN/$LABEL" || true
  else
    log "Digest agent not loaded"
  fi

  if [ -f "$PLIST" ]; then
    log "Removing $PLIST"
    rm "$PLIST"
  else
    log "No plist at $PLIST"
  fi
}

uninstall_linux() {
  local SERVICE_NAME="augmentagent-digest.service"
  local TIMER_NAME="augmentagent-digest.timer"
  local UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

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

  for f in "$UNIT_DIR/$TIMER_NAME" "$UNIT_DIR/$SERVICE_NAME"; do
    if [ -f "$f" ]; then
      log "Removing $f"
      rm "$f"
    fi
  done

  systemctl --user daemon-reload
}

case "$(uname -s)" in
  Darwin) uninstall_macos ;;
  Linux)  uninstall_linux ;;
  *)      log "unsupported platform: $(uname -s) — nothing to do" ;;
esac

log "Done."
