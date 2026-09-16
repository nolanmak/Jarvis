#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# Codex's turn fails on its own content: the conversation overflowed the
# model's context window (#1040). Another provider would be handed the same
# oversized request, and the provider itself is healthy, so this must neither
# latch codex nor advance the chain.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count codex

printf '%s\n' '{"type":"thread.started","thread_id":"fake-thread"}'
printf '%s\n' '{"type":"turn.failed","error":{"message":"Codex ran out of room in the model'"'"'s context window. Start a new thread or clear earlier history before retrying."}}'
exit 1
