#!/usr/bin/env bash
# #909 — the auto-update timer must fire again after a re-login.
#
# The rendered timer only had OnBootSec + OnUnitActiveSec. After the user
# service manager restarts (logout/login, or the session collapsing as it did
# in the 2026-08-31 OOM, #897) OnBootSec elapsed days ago and OnUnitActiveSec
# is relative to a service activation that never comes, so NextElapse is
# "infinity" and nothing deploys — observed live: 6 hours of merged PRs, old
# binary still running. OnStartupSec is relative to the *service manager's*
# start, which for a user manager is login.
#
# Drives the real installer against a throwaway HOME with `systemctl` stubbed.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/home"
printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/bin/systemctl"
chmod +x "$TMP/bin/systemctl"

if PATH="$TMP/bin:$PATH" HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/home/.config" \
   XDG_STATE_HOME="$TMP/home/.local/state" USER="${USER:-tester}" \
   bash "$REPO_ROOT/scripts/install-autoupdate.sh" >"$TMP/install.log" 2>&1; then
  ok "installer runs against a throwaway HOME"
else
  bad "installer runs against a throwaway HOME" "$(tail -5 "$TMP/install.log")"
fi

TIMER="$TMP/home/.config/systemd/user/augmentagent-update.timer"
if [ -f "$TIMER" ]; then ok "timer rendered"; else bad "timer rendered" "missing $TIMER"; fi

# OnStartupSec is the fix; the other three must survive it.
for d in "OnStartupSec=2min" "OnBootSec=2min" "OnUnitActiveSec=300s" "Persistent=true"; do
  if grep -qx "$d" "$TIMER" 2>/dev/null; then ok "timer has $d"; else bad "timer has $d" "$(cat "$TIMER" 2>/dev/null)"; fi
done

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
