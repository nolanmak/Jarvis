#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# Codex loses its connection to the model API mid-turn: a transport failure.
# The control for #1040's narrowing: provider-side failures like this one
# still map to Unavailable, latch codex, and fail over.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count codex

printf '%s\n' '{"type":"thread.started","thread_id":"fake-thread"}'
printf '%s\n' '{"type":"error","message":"Reconnecting... 5/5 (stream disconnected before completion: error sending request)"}'
printf '%s\n' '{"type":"turn.failed","error":{"message":"stream disconnected before completion: error sending request"}}'
exit 1
