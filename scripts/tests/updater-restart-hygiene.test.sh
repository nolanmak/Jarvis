#!/usr/bin/env bash
# #903 — restart hygiene for scripts/check-for-updates.sh.
#
# The updater restarted augmentagent.service 12 times on 2026-08-31 (one per
# merged PR), the last one into a host already under memory pressure (#897).
# Every restart re-runs each channel's first tick — exactly when the journal
# replay fires. These drive the REAL updater against a throwaway repo with
# `cargo` / `systemctl` stubbed, and assert two new deferrals behave exactly
# like the #844 in-flight deferral: log `restart deferred: …`, withhold the
# build stamp (#826 invariant), retry next tick.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# Same shape as updater-stamp.test.sh: a repo one commit behind its origin,
# the change touching crates/ so a rebuild + restart are required.
make_case() {
  TMP=$(mktemp -d); export TMP
  git init -q --bare "$TMP/origin.git"
  git clone -q "$TMP/origin.git" "$TMP/work" 2>/dev/null
  mkdir -p "$TMP/work/scripts/lib" "$TMP/work/crates"
  cp "$REPO_ROOT/scripts/check-for-updates.sh" "$TMP/work/scripts/"
  cp "$REPO_ROOT/scripts/lib/service-restart.sh" "$TMP/work/scripts/lib/"
  echo base > "$TMP/work/crates/x.rs"
  git -C "$TMP/work" add -A
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qm base
  git -C "$TMP/work" push -q origin HEAD:main
  git -C "$TMP/work" branch -q -M main 2>/dev/null || true
  echo newer > "$TMP/work/crates/x.rs"
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qam newer
  git -C "$TMP/work" push -q origin main
  git -C "$TMP/work" reset -q --hard HEAD~1

  mkdir -p "$TMP/bin" "$TMP/state/augmentagent"
  # Stubs record what the updater actually invoked.
  cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
echo build >> "$STUB_DIR/cargo.calls"
exit 0
STUB
  cat > "$TMP/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
d="$STUB_DIR"
for a in "$@"; do
  case "$a" in
    list-unit-files) mode=list ;; show) mode=show ;;
    restart) mode=restart ;; is-active) mode=active ;;
  esac
done
pidfile="$d/mainpid"
[ -s "$pidfile" ] || echo 100 > "$pidfile"
case "${mode:-}" in
  # Answer per pattern: the tenant glob must NOT list the prod unit, or the
  # updater's tenant loop would "restart" augmentagent.service as a tenant.
  list)    case "$*" in *augmentagent-tenant-*) ;; *) echo "augmentagent.service enabled enabled" ;; esac; exit 0 ;;
  show)    cat "$pidfile"; exit 0 ;;
  restart) echo "$*" >> "$d/restart.calls"; echo $(( $(cat "$pidfile") + 1 )) > "$pidfile"; exit 0 ;;
  active)  exit 0 ;;
esac
exit 0
STUB
  chmod +x "$TMP/bin/cargo" "$TMP/bin/systemctl"
  STUB_DIR="$TMP"; export STUB_DIR
  STAMP_FILE="$TMP/state/augmentagent/built-commit"
  LOG_FILE="$TMP/state/augmentagent/update.log"
  HISTORY="$TMP/state/augmentagent/restart-history"
  # Deterministic host memory: the test decides, not the machine running it.
  MEMINFO="$TMP/meminfo"
  printf 'MemTotal:       16000000 kB\nMemAvailable:    8000000 kB\nSwapFree:        4000000 kB\n' > "$MEMINFO"
}

low_memory() { printf 'MemTotal:       16000000 kB\nMemAvailable:    1048576 kB\nSwapFree:              0 kB\n' > "$MEMINFO"; }
spend_budget() { local now; now=$(date +%s); printf '%s\n%s\n%s\n' "$((now-600))" "$((now-1200))" "$((now-1800))" > "$HISTORY"; }

# run_updater — extra env for a case comes from the caller's environment.
run_updater() {
  ( cd "$TMP/work" \
    && PATH="$TMP/bin:$PATH" XDG_STATE_HOME="$TMP/state" HOME="$TMP" \
       AUGMENTAGENT_MEMINFO_PATH="$MEMINFO" \
       ./scripts/check-for-updates.sh >/dev/null 2>&1 )
}

