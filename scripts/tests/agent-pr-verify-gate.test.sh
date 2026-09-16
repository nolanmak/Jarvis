#!/usr/bin/env bash
# #1048: the PR verification gate must demand a bridge-suite receipt when a PR
# changes the Codex enforcement scripts (bridge, command sandbox, build VM,
# dependency proxy, provider supervisor). CI never runs the real-VM tests and
# PRs are opened before CI reports, so the receipt records an enforcing local
# run (and the VM run where it applies).
#
# Drives the real scripts/agent-pr-verify-gate.sh with synthetic PreToolUse
# events against throwaway git repositories. No network, no real PRs.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GATE="$REPO_ROOT/scripts/agent-pr-verify-gate.sh"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# A developer's (or CI's) git environment must not leak into the fixtures.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE

g() { git -c user.name=Synthetic -c user.email=fixture@example.com \
        -c core.hooksPath=/dev/null -c commit.gpgsign=false "$@"; }

# make_repo <path...>: a repo whose `main` has a baseline and whose checked-out
# branch changes every given path. No `origin`, so the gate diffs against main.
# Called via $(...), so each repo gets its own mktemp dir (no shared counter)
# and git's chatter goes to stderr, never into the captured path.
make_repo() {
  local repo f
  repo=$(mktemp -d "$TMP/repo.XXXXXX")
  {
    g -C "$repo" init -q -b main
    echo baseline > "$repo/README.md"
    g -C "$repo" add README.md
    g -C "$repo" commit -qm baseline
    g -C "$repo" checkout -qb feature
    for f in "$@"; do
      mkdir -p "$repo/$(dirname "$f")"
      echo "synthetic change" >> "$repo/$f"
    done
    g -C "$repo" add -A
    g -C "$repo" commit -qm change
  } >&2
  printf '%s' "$repo"
}

# new_repo <path...>: sets $repo, aborting the whole test if the fixture is
# not a real repo under $TMP (so no later step writes relative to the cwd).
new_repo() {
  repo=$(make_repo "$@")
  if [[ "$repo" != "$TMP"/repo.* || ! -d "$repo/.git" ]]; then
    echo "fixture repository creation failed: $repo" >&2
    exit 1
  fi
}

# run_gate <repo> <command>: the gate's stdout for one Bash PreToolUse event.
run_gate() {
  jq -cn --arg c "$2" '{tool_name:"Bash", tool_input:{command:$c}}' \
    | (cd "$1" && bash "$GATE")
}

is_block() {
  printf '%s' "$1" | jq -e 'try (.decision == "block") catch false' >/dev/null 2>&1
}

reason() { printf '%s' "$1" | jq -r '.reason // empty' 2>/dev/null; }

PR='gh pr create --title synthetic --body synthetic'

BRIDGE_SCRIPTS=(
  scripts/codex-tool-bridge.py
  scripts/codex-command-sandbox.py
  scripts/codex-build-vm.py
  scripts/build-dependency-proxy.py
  scripts/provider-supervisor.py
)

for script in "${BRIDGE_SCRIPTS[@]}"; do
  new_repo "$script"
  out=$(run_gate "$repo" "$PR"); rc=$?
  if [ "$rc" -eq 0 ] && is_block "$out" && reason "$out" | grep -qF -- "- $script"; then
    ok "$script change without a receipt is blocked and named"
  else
    bad "$script change without a receipt is blocked and named" "rc=$rc out=${out:0:300}"
  fi
done

# The block message must say what a bridge-suite receipt contains.
new_repo scripts/codex-command-sandbox.py
msg=$(reason "$(run_gate "$repo" "$PR")")
for needle in \
  "python3 scripts/tests/host_capabilities.py" \
  "python3 -m unittest discover -s tests -p '*_test.py' -v" \
  "command sandbox: enforceable" \
  "JARVIS_TEST_VM_CONFIG"; do
  if grep -qF -- "$needle" <<<"$msg"; then
    ok "bridge block message includes: $needle"
  else
    bad "bridge block message includes: $needle" "${msg:0:400}"
  fi
done

# A non-empty receipt keyed by HEAD unlocks the gate; an empty one does not.
new_repo scripts/codex-tool-bridge.py
receipt="$repo/.claude/agent-test-receipts/$(git -C "$repo" rev-parse HEAD).txt"
mkdir -p "$(dirname "$receipt")"
: > "$receipt"
out=$(run_gate "$repo" "$PR")
if is_block "$out"; then ok "an empty receipt does not unlock the gate"
else bad "an empty receipt does not unlock the gate" "$out"; fi
printf 'command: synthetic suite run\n' > "$receipt"
out=$(run_gate "$repo" "$PR"); rc=$?
if [ "$rc" -eq 0 ] && ! is_block "$out"; then ok "a non-empty HEAD receipt unlocks the gate"
else bad "a non-empty HEAD receipt unlocks the gate" "rc=$rc out=${out:0:300}"; fi

# Suite-only and unrelated changes stay ungated (CI runs the suites).
for path in scripts/tests/codex_tool_bridge_test.py scripts/other-tool.py docs/TESTING.md; do
  new_repo "$path"
  out=$(run_gate "$repo" "$PR"); rc=$?
  if [ "$rc" -eq 0 ] && ! is_block "$out"; then ok "$path change is not gated"
  else bad "$path change is not gated" "rc=$rc out=${out:0:300}"; fi
done

# Commands other than `gh pr create` pass through untouched.
new_repo scripts/codex-tool-bridge.py
out=$(run_gate "$repo" "gh pr view 1"); rc=$?
if [ "$rc" -eq 0 ] && [ -z "$out" ]; then ok "non-create gh commands are ignored"
else bad "non-create gh commands are ignored" "rc=$rc out=$out"; fi

# Regression: the original Rust runtime bucket still blocks, without the
# bridge-suite instructions that do not apply to it.
new_repo crates/augmentagent-channel-core/src/reasoner.rs
out=$(run_gate "$repo" "$PR")
if is_block "$out"; then ok "reasoner.rs still requires a receipt"
else bad "reasoner.rs still requires a receipt" "$out"; fi
if reason "$out" | grep -qF "host_capabilities.py"; then
  bad "reasoner.rs block omits the bridge-suite section" "$(reason "$out" | head -5)"
else
  ok "reasoner.rs block omits the bridge-suite section"
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
