#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
LOG="$TMP/update.log"
log() { printf '%s\n' "$*" >> "$LOG"; }
. "$ROOT/scripts/lib/update-build.sh"
cat > "$TMP/cli" <<'STUB'
#!/usr/bin/env bash
if [ "$*" = 'disk build --help' ]; then
  case "${FAKE_MODE:-new}" in
    new) echo '--update'; exit 0 ;;
    legacy) echo 'disk build'; exit 0 ;;
    none) exit 2 ;;
  esac
fi
printf '%s\n' "$*" >> "$FAKE_CALLS"
printf '%s\n' "${FAKE_OUTPUT:-build output}" >&2
exit "${FAKE_CODE:-0}"
STUB
cp "$TMP/cli" "$TMP/cargo"
chmod +x "$TMP/cli" "$TMP/cargo"
export FAKE_CALLS="$TMP/calls" PATH="$TMP:$PATH"
check() {
  local expected="$1" actual=0
  : > "$LOG"; : > "$FAKE_CALLS"
  if run_update_build "$TMP/cli" "$TMP/target"; then actual=0; else actual=$?; fi
  [ "$actual" -eq "$expected" ] || { cat "$LOG"; exit 1; }
}
export FAKE_MODE=new FAKE_CODE=75 FAKE_OUTPUT='storage busy: another writer'
check 75
grep -q 'update deferred: storage busy' "$LOG"
if grep -q 'RUST BUILD FAILED\|low disk' "$LOG"; then cat "$LOG"; exit 1; fi
grep -q 'disk build --update --target-dir' "$FAKE_CALLS"
export FAKE_CODE=73 FAKE_OUTPUT='insufficient headroom'
check 73; grep -q 'low disk space' "$LOG"
export FAKE_CODE=76 FAKE_OUTPUT='cannot inspect admission'
check 76; grep -q 'admission unavailable' "$LOG"
export FAKE_CODE=101 FAKE_OUTPUT='error: real compiler failure'
check 1; grep -q 'RUST BUILD FAILED' "$LOG"
export FAKE_CODE=0 FAKE_OUTPUT='finished release build'
check 0
export FAKE_MODE=legacy FAKE_CODE=1
export FAKE_OUTPUT=$'Error: gate refused: low disk: storage lease unavailable\n\nCaused by:\n    gate refused: low disk: build storage busy; retry admission'
check 75
export FAKE_OUTPUT=$'error: type mismatch\nsource: "build storage busy; retry admission"'
check 1; grep -q 'RUST BUILD FAILED' "$LOG"
export FAKE_MODE=none FAKE_CODE=0 FAKE_OUTPUT='bootstrap'
check 0; grep -q '^build --release ' "$FAKE_CALLS"
# The producer must repair cached output before CLI admission, and new
# Cargo output before any subsequent preflight/service launch.
mkdir -p "$TMP/target/release"
cat > "$TMP/target/release/augmentagent" <<'STUB'
#!/usr/bin/env bash
if (( (8#$(python3 -c 'import os,sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o777)[2:])' "$0") & 8#022) != 0 )); then
  echo unsafe-cli >> "$FAKE_CALLS"
fi
if [ "$*" = 'disk build --help' ]; then echo '--update'; exit 0; fi
chmod 775 "$(dirname "$0")/augmentagent" "$(dirname "$0")/augmentagent-mcp-memory"
STUB
cp "$TMP/target/release/augmentagent" "$TMP/target/release/augmentagent-mcp-memory"
chmod 775 "$TMP/target/release/"*
: > "$FAKE_CALLS"
run_update_build "$TMP/target/release/augmentagent" "$TMP/target"
! grep -q unsafe-cli "$FAKE_CALLS"
[ "$(python3 -c 'import os,sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o777)[2:])' "$TMP/target/release/augmentagent")" = 755 ]
[ "$(python3 -c 'import os,sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o777)[2:])' "$TMP/target/release/augmentagent-mcp-memory")" = 755 ]
echo '9 update build outcome and permission cases passed'