daemon_restarted() { grep -q "augmentagent.service" "$TMP/restart.calls" 2>/dev/null; }

echo "check-for-updates.sh restart hygiene (#903):"

# --- low_memory_defers_restart_and_keeps_stamp ----------------------------
make_case; low_memory
run_updater; rc=$?
daemon_restarted && bad "low memory: does not restart the daemon" "systemctl restart was invoked" \
                 || ok "low memory: does not restart the daemon"
[ -e "$TMP/cargo.calls" ] && bad "low memory: skips the release build too" "cargo was invoked" \
                          || ok "low memory: skips the release build too"
[ -s "$STAMP_FILE" ] && bad "low memory: withholds the build stamp" "stamp: $(cat "$STAMP_FILE")" \
                     || ok "low memory: withholds the build stamp"
grep -q "restart deferred" "$LOG_FILE" && ok "low memory: logs 'restart deferred'" \
                                       || bad "low memory: logs 'restart deferred'" "$(tail -3 "$LOG_FILE")"
[ "$rc" -ne 0 ] && ok "low memory: exits non-zero like every withheld stamp" \
                || bad "low memory: exits non-zero like every withheld stamp" "exit was 0"
# Memory recovers → the next tick deploys via the stamp-mismatch path.
printf 'MemAvailable:    8000000 kB\n' > "$MEMINFO"
run_updater
daemon_restarted && ok "low memory: a later tick restarts once memory recovers" \
                 || bad "low memory: a later tick restarts once memory recovers" "still no restart"
[ -s "$STAMP_FILE" ] && ok "low memory: …and writes the stamp then" \
                     || bad "low memory: …and writes the stamp then" "stamp absent"
rm -rf "$TMP"

# --- restart_budget_exhausted_defers ---------------------------------------
make_case; spend_budget
run_updater; rc=$?
daemon_restarted && bad "budget spent: does not restart the daemon" "systemctl restart was invoked" \
                 || ok "budget spent: does not restart the daemon"
[ -s "$STAMP_FILE" ] && bad "budget spent: withholds the build stamp" "stamp written" \
                     || ok "budget spent: withholds the build stamp"
grep -q "restart deferred" "$LOG_FILE" && ok "budget spent: logs 'restart deferred'" \
                                       || bad "budget spent: logs 'restart deferred'" "$(tail -3 "$LOG_FILE")"
[ "$rc" -ne 0 ] && ok "budget spent: exits non-zero" || bad "budget spent: exits non-zero" "exit was 0"
# Only restarts inside the last hour count.
now=$(date +%s); printf '%s\n%s\n%s\n' "$((now-4000))" "$((now-5000))" "$((now-6000))" > "$HISTORY"
run_updater
daemon_restarted && ok "budget spent: restarts older than an hour do not count" \
                 || bad "budget spent: restarts older than an hour do not count" "still deferred"
rm -rf "$TMP"

# --- force_overrides_both --------------------------------------------------
make_case; low_memory; spend_budget
AUGMENTAGENT_RESTART_FORCE=1 run_updater; rc=$?
daemon_restarted && ok "force: restarts despite low memory and a spent budget" \
                 || bad "force: restarts despite low memory and a spent budget" "no restart"
[ -s "$STAMP_FILE" ] && ok "force: writes the stamp" || bad "force: writes the stamp" "stamp absent"
[ "$rc" -eq 0 ] && ok "force: exits zero" || bad "force: exits zero" "exit was $rc"
rm -rf "$TMP"

# --- healthy deploy records its restart --------------------------------------
make_case
run_updater; rc=$?
daemon_restarted && ok "healthy: restarts" || bad "healthy: restarts" "no restart"
[ -s "$STAMP_FILE" ] && ok "healthy: writes the stamp" || bad "healthy: writes the stamp" "stamp absent"
[ "$(grep -c . "$HISTORY" 2>/dev/null)" = "1" ] && ok "healthy: records the restart in restart-history" \
  || bad "healthy: records the restart in restart-history" "history: $(cat "$HISTORY" 2>/dev/null)"
rm -rf "$TMP"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
