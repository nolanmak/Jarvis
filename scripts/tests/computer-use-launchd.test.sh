#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/home"
cat > "$scratch/bin/uname" <<'EOF'
#!/bin/sh
printf 'Darwin\n'
EOF
cat > "$scratch/bin/launchctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$TEST_LAUNCHCTL_CALLS"
case "$1" in print) exit 1 ;; esac
EOF
cat > "$scratch/bin/plutil" <<'EOF'
#!/bin/sh
python3 - "$2" <<'PY'
import plistlib, sys
plistlib.load(open(sys.argv[1], 'rb'))
PY
EOF
chmod +x "$scratch/bin/"*
export HOME="$scratch/home" XDG_STATE_HOME="$scratch/state"
export PATH="$scratch/bin:$PATH" TEST_LAUNCHCTL_CALLS="$scratch/launchctl-calls"
export JARVIS_COMPUTER_STATE="$scratch/private state"
export JARVIS_COMPUTER_SOCKET="$scratch/private state/worker.sock"
export JARVIS_COMPUTER_CHROME_BRIDGE="$scratch/bridge & consent"
export JARVIS_COMPUTER_LOCK_DIRECTORY="$scratch/bridge locks"
bash "$repo/scripts/install-computer-use.sh"
plist="$HOME/Library/LaunchAgents/com.nolanmak.augmentagent.computer-use.plist"
python3 - "$plist" <<'PY'
import os, plistlib, sys
p = plistlib.load(open(sys.argv[1], 'rb'))
env = p['EnvironmentVariables']
assert p['Label'] == 'com.nolanmak.augmentagent.computer-use'
assert env['JARVIS_COMPUTER_STATE'] == os.environ['JARVIS_COMPUTER_STATE']
assert env['JARVIS_COMPUTER_SOCKET'] == os.environ['JARVIS_COMPUTER_SOCKET']
assert env['JARVIS_COMPUTER_CHROME_BRIDGE'] == os.environ['JARVIS_COMPUTER_CHROME_BRIDGE']
assert env['JARVIS_COMPUTER_LOCK_DIRECTORY'] == os.environ['JARVIS_COMPUTER_LOCK_DIRECTORY']
assert p['RunAtLoad'] and p['KeepAlive']
assert p['Umask'] == 63
assert p['ProgramArguments'][0] == '/bin/bash'
assert p['ProgramArguments'][1].endswith('/sidecars/computer-use/start.sh')
assert os.stat(os.environ['JARVIS_COMPUTER_STATE']).st_mode & 0o777 == 0o700
PY
bash "$repo/scripts/install-computer-use.sh"
bash "$repo/scripts/uninstall-computer-use.sh"
test ! -e "$plist"
printf 'computer-use launchd lifecycle: pass\n'
