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

echo "trim_gate_cache_if_idle (#891):"
CACHE_DIR=$(mktemp -d)
export AUGMENTAGENT_GATE_TARGET_DIR="$CACHE_DIR/gate"
export AUGMENTAGENT_GATE_CACHE_MAX_MB=1
export AUGMENTAGENT_SELFIMPROVE_LOCK="$CACHE_DIR/self-improve.lock"
GATE_CACHE_DIR="$AUGMENTAGENT_GATE_TARGET_DIR"; GATE_CACHE_MAX_MB=1
SELF_IMPROVE_LOCK="$AUGMENTAGENT_SELFIMPROVE_LOCK"
RESUME_LANE_LOCK="$CACHE_DIR/self-improve-resume.lock"
mkdir -p "$GATE_CACHE_DIR/debug"; dd if=/dev/zero of="$GATE_CACHE_DIR/debug/blob" bs=1M count=3 status=none

# A lane mid-build (holding either lock) must block the trim.
touch "$RESUME_LANE_LOCK"
# Short hold, then WAIT for it: `flock -c` hands the locked fd to its child,
# so killing the flock pid does not release the lock.
flock -x "$RESUME_LANE_LOCK" -c 'sleep 3' & HOLDER=$!
sleep 0.3
trim_gate_cache_if_idle >/dev/null 2>&1
check "leaves the cache alone while a lane holds its lock" "$?" "1"
[ -d "$GATE_CACHE_DIR/debug" ] && ok "debug/ survives while a build may be using it" \
  || bad "debug/ survives while a build may be using it" "it was deleted under a live build"
wait "$HOLDER" 2>/dev/null

# Idle + over cap -> trimmed.
trim_gate_cache_if_idle >/dev/null 2>&1
check "trims when over cap and every lane is idle" "$?" "0"
[ -d "$GATE_CACHE_DIR/debug" ] && bad "drops debug/ on trim" "still present" || ok "drops debug/ on trim"

# Under cap -> untouched.
mkdir -p "$GATE_CACHE_DIR/debug"; GATE_CACHE_MAX_MB=100000
trim_gate_cache_if_idle >/dev/null 2>&1
check "leaves a cache under the cap alone" "$?" "1"
rm -rf "$CACHE_DIR"
unset AUGMENTAGENT_GATE_TARGET_DIR AUGMENTAGENT_GATE_CACHE_MAX_MB AUGMENTAGENT_SELFIMPROVE_LOCK

echo "memory_pressure_ok / restart_budget_ok (#903):"
HYG_DIR=$(mktemp -d)
MEMINFO_PATH="$HYG_DIR/meminfo"
RESTART_MIN_AVAIL_MB=3072
RESTART_HISTORY="$HYG_DIR/restart-history"
RESTARTS_PER_HOUR=3
unset AUGMENTAGENT_RESTART_FORCE

printf 'MemTotal:       16000000 kB\nMemAvailable:    8000000 kB\n' > "$MEMINFO_PATH"
memory_pressure_ok >/dev/null 2>&1
check "memory: proceeds with plenty of MemAvailable" "$?" "0"

printf 'MemTotal:       16000000 kB\nMemAvailable:    1048576 kB\n' > "$MEMINFO_PATH"
HYG_LOG=$(memory_pressure_ok 2>&1); rc=$?
check "memory: defers when MemAvailable is under the floor" "$rc" "1"
case "$HYG_LOG" in
  *"restart deferred"*"MemAvailable"*) ok "memory: says why (restart deferred: MemAvailable=…)" ;;
  *) bad "memory: says why (restart deferred: MemAvailable=…)" "log was: $HYG_LOG" ;;
esac

rm -f "$MEMINFO_PATH"
memory_pressure_ok >/dev/null 2>&1
check "memory: fails open when meminfo is unreadable (macOS/containers)" "$?" "0"

printf 'MemAvailable:    1048576 kB\n' > "$MEMINFO_PATH"
AUGMENTAGENT_RESTART_FORCE=1 memory_pressure_ok >/dev/null 2>&1
check "memory: AUGMENTAGENT_RESTART_FORCE=1 overrides" "$?" "0"

restart_budget_ok >/dev/null 2>&1
check "budget: proceeds with no history" "$?" "0"

HYG_NOW=$(date +%s)
printf '%s\n%s\n%s\n' "$((HYG_NOW-600))" "$((HYG_NOW-1200))" "$((HYG_NOW-1800))" > "$RESTART_HISTORY"
HYG_LOG=$(restart_budget_ok 2>&1); rc=$?
check "budget: defers after 3 restarts inside the hour" "$rc" "1"
case "$HYG_LOG" in
  *"restart deferred"*) ok "budget: says why (restart deferred: …)" ;;
  *) bad "budget: says why (restart deferred: …)" "log was: $HYG_LOG" ;;
esac

printf '%s\n%s\n%s\n' "$((HYG_NOW-4000))" "$((HYG_NOW-5000))" "$((HYG_NOW-6000))" > "$RESTART_HISTORY"
restart_budget_ok >/dev/null 2>&1
check "budget: restarts older than an hour do not count" "$?" "0"

printf '%s\n%s\n%s\n' "$((HYG_NOW-600))" "$((HYG_NOW-1200))" "$((HYG_NOW-1800))" > "$RESTART_HISTORY"
AUGMENTAGENT_RESTART_FORCE=1 restart_budget_ok >/dev/null 2>&1
check "budget: AUGMENTAGENT_RESTART_FORCE=1 overrides" "$?" "0"

printf '%s\n%s\n' "$((HYG_NOW-9000))" "$((HYG_NOW-100))" > "$RESTART_HISTORY"
record_restart >/dev/null 2>&1
check "record_restart: returns 0" "$?" "0"
if [ "$(grep -c . "$RESTART_HISTORY")" = "2" ] && ! grep -q "^$((HYG_NOW-9000))$" "$RESTART_HISTORY" \
   && grep -q "^$((HYG_NOW-100))$" "$RESTART_HISTORY"; then
  ok "record_restart: appends now and prunes entries older than an hour"
else
  bad "record_restart: appends now and prunes entries older than an hour" "history: $(tr '\n' ' ' < "$RESTART_HISTORY")"
fi
rm -rf "$HYG_DIR"

echo "should_write_stamp:"
should_write_stamp 0; check "writes the stamp when nothing failed" "$?" "0"
should_write_stamp 1; check "withholds the stamp when a required restart failed" "$?" "1"
should_write_stamp 2; check "withholds the stamp when several failed" "$?" "1"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
