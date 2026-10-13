#!/usr/bin/env bash
# Remove the source-app watchdog. Source databases and exported bundles are
# never touched: this only stops the relaunching.
set -euo pipefail
label="com.nolanmak.augmentagent.sync-watchdog"
plist="$HOME/Library/LaunchAgents/$label.plist"
domain="gui/$(id -u)"

if launchctl print "$domain/$label" >/dev/null 2>&1; then
  launchctl bootout "$domain/$label" 2>/dev/null || true
fi
rm -f "$plist"
printf 'source-app watchdog removed; exported data retained\n'
