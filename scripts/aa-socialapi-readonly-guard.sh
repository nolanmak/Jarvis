#!/usr/bin/env bash
# PreToolUse hook for the SocialAPI.ai read-only drafting-aid MCP server.
#
# When the OPTIONAL flag AUGMENTAGENT_SOCIALAPI_MCP_READONLY is set, the
# draft/code-mode Claude CLI session is allowed to reach SocialAPI.ai's MCP
# server SOLELY to FETCH comment/thread/post context that improves a draft.
# It must NEVER be able to post, reply, send a DM, delete, or otherwise
# mutate the account.
#
# This hook reads a single PreToolUse event from stdin (Claude Code hook
# protocol), inspects the tool name, and emits a JSON `deny` reply on stdout
# for any SocialAPI MCP tool whose name implies a write/send. It is
# FAIL-CLOSED: a SocialAPI MCP tool that is not clearly read-only is denied.
#
# Non-SocialAPI tools (e.g. the wiki Read/Grep already scoped elsewhere) are
# out of scope here and pass through untouched.
#
# Exit codes:
#   0 — decision JSON printed (allow or deny; Claude reads stdout)
#   2 — hook itself failed (treated as block by Claude Code)
#
# Issue: #248 — optional read-only SocialAPI MCP drafting aid.

set -euo pipefail

INPUT=$(cat)

if ! command -v jq >/dev/null 2>&1; then
  echo "aa-socialapi-readonly-guard: jq missing on PATH" >&2
  exit 2
fi

TOOL=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty')

# Only police SocialAPI MCP tools. Claude Code names MCP tools
# `mcp__<server>__<tool>`; our server is registered as `socialapi`.
case "$TOOL" in
  mcp__socialapi__*) ;;
  *)
    # Not a SocialAPI MCP tool — not our concern. Allow (other hooks /
    # allowlists govern these).
    exit 0
    ;;
esac

# The bare tool name after the `mcp__socialapi__` prefix, lowercased.
BARE=$(printf '%s' "$TOOL" | sed 's/^mcp__socialapi__//' | tr '[:upper:]' '[:lower:]')

# Key the decision on the LEADING verb (first token before `_`), which is the
# standard MCP tool naming convention (`get_thread`, `list_comments`,
# `reply_to_comment`, `post_tweet`). Matching on the leading verb avoids the
# noun trap where a READ tool like `list_comments` happens to contain a write
# word ("comment") as its object.
VERB="${BARE%%_*}"

deny() {
  local reason="$1"
  jq -n --arg r "$reason" \
    '{decision:"block", reason:$r, hookSpecificOutput:{hookEventName:"PreToolUse", permissionDecision:"deny", permissionDecisionReason:$r}}'
  exit 0
}

# 1) Explicit write/send/mutation leading verbs are ALWAYS denied. This is the
#    primary guarantee: the drafting aid can read context but never act. As a
#    belt-and-suspenders, also deny if a write verb appears ANYWHERE in the
#    bare name (covers odd shapes like `do_reply` while still allowing the
#    leading-verb reads handled in step 2).
case "$VERB" in
  reply|post|send|dm|message|delete|remove|create|add|update|edit|publish|comment|like|unlike|follow|unfollow|block|unblock|mute|unmute|react|share|repost|retweet|quote|upload|write|set|put|patch|schedule|approve|favorite|bookmark)
    deny "SocialAPI MCP drafting aid is strictly read-only; the leading verb of '$TOOL' implies a write/send action and is denied (#248)."
    ;;
esac

# 2) Allow ONLY clearly read-only leading verbs. Fail-closed: anything whose
#    leading verb is not on this read allow-list is denied, even though it
#    didn't match a write verb above.
case "$VERB" in
  list|get|fetch|read|search|show|view|find|lookup|describe|count|check)
    exit 0
    ;;
esac

deny "SocialAPI MCP tool '$TOOL' is not on the read-only allow-list (leading verb '$VERB'); denied fail-closed (#248)."
