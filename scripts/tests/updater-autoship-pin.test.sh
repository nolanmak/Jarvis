#!/usr/bin/env bash
# #2468 — the auto-ship worker runs a pinned build; main deploys never move it.
#
# The worker reloads when its executable inode or its deploy stamp changes
# (autoship_worker.rs). Pinned, those are bin/augmentagent under the pin root
# and autoship-pinned-built. So "the worker is not restarted" means: neither
# changes, and nothing restarts augmentagent-autopr.service.
#
# Drives the real updater against a throwaway repo with cargo, systemctl,
# curl and /proc stubbed, and asserts:
#   - the decision table: an unchanged pin is a no-op; a moved pin rebuilds;
#   - first deploy with no pin: pins the build this deploy installs, by copy
#     (no second compile), and nothing else about the deploy changes;
#   - an unrelated merge to main rebuilds and restarts the daemon as before,
#     but leaves the worker's binary and stamp untouched;
#   - scripts/autoship-pin.sh <ref> makes the next tick build that commit on
#     the pin root's own target dir, install it and re-provision the worker;
#   - a second tick with the same pin does nothing; an invalid pin changes
#     nothing; "main" unpins after re-provisioning.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected '$3', got '$2'"; fi; }

echo "autoship pin decision table (#2468):"
# shellcheck source=../lib/autoship-pin.sh
. "$REPO_ROOT/scripts/lib/autoship-pin.sh"
A=$(printf 'a%.0s' {1..40}); B=$(printf 'b%.0s' {1..40})
check "unchanged pin, binary present: no rebuild, no reload" "$(autoship_pin_decision "$A" "$A" 1 "$B")" unchanged
check "pin moved: rebuild and reload"                        "$(autoship_pin_decision "$B" "$A" 1 "$B")" install
check "pinned binary missing: rebuild"                       "$(autoship_pin_decision "$A" "$A" 0 "")" install
check "no pin, a deploy to bootstrap from: bootstrap"        "$(autoship_pin_decision "" "" 0 "$A")" bootstrap
check "no pin, nothing to bootstrap from: nothing"           "$(autoship_pin_decision "" "" 0 "")" none
check "unpinned while a pinned build is installed: unpin"    "$(autoship_pin_decision main "$A" 1 "$B")" unpin
check "unpinned and already following main: nothing"         "$(autoship_pin_decision main main 1 "$B")" none

make_case() {
  TMP=$(cd "$(mktemp -d)" && pwd -P); export TMP
  git init -q --bare "$TMP/origin.git"
  git clone -q "$TMP/origin.git" "$TMP/work" 2>/dev/null
  cp "$REPO_ROOT/.gitignore" "$TMP/work/.gitignore"
  mkdir -p "$TMP/work/scripts/lib" "$TMP/work/crates" "$TMP/work/target/release"
  cp "$REPO_ROOT/scripts/check-for-updates.sh" "$REPO_ROOT/scripts/autoship-pin.sh" \
     "$REPO_ROOT/scripts/recover-autopr-worker.py" "$REPO_ROOT/scripts/secure-release-executables.py" "$TMP/work/scripts/"
  cp "$REPO_ROOT/scripts/lib/service-restart.sh" "$REPO_ROOT/scripts/lib/update-build.sh" \
     "$REPO_ROOT/scripts/lib/autoship-pin.sh" "$TMP/work/scripts/lib/"
  printf 'import sys; sys.exit(0)\n' > "$TMP/work/scripts/pdf-runtime.py"
  printf '#!/bin/sh\nexec ./target/release/augmentagent "$@"\n' > "$TMP/work/scripts/run-rs.sh"
  # Provisioning is tested on its own (autopr_worker_install_test.py); here
  # it records what the updater asked of it.
  printf '#!/bin/sh\necho "ensure $(cat "${XDG_CONFIG_HOME}/augmentagent/autoship-pin" 2>/dev/null)" >> "$STUB_DIR/ensure.calls"\n' \
    > "$TMP/work/scripts/ensure-autopr-worker.sh"
  chmod +x "$TMP/work/scripts/"*.sh
  echo base > "$TMP/work/crates/x.rs"
  git -C "$TMP/work" add -A
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qm base
  git -C "$TMP/work" branch -q -M main 2>/dev/null || true
  git -C "$TMP/work" push -q origin HEAD:main
  BASE=$(git -C "$TMP/work" rev-parse HEAD)

  # The deploy build: a CLI that offers the worker, and the memory server.
  # Anything else fails, as an older CLI without `disk build` does, so the
  # build falls back to the cargo stub.
  printf '#!/bin/sh\n# deploy build\ncase "$1 $2" in\n"auto-pr-worker --help") echo "Usage: augmentagent auto-pr-worker" ;;\n"deploy prune") ;;\n*) exit 2 ;;\nesac\n' \
    > "$TMP/work/target/release/augmentagent"
  chmod 755 "$TMP/work/target/release/augmentagent"
  cp -p "$TMP/work/target/release/augmentagent" "$TMP/work/target/release/augmentagent-mcp-memory"

  mkdir -p "$TMP/bin" "$TMP/state/augmentagent" "$TMP/proc" "$TMP/config"
  # Cargo records where it built. With a target dir (the pinned build) it
  # writes fresh executables there that name the source they came from.
  cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
echo "build cwd=$PWD target=${CARGO_TARGET_DIR:-}" >> "$STUB_DIR/cargo.calls"
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  mkdir -p "$CARGO_TARGET_DIR/release"
  for name in augmentagent augmentagent-mcp-memory; do
    rm -f "$CARGO_TARGET_DIR/release/$name"
    printf '#!/bin/sh\n# pinned build of %s\n' "$(cat crates/x.rs)" > "$CARGO_TARGET_DIR/release/$name"
    chmod 755 "$CARGO_TARGET_DIR/release/$name"
  done
fi
exit 0
STUB
  printf '#!/bin/sh\nexit 0\n' > "$TMP/bin/npm"
  cat > "$TMP/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
d="$STUB_DIR"
prop=""
for a in "$@"; do
  case "$a" in
    list-unit-files) mode=list ;; cat) mode=cat ;; show) mode=show ;;
    restart) mode=restart ;; is-active) mode=active ;; start) mode=start ;;
    MainPID|ExecStart|DropInPaths) prop="$a" ;;
  esac
