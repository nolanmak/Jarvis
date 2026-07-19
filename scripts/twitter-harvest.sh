#!/usr/bin/env bash
# twitter-harvest.sh — interactive session harvest for AugmentAgent's X/Twitter channel.
#
# Prompts for the values you need (pasted from Chrome devtools on x.com),
# writes a JSON session file, then either ships it to the daemon host via
# Tailscale SSH (if TWITTER_SSH_TARGET is set) or prints the two commands
# to run yourself. Mirrors scripts/linkedin-harvest.sh.
#
# Usage:
#   ./scripts/twitter-harvest.sh                       # local JSON only
#   TWITTER_SSH_TARGET=nolan@host ./scripts/twitter-harvest.sh   # remote install
#
# Chrome devtools walkthrough:
#   1. Open https://x.com/messages (must be logged in)
#   2. Devtools -> Application tab -> Storage -> Cookies -> https://x.com
#   3. Copy the "Value" column for: auth_token, ct0
#   4. user_id + screen_name: devtools -> Network tab -> reload -> click any
#      request to /i/api/graphql/* or /i/api/1.1/* -> the response/headers
#      carry your numeric id; screen_name is your @handle minus the @.
#
# SECURITY: the resulting file contains your X session cookie. Treat it like
# a password — anyone with auth_token + ct0 can read and post as you until
# you log out on x.com (which rotates auth_token). .gitignore excludes
# twitter-auth.json / twitter-session.json.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
OUT_LOCAL="${OUT_LOCAL:-$REPO_ROOT/twitter-session.json}"

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

USER_ID="$(read_value user_id 'Your numeric X user id (e.g. 1450000000000000000):')"
SCREEN_NAME="$(read_value screen_name 'Your @handle WITHOUT the @ (e.g. nolanmak):')"
AUTH_TOKEN="$(read_value auth_token 'auth_token cookie value:')"
CT0="$(read_value ct0 'ct0 cookie value:')"

jsonescape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '%s' "$s"
}

NOW_MS="$(date +%s)000"

cat > "$OUT_LOCAL" <<JSON
{
  "user_id": "$(jsonescape "$USER_ID")",
  "screen_name": "$(jsonescape "$SCREEN_NAME")",
  "cookies": {
    "auth_token": "$(jsonescape "$AUTH_TOKEN")",
    "ct0": "$(jsonescape "$CT0")"
  },
  "user_agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
  "harvested_at_ms": $NOW_MS
}
JSON

chmod 600 "$OUT_LOCAL"
echo ""
echo "wrote $OUT_LOCAL (bearer defaults to the public web token; override via AUGMENTAGENT_TWITTER_BEARER if X rotates it)"

# --- transport to the daemon host ---

if [[ -z "${TWITTER_SSH_TARGET:-}" ]]; then
    cat <<EOF

No TWITTER_SSH_TARGET set, so the session stayed on this machine. To install
it on the daemon host:

  scp '$OUT_LOCAL' <user>@<host>:~/twitter-session.json
  ssh <user>@<host> \\
      'cd ~/AugmentAgent && ./target/release/augmentagent twitter login --session-json ~/twitter-session.json && rm ~/twitter-session.json'

Or re-run with TWITTER_SSH_TARGET=<user>@<host> to do it in one shot.
EOF
    exit 0
fi

REMOTE_PATH="${TWITTER_REMOTE_PATH:-~/twitter-session.json}"
REMOTE_REPO="${TWITTER_REMOTE_REPO:-~/AugmentAgent}"

echo ""
echo "shipping to $TWITTER_SSH_TARGET ..."
scp "$OUT_LOCAL" "$TWITTER_SSH_TARGET:$REMOTE_PATH"

echo "running \`twitter login\` on $TWITTER_SSH_TARGET ..."
# shellcheck disable=SC2029
ssh "$TWITTER_SSH_TARGET" \
    "cd $REMOTE_REPO && ./target/release/augmentagent twitter login --session-json $REMOTE_PATH && rm $REMOTE_PATH"

echo ""
echo "done. remote session installed; auto-updater restarts the daemon on the next rebuild cycle."
echo "optional: rm the local copy -> rm '$OUT_LOCAL'"
