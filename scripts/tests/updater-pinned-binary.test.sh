#!/usr/bin/env bash
# #1571 — the updater must notice when the unit no longer runs what it builds.
#
# 2026-10-20 → 10-24: a systemd drop-in replaced augmentagent.service's
# ExecStart with a wrapper that exec'd a frozen copy of the daemon. Every tick
# the REAL updater pulled, rebuilt, verifiably bounced the unit (MainPID
# changed, unit active) — and the unit came back on the copy. The stamp was
# withheld, but the log only ever said "restart could not be verified", and
# the forced restarts killed in-flight auto-PR builds for four days.
#
# These drive the real updater against a throwaway repo with `cargo`,
# `systemctl`, `curl` and /proc stubbed, and assert:
#   - an ExecStart override is reported as DEPLOY PINNED, alerted ONCE, the
#     restart is skipped and the stamp withheld;
#   - a unit that bounces onto a different executable is reported the same
#     way after the restart;
#   - retiring the override makes the next tick deploy normally and clears
#     the alert memory.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# Same shape as updater-restart-hygiene.test.sh: a repo one commit behind its
# origin, the change touching crates/ so a rebuild + restart are required.
make_case() {
  TMP=$(mktemp -d); export TMP
  git init -q --bare "$TMP/origin.git"
  git clone -q "$TMP/origin.git" "$TMP/work" 2>/dev/null
  mkdir -p "$TMP/work/scripts/lib" "$TMP/work/crates" "$TMP/work/target/release"
  cp "$REPO_ROOT/scripts/check-for-updates.sh" "$TMP/work/scripts/"
  printf 'import sys; sys.exit(0)\n' > "$TMP/work/scripts/pdf-runtime.py"
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

  # The binary the updater "builds" and the frozen copy a rollout might pin.
  printf '#!/bin/sh\n' > "$TMP/work/target/release/augmentagent"
  mkdir -p "$TMP/frozen"
  printf '#!/bin/sh\n' > "$TMP/frozen/augmentagent"

  mkdir -p "$TMP/bin" "$TMP/state/augmentagent" "$TMP/proc"
  cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
echo build >> "$STUB_DIR/cargo.calls"
exit 0
STUB
  # systemctl answers `show -p <prop> --value` per property. ExecStart and
  # DropInPaths come from files the case writes; MainPID advances on every
  # restart and a fake /proc/<pid>/exe points at whatever `exe_target` says.
  cat > "$TMP/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
d="$STUB_DIR"
prop=""
for a in "$@"; do
  case "$a" in
    list-unit-files) mode=list ;; cat) mode=cat ;; show) mode=show ;;
    restart) mode=restart ;; is-active) mode=active ;;
    MainPID|ExecStart|DropInPaths) prop="$a" ;;
  esac
done
pidfile="$d/mainpid"
[ -s "$pidfile" ] || echo 100 > "$pidfile"
spawn() {
  pid=$1
  mkdir -p "$d/proc/$pid"
  ln -sfn "$(cat "$d/exe_target")" "$d/proc/$pid/exe"
}
case "${mode:-}" in
  cat)     exit 1 ;;
  list)    case "$*" in *augmentagent-tenant-*) ;; *) echo "augmentagent.service enabled enabled" ;; esac; exit 0 ;;
  show)
    case "$prop" in
      MainPID)     cat "$pidfile" ;;
      ExecStart)   cat "$d/execstart" ;;
      DropInPaths) cat "$d/dropins" 2>/dev/null || true ;;
      *)           cat "$pidfile" ;;
    esac
    exit 0 ;;
  restart)
    echo "$*" >> "$d/restart.calls"
    next=$(( $(cat "$pidfile") + 1 )); echo "$next" > "$pidfile"; spawn "$next"
    exit 0 ;;
  active)  exit 0 ;;
esac
exit 0
STUB
  # Discord webhook: record every owner alert instead of sending it.
  cat > "$TMP/bin/curl" <<'STUB'
#!/usr/bin/env bash
echo "$@" >> "$STUB_DIR/curl.calls"
exit 0
STUB
  chmod +x "$TMP/bin/cargo" "$TMP/bin/systemctl" "$TMP/bin/curl"
  # Exercise the Linux (systemd) branch on any host (#1079: CI runs macOS too).
  printf '#!/usr/bin/env bash\necho Linux\n' > "$TMP/bin/uname" && chmod +x "$TMP/bin/uname"
  STUB_DIR="$TMP"; export STUB_DIR
  STAMP_FILE="$TMP/state/augmentagent/built-commit"
  LOG_FILE="$TMP/state/augmentagent/update.log"
  ALERT_STAMP="$TMP/state/augmentagent/deploy-pinned-alerted"
  MEMINFO="$TMP/meminfo"
  printf 'MemTotal:       16000000 kB\nMemAvailable:    8000000 kB\nSwapFree:        4000000 kB\n' > "$MEMINFO"
  # Healthy defaults: the canonical launcher, no drop-ins, our binary.
  canonical_exec
  printf '%s\n' "$TMP/work/target/release/augmentagent" > "$TMP/exe_target"
  : > "$TMP/dropins"
}

