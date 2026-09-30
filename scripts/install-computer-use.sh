#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
source "$repo/scripts/lib/launchd-install.sh"
[ "$(uname -s)" = Darwin ] || { echo 'computer-use launchd installer is macOS-only' >&2; exit 1; }
label=com.nolanmak.augmentagent.computer-use
plist="$HOME/Library/LaunchAgents/$label.plist"
state="${JARVIS_COMPUTER_STATE:-${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent/computer-use}"
socket="${JARVIS_COMPUTER_SOCKET:-$state/worker.sock}"
bridge="${JARVIS_COMPUTER_CHROME_BRIDGE:-$HOME/.local/share/newsletterbuddy/staging/chrome-bridge}"
locks="${JARVIS_COMPUTER_LOCK_DIRECTORY:-$HOME/.local/share/newsletterbuddy/staging/worker-tmp}"
for path in "$state" "$socket" "$bridge" "$locks"; do
  case "$path" in /*) ;; *) echo "computer-use paths must be absolute: $path" >&2; exit 1 ;; esac
done
[ -f "$repo/sidecars/computer-use/node_modules/playwright/package.json" ] || {
  echo 'run npm ci in sidecars/computer-use before installing the worker' >&2; exit 1;
}
node="$(command -v node)" || { echo 'Node.js is required' >&2; exit 1; }
python="$(command -v python3)" || { echo 'Python 3 is required' >&2; exit 1; }
launch_path="$(launchd_service_path node python3)"
(umask 077; mkdir -p "$state" "$(dirname "$socket")" "${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent" "$(dirname "$plist")")
chmod 700 "$state" "$(dirname "$socket")" "${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
candidate="$(launchd_candidate "$plist")"
repo_xml="$(launchd_xml_escape "$repo")"
start_xml="$(launchd_xml_escape "$repo/sidecars/computer-use/start.sh")"
home_xml="$(launchd_xml_escape "$HOME")"
path_xml="$(launchd_xml_escape "$launch_path")"
node_xml="$(launchd_xml_escape "$node")"
python_xml="$(launchd_xml_escape "$python")"
state_xml="$(launchd_xml_escape "$state")"
socket_xml="$(launchd_xml_escape "$socket")"
bridge_xml="$(launchd_xml_escape "$bridge")"
locks_xml="$(launchd_xml_escape "$locks")"
log_xml="$(launchd_xml_escape "$log_dir/computer-use.log")"
cat > "$candidate" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>WorkingDirectory</key><string>$repo_xml</string>
  <key>ProgramArguments</key><array><string>/bin/bash</string><string>$start_xml</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>$home_xml</string>
    <key>PATH</key><string>$path_xml</string>
    <key>JARVIS_COMPUTER_NODE</key><string>$node_xml</string>
    <key>JARVIS_COMPUTER_PYTHON</key><string>$python_xml</string>
    <key>JARVIS_COMPUTER_STATE</key><string>$state_xml</string>
    <key>JARVIS_COMPUTER_SOCKET</key><string>$socket_xml</string>
    <key>JARVIS_COMPUTER_CHROME_BRIDGE</key><string>$bridge_xml</string>
    <key>JARVIS_COMPUTER_LOCK_DIRECTORY</key><string>$locks_xml</string>
  </dict>
  <key>Umask</key><integer>63</integer>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>StandardOutPath</key><string>$log_xml</string>
  <key>StandardErrorPath</key><string>$log_xml</string>
</dict></plist>
PLIST
launchd_install "$label" "$plist" "$candidate" true
echo "Installed $label; status: augmentagent service --unit augmentagent-computer-use.service status; logs: augmentagent logs --unit augmentagent-computer-use.service" >&2
if [ ! -d "$bridge" ]; then
  echo "Chrome bridge is not ready at $bridge; start the shared bridge and grant Chrome debugging consent before research tasks" >&2
fi
