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
  Write|Edit) ;;
  *) exit 0 ;;
esac

CANDIDATE=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty')
[[ -z "$CANDIDATE" ]] && exit 0

if [[ "$CANDIDATE" = /* ]]; then
  ABS="$CANDIDATE"
else
  ABS="$PWD/$CANDIDATE"
fi
if RESOLVED=$(readlink -m -- "$ABS" 2>/dev/null); then
  ABS="$RESOLVED"
fi

if [[ "$ABS" == "$WIKI_ROOT_ABS/journal" || "$ABS" == "$WIKI_ROOT_ABS/journal/"* ]]; then
  REASON="journal/ is machine-managed (the deterministic ShadowNote mirror). Do not create or edit pages under journal/ — record durable facts on people/threads/about pages and cite the shadownote:<id>:<version> messageId instead. Tool=$TOOL path=$ABS"
  jq -n --arg r "$REASON" \
    '{decision:"block", reason:$r, hookSpecificOutput:{hookEventName:"PreToolUse", permissionDecision:"deny", permissionDecisionReason:$r}}'
  exit 0
fi
exit 0
