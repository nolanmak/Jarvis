#!/usr/bin/env bash
# PR #922: the wiki-query scope guard must keep its wiki-root sandbox AND
# allow read-only tools into the transcript clone (#915) when
# AUGMENTAGENT_TRANSCRIPTS_DIR is set. Write/Edit stay wiki-only:
# FlyOnTheWall owns that repo and this side never writes there.
#
# Drives the real scripts/aa-wiki-scope-guard.sh with synthetic hook events.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GUARD="$REPO_ROOT/scripts/aa-wiki-scope-guard.sh"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
WIKI="$TMP/wiki";               mkdir -p "$WIKI/people"
TRANSCRIPTS="$TMP/transcripts"; mkdir -p "$TRANSCRIPTS/meetings"
# A sibling whose name shares the transcript dir as a string prefix: the
# guard's trailing-slash compare must not let it through.
mkdir -p "$TMP/transcripts-evil"
echo hi > "$WIKI/people/dana.md"
echo hi > "$TRANSCRIPTS/meetings/m1.md"
echo hi > "$TMP/transcripts-evil/m1.md"
echo hi > "$TMP/loose.md"

# event <tool> <key> <path> → hook-event JSON on stdout.
event() {
  jq -cn --arg t "$1" --arg k "$2" --arg p "$3" \
    '{tool_name:$t, tool_input:{($k):$p}}'
}

# run_guard <tool> <key> <path> [extra-env...]; echoes guard stdout,
# returns its exit code.
run_guard() {
  local tool="$1" key="$2" path="$3"; shift 3
  event "$tool" "$key" "$path" \
    | env WIKI_ROOT="$WIKI" "$@" bash "$GUARD"
}

# The guard blocks by printing a decision JSON (exit 0) or by failing hard
# (exit 2); an allow is exit 0 with no block decision on stdout.
is_block() {
  printf '%s' "$1" | jq -e 'try (.decision == "block") catch false' >/dev/null 2>&1
}

expect_allow() { # <desc> <tool> <key> <path> [extra-env...]
  local desc="$1"; shift
  local out
  out=$(run_guard "$@")
  local rc=$?
  if [ "$rc" -eq 0 ] && ! is_block "$out"; then
    ok "$desc"
  else
    bad "$desc" "rc=$rc out=$out"
  fi
}

expect_block() { # <desc> <tool> <key> <path> [extra-env...]
  local desc="$1"; shift
  local out
  out=$(run_guard "$@")
  local rc=$?
  if [ "$rc" -ne 0 ] || is_block "$out"; then
    ok "$desc"
  else
    bad "$desc" "guard allowed it: rc=$rc out=$out"
  fi
}

T_ENV="AUGMENTAGENT_TRANSCRIPTS_DIR=$TRANSCRIPTS"

# The wiki sandbox itself (regression, #127).
expect_allow "Read inside the wiki is allowed" \
  Read file_path "$WIKI/people/dana.md" "$T_ENV"
expect_block "Read outside wiki and transcripts is blocked" \
  Read file_path "$TMP/loose.md" "$T_ENV"
expect_block "Write outside the wiki stays blocked" \
  Write file_path "$TMP/loose.md" "$T_ENV"

# #915/#922 — read-only access to the transcript clone.
expect_allow "Read inside the transcript clone is allowed" \
  Read file_path "$TRANSCRIPTS/meetings/m1.md" "$T_ENV"
expect_allow "Grep scoped to the transcript clone is allowed" \
  Grep path "$TRANSCRIPTS/meetings" "$T_ENV"
expect_allow "Glob scoped to the transcript clone is allowed" \
  Glob path "$TRANSCRIPTS" "$T_ENV"

# The clone is read-only: FlyOnTheWall owns it.
expect_block "Write into the transcript clone is blocked" \
  Write file_path "$TRANSCRIPTS/meetings/m1.md" "$T_ENV"
expect_block "Edit into the transcript clone is blocked" \
  Edit file_path "$TRANSCRIPTS/meetings/m1.md" "$T_ENV"

# Scope hygiene.
expect_block "Without the env var a transcript Read is blocked" \
  Read file_path "$TRANSCRIPTS/meetings/m1.md"
expect_block "A string-prefix sibling dir does not ride along" \
  Read file_path "$TMP/transcripts-evil/m1.md" "$T_ENV"
expect_block "An unresolvable transcripts dir grants nothing" \
  Read file_path "$TMP/transcripts-evil/m1.md" "AUGMENTAGENT_TRANSCRIPTS_DIR=$TMP/does-not-exist"
expect_allow "The wiki stays allowed when the transcripts dir is unresolvable" \
  Read file_path "$WIKI/people/dana.md" "AUGMENTAGENT_TRANSCRIPTS_DIR=$TMP/does-not-exist"

printf '\n%d ok, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