done
pidfile="$d/mainpid"
[ -s "$pidfile" ] || echo 100 > "$pidfile"
case "${mode:-}" in
  cat)     exit 1 ;;
  list)    case "$*" in *augmentagent-tenant-*) ;; *) echo "augmentagent.service enabled enabled" ;; esac; exit 0 ;;
  show)
    case "$prop" in
      MainPID)     cat "$pidfile" ;;
      ExecStart)   printf '{ path=%s ; argv[]=%s serve --dry-run false ; ignore_errors=no ; }\n' "$d/work/scripts/run-rs.sh" "$d/work/scripts/run-rs.sh" ;;
      DropInPaths) ;;
      *)           cat "$pidfile" ;;
    esac
    exit 0 ;;
  restart|start)
    echo "$*" >> "$d/restart.calls"
    next=$(( $(cat "$pidfile") + 1 )); echo "$next" > "$pidfile"
    mkdir -p "$d/proc/$next"; ln -sfn "$d/work/target/release/augmentagent" "$d/proc/$next/exe"
    exit 0 ;;
  active)  exit 0 ;;
esac
exit 0
STUB
  printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/bin/curl"
  printf '#!/usr/bin/env bash\necho Linux\n' > "$TMP/bin/uname"
  chmod +x "$TMP/bin/"*
  STUB_DIR="$TMP"; export STUB_DIR
  : > "$TMP/cargo.calls"; : > "$TMP/restart.calls"; : > "$TMP/ensure.calls"
  MEMINFO="$TMP/meminfo"
  printf 'MemTotal:       16000000 kB\nMemAvailable:    8000000 kB\nSwapFree:        4000000 kB\n' > "$MEMINFO"
  PIN_FILE="$TMP/config/augmentagent/autoship-pin"
  PIN_STAMP="$TMP/state/augmentagent/autoship-pinned-built"
  BUILT_STAMP="$TMP/state/augmentagent/built-commit"
  PIN_ROOT="$TMP/pinroot"
  PINNED_BIN="$PIN_ROOT/bin/augmentagent"
  LOG_FILE="$TMP/state/augmentagent/update.log"
}

# Land a commit on origin/main touching crates/ (a Rust rebuild + restart).
merge_to_main() {
  local clone="$TMP/other"
  [ -d "$clone" ] || git clone -q "$TMP/origin.git" "$clone" 2>/dev/null
  git -C "$clone" pull -q origin main 2>/dev/null
  echo "$1" > "$clone/crates/x.rs"
  git -C "$clone" -c user.email=t@e -c user.name=t commit -qam "$1"
  git -C "$clone" push -q origin HEAD:main
  git -C "$clone" rev-parse HEAD
}

