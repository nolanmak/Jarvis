#!/usr/bin/env bash
# Install the launchd job that keeps the data-source apps alive.
#
# Separate from the exporters' own schedules on purpose: the watchdog's job is
# to make sure the apps those exporters read from are actually writing. See
# scripts/sync-source-watchdog.sh for why store freshness is the signal.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
source "$repo_root/scripts/lib/launchd-install.sh"

[ "$(uname -s)" = "Darwin" ] || {
  printf 'the source-app watchdog is macOS-only (launchd + open -a)\n' >&2
  exit 1
}

label="com.nolanmak.augmentagent.sync-watchdog"
agents="$HOME/Library/LaunchAgents"
plist="$agents/$label.plist"
script="$repo_root/scripts/sync-source-watchdog.sh"
log_dir="$HOME/Library/Logs/augmentagent"
interval="${WATCHDOG_INTERVAL_S:-900}"   # 15 min; the stores tolerate that easily

[ -x "$script" ] || { printf 'missing or not executable: %s\n' "$script" >&2; exit 1; }
mkdir -p "$agents" "$log_dir"

path="$(launchd_service_path open)"
candidate="$(launchd_candidate "$plist")"
cat > "$candidate" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>$(launchd_xml_escape "$script")</string></array>
  <key>WorkingDirectory</key><string>$(launchd_xml_escape "$repo_root")</string>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>$(launchd_xml_escape "$path")</string>
  </dict>
  <key>StartInterval</key><integer>$interval</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$(launchd_xml_escape "$log_dir/sync-watchdog.log")</string>
  <key>StandardErrorPath</key><string>$(launchd_xml_escape "$log_dir/sync-watchdog.log")</string>
</dict></plist>
PLIST

launchd_install "$label" "$plist" "$candidate" true
printf 'source-app watchdog installed: %s (every %ss)\n' "$label" "$interval"
printf 'log: %s\n' "$log_dir/sync-watchdog.log"
