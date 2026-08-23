#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# Impersonates the `claude` CLI hitting its session limit: consumes stdin,
# then emits the quota refusal EXACTLY the way the real CLI does — as a
# SUCCESSFUL stream-json completion whose text is the refusal (#448's
# infamous failure shape), exit 0.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count claude

refusal="You've hit your session limit · resets 9:30am (America/New_York)"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$refusal"
printf '{"type":"result","result":"%s"}\n' "$refusal"
