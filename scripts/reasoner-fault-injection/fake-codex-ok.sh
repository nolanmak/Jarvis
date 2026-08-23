#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# A healthy `codex exec --json` run: the JSONL event stream the adapter
# reduces to a final `agent_message`, exit 0.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count codex

printf '%s\n' '{"type":"thread.started","thread_id":"fake-thread"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"PONG-FROM-FAKE-CODEX"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":0}}'
