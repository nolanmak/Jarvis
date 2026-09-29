#!/usr/bin/env bash
# Remove one optional macOS sidecar job while keeping its state and session.
set -euo pipefail
[ "$#" -eq 1 ] || { echo 'usage: uninstall-sidecar.sh renderer|fetch|wa-sidecar' >&2; exit 2; }
case "$1" in
  renderer|fetch|wa-sidecar) ;;
  *) echo 'unknown sidecar' >&2; exit 2 ;;
esac
[ "$(uname -s)" = Darwin ] || { echo 'sidecar LaunchAgent removal requires macOS' >&2; exit 1; }
label="com.nolanmak.augmentagent.$1"
target="gui/$(id -u)/$label"
plist="$HOME/Library/LaunchAgents/$label.plist"
if launchctl print "$target" >/dev/null 2>&1; then
  launchctl bootout "$target"
fi
rm -f "$plist"
printf 'Removed %s; sidecar state and account sessions were retained.\n' "$label" >&2
