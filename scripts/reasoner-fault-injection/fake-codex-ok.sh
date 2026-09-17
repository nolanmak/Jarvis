#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# A healthy `codex exec --json` run: the JSONL event stream the adapter
# reduces to a final `agent_message`, exit 0. `turn.completed` carries the
# usage object codex-cli 0.154 emits (all five fields), which the adapter
# records in token-usage.jsonl (#1047).
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count codex

printf '%s\n' '{"type":"thread.started","thread_id":"fake-thread"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"PONG-FROM-FAKE-CODEX"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1200,"cached_input_tokens":800,"cache_write_input_tokens":0,"output_tokens":40,"reasoning_output_tokens":16}}'
