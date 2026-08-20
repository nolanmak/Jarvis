#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
#
# Impersonates the `claude` CLI hitting its session limit: consumes stdin,
# then emits the quota refusal EXACTLY the way the real CLI does — as a
# SUCCESSFUL stream-json completion whose text is the refusal (#448's
# infamous failure shape), exit 0.
#
# Usage:
#   FAKE_CLAUDE_COUNT_FILE=/tmp/x CLAUDE_CLI=$PWD/scripts/reasoner-fault-injection/fake-claude-quota.sh \
#   AUGMENTAGENT_REASONER_CHAIN=claude,cerebras \
#   AUGMENTAGENT_COOLDOWN_FILE=/tmp/cooldowns.json \
#   ./target/release/augmentagent reasoner-selftest
#
# FAKE_CLAUDE_COUNT_FILE (optional) counts invocations so a test can assert
# that a latched provider is NOT spawned on subsequent calls.
set -euo pipefail

cat >/dev/null

if [[ -n "${FAKE_CLAUDE_COUNT_FILE:-}" ]]; then
  prev=0
  [[ -f "$FAKE_CLAUDE_COUNT_FILE" ]] && prev=$(cat "$FAKE_CLAUDE_COUNT_FILE")
  echo $((prev + 1)) > "$FAKE_CLAUDE_COUNT_FILE"
fi

refusal="You've hit your session limit · resets 9:30am (America/New_York)"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$refusal"
printf '{"type":"result","result":"%s"}\n' "$refusal"
