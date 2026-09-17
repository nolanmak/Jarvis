#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# Codex finishes its turn (a tool call completed, exit 0) but never emits a
# final `agent_message` (#1040). Content-level, like claude's EmptyOutput:
# the adapter must return an untyped error, so nothing latches and the chain
# does not advance to re-run the call's work on another provider.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count codex

printf '%s\n' '{"type":"thread.started","thread_id":"fake-thread"}'
printf '%s\n' '{"type":"item.completed","item":{"id":"fake-tool","type":"mcp_tool_call","server":"jarvis","tool":"Write","arguments":{"file_path":"synthetic.md","content":"synthetic"},"result":{"content":[{"type":"text","text":"written"}]},"status":"completed"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":0}}'
