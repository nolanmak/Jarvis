#!/usr/bin/env bash
# Remove only the service registration; accounts, keys and routing stay private
# on disk so a later reinstall can reuse them.
set -euo pipefail
case "$(uname -s)" in
  Darwin)
    label=com.nolanmak.augmentagent.model-router
    target="gui/$(id -u)/$label"
    plist="$HOME/Library/LaunchAgents/$label.plist"
    if launchctl print "$target" >/dev/null 2>&1; then
      launchctl bootout "$target"
    fi
    rm -f "$plist"
    ;;
  Linux)
    unit=augmentagent-model-router.service
    file="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$unit"
    if systemctl --user cat "$unit" >/dev/null 2>&1; then
      systemctl --user disable --now "$unit"
    fi
    rm -f "$file"
    systemctl --user daemon-reload
    ;;
  *) echo 'model router uninstall requires Linux or macOS' >&2; exit 1 ;;
esac
printf 'Removed the model router service; accounts and credentials were retained.\n' >&2
