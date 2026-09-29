#!/usr/bin/env bash
# Stop the optional adapter but retain its private credentials and job journal.
set -euo pipefail
case "$(uname -s)" in
  Darwin)
    label=com.nolanmak.augmentagent.runpod-adapter
    target="gui/$(id -u)/$label"
    plist="$HOME/Library/LaunchAgents/$label.plist"
    if launchctl print "$target" >/dev/null 2>&1; then
      launchctl bootout "$target"
    fi
    rm -f "$plist"
    ;;
  Linux)
    unit=augmentagent-runpod-adapter.service
    file="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$unit"
    if systemctl --user cat "$unit" >/dev/null 2>&1; then
      systemctl --user disable --now "$unit"
    fi
    rm -f "$file"
    systemctl --user daemon-reload
    ;;
  *) echo 'Runpod adapter uninstall requires Linux or macOS' >&2; exit 1 ;;
esac
printf 'Removed the Runpod adapter service; credentials and job journal were retained.\n' >&2