canonical_exec() {
  printf '{ path=%s/work/scripts/run-rs.sh ; argv[]=%s/work/scripts/run-rs.sh --wiki-dir ./wiki serve --dry-run false ; ignore_errors=no }\n' "$TMP" "$TMP" > "$TMP/execstart"
}
pinned_exec() {
  printf '{ path=%s/frozen/run-phone.sh ; argv[]=%s/frozen/run-phone.sh ; ignore_errors=no }\n' "$TMP" "$TMP" > "$TMP/execstart"
  printf '%s/.config/systemd/user/augmentagent.service.d/phone-rollout.conf\n' "$TMP" > "$TMP/dropins"
  printf '%s\n' "$TMP/frozen/augmentagent" > "$TMP/exe_target"
}

run_updater() {
  ( cd "$TMP/work" \
    && PATH="$TMP/bin:$PATH" XDG_STATE_HOME="$TMP/state" HOME="$TMP" \
       AUGMENTAGENT_MEMINFO_PATH="$MEMINFO" AUGMENTAGENT_PROC_ROOT="$TMP/proc" \
       AUGMENTAGENT_RESTART_FORCE=1 DISCORD_WEBHOOK_URL="https://discord.invalid/hook" \
       ./scripts/check-for-updates.sh >/dev/null 2>&1 )
}

# `grep -c` prints 0 AND exits 1 on no match, so count by hand.
count()     { local n; n=$(grep -c "$1" "$2" 2>/dev/null); printf '%s' "${n:-0}"; }
restarts()  { count "augmentagent.service" "$TMP/restart.calls"; }
alerts()    { count . "$TMP/curl.calls"; }
pinned_log(){ count "DEPLOY PINNED" "$LOG_FILE"; }

echo "check-for-updates.sh pinned-deploy detection (#1571):"

# --- an ExecStart override: reported, alerted once, no restart, no stamp -----
make_case; pinned_exec
run_updater; rc=$?
[ "$(restarts)" = 0 ] && ok "override: does not restart (it would relaunch the frozen binary)" \
                      || bad "override: does not restart (it would relaunch the frozen binary)" "restarts: $(restarts)"
[ -s "$STAMP_FILE" ] && bad "override: withholds the build stamp" "stamp: $(cat "$STAMP_FILE")" \
                     || ok "override: withholds the build stamp"
[ "$rc" -ne 0 ] && ok "override: exits non-zero like every withheld stamp" \
                || bad "override: exits non-zero like every withheld stamp" "exit was 0"
grep -q "DEPLOY PINNED: augmentagent.service has ExecStart overridden away from scripts/run-rs.sh" "$LOG_FILE" \
  && ok "override: logs DEPLOY PINNED with the cause" \
  || bad "override: logs DEPLOY PINNED with the cause" "$(tail -3 "$LOG_FILE")"
grep -q "phone-rollout.conf" "$LOG_FILE" && ok "override: names the drop-in" \
                                          || bad "override: names the drop-in" "$(grep 'DEPLOY PINNED' "$LOG_FILE" | tail -1)"
grep -q "daemon-reload" "$LOG_FILE" && ok "override: says how to retire it" \
                                     || bad "override: says how to retire it" "$(grep 'DEPLOY PINNED' "$LOG_FILE" | tail -1)"
[ "$(alerts)" = 1 ] && ok "override: alerts the owner once" || bad "override: alerts the owner once" "alerts: $(alerts)"
grep -q "DEPLOY PINNED" "$TMP/curl.calls" && ok "override: the alert carries the diagnosis" \
                                            || bad "override: the alert carries the diagnosis" "$(cat "$TMP/curl.calls")"
# Second tick, same cause: logged again, NOT alerted again.
run_updater
[ "$(pinned_log)" = 2 ] && ok "override: every tick logs it" || bad "override: every tick logs it" "logged $(pinned_log) times"
[ "$(alerts)" = 1 ] && ok "override: the same cause is not re-alerted every 5 minutes" \
                    || bad "override: the same cause is not re-alerted every 5 minutes" "alerts: $(alerts)"
