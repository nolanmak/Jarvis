#!/usr/bin/env bash
# PreToolUse hook for the wiki-ingest Claude CLI session (#1094).
#
# `wiki/journal/` is MACHINE-MANAGED: the deterministic ShadowNote mirror
# (#1010) writes every entry and its revision history there. The Capture
# ingest agent must extract durable facts to people/threads/about pages —
# it must never create or edit derived pages under journal/ (observed
# failure: a note's second morning block re-emitted as a fabricated
# next-day entry with created > updated).
#
# Protocol: one JSON hook event on stdin; emit a block decision on stdout.
# Exit 0 with no output = allow. Exit 2 = hook failure (treated as block).
#
# Required env: WIKI_ROOT (absolute path to the wiki directory).

set -euo pipefail
export LC_ALL=C

# #1078 — shared reserved-injection-name list, sourced so this guard and
# aa-wiki-scope-guard.sh cannot drift from each other or from the canonical
# PROTECTED_INJECTION_NAMES in codex_tools.rs.
GUARD_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=aa-wiki-protected-names.sh
source "$GUARD_DIR/aa-wiki-protected-names.sh"

if [[ -z "${WIKI_ROOT:-}" ]]; then
  echo "aa-journal-guard: WIKI_ROOT unset" >&2
  exit 2
fi
if ! WIKI_ROOT_ABS=$(readlink -f -- "$WIKI_ROOT" 2>/dev/null); then
  echo "aa-journal-guard: WIKI_ROOT does not resolve: $WIKI_ROOT" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "aa-journal-guard: jq missing on PATH" >&2
  exit 2
fi

INPUT=$(cat)
TOOL=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty')
case "$TOOL" in
  Write|Edit)
    CANDIDATE=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty')
    ;;
  NotebookEdit)
    # NotebookEdit names its target `notebook_path`, not `file_path` (#1078).
    CANDIDATE=$(printf '%s' "$INPUT" | jq -r '.tool_input.notebook_path // empty')
    ;;
  *) exit 0 ;;
esac
[[ -z "$CANDIDATE" ]] && exit 0

if [[ "$CANDIDATE" = /* ]]; then
  ABS="$CANDIDATE"
else
  ABS="$PWD/$CANDIDATE"
fi
if RESOLVED=$(readlink -m -- "$ABS" 2>/dev/null); then
  ABS="$RESOLVED"
fi

# #1078 — deny writes to instruction/config files Claude Code or Codex
# auto-load (CLAUDE.md, CLAUDE.local.md, .claude, .mcp.json, AGENTS.md). One
# planted here would steer every later wiki call and survive in the private
# mirror. Checked relative to the wiki root so the root's own components cannot
# false-positive; shares the reserved list with aa-wiki-scope-guard.sh.
if [[ "$ABS" == "$WIKI_ROOT_ABS"/* ]] \
   && aa_path_has_protected_injection_name "${ABS#"$WIKI_ROOT_ABS"/}"; then
  REASON="Refusing to write an instruction/config file Claude Code or Codex auto-loads (CLAUDE.md, CLAUDE.local.md, .claude, .mcp.json, AGENTS.md). A file planted here persists as instructions into every later wiki call and through the mirror (#1078). Tool=$TOOL path=$ABS"
  jq -n --arg r "$REASON" \
    '{decision:"block", reason:$r, hookSpecificOutput:{hookEventName:"PreToolUse", permissionDecision:"deny", permissionDecisionReason:$r}}'
  exit 0
fi

if [[ "$ABS" == "$WIKI_ROOT_ABS/journal" || "$ABS" == "$WIKI_ROOT_ABS/journal/"* ]]; then
  REASON="journal/ is machine-managed (the deterministic ShadowNote mirror). Do not create or edit pages under journal/ — record durable facts on people/threads/about pages and cite the shadownote:<id>:<version> messageId instead. Tool=$TOOL path=$ABS"
  jq -n --arg r "$REASON" \
    '{decision:"block", reason:$r, hookSpecificOutput:{hookEventName:"PreToolUse", permissionDecision:"deny", permissionDecisionReason:$r}}'
  exit 0
fi
exit 0