run_env() {
  env PATH="$TMP/bin:$PATH" XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config" HOME="$TMP" \
    AUGMENTAGENT_AUTOSHIP_PIN=on AUGMENTAGENT_AUTOSHIP_PIN_ROOT="$PIN_ROOT" \
    AUGMENTAGENT_MEMINFO_PATH="$MEMINFO" AUGMENTAGENT_PROC_ROOT="$TMP/proc" \
    AUGMENTAGENT_EXE_SETTLE_SECS=0 AUGMENTAGENT_RESTART_POLL_SECS=0.1 \
    AUGMENTAGENT_RESTART_FORCE=1 "$@"
}
run_updater() { ( cd "$TMP/work" && run_env ./scripts/check-for-updates.sh >/dev/null 2>&1 ); }
pin_cli() { ( cd "$TMP/work" && run_env ./scripts/autoship-pin.sh "$@" >/dev/null 2>&1 ); }

count() { local n; n=$(grep -c -- "$1" "$2" 2>/dev/null); printf '%s' "${n:-0}"; }
inode() { ls -i "$1" 2>/dev/null | awk '{print $1}'; }
pinned_builds() { count "target=$PIN_ROOT/target" "$TMP/cargo.calls"; }
worker_restarts() { count "augmentagent-autopr" "$TMP/restart.calls"; }

echo "check-for-updates.sh with a pinned auto-ship worker (#2468):"
make_case

# --- first deploy, no pin: bootstrap onto exactly this deploy's build -------
FIRST=$(merge_to_main first)
run_updater
check "first deploy: the daemon deploys as before"         "$(cat "$BUILT_STAMP" 2>/dev/null)" "$FIRST"
check "first deploy: pins the build this deploy installs"  "$(tr -d '[:space:]' < "$PIN_FILE" 2>/dev/null)" "$FIRST"
check "first deploy: records the installed pinned build"   "$(tr -d '[:space:]' < "$PIN_STAMP" 2>/dev/null)" "$FIRST"
if cmp -s "$PINNED_BIN" "$TMP/work/target/release/augmentagent"; then ok "first deploy: installs the deploy binary as the pinned build"
else bad "first deploy: installs the deploy binary as the pinned build" "$(ls -l "$PIN_ROOT/bin" 2>&1)"; fi
check "first deploy: no second compile"                    "$(pinned_builds)" 0
check "first deploy: one ordinary deploy build"            "$(count build "$TMP/cargo.calls")" 1
check "first deploy: logs the bootstrap"                   "$(count "AUTOSHIP PIN BOOTSTRAP: no pin recorded; pinned the worker to $FIRST" "$LOG_FILE")" 1
check "first deploy: never restarts the worker unit"       "$(worker_restarts)" 0

# --- an unrelated merge: the daemon redeploys, the worker does not move -----
INODE=$(inode "$PINNED_BIN"); DAEMON_RESTARTS=$(count "augmentagent.service" "$TMP/restart.calls")
SECOND=$(merge_to_main second)
run_updater
check "unrelated merge: the daemon deploys it"             "$(cat "$BUILT_STAMP")" "$SECOND"
[ "$(count "augmentagent.service" "$TMP/restart.calls")" -gt "$DAEMON_RESTARTS" ] \
  && ok "unrelated merge: the daemon is restarted onto it" \
  || bad "unrelated merge: the daemon is restarted onto it" "$(cat "$TMP/restart.calls")"
check "unrelated merge: pinned binary inode unchanged (no worker reload)" "$(inode "$PINNED_BIN")" "$INODE"
check "unrelated merge: pinned stamp unchanged (no worker reload)"        "$(tr -d '[:space:]' < "$PIN_STAMP")" "$FIRST"
check "unrelated merge: pin unchanged"                     "$(tr -d '[:space:]' < "$PIN_FILE")" "$FIRST"
check "unrelated merge: no pinned rebuild"                 "$(pinned_builds)" 0
check "unrelated merge: never restarts the worker unit"    "$(worker_restarts)" 0