[ "$(restarts)" = 0 ] && ok "override: still no restart" || bad "override: still no restart" "restarts: $(restarts)"
# Override retired → the next tick deploys through the stamp-mismatch path.
canonical_exec; : > "$TMP/dropins"; printf '%s\n' "$TMP/work/target/release/augmentagent" > "$TMP/exe_target"
run_updater; rc=$?
[ "$(restarts)" = 1 ] && ok "retired: the next tick restarts" || bad "retired: the next tick restarts" "restarts: $(restarts)"
[ -s "$STAMP_FILE" ] && ok "retired: …and writes the stamp" || bad "retired: …and writes the stamp" "stamp absent"
[ "$rc" -eq 0 ] && ok "retired: exits zero" || bad "retired: exits zero" "exit was $rc"
[ -e "$ALERT_STAMP" ] && bad "retired: a verified deploy clears the alert memory" "alert stamp still present" \
                      || ok "retired: a verified deploy clears the alert memory"
[ "$(alerts)" = 1 ] && ok "retired: recovery sends no extra message" || bad "retired: recovery sends no extra message" "alerts: $(alerts)"
rm -rf "$TMP"

# --- canonical ExecStart, but the unit comes back on another executable ------
make_case
printf '%s\n' "$TMP/frozen/augmentagent" > "$TMP/exe_target"
run_updater; rc=$?
[ "$(restarts)" = 1 ] && ok "wrong exe: the restart itself happens" || bad "wrong exe: the restart itself happens" "restarts: $(restarts)"
grep -q "DEPLOY PINNED: augmentagent.service came back running $TMP/frozen/augmentagent instead of target/release/augmentagent" "$LOG_FILE" \
  && ok "wrong exe: reports what the unit is actually running" \
  || bad "wrong exe: reports what the unit is actually running" "$(grep 'DEPLOY PINNED' "$LOG_FILE" | tail -1)"
[ -s "$STAMP_FILE" ] && bad "wrong exe: withholds the build stamp" "stamp written" || ok "wrong exe: withholds the build stamp"
[ "$rc" -ne 0 ] && ok "wrong exe: exits non-zero" || bad "wrong exe: exits non-zero" "exit was 0"
[ "$(alerts)" = 1 ] && ok "wrong exe: alerts the owner once" || bad "wrong exe: alerts the owner once" "alerts: $(alerts)"
run_updater
[ "$(alerts)" = 1 ] && ok "wrong exe: not re-alerted on the next tick" || bad "wrong exe: not re-alerted on the next tick" "alerts: $(alerts)"
rm -rf "$TMP"

# --- a rebuilt binary under a running process is a pending restart, not a pin
make_case
printf '%s (deleted)\n' "$TMP/work/target/release/augmentagent" > "$TMP/exe_target"
run_updater; rc=$?
[ "$(pinned_log)" = 0 ] && ok "deleted suffix: a replaced-on-disk binary is not a pinned deploy" \
                        || bad "deleted suffix: a replaced-on-disk binary is not a pinned deploy" "$(grep 'DEPLOY PINNED' "$LOG_FILE")"
[ -s "$STAMP_FILE" ] && ok "deleted suffix: writes the stamp" || bad "deleted suffix: writes the stamp" "stamp absent"
rm -rf "$TMP"

# --- no evidence (empty or malformed ExecStart) fails open -------------------
make_case
printf '100\n' > "$TMP/execstart"
run_updater; rc=$?
[ "$(pinned_log)" = 0 ] && ok "no evidence: an unreadable ExecStart is not a pin" \
                        || bad "no evidence: an unreadable ExecStart is not a pin" "$(grep 'DEPLOY PINNED' "$LOG_FILE")"
[ -s "$STAMP_FILE" ] && ok "no evidence: deploys normally" || bad "no evidence: deploys normally" "stamp absent"
rm -rf "$TMP"

# --- healthy deploy: nothing pinned, no alert, stamp written ----------------
make_case
run_updater; rc=$?
[ "$(pinned_log)" = 0 ] && ok "healthy: no pinned-deploy report" || bad "healthy: no pinned-deploy report" "$(grep 'DEPLOY PINNED' "$LOG_FILE")"
[ "$(alerts)" = 0 ] && ok "healthy: no owner alert" || bad "healthy: no owner alert" "alerts: $(alerts)"
[ -s "$STAMP_FILE" ] && ok "healthy: writes the stamp" || bad "healthy: writes the stamp" "stamp absent"
[ "$rc" -eq 0 ] && ok "healthy: exits zero" || bad "healthy: exits zero" "exit was $rc"
rm -rf "$TMP"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
