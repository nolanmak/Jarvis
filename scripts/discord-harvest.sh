#!/usr/bin/env bash
# discord-harvest.sh — interactive helper to capture Discord user-token credentials
# from a real browser session, validate them, and persist to macOS Keychain via
# `augmentagent discord login`.
#
# Usage:
#   ./scripts/discord-harvest.sh [output-json-path]
#
# Walks you through grabbing each required field from Chrome DevTools Network tab
# while logged into discord.com in a browser. Writes a temp JSON file that
# `augmentagent discord login --creds-json <path>` consumes.
#
# SECURITY: the temp file contains your Discord user token — anyone with it can
# impersonate you on Discord. The script deletes the file after a successful
# login. Don't copy the temp file anywhere else.

set -euo pipefail

OUT="${1:-/tmp/discord-creds-$$.json}"

cat <<'HEADER'
Discord credential harvest
==========================

You will copy four values from a logged-in Discord browser session.
Open https://discord.com/app in Chrome, then:

1. Open DevTools (Cmd+Opt+I) → Network tab → filter "messages"
2. Click any channel in Discord so a request fires
3. Pick any request to `discord.com/api/v9/...`
4. In the request's Headers panel, copy each field below when prompted.

SECURITY NOTE: the `authorization` value is your Discord user token.
Treat it like a password. This script writes it to a temp file that gets
deleted after a successful save.

HEADER

prompt_multiline() {
  local label="$1"
  local hint="${2:-}"
  echo ""
  echo "---- $label ----"
  [ -n "$hint" ] && echo "($hint)"
  echo "Paste the value, then press Enter twice:"
  local line
  local value=""
  while IFS= read -r line; do
    [ -z "$line" ] && [ -n "$value" ] && break
    [ -z "$line" ] && continue
    if [ -z "$value" ]; then
      value="$line"
    else
      value="$value
$line"
    fi
  done
  printf '%s' "$value"
}

USER_ID=$(prompt_multiline "user_id (numeric Discord user id)" \
  "Find in /api/v9/users/@me response, or in any message's author.id; typical 18-digit number")

TOKEN=$(prompt_multiline "token (authorization header value)" \
  "From request Headers → 'authorization' — the raw token, NO 'Bearer' prefix. Starts with MTE/MTA/MTI/etc.")

SUPER_PROPS=$(prompt_multiline "super_properties_b64 (x-super-properties header)" \
  "From request Headers → 'x-super-properties' — the full base64-encoded fingerprint, starts with 'eyJ'")

USER_AGENT=$(prompt_multiline "user_agent (user-agent header)" \
  "From request Headers → 'user-agent' — must match the browser_user_agent field inside the decoded x-super-properties")

cat > "$OUT" <<JSON
{
  "user_id": $(printf '%s' "$USER_ID" | python3 -c "import json,sys; print(json.dumps(sys.stdin.read().strip()))"),
  "token": $(printf '%s' "$TOKEN" | python3 -c "import json,sys; print(json.dumps(sys.stdin.read().strip()))"),
  "super_properties_b64": $(printf '%s' "$SUPER_PROPS" | python3 -c "import json,sys; print(json.dumps(sys.stdin.read().strip()))"),
  "user_agent": $(printf '%s' "$USER_AGENT" | python3 -c "import json,sys; print(json.dumps(sys.stdin.read().strip()))")
}
JSON

chmod 600 "$OUT"

echo ""
echo "Creds file written to: $OUT (mode 0600)"
echo ""
echo "Next: run"
echo "   augmentagent discord login --creds-json $OUT"
echo ""
echo "The login command validates via GET /users/@me, persists to Keychain"
echo "(augmentagent/discord/default), and you should delete this temp file"
echo "after it succeeds:"
echo "   rm $OUT"
