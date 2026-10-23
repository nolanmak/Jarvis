#!/usr/bin/env bash
# #826 integration test: the build stamp must NOT advance when the daemon
# did not verifiably restart.
#
# This drives the real scripts/check-for-updates.sh against a throwaway git
# repo with stubbed `cargo` and `systemctl`, because the bug was not in any
# one function — it was that the stamp write sat downstream of a restart
# branch that could be skipped without anyone noticing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# Build a repo that is one commit behind its origin, with the change touching
# crates/ so NEEDS_REBUILD fires.
make_case() {
  TMP=$(mktemp -d)
  export TMP
  git init -q --bare "$TMP/origin.git"
  git clone -q "$TMP/origin.git" "$TMP/work" 2>/dev/null
  mkdir -p "$TMP/work/scripts/lib" "$TMP/work/crates"
  cp "$REPO_ROOT/scripts/check-for-updates.sh" "$TMP/work/scripts/"
  printf 'import os, sys; sys.exit(int(os.environ.get("PDF_INSTALL_FAIL", "0")))\n' > "$TMP/work/scripts/pdf-runtime.py"
  cp "$REPO_ROOT/scripts/lib/service-restart.sh" "$TMP/work/scripts/lib/"
  echo base > "$TMP/work/crates/x.rs"
  git -C "$TMP/work" add -A
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qm base
  git -C "$TMP/work" push -q origin HEAD:main
  git -C "$TMP/work" branch -q -M main 2>/dev/null || true
  # A newer commit on origin that touches crates/.
  echo newer > "$TMP/work/crates/x.rs"
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qam newer
  git -C "$TMP/work" push -q origin main
  git -C "$TMP/work" reset -q --hard HEAD~1     # local is now behind origin/main

  # Stubs.
  mkdir -p "$TMP/bin" "$TMP/state"
  cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
exit 0
STUB
  cat > "$TMP/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
d="$SYSTEMCTL_STUB_DIR"
for a in "$@"; do
  case "$a" in
    list-unit-files) mode=list ;; cat) mode=cat ;; show) mode=show ;;
    restart) mode=restart ;; is-active) mode=active ;;
  esac
done
pidfile="$d/mainpid"
[ -s "$pidfile" ] || echo 100 > "$pidfile"
case "${mode:-}" in
  # Optional sidecars are not installed in this fixture. The updater checks
  # `systemctl --user cat` before rebuilding one.
  cat)     exit 1 ;;
  list)    echo "augmentagent.service enabled enabled"; exit 0 ;;
  show)    cat "$pidfile"; exit 0 ;;
  # BOUNCES=1 models a real restart (new MainPID); BOUNCES=0 models the
  # 2026-08-27 failure, where the command returns 0 but nothing came back.
  restart) [ "${BOUNCES:-1}" = 1 ] && echo $(( $(cat "$pidfile") + 1 )) > "$pidfile"; exit 0 ;;
  active)  exit 0 ;;
esac
exit 0
STUB
  chmod +x "$TMP/bin/cargo" "$TMP/bin/systemctl"
  # The built daemon: records what the updater asks it to do.
  mkdir -p "$TMP/work/target/release"
  cat > "$TMP/work/target/release/augmentagent" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$SYSTEMCTL_STUB_DIR/daemon-calls"
STUB
  chmod +x "$TMP/work/target/release/augmentagent"
  # Exercise the Linux (systemd) branch on any host (#1079: CI runs macOS too).
  printf '#!/usr/bin/env bash\necho Linux\n' > "$TMP/bin/uname" && chmod +x "$TMP/bin/uname"
  SYSTEMCTL_STUB_DIR="$TMP"; export SYSTEMCTL_STUB_DIR
  STAMP_FILE="$TMP/state/augmentagent/built-commit"
}

run_updater() {
  ( cd "$TMP/work" \
    && PATH="$TMP/bin:$PATH" XDG_STATE_HOME="$TMP/state" HOME="$TMP" BOUNCES="$1" \
       ./scripts/check-for-updates.sh >/dev/null 2>&1 )
}

echo "check-for-updates.sh stamp policy:"

# A renderer install failure must stop deployment before restart/stamping.
make_case
export PDF_INSTALL_FAIL=1
run_updater 1; rc=$?
unset PDF_INSTALL_FAIL
[ "$rc" -ne 0 ] && ok "PDF provisioning failure stops deployment" \
                || bad "PDF provisioning failure stops deployment"
[ ! -s "$STAMP_FILE" ] && ok "PDF failure withholds build stamp" \
                       || bad "PDF failure withholds build stamp"
[ ! -f "$TMP/mainpid" ] && ok "PDF failure does not restart daemon" \
                       || bad "PDF failure does not restart daemon"
grep -q "PDF RUNTIME INSTALL FAILED" "$TMP/state/augmentagent/update.log" \
  && ok "PDF failure is actionable in update log" || bad "PDF failure is actionable in update log"
rm -rf "$TMP"

# --- the 2026-08-27 failure: restart runs but the process never bounces ---
make_case
run_updater 0; rc=$?
[ "$rc" -ne 0 ] && ok "exits non-zero when the daemon did not verifiably restart" \
                || bad "exits non-zero when the daemon did not verifiably restart" "exit was $rc"
if [ -s "$STAMP_FILE" ]; then
  bad "withholds the build stamp after an unverified restart" \
      "stamp was written: $(cat "$STAMP_FILE")"
else
  ok "withholds the build stamp after an unverified restart"
fi
grep -q "NOT writing the build stamp" "$TMP/state/augmentagent/update.log" \
  && ok "says why in the log" || bad "says why in the log" "no explanation logged"
grep -qx "deploy prune" "$TMP/daemon-calls" 2>/dev/null \
  && bad "keeps rollback material after an unverified restart (#1410)" "deploy prune ran" \
  || ok "keeps rollback material after an unverified restart (#1410)"

# The retry path: with the stamp absent, the next tick must try again rather
# than reporting `up to date`. This is the property the old code destroyed.
run_updater 1 >/dev/null 2>&1; retry_rc=$?
if [ -s "$STAMP_FILE" ]; then
  ok "a later tick recovers and writes the stamp once the restart works"
else
  bad "a later tick recovers and writes the stamp once the restart works" \
    "stamp still absent; retry exit $retry_rc; $(tail -n 8 "$TMP/state/augmentagent/update.log")"
fi
grep -qx "deploy prune" "$TMP/daemon-calls" 2>/dev/null \
  && ok "prunes old rollback material once the deploy is verified (#1410)" \
  || bad "prunes old rollback material once the deploy is verified (#1410)" "deploy prune never ran"
rm -rf "$TMP"

# --- healthy deploy is unchanged ---
make_case
run_updater 1; rc=$?
[ "$rc" -eq 0 ] && ok "exits zero on a healthy deploy" || bad "exits zero on a healthy deploy" "exit was $rc"
[ -s "$STAMP_FILE" ] && ok "writes the build stamp on a healthy deploy" \
                     || bad "writes the build stamp on a healthy deploy" "stamp absent"
rm -rf "$TMP"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
