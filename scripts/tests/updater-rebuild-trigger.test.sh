#!/usr/bin/env bash
# The auto-updater must rebuild the Rust binaries whenever a change alters
# what is compiled into them, not only when crates/ or Cargo files change.
#
# Files outside crates/ are pulled into the binary with include_str!/
# include_bytes!: agent prompts under schema/, the .env.example key list, and
# the Codex tool bridge, command sandbox, build VM runner, dependency gateway
# and provider supervisor scripts. Before this test, a PR touching only those
# merged cleanly but never deployed: the updater saw no crates/ change, skipped
# the build, advanced the stamp, and the daemon kept the old embedded copy.
#
# Drives the real scripts/check-for-updates.sh against a throwaway repo with a
# stubbed cargo that records whether it was asked to build.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# include targets that are only referenced from test code: changing them does
# not change a production binary, so they must not cost a release rebuild.
# A new include outside crates/ must be classified here or trigger a rebuild.
TEST_ONLY_EMBEDS=(
  docs/reasoner-capabilities.json
  eval/autopr-cases.json
  eval/last-run.json
  scripts/tests/fixtures/scoped-document.pdf
  skills/email-triage/SKILL.md
)

# One throwaway repo, one commit behind origin, where the newer commit changes
# exactly <path>. Echoes nothing; sets TMP.
make_case() {
  local path="$1"
  TMP=$(mktemp -d)
  git init -q --bare "$TMP/origin.git"
  git clone -q "$TMP/origin.git" "$TMP/work" 2>/dev/null
  mkdir -p "$TMP/work/scripts/lib" "$(dirname "$TMP/work/$path")"
  cp "$REPO_ROOT/scripts/check-for-updates.sh" "$TMP/work/scripts/"
  cp "$REPO_ROOT/scripts/lib/service-restart.sh" "$TMP/work/scripts/lib/"
  echo base > "$TMP/work/$path"
  git -C "$TMP/work" add -A
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qm base
  git -C "$TMP/work" push -q origin HEAD:main
  git -C "$TMP/work" branch -q -M main 2>/dev/null || true
  echo newer > "$TMP/work/$path"
  git -C "$TMP/work" -c user.email=t@e -c user.name=t commit -qam newer
  git -C "$TMP/work" push -q origin main
  git -C "$TMP/work" reset -q --hard HEAD~1

  mkdir -p "$TMP/bin" "$TMP/state"
  cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$STUB_DIR/cargo-calls"
exit 0
STUB
  cat > "$TMP/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
d="$STUB_DIR"
echo "$*" >> "$d/service-calls"
for a in "$@"; do
  case "$a" in
    list-unit-files) mode=list ;; show) mode=show ;;
    restart) mode=restart ;; is-active) mode=active ;;
  esac
done
pidfile="$d/mainpid"
[ -s "$pidfile" ] || echo 100 > "$pidfile"
case "${mode:-}" in
  list)    printf "%s\n" "augmentagent.service enabled enabled" "augmentagent-computer-use.service enabled enabled"; exit 0 ;;
  show)    cat "$pidfile"; exit 0 ;;
  restart) echo $(( $(cat "$pidfile") + 1 )) > "$pidfile"; exit 0 ;;
  active)  exit 0 ;;
esac
exit 0
STUB
  cat > "$TMP/bin/npm" <<'STUB'
#!/usr/bin/env bash
echo "$PWD $*" >> "$STUB_DIR/npm-calls"
exit 0
STUB
  chmod +x "$TMP/bin/cargo" "$TMP/bin/systemctl" "$TMP/bin/npm"
  # Exercise the Linux (systemd) branch on any host (#1079: CI runs macOS too).
  printf '#!/usr/bin/env bash\necho Linux\n' > "$TMP/bin/uname" && chmod +x "$TMP/bin/uname"
}

# Exit 0 when the updater invoked `cargo build`, 1 otherwise.
updater_rebuilt() {
  ( cd "$TMP/work" \
    && env -u DISCORD_WEBHOOK_URL PATH="$TMP/bin:$PATH" STUB_DIR="$TMP" \
       XDG_STATE_HOME="$TMP/state" HOME="$TMP" AUGMENTAGENT_RESTART_FORCE=1 \
       ./scripts/check-for-updates.sh >/dev/null 2>&1 )
  grep -q '^build ' "$TMP/cargo-calls" 2>/dev/null
}

expect_rebuild() {
  make_case "$1"
  if updater_rebuilt; then ok "rebuilds when $1 changes"
  else bad "rebuilds when $1 changes" "cargo build was not invoked; the daemon would keep the stale embedded copy"; fi
  rm -rf "$TMP"
}

expect_no_rebuild() {
  make_case "$1"
  if updater_rebuilt; then bad "skips the rebuild when only $1 changes" "cargo build was invoked for a file no binary embeds"
  else ok "skips the rebuild when only $1 changes"; fi
  rm -rf "$TMP"
}

echo "check-for-updates.sh rebuild trigger:"

# Control: the pre-existing trigger still fires.
expect_rebuild crates/augmentagent-channel-core/src/lib.rs

# Compile-embedded production files outside crates/.
expect_rebuild scripts/codex-tool-bridge.py
expect_rebuild scripts/codex-command-sandbox.py
expect_rebuild scripts/codex-build-vm.py
expect_rebuild scripts/build-dependency-proxy.py
expect_rebuild scripts/provider-supervisor.py
expect_rebuild schema/wiki-ask.md
expect_rebuild .env.example

# Not embedded: docs, other scripts, test-only includes.
expect_no_rebuild docs/BUILD-VM.md
expect_no_rebuild scripts/check-no-personal-data.sh
expect_no_rebuild eval/last-run.json

# Drift guard: every include_str!/include_bytes! target outside crates/ in the
# real tree is either classified test-only above or triggers a rebuild.
make_case sidecars/computer-use/server.mjs
updater_rebuilt || true
if grep -q 'sidecars/computer-use ci' "$TMP/npm-calls" 2>/dev/null && grep -q 'restart augmentagent-computer-use.service' "$TMP/service-calls"; then
  ok "deploys dependencies and restarts browser worker on sidecar changes"
else
  bad "deploys dependencies and restarts browser worker on sidecar changes" "sidecar would keep stale code/dependencies"
fi
rm -rf "$TMP"
echo "include drift guard:"
mapfile -t EMBEDS < <(
  cd "$REPO_ROOT" && grep -rnoE 'include_(str|bytes)!\("[^"]*"\)' crates/ \
    | while IFS=: read -r file _line match; do
        rel=$(printf '%s' "$match" | sed -E 's/^include_(str|bytes)!\("//; s/"\)$//')
        realpath -m --relative-to=. "$(dirname "$file")/$rel"
      done | grep -v '^crates/' | sort -u
)
[ "${#EMBEDS[@]}" -gt 0 ] && ok "found ${#EMBEDS[@]} embedded files outside crates/" \
                          || bad "found embedded files outside crates/" "grep returned nothing; the guard would pass vacuously"
for path in "${EMBEDS[@]}"; do
  skip=0
  for t in "${TEST_ONLY_EMBEDS[@]}"; do [ "$t" = "$path" ] && skip=1; done
  [ "$skip" = 1 ] && continue
  expect_rebuild "$path"
done
for t in "${TEST_ONLY_EMBEDS[@]}"; do
  printf '%s\n' "${EMBEDS[@]}" | grep -qxF "$t" \
    && ok "test-only entry $t is still an include target" \
    || bad "test-only entry $t is still an include target" "remove it from TEST_ONLY_EMBEDS"
done

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
