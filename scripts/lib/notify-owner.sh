#!/usr/bin/env bash
# Sourceable "tell the owner on Discord" helper for standalone shell jobs.
#
# Deliberately a copy of the identical function in check-for-updates.sh rather
# than a refactor of it: that script is a guarded deploy path the pipeline
# refuses diffs against, so the two live side by side. Keep them in sync by
# hand if the webhook shape ever changes.
#
# Best effort and never fatal. Every caller is a cron/launchd tick whose real
# job must still complete when the webhook is unset, down, or unreachable —
# an alert that can abort the tick is worse than no alert.

# Prefix identifying which job posted. Callers set this before sourcing or
# before calling; the updater's copy uses ⚙️.
NOTIFY_OWNER_PREFIX="${NOTIFY_OWNER_PREFIX:-🔔}"

# notify_owner <text>
#
# Status: 0 the message reached the channel, 1 delivery was attempted and
# failed (worth retrying), 2 there is nothing to deliver with — no webhook
# configured, no curl, no python3 (retrying changes nothing). A caller that
# throttles its alerts needs to tell those three apart; one that does not can
# ignore the status entirely, since the function only ever returns.
notify_owner() {
  [ -n "${DISCORD_WEBHOOK_URL:-}" ] || return 2
  command -v curl >/dev/null 2>&1 || return 2
  local body
  # JSON-encode through python3 so an interpolated age or app name can never
  # break the payload. No python3, no post — an unescaped body is worse.
  body=$(printf '%s' "$NOTIFY_OWNER_PREFIX $*" \
    | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null) || return 2
  [ -n "$body" ] || return 2
  curl -fsS -m 10 -X POST -H 'Content-Type: application/json' \
    --data "$(printf '{"content": %s}' "$body")" \
    "$DISCORD_WEBHOOK_URL" >/dev/null 2>&1 || return 1
  return 0
}
