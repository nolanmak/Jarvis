#!/bin/bash
# Tear down the AugmentAgent auto-start service and remove its unit file.
# Cross-platform: launchd plist on macOS, systemd user unit on Linux.
#
# Usage: ./scripts/uninstall-autostart.sh

set -euo pipefail

LABEL="com.nolanmak.augmentagent"

log() { printf '\033[1;36m[uninstall-autostart]\033[0m %s\n' "$*" >&2; }

uninstall_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local UID_NUM
  UID_NUM="$(id -u)"
  local DOMAIN="gui/$UID_NUM"

  if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    log "Booting out $DOMAIN/$LABEL"
    launchctl bootout "$DOMAIN/$LABEL" || true
  else
    log "Agent not currently loaded"
  fi

  if [ -f "$PLIST" ]; then
    log "Removing $PLIST"
    rm "$PLIST"
  else
    log "No plist at $PLIST"
  fi

  log "Done. Daemon will not auto-start on next login."
}

uninstall_linux() {
  local UNIT_NAME="augmentagent.service"
  local UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  local UNIT="$UNIT_DIR/$UNIT_NAME"

  if ! command -v systemctl >/dev/null 2>&1; then
    log "systemctl not found — nothing to do"
    return
  fi

  if systemctl --user list-unit-files "$UNIT_NAME" 2>/dev/null | grep -q "$UNIT_NAME"; then
    log "Stopping + disabling $UNIT_NAME"
    systemctl --user disable --now "$UNIT_NAME" || true
  else
    log "Unit $UNIT_NAME not loaded"
  fi

  if [ -f "$UNIT" ]; then
    log "Removing $UNIT"
    rm "$UNIT"
  else
    log "No unit file at $UNIT"
  fi

  systemctl --user daemon-reload
  log "Done. Daemon will not auto-start on next login."
}

case "$(uname -s)" in
  Darwin) uninstall_macos ;;
  Linux)  uninstall_linux ;;
  *)      log "unsupported platform: $(uname -s) — nothing to do" ;;
esac
