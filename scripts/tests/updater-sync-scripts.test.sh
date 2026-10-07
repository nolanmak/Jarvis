#!/usr/bin/env bash
# The cron data-sync jobs run their Python straight out of this checkout
# (`scripts/apple-notes/sync.py`), so a merged fix only reaches them once the
# updater pulls. Pulling is not enough on its own: nothing in the checkout tells
# the operator a sync job's code moved, and these jobs are scheduled outside the
# daemon the updater restarts.
#
# A change under scripts/apple-notes/ or scripts/imessage/ must therefore be
# REPORTED by the updater, and a change that touches neither must not be — most
# pushes to main touch neither, and a notice on every push is noise that gets
# ignored.
#
# Drives the real scripts/check-for-updates.sh against a throwaway repo.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# One throwaway repo, one commit behind origin, where the newer commit changes
# exactly <path>. Echoes nothing; sets TMP.
make_case() {
  local path="$1"
  TMP=$(mktemp -d)
  git init -q --bare "$TMP/origin.git"
  git clone -q "$TMP/origin.git" "$TMP/work" 2>/dev/null
  mkdir -p "$TMP/work/scripts/lib" "$(dirname "$TMP/work/$path")"
  cp "$REPO_ROOT/scripts/check-for-updates.sh" "$TMP/work/scripts/"
  printf 'import os, sys; sys.exit(int(os.environ.get("PDF_INSTALL_FAIL", "0")))\n' > "$TMP/work/scripts/pdf-runtime.py"
  cp "$REPO_ROOT/scripts/lib/service-restart.sh" "$TMP/work/scripts/lib/"
  echo "# base" > "$TMP/work/$path"
  git -C "$TMP/work" add -A
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qm base
  git -C "$TMP/work" push -q origin HEAD:main
  git -C "$TMP/work" branch -q -M main 2>/dev/null || true
  echo "# newer" > "$TMP/work/$path"
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qam newer
  git -C "$TMP/work" push -q origin main
  git -C "$TMP/work" reset -q --hard HEAD~1

  mkdir -p "$TMP/bin" "$TMP/state"
  for stub in cargo npm; do
    # shellcheck disable=SC2016  # $* and $STUB_DIR must reach the stub unexpanded
    printf '#!/usr/bin/env bash\necho "$*" >> "$STUB_DIR/%s-calls"\nexit 0\n' "$stub" > "$TMP/bin/$stub"
    chmod +x "$TMP/bin/$stub"
  done
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
  list)    printf "%s\n" "augmentagent.service enabled enabled"; exit 0 ;;
  show)    cat "$pidfile"; exit 0 ;;
  restart) echo $(( $(cat "$pidfile") + 1 )) > "$pidfile"; exit 0 ;;
  active)  exit 0 ;;
esac
exit 0
STUB
  chmod +x "$TMP/bin/systemctl"
  printf '#!/usr/bin/env bash\necho Linux\n' > "$TMP/bin/uname" && chmod +x "$TMP/bin/uname"
}

# log() appends to $LOG_DIR/update.log rather than stdout, so the log file is
# the operator-visible channel this asserts on.
run_updater() {
  ( cd "$TMP/work" \
    && env -u DISCORD_WEBHOOK_URL PATH="$TMP/bin:$PATH" STUB_DIR="$TMP" \
       XDG_STATE_HOME="$TMP/state" HOME="$TMP" AUGMENTAGENT_RESTART_FORCE=1 \
       ./scripts/check-for-updates.sh >/dev/null 2>&1 )
  cat "$TMP/state/augmentagent/update.log" 2>/dev/null
}

expect_reported() {
  make_case "$1"
  if run_updater | grep -q 'data sync script'; then
    ok "reports the change when $1 moves"
  else
    bad "reports the change when $1 moves" \
        "no 'data sync script' line; a merged sync fix lands silently and the cron job keeps running old code"
  fi
  rm -rf "$TMP"
}

expect_silent() {
  make_case "$1"
  if run_updater | grep -q 'data sync script'; then
    bad "stays silent when only $1 moves" \
        "reported a sync-script change for a path that is not one; every push to main would notify"
  else
    ok "stays silent when only $1 moves"
  fi
  rm -rf "$TMP"
}

# The reporting path must survive `set -euo pipefail`. A call to a function that
# does not exist returns 127 and aborts the script mid-run, silently skipping
# everything after it — including the auto-register block. Asserting on the log
# alone misses this, because the log line is written BEFORE the notify call.
expect_completes() {
  make_case scripts/apple-notes/sync.py
  ( cd "$TMP/work" \
    && env -u DISCORD_WEBHOOK_URL PATH="$TMP/bin:$PATH" STUB_DIR="$TMP" \
       XDG_STATE_HOME="$TMP/state" HOME="$TMP" AUGMENTAGENT_RESTART_FORCE=1 \
       ./scripts/check-for-updates.sh >/dev/null 2>&1 )
  local status=$?
  if [ "$status" -eq 0 ]; then
    ok "finishes the run cleanly after reporting a sync-script change"
  else
    bad "finishes the run cleanly after reporting a sync-script change" \
        "exit $status; a call to an undefined function returns 127 and aborts under set -e"
  fi
  rm -rf "$TMP"
}

# Every function the reporting path calls must actually be defined in the script.
expect_functions_defined() {
  local missing=""
  for fn in log notify_owner; do
    grep -qE "^${fn}\(\)" "$REPO_ROOT/scripts/check-for-updates.sh" || missing="$missing $fn"
  done
  if [ -z "$missing" ]; then
    ok "the reporting path's helper functions are defined"
  else
    bad "the reporting path's helper functions are defined" "undefined:$missing"
  fi
}

echo "check-for-updates.sh data-sync-script reporting:"

# Every script the cron jobs execute, directly or by import.
expect_reported scripts/apple-notes/sync.py
expect_reported scripts/apple-notes/apple_notes_sync.py
expect_reported scripts/apple-notes/scrub.py
expect_reported scripts/imessage/sync.py
expect_reported scripts/imessage/imessage_sync.py

# Paths that must not trigger it: the common case of a push to main.
expect_silent crates/augmentagent-channel-core/src/lib.rs
expect_silent docs/IMESSAGE.md
expect_silent src/dashboard.ts

expect_completes
expect_functions_defined

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
