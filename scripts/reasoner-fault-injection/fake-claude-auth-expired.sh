#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# Impersonates the `claude` CLI whose OAuth login has expired (#1499):
# consumes stdin, then emits the auth refusal the way the real CLI does — as
# a SUCCESSFUL stream-json completion whose text is the error, exit 0.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count claude

refusal="Failed to authenticate: OAuth session expired and could not be refreshed"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$refusal"
printf '{"type":"result","result":"%s"}\n' "$refusal"
