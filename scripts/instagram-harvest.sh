#!/usr/bin/env bash
# instagram-harvest.sh — interactive cookie harvest for AugmentAgent's
# Instagram channel.
#
# Prompts for the session values you need (pasted from Chrome devtools on
# instagram.com), writes a JSON cookie file, then either ships it to the
# daemon host via Tailscale SSH (if INSTAGRAM_SSH_TARGET is set) or prints
# the commands to run yourself.
#
# Usage:
#   ./scripts/instagram-harvest.sh                                  # local JSON only
#   INSTAGRAM_SSH_TARGET=nolan@host ./scripts/instagram-harvest.sh   # remote install
#
# Chrome devtools walkthrough:
#   1. Open https://www.instagram.com/ (must be logged in)
#   2. devtools → Application → Storage → Cookies → https://www.instagram.com
#   3. Copy the "Value" column for: sessionid, csrftoken, ds_user_id, mid, ig_did
#      (rur is optional; include it if present)
#   4. ds_user_id is also the numeric value of the ds_user_id cookie — paste
#      the same value into both prompts.
#
# See docs/instagram-protocol.md for what each value is. NOTE: that protocol
# spec is reconstructed from public knowledge and REQUIRES LIVE OPERATOR
# VALIDATION — the first real harvest is also the validation run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
OUT_LOCAL="${OUT_LOCAL:-$REPO_ROOT/instagram-cookies.json}"

read_value() {
    local name="$1"
    local prompt="$2"
    local value=""
    printf '\n%s\n' "$prompt" >&2
    IFS= read -r -p "$name: " value
    if [[ -z "$value" ]]; then
        echo "error: $name must not be empty" >&2
        exit 1
    fi
    printf '%s' "$value"
}

read_optional() {
    local name="$1"
    local prompt="$2"
    local value=""
    printf '\n%s\n' "$prompt" >&2
    IFS= read -r -p "$name (optional, Enter to skip): " value
    printf '%s' "$value"
}

DS_USER_ID="$(read_value ds_user_id 'Your numeric account id (the ds_user_id cookie value):')"
USERNAME="$(read_optional username 'Your @handle (informational only):')"
SESSIONID="$(read_value sessionid 'sessionid cookie value:')"
CSRFTOKEN="$(read_value csrftoken 'csrftoken cookie value:')"
MID="$(read_value mid 'mid cookie value:')"
IG_DID="$(read_value ig_did 'ig_did cookie value:')"
RUR="$(read_optional rur 'rur cookie value:')"

jsonescape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '%s' "$s"
}

NOW_MS="$(date +%s)000"

# Build the cookies object; include rur only if provided.
RUR_LINE=""
if [[ -n "$RUR" ]]; then
    RUR_LINE=",
    \"rur\": \"$(jsonescape "$RUR")\""
fi

cat > "$OUT_LOCAL" <<JSON
{
  "ds_user_id": "$(jsonescape "$DS_USER_ID")",
  "username": "$(jsonescape "$USERNAME")",
  "cookies": {
    "sessionid": "$(jsonescape "$SESSIONID")",
    "csrftoken": "$(jsonescape "$CSRFTOKEN")",
    "ds_user_id": "$(jsonescape "$DS_USER_ID")",
    "mid": "$(jsonescape "$MID")",
    "ig_did": "$(jsonescape "$IG_DID")"$RUR_LINE
  },
  "user_agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
  "harvested_at_ms": $NOW_MS
}
JSON

chmod 600 "$OUT_LOCAL"
echo ""
echo "wrote $OUT_LOCAL"

if [[ -z "${INSTAGRAM_SSH_TARGET:-}" ]]; then
    cat <<EOF

No INSTAGRAM_SSH_TARGET set, so cookies stayed on this machine. To install
them on the daemon host:

  scp '$OUT_LOCAL' nolan-makatche@100.91.92.24:~/instagram-cookies.json
  ssh nolan-makatche@100.91.92.24 \\
      'cd ~/AugmentAgent && ./target/release/augmentagent instagram login --cookies-json ~/instagram-cookies.json && rm ~/instagram-cookies.json'

Or re-run with INSTAGRAM_SSH_TARGET=<user>@<host> to do it in one shot.
EOF
    exit 0
fi

REMOTE_PATH="${INSTAGRAM_REMOTE_PATH:-~/instagram-cookies.json}"
REMOTE_REPO="${INSTAGRAM_REMOTE_REPO:-~/AugmentAgent}"

echo ""
echo "shipping to $INSTAGRAM_SSH_TARGET ..."
scp "$OUT_LOCAL" "$INSTAGRAM_SSH_TARGET:$REMOTE_PATH"

echo "running 'instagram login' on $INSTAGRAM_SSH_TARGET ..."
# shellcheck disable=SC2029
ssh "$INSTAGRAM_SSH_TARGET" \
    "cd $REMOTE_REPO && ./target/release/augmentagent instagram login --cookies-json $REMOTE_PATH && rm $REMOTE_PATH"

echo ""
echo "done. remote cookies are installed; auto-updater will restart the daemon on the next rebuild cycle."
echo "optional: rm the local copy → rm '$OUT_LOCAL'"
