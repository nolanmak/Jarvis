#!/usr/bin/env bash
# Tests for scripts/lib/service-restart.sh (#826).
#
# The auto-updater built a new binary, skipped the service restart, and wrote
# the build stamp anyway. Because the stamp then equalled HEAD, its own
# "artifacts last built from X" self-check was satisfied forever and every
# later run reported `up to date` while the daemon served old code.
#
# These drive a stubbed `systemctl` so every branch is reachable without
# touching real units.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0
FAIL=0

ok()   { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected '$3', got '$2'"; fi; }

# --- stub systemctl -------------------------------------------------------
# Behaviour is driven by files in $SR_STUB_DIR so each case configures it
# without redefining the command.
setup_stub() {
  SR_STUB_DIR=$(mktemp -d)
  export SR_STUB_DIR
  STUB_BIN="$SR_STUB_DIR/bin"
  mkdir -p "$STUB_BIN"
  cat > "$STUB_BIN/systemctl" <<'STUB'
#!/usr/bin/env bash
d="$SR_STUB_DIR"
r() { cat "$d/$1" 2>/dev/null || printf '%s' "$2"; }
for a in "$@"; do
  case "$a" in
    list-unit-files) mode=list ;;
    show)            mode=show ;;
    restart)         mode=restart ;;
    is-active)       mode=active ;;
  esac
done
case "${mode:-}" in
  list)
    rc=$(r list_rc 0)
    [ "$rc" != 0 ] && { echo "Failed to connect to bus: No medium found" >&2; exit "$rc"; }
    [ "$(r registered 1)" = 1 ] && echo "augmentagent.service enabled enabled"
    exit 0 ;;
  show)
    if [ -e "$d/restarted" ]; then r pid_after 222; else r pid_before 111; fi
    echo; exit 0 ;;
  restart)
    rc=$(r restart_rc 0)
    [ "$rc" = 0 ] && touch "$d/restarted"
    exit "$rc" ;;
  active) exit "$(r active_rc 0)" ;;
esac
exit 0
STUB
  chmod +x "$STUB_BIN/systemctl"
  PATH="$STUB_BIN:$PATH"
  export PATH
}
teardown_stub() { rm -rf "$SR_STUB_DIR"; }

set_stub() { printf '%s' "$2" > "$SR_STUB_DIR/$1"; }

# shellcheck source=/dev/null
. "$REPO_ROOT/scripts/lib/service-restart.sh" 2>/dev/null || {
  echo "FATAL: scripts/lib/service-restart.sh not found (this is the red state)"; exit 1;
}

echo "restart_unit:"

setup_stub
set_stub registered 0
restart_unit augmentagent.service >/dev/null 2>&1
check "refuses when the unit is not registered" "$?" "1"
teardown_stub

setup_stub
set_stub list_rc 1
LOG_CAPTURE=$(restart_unit augmentagent.service 2>&1); rc=$?
check "refuses when systemctl itself cannot be queried" "$rc" "1"
case "$LOG_CAPTURE" in
  *"No medium found"*) ok "surfaces the systemctl error instead of swallowing it" ;;
  *) bad "surfaces the systemctl error instead of swallowing it" "log was: $LOG_CAPTURE" ;;
esac
teardown_stub

setup_stub
set_stub restart_rc 1
restart_unit augmentagent.service >/dev/null 2>&1
check "reports failure when the restart command fails" "$?" "1"
teardown_stub

setup_stub
set_stub pid_before 777
set_stub pid_after 777
restart_unit augmentagent.service >/dev/null 2>&1
check "reports failure when MainPID did not change (no real bounce)" "$?" "1"
teardown_stub

setup_stub
set_stub active_rc 3
restart_unit augmentagent.service >/dev/null 2>&1
check "reports failure when the unit is not active afterwards" "$?" "1"
teardown_stub

setup_stub
restart_unit augmentagent.service >/dev/null 2>&1
check "succeeds when the process actually bounced" "$?" "0"
teardown_stub

echo "maybe_defer_restart (#844):"

DEFER_DIR=$(mktemp -d)
export AUGMENTAGENT_SELFIMPROVE_LOCK="$DEFER_DIR/self-improve.lock"
export AUGMENTAGENT_RESTART_DEFER_STAMP="$DEFER_DIR/deferred-since"
export AUGMENTAGENT_RESTART_DEFER_MAX_SECS=2400
# Re-read the config now that the overrides exist.
SELF_IMPROVE_LOCK="$AUGMENTAGENT_SELFIMPROVE_LOCK"
RESTART_DEFER_STAMP="$AUGMENTAGENT_RESTART_DEFER_STAMP"
RESTART_DEFER_MAX_SECS="$AUGMENTAGENT_RESTART_DEFER_MAX_SECS"

# No lock file at all -> proceed.
maybe_defer_restart >/dev/null 2>&1
check "proceeds when no run has ever taken the lock" "$?" "1"

# Lock file present but NOT held -> proceed (a finished run leaves the file).
touch "$AUGMENTAGENT_SELFIMPROVE_LOCK"
maybe_defer_restart >/dev/null 2>&1
check "proceeds when the lock file exists but nothing holds it" "$?" "1"

# Held lock -> defer, and record when the deferral started.
flock -x "$AUGMENTAGENT_SELFIMPROVE_LOCK" -c 'sleep 30' &
HOLDER=$!
sleep 0.3
maybe_defer_restart >/dev/null 2>&1
check "defers while a run holds the lock" "$?" "0"
[ -s "$AUGMENTAGENT_RESTART_DEFER_STAMP" ] \
  && ok "records when the deferral began" \
  || bad "records when the deferral began" "stamp file empty/absent"

# Still held but the budget is exhausted -> proceed anyway.
printf '%s\n' "$(( $(date +%s) - 3000 ))" > "$AUGMENTAGENT_RESTART_DEFER_STAMP"
maybe_defer_restart >/dev/null 2>&1
check "restarts anyway once the deferral budget is exhausted" "$?" "1"
[ -e "$AUGMENTAGENT_RESTART_DEFER_STAMP" ] \
  && bad "clears its state after a forced proceed" "stamp survived" \
  || ok "clears its state after a forced proceed"

kill "$HOLDER" 2>/dev/null; wait "$HOLDER" 2>/dev/null

# Lock released -> proceed and clear any leftover stamp.
printf '9\n' > "$AUGMENTAGENT_RESTART_DEFER_STAMP"
maybe_defer_restart >/dev/null 2>&1
check "proceeds again once the run has finished" "$?" "1"
[ -e "$AUGMENTAGENT_RESTART_DEFER_STAMP" ] \
  && bad "clears stale deferral state when the lock is free" "stamp survived" \
  || ok "clears stale deferral state when the lock is free"
rm -rf "$DEFER_DIR"
unset AUGMENTAGENT_SELFIMPROVE_LOCK AUGMENTAGENT_RESTART_DEFER_STAMP

echo "should_write_stamp:"
should_write_stamp 0; check "writes the stamp when nothing failed" "$?" "0"
should_write_stamp 1; check "withholds the stamp when a required restart failed" "$?" "1"
should_write_stamp 2; check "withholds the stamp when several failed" "$?" "1"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
