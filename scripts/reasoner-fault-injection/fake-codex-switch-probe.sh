#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
count_file="$HOME/.fake-cli/codex.count"
prior_count=$(cat "$count_file" 2>/dev/null || printf '0')
prompt=$(cat)
if (( prior_count > 0 )); then
  [[ "$prompt" == *'<conversation_history>'* ]] || { echo 'missing conversation history' >&2; exit 1; }
  [[ "$prompt" == *'assistant: TOOL_PROBE_01234567-89ab-cdef-0123-456789abcdef'* ]] || {
    echo 'missing prior model answer' >&2; exit 1;
  }
fi
error_log="$HOME/.fake-cli/switch-error.log"
mkdir -p "$(dirname "$error_log")"
if ! printf '%s' "$prompt" | "$(dirname "${BASH_SOURCE[0]}")/fake-codex-read-probe.sh" "$@" 2>"$error_log"; then
  cat "$error_log" >&2
  exit 1
fi
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":1200,"cached_input_tokens":800,"cache_write_input_tokens":0,"output_tokens":40,"reasoning_output_tokens":16}}'
