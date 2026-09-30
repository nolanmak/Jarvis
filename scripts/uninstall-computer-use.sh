#!/usr/bin/env bash
set -euo pipefail
[ "$(uname -s)" = Darwin ] || { echo 'computer-use launchd uninstaller is macOS-only' >&2; exit 1; }
label=com.nolanmak.augmentagent.computer-use
plist="$HOME/Library/LaunchAgents/$label.plist"
domain="gui/$(id -u)"
if launchctl print "$domain/$label" >/dev/null 2>&1; then
  launchctl bootout "$domain/$label"
fi
rm -f "$plist"
echo "Removed $label" >&2
