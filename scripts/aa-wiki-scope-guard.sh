#!/usr/bin/env bash
# PreToolUse hook for the wiki-query Claude CLI session.
#
# Enforces path scoping on Read/Write/Edit/Glob/Grep so the agent cannot
# touch anything outside $WIKI_ROOT. Reads a single JSON event from stdin
# (Claude Code hook protocol), inspects the `file_path` / `path` argument,
# and emits a JSON `{"decision":"block",...}` reply on stdout when the
# path resolves outside the wiki tree.
#
# Exit codes:
#   0 — decision JSON printed (allow or block; Claude reads stdout)
#   2 — hook itself failed (treated as block by Claude Code)
#
# Required env: WIKI_ROOT (absolute path to the wiki directory)
# Optional env: AUGMENTAGENT_TRANSCRIPTS_DIR — the FlyOnTheWall transcript
#   clone (#915). Read/Glob/Grep are additionally allowed under it;
#   Write/Edit stay wiki-only because FlyOnTheWall owns that repo.
#
# Issue: #127 — Read/Write/Glob/Grep ignored the wiki-root scope claim
# in `schema/wiki-ask.md`. Only the Bash allowlist was actually enforced.

set -euo pipefail

if [[ -z "${WIKI_ROOT:-}" ]]; then
  echo "aa-wiki-scope-guard: WIKI_ROOT unset" >&2
  exit 2
fi

# Canonicalise WIKI_ROOT so trailing slashes / symlinks don't desync the
# comparison with the canonicalised tool-input path below.
if ! WIKI_ROOT_ABS=$(readlink -f -- "$WIKI_ROOT" 2>/dev/null); then
  echo "aa-wiki-scope-guard: WIKI_ROOT does not resolve: $WIKI_ROOT" >&2
  exit 2
fi

# Read the hook event JSON from stdin. Single line is typical but we tolerate
# multi-line payloads by slurping all of stdin.
INPUT=$(cat)

# Extract tool name and the candidate path. `jq` is on every dev box this
# daemon runs on; falling back to grep would be brittle for nested JSON.
if ! command -v jq >/dev/null 2>&1; then
  echo "aa-wiki-scope-guard: jq missing on PATH" >&2
  exit 2
fi

TOOL=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty')

# Pull the path argument by tool. Read/Write/Edit use `file_path`; Glob/Grep
# use the optional `path` (defaults to cwd if omitted — which is wiki_root,
# so absent path = allow).
case "$TOOL" in
  Read|Write|Edit)
    CANDIDATE=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty')
    ;;
  Glob|Grep)
    CANDIDATE=$(printf '%s' "$INPUT" | jq -r '.tool_input.path // empty')
    if [[ -z "$CANDIDATE" ]]; then
      # No path = scoped to cwd, which is wiki_root. Allow.
      exit 0
    fi
    ;;
  *)
    # We only register this hook for the five file tools above, but be
    # defensive: anything else is allowed (Bash has its own allowlist).
    exit 0
    ;;
esac

if [[ -z "$CANDIDATE" ]]; then
  # No file_path on a Read/Write/Edit is malformed; let Claude surface its
  # own validation error rather than masking it.
  exit 0
fi

# Make CANDIDATE absolute. If the path doesn't exist yet (e.g. Write
# creating a new file), readlink -f still resolves the parent and joins —
# but for safety we fall back to a manual absolute join when the parent
# also doesn't exist.
if [[ "$CANDIDATE" = /* ]]; then
  ABS="$CANDIDATE"
else
  ABS="$PWD/$CANDIDATE"
fi
# Resolve symlinks / `..` segments where possible. `-m` (canonicalize-missing)
# is GNU readlink; it doesn't require the target to exist, which we need for
# Write to new files.
if RESOLVED=$(readlink -m -- "$ABS" 2>/dev/null); then
  ABS="$RESOLVED"
fi

# Allow when ABS is inside WIKI_ROOT_ABS. We compare with a trailing slash
# to avoid `/wiki-evil` matching when WIKI_ROOT is `/wiki`.
if [[ "$ABS" == "$WIKI_ROOT_ABS" || "$ABS" == "$WIKI_ROOT_ABS"/* ]]; then
  exit 0
fi

# PR #922 — #915 hands the transcript clone to the ask session via
# `--add-dir`, but this guard used to reject every Read/Grep/Glob on it,
# so "grep the words actually said" was dead on arrival. Allow the READ
# tools under AUGMENTAGENT_TRANSCRIPTS_DIR when it resolves (readlink -f
# requires the directory to exist, so a bogus value grants nothing).
# Write/Edit are deliberately absent: FlyOnTheWall owns that repo and
# this side never writes there.
if [[ "$TOOL" =~ ^(Read|Glob|Grep)$ && -n "${AUGMENTAGENT_TRANSCRIPTS_DIR:-}" ]]; then
  if TRANSCRIPTS_ABS=$(readlink -f -- "$AUGMENTAGENT_TRANSCRIPTS_DIR" 2>/dev/null); then
    if [[ "$ABS" == "$TRANSCRIPTS_ABS" || "$ABS" == "$TRANSCRIPTS_ABS"/* ]]; then
      exit 0
    fi
  fi
fi

# Allow Read on Discord-attachment tempfiles. The discord crate
# (augmentagent-approval-discord) downloads inbound attachments to
# /tmp/aa-{img,txt,doc}-<msg_id>-<idx>.<ext> and instructs the model to
# Read them. Without this exception the wiki-root scope check blocks the
# Read and the user sees "I can't open this attachment". Only Read is
# permitted on these paths — Write/Edit/Glob/Grep stay scoped to the
# wiki. The regex pins the canonical filename shape (digits-only ids,
# alphanumeric extension) so a literal name like `aa-txt-..` cannot
# slip past as a `/tmp/aa-txt-*` glob would.
if [[ "$TOOL" == "Read" && "$ABS" =~ ^/tmp/aa-(txt|img|doc)-[0-9]+-[0-9]+\.[a-zA-Z0-9]+$ ]]; then
  exit 0
fi

# Block with a clear, structured JSON reason. Claude relays this to the
# model so it can adjust and try again inside the sandbox.
REASON="Path is outside the wiki root sandbox. Tool=$TOOL path=$ABS wiki_root=$WIKI_ROOT_ABS. The wiki-query agent may only read/write inside the wiki."
jq -n --arg r "$REASON" \
  '{decision:"block", reason:$r, hookSpecificOutput:{hookEventName:"PreToolUse", permissionDecision:"deny", permissionDecisionReason:$r}}'
exit 0
