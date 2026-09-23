#!/usr/bin/env bash
# #1094 + #1078 — the wiki-ingest guard must (a) hard-block Write/Edit/NotebookEdit
# under journal/ (machine-managed ShadowNote mirror) and (b) deny writes to the
# instruction/config files Claude Code or Codex auto-load (CLAUDE.md, .claude,
# .mcp.json, AGENTS.md), while leaving ordinary people/threads/about pages
# writable. Drives the real scripts/aa-journal-guard.sh with synthetic events.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GUARD="$REPO_ROOT/scripts/aa-journal-guard.sh"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
WIKI="$TMP/wiki"; mkdir -p "$WIKI/people" "$WIKI/journal"
# A stand-in for the daemon's repo checkout: the cwd the ingest CLI used to
# inherit, holding its own CLAUDE.md (#1078).
REPO="$TMP/repo"; mkdir -p "$REPO"; printf 'repo instructions\n' > "$REPO/CLAUDE.md"
# cwd the guard runs in; relative paths resolve against it like the CLI's do.
GUARD_CWD="$WIKI"

event() {
  jq -cn --arg t "$1" --arg k "$2" --arg p "$3" \
    '{tool_name:$t, tool_input:{($k):$p}}'
}
run_guard() {
  local tool="$1" key="$2" path="$3"; shift 3
  event "$tool" "$key" "$path" | (cd "$GUARD_CWD" && env WIKI_ROOT="$WIKI" "$@" bash "$GUARD")
}
is_block() {
  printf '%s' "$1" | jq -e 'try (.decision == "block") catch false' >/dev/null 2>&1
}
expect_allow() { # <desc> <tool> <key> <path>
  local desc="$1"; shift
  local out; out=$(run_guard "$@"); local rc=$?
  if [ "$rc" -eq 0 ] && ! is_block "$out"; then ok "$desc"; else bad "$desc" "rc=$rc out=$out"; fi
}
expect_block() { # <desc> <tool> <key> <path>
  local desc="$1"; shift
  local out; out=$(run_guard "$@"); local rc=$?
  if [ "$rc" -ne 0 ] || is_block "$out"; then ok "$desc"; else bad "$desc" "guard allowed it: rc=$rc out=$out"; fi
}

# #1094 regression — journal/ is machine-managed.
expect_block "Write under journal/ is blocked" \
  Write file_path "$WIKI/journal/2026-09-21.md"
expect_allow "Write an ordinary people page is allowed" \
  Write file_path "$WIKI/people/dana.md"

# #1078 — instruction/config files are denied, case-insensitive, any component.
expect_block "Write CLAUDE.md is blocked" \
  Write file_path "$WIKI/CLAUDE.md"
expect_block "Write people/claude.md is blocked (case-insensitive)" \
  Write file_path "$WIKI/people/claude.md"
expect_block "Write .claude/settings.json is blocked" \
  Write file_path "$WIKI/.claude/settings.json"
expect_block "Write .mcp.json is blocked" \
  Write file_path "$WIKI/.mcp.json"
expect_block "Write AGENTS.md is blocked" \
  Write file_path "$WIKI/AGENTS.md"
expect_block "Edit CLAUDE.local.md is blocked" \
  Edit file_path "$WIKI/CLAUDE.local.md"
expect_block "NotebookEdit of CLAUDE.md (notebook_path arg) is blocked" \
  NotebookEdit notebook_path "$WIKI/CLAUDE.md"
expect_allow "Write people/claude-shannon.md is allowed (no false positive)" \
  Write file_path "$WIKI/people/claude-shannon.md"

# #1078 — writes must stay inside the wiki. The ingest CLI used to inherit the
# daemon's repo cwd, where acceptEdits would let it edit the repo's own
# CLAUDE.md; the guard now denies any write outside WIKI_ROOT, whatever cwd is.
GUARD_CWD="$REPO"
expect_block "Edit <repo>/CLAUDE.md (absolute) from a non-wiki cwd is blocked" \
  Edit file_path "$REPO/CLAUDE.md"
expect_block "Write CLAUDE.md (relative) from a non-wiki cwd is blocked" \
  Write file_path "CLAUDE.md"
expect_block "Write an ordinary file outside the wiki is blocked" \
  Write file_path "$REPO/notes.md"
expect_allow "Write an absolute wiki page from a non-wiki cwd is still allowed" \
  Write file_path "$WIKI/people/dana.md"
GUARD_CWD="$WIKI"
expect_allow "Write a wiki-relative page from the wiki cwd is allowed (no false positive)" \
  Write file_path "people/dana.md"
expect_block "Write a wiki-relative CLAUDE.md from the wiki cwd is blocked" \
  Write file_path "CLAUDE.md"
expect_block "Write ../CLAUDE.md escaping the wiki is blocked" \
  Write file_path "../CLAUDE.md"

printf '\n%d ok, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
