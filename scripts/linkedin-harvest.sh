#!/usr/bin/env bash
# linkedin-harvest.sh — interactive cookie harvest for AugmentAgent's LinkedIn channel.
#
# Prompts for the four values you need (pasted from Chrome devtools on
# linkedin.com), writes a JSON cookie file, then either ships it to the
# daemon host via Tailscale SSH (if LINKEDIN_SSH_TARGET is set) or prints
# the two commands to run yourself.
#
# Usage:
#   ./scripts/linkedin-harvest.sh                       # local JSON only
#   LINKEDIN_SSH_TARGET=nolan@host ./scripts/linkedin-harvest.sh   # remote install
#
# Chrome devtools walkthrough:
#   1. Open https://www.linkedin.com/messaging/ (must be logged in)
#   2. Open devtools → Application tab → Storage → Cookies → https://www.linkedin.com
#   3. Copy the "Value" column for: li_at, JSESSIONID, bcookie
#   4. member_urn: click your avatar → Me → View Profile → the URL will be
#      https://www.linkedin.com/in/<yourhandle>/ — but we want the fsd_profile
#      URN. Easiest way: on linkedin.com/messaging, devtools → Network tab →
#      reload → click any voyager/api/* request → check the request body or
#      URL query for "urn:li:fsd_profile:ACoAA..." — copy that whole URN.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
OUT_LOCAL="${OUT_LOCAL:-$REPO_ROOT/linkedin-cookies.json}"

read_value() {
    local name="$1"
    local prompt="$2"
    local value=""
    # Read on its own line; allow arbitrary characters including quotes.
    printf '\n%s\n' "$prompt" >&2
    IFS= read -r -p "$name: " value
    if [[ -z "$value" ]]; then
        echo "error: $name must not be empty" >&2
        exit 1
    fi
    printf '%s' "$value"
}

MEMBER_URN="$(read_value member_urn 'Your own fsd_profile URN (e.g. urn:li:fsd_profile:ACoAA...):')"
LI_AT="$(read_value li_at 'li_at cookie value:')"
JSESSIONID="$(read_value JSESSIONID 'JSESSIONID cookie value (PASTE WITH THE SURROUNDING QUOTES, e.g. "ajax:0103..."):')"
BCOOKIE="$(read_value bcookie 'bcookie value:')"

# Build JSON without python (works on bare Linux + macOS). All four values
# go through jq-style escape by Python-free hand rolling — we only need to
# escape backslashes and double quotes inside string literals.
jsonescape() {
    # Backslash first, then double quote.
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '%s' "$s"
}

NOW_MS="$(date +%s)000"

cat > "$OUT_LOCAL" <<JSON
{
  "member_urn": "$(jsonescape "$MEMBER_URN")",
  "cookies": {
    "li_at": "$(jsonescape "$LI_AT")",
    "JSESSIONID": "$(jsonescape "$JSESSIONID")",
    "bcookie": "$(jsonescape "$BCOOKIE")"
  },
  "user_agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
  "harvested_at_ms": $NOW_MS
}
JSON

chmod 600 "$OUT_LOCAL"
echo ""
echo "wrote $OUT_LOCAL"

# --- transport to the daemon host ---

if [[ -z "${LINKEDIN_SSH_TARGET:-}" ]]; then
    cat <<EOF

No LINKEDIN_SSH_TARGET set, so cookies stayed on this machine. To install
them on the daemon host:

  scp '$OUT_LOCAL' <user>@<host>:~/linkedin-cookies.json
  ssh <user>@<host> \\
      'cd ~/AugmentAgent && ./target/release/augmentagent linkedin login --cookies-json ~/linkedin-cookies.json && rm ~/linkedin-cookies.json'

Or re-run this script with LINKEDIN_SSH_TARGET=<user>@<host> to do it in one shot.
EOF
    exit 0
fi

REMOTE_PATH="${LINKEDIN_REMOTE_PATH:-~/linkedin-cookies.json}"
REMOTE_REPO="${LINKEDIN_REMOTE_REPO:-~/AugmentAgent}"

echo ""
echo "shipping to $LINKEDIN_SSH_TARGET ..."
scp "$OUT_LOCAL" "$LINKEDIN_SSH_TARGET:$REMOTE_PATH"

echo "running `linkedin login` on $LINKEDIN_SSH_TARGET ..."
# shellcheck disable=SC2029
ssh "$LINKEDIN_SSH_TARGET" \
    "cd $REMOTE_REPO && ./target/release/augmentagent linkedin login --cookies-json $REMOTE_PATH && rm $REMOTE_PATH"

echo ""
echo "done. remote cookies are installed; auto-updater will restart the daemon on the next rebuild cycle."
echo "optional: rm the local copy → rm '$OUT_LOCAL'"