# --- moving the pin: one rebuild of that commit, installed, re-provisioned --
pin_cli "$BASE" --no-apply
check "pin CLI: records the requested commit"              "$(tr -d '[:space:]' < "$PIN_FILE")" "$BASE"
check "pin CLI: --no-apply leaves the installed build"     "$(tr -d '[:space:]' < "$PIN_STAMP")" "$FIRST"
: > "$TMP/ensure.calls"
run_updater
check "pin moved: builds the pinned commit once"           "$(pinned_builds)" 1
check "pin moved: builds from the exported pinned source"  "$(count "cwd=$PIN_ROOT/src target=$PIN_ROOT/target" "$TMP/cargo.calls")" 1
check "pin moved: installs that build"                     "$(sed -n 2p "$PINNED_BIN" 2>/dev/null)" "# pinned build of base"
[ "$(inode "$PINNED_BIN")" != "$INODE" ] && ok "pin moved: new executable inode (the worker reloads)" \
                                        || bad "pin moved: new executable inode (the worker reloads)" "inode still $INODE"
check "pin moved: stamp names the new pin"                 "$(tr -d '[:space:]' < "$PIN_STAMP")" "$BASE"
check "pin moved: worker re-provisioned"                   "$(count "ensure $BASE" "$TMP/ensure.calls")" 1
check "pin moved: main deploy untouched"                   "$(cat "$BUILT_STAMP")" "$SECOND"
check "pin moved: the worker reloads itself; no unit restart" "$(worker_restarts)" 0

# --- same pin again: nothing ------------------------------------------------
INODE=$(inode "$PINNED_BIN"); : > "$TMP/ensure.calls"
run_updater
check "unchanged pin: no rebuild"                          "$(pinned_builds)" 1
check "unchanged pin: binary inode unchanged"              "$(inode "$PINNED_BIN")" "$INODE"
check "unchanged pin: no re-provisioning"                  "$(count ensure "$TMP/ensure.calls")" 0

# --- a pin that is not on origin/main is refused ----------------------------
git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -q --allow-empty -m local-only
LOCAL_ONLY=$(git -C "$TMP/work" rev-parse HEAD)
pin_cli "$LOCAL_ONLY" --no-apply; rc=$?
[ "$rc" -ne 0 ] && ok "pin CLI: refuses a commit that is not on origin/main" || bad "pin CLI: refuses a commit that is not on origin/main" "exit 0"
check "pin CLI: refusal leaves the pin"                    "$(tr -d '[:space:]' < "$PIN_FILE")" "$BASE"
git -C "$TMP/work" reset -q --hard "$SECOND"

# --- an invalid pin changes nothing -----------------------------------------
printf 'not-a-sha\n' > "$PIN_FILE"
run_updater
check "invalid pin: no rebuild"                            "$(pinned_builds)" 1
check "invalid pin: binary inode unchanged"                "$(inode "$PINNED_BIN")" "$INODE"
check "invalid pin: logged"                                "$(count "AUTOSHIP PIN INVALID" "$LOG_FILE")" 1

# --- unpin: back onto main, after the unit is re-provisioned ----------------
pin_cli main --no-apply
: > "$TMP/ensure.calls"
run_updater
check "unpin: re-provisions the worker"                    "$(count "ensure main" "$TMP/ensure.calls")" 1
check "unpin: stamp tells the pinned worker to reload"     "$(tr -d '[:space:]' < "$PIN_STAMP")" main
check "unpin: logged"                                      "$(count "AUTOSHIP UNPINNED" "$LOG_FILE")" 1
: > "$TMP/ensure.calls"
run_updater
check "unpinned: a further tick does nothing"              "$(count ensure "$TMP/ensure.calls")" 0
rm -rf "$TMP"

# --- off by default where it is not the deploy host's platform --------------
make_case
printf '#!/usr/bin/env bash\necho Darwin\n' > "$TMP/bin/uname"
FIRST=$(merge_to_main first)
( cd "$TMP/work" && env PATH="$TMP/bin:$PATH" XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config" HOME="$TMP" \
    AUGMENTAGENT_AUTOSHIP_PIN_ROOT="$PIN_ROOT" AUGMENTAGENT_MEMINFO_PATH="$MEMINFO" \
    ./scripts/check-for-updates.sh >/dev/null 2>&1 )
[ -e "$PIN_FILE" ] && bad "non-Linux: no pin is bootstrapped" "$(cat "$PIN_FILE")" || ok "non-Linux: no pin is bootstrapped"
rm -rf "$TMP"

echo
echo "updater-autoship-pin: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
