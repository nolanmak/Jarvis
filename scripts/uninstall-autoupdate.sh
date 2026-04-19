#!/bin/bash
set -euo pipefail

LABEL="com.nolanmak.augmentagent.updater"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

log() { printf '\033[1;36m[uninstall-autoupdate]\033[0m %s\n' "$*" >&2; }

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
