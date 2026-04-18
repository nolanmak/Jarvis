#!/bin/bash
# Tear down the AugmentAgent LaunchAgent and remove its plist.
#
# Usage: ./scripts/uninstall-autostart.sh

set -euo pipefail

LABEL="com.nolanmak.augmentagent"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
UID_NUM="$(id -u)"
DOMAIN="gui/$UID_NUM"

log() { printf '\033[1;36m[uninstall-autostart]\033[0m %s\n' "$*" >&2; }

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
