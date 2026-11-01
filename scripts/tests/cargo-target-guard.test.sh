#!/usr/bin/env bash
# #1754: the Claude Code PreToolUse guard blocks Cargo builds that pick their
# own target dir outside the managed wrapper, so the cache is registered and
# housekeeping can see it. Drives the real script with synthetic events.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GUARD="$REPO_ROOT/scripts/cargo-target-guard.py"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

run() { # run <command-string> -> prints guard stdout
  jq -n --arg c "$1" '{tool_name:"Bash",tool_input:{command:$c}}' | "$GUARD"
}

expect_block() {
  local out; out=$(run "$1"); local code=$?
  if [ $code -ne 0 ]; then bad "block: $1" "exit $code"; return; fi
  if printf '%s' "$out" | jq -e '.decision == "block" and (.reason | contains("scripts/cargo-managed.sh"))' >/dev/null 2>&1; then
    ok "block: $1"
  else
    bad "block: $1" "got: ${out:-<empty>}"
  fi
}

expect_allow() {
  local out; out=$(run "$1"); local code=$?
  if [ $code -eq 0 ] && [ -z "$out" ]; then ok "allow: $1"; else bad "allow: $1" "exit $code, got: $out"; fi
}

echo "blocked (a bypass of the managed wrapper):"
expect_block 'CARGO_TARGET_DIR=/x cargo build'
expect_block 'CARGO_TARGET_DIR="/mnt/build/my target" cargo test -p augmentagent-cli'
expect_block 'CARGO_BUILD_TARGET_DIR=/x cargo build'
expect_block 'env CARGO_TARGET_DIR=/x cargo build --release'
expect_block 'cargo test --target-dir /x'
expect_block 'cargo build --target-dir=/x'
expect_block '. "$HOME/.cargo/env" && cd w && CARGO_TARGET_DIR=/x cargo build'
expect_block 'cargo fmt; CARGO_TARGET_DIR=/x cargo clippy 2>&1 | tail -5'
expect_block 'scripts/cargo-managed.sh test --target-dir /x'
expect_block './target/release/augmentagent disk build --target-dir /a -- cargo build --target-dir=/b'
expect_block $'echo start\nCARGO_TARGET_DIR=/x cargo build'

echo "allowed:"
expect_allow './scripts/cargo-managed.sh test -p augmentagent-cli'
expect_allow 'CARGO_TARGET_DIR=/mnt/build/x ./scripts/cargo-managed.sh build --release'
expect_allow "bash $REPO_ROOT/scripts/cargo-managed.sh test"
expect_allow 'augmentagent disk build --target-dir /x -- cargo build'
expect_allow 'cargo build --release'
expect_allow 'cargo run -p augmentagent-cli -- --target-dir /x'
expect_allow 'echo $CARGO_TARGET_DIR'
expect_allow 'export CARGO_TARGET_DIR=/x'
expect_allow 'grep -rn CARGO_TARGET_DIR= scripts/'
expect_allow 'AUGMENTAGENT_ALLOW_UNMANAGED_TARGET=1 CARGO_TARGET_DIR=/x cargo build'
expect_allow 'echo "unbalanced'
expect_allow 'git status'

echo "protocol:"
out=$(printf '{"tool_name":"Read","tool_input":{"file_path":"/x"}}' | "$GUARD"); [ -z "$out" ] && ok "non-Bash tool passes" || bad "non-Bash tool passes" "$out"
out=$(printf 'not json' | "$GUARD"); code=$?; [ $code -eq 0 ] && [ -z "$out" ] && ok "malformed input fails open" || bad "malformed input fails open" "exit $code $out"
jq -e '[.hooks.PreToolUse[] | select(.matcher=="Bash") | .hooks[].command] | any(contains("scripts/cargo-target-guard.py"))' \
  "$REPO_ROOT/.claude/settings.json" >/dev/null && ok "settings.json registers the guard" || bad "settings.json registers the guard"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
