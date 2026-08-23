#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# Codex out of quota: a `turn.failed` event carrying the usage-limit text,
# plus a non-zero exit — what the adapter maps to RateLimited.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count codex

printf '%s\n' '{"type":"turn.failed","error":{"message":"You'"'"'ve hit your usage limit. Try again at Aug 20th, 2026 10:27 AM."}}'
exit 1
