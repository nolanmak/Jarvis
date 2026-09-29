#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/home/Library/LaunchAgents" "$scratch/config/augmentagent" "$scratch/data/augmentagent/runpod-adapter/state"
cat > "$scratch/bin/uname" <<'EOF'
#!/bin/sh
printf 'Darwin\n'
EOF
cat > "$scratch/bin/launchctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$TEST_LAUNCHCTL_CALLS"
EOF
chmod +x "$scratch/bin/"*
export HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/config" XDG_DATA_HOME="$scratch/data"
export PATH="$scratch/bin:$PATH" TEST_LAUNCHCTL_CALLS="$scratch/calls"
plist="$HOME/Library/LaunchAgents/com.nolanmak.augmentagent.runpod-adapter.plist"
printf 'plist' > "$plist"
printf 'secret' > "$XDG_CONFIG_HOME/augmentagent/runpod-adapter.env"
printf 'journal' > "$XDG_DATA_HOME/augmentagent/runpod-adapter/state/jobs.sqlite3"
bash "$repo/scripts/uninstall-runpod-adapter.sh"
test ! -e "$plist"
test "$(cat "$XDG_CONFIG_HOME/augmentagent/runpod-adapter.env")" = secret
test "$(cat "$XDG_DATA_HOME/augmentagent/runpod-adapter/state/jobs.sqlite3")" = journal
grep -Eq 'bootout gui/[0-9]+/com.nolanmak.augmentagent.runpod-adapter' "$TEST_LAUNCHCTL_CALLS"
bash "$repo/scripts/uninstall-runpod-adapter.sh"
printf 'runpod-adapter uninstall: pass\n'
