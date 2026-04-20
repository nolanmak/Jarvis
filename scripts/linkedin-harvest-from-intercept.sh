#!/usr/bin/env bash
# linkedin-harvest-from-intercept.sh — lift LinkedIn cookies straight from the
# Claude Intercept MITM proxy's capture DB. Zero prompts, zero devtools.
#
# Only works if Claude Intercept previously captured your logged-in LinkedIn
# traffic (e.g. after running /intercept + browsing linkedin.com).
#
# Usage:
#   ./scripts/linkedin-harvest-from-intercept.sh
#   LINKEDIN_SSH_TARGET=nolan@host ./scripts/linkedin-harvest-from-intercept.sh
#
# Produces the same JSON shape as ./linkedin-harvest.sh — both feed
# `augmentagent linkedin login`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
OUT_LOCAL="${OUT_LOCAL:-$REPO_ROOT/linkedin-auth.json}"
CAPTURES_DB="${CAPTURES_DB:-/Users/nolanmakatche/claude_intercept/captures/captures.db}"

if [[ ! -f "$CAPTURES_DB" ]]; then
    echo "error: captures db not found at $CAPTURES_DB" >&2
    echo "       set CAPTURES_DB=<path> or run /intercept first" >&2
    exit 1
fi

# Need python3 for JSON parsing of request_headers (which is itself JSON).
if ! command -v python3 >/dev/null; then
    echo "error: python3 required for JSON extraction" >&2
    exit 1
fi

# Pull the most recent voyager/messaging request — those always carry the
# full cookie header, the csrf token, and reference the member URN.
python3 - "$CAPTURES_DB" > "$OUT_LOCAL.tmp" <<'PY'
import json
import re
import sqlite3
import sys
import time

db_path = sys.argv[1]
con = sqlite3.connect(db_path)
row = con.execute(
    """
    SELECT request_headers, url, request_body
    FROM captures
    WHERE host='www.linkedin.com'
      AND request_headers LIKE '%li_at%'
      AND request_headers LIKE '%JSESSIONID%'
      AND (path LIKE '%voyager%' OR path LIKE '%messaging%')
    ORDER BY timestamp DESC
    LIMIT 1
    """
).fetchone()
if not row:
    sys.stderr.write(
        "no LinkedIn voyager/messaging capture with auth cookies in db\n"
    )
    sys.exit(2)

hdrs_json, url, body = row
try:
    hdrs = json.loads(hdrs_json)
except json.JSONDecodeError as e:
    sys.stderr.write(f"request_headers is not valid JSON: {e}\n")
    sys.exit(3)

cookie_header = hdrs.get("cookie") or hdrs.get("Cookie")
if not cookie_header:
    sys.stderr.write("no cookie header on that request\n")
    sys.exit(4)

# Parse `a=b; c=d; ...` into a dict. Values may contain `=`, so split once.
jar = {}
for part in cookie_header.split(";"):
    part = part.strip()
    if not part or "=" not in part:
        continue
    k, _, v = part.partition("=")
    jar[k.strip()] = v.strip()

need = ["li_at", "JSESSIONID", "bcookie"]
missing = [c for c in need if c not in jar]
if missing:
    sys.stderr.write(f"missing cookies in jar: {missing}\n")
    sys.exit(5)

# Member URN: search request body + URL for urn:li:fsd_profile:ACoAA...
# URLs usually carry the URL-encoded form (urn%3Ali%3Afsd_profile%3A), so
# try both before falling back to other captures.
import urllib.parse

def find_urn(blob):
    if not blob:
        return None
    decoded = urllib.parse.unquote(blob)
    m = re.search(r"urn:li:fsd_profile:[A-Za-z0-9_\-]+", decoded)
    return m.group(0) if m else None

member_urn = find_urn(url) or find_urn(body)
if not member_urn:
    # Fall back: any recent LinkedIn URL with mailboxUrn.
    row2 = con.execute(
        """
        SELECT url FROM captures
        WHERE host='www.linkedin.com' AND url LIKE '%mailboxUrn%'
        ORDER BY timestamp DESC LIMIT 1
        """
    ).fetchone()
    if row2:
        member_urn = find_urn(row2[0])
if not member_urn:
    sys.stderr.write("could not locate member_urn in captured traffic\n")
    sys.exit(6)

ua = hdrs.get("user-agent") or hdrs.get("User-Agent") or (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36"
)

out = {
    "member_urn": member_urn,
    "cookies": {k: jar[k] for k in need},
    "user_agent": ua,
    "harvested_at_ms": int(time.time() * 1000),
}
json.dump(out, sys.stdout, indent=2)
print()
PY

mv "$OUT_LOCAL.tmp" "$OUT_LOCAL"
chmod 600 "$OUT_LOCAL"

MEMBER="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["member_urn"])' "$OUT_LOCAL")"

echo ""
echo "extracted from $CAPTURES_DB"
echo "wrote         $OUT_LOCAL"
echo "member_urn    $MEMBER"

# --- transport to the daemon host ---

if [[ -z "${LINKEDIN_SSH_TARGET:-}" ]]; then
    cat <<EOF

Cookies stayed on this machine. To install on the daemon host:

  scp '$OUT_LOCAL' nolan-makatche@100.91.92.24:~/linkedin-cookies.json
  ssh nolan-makatche@100.91.92.24 \\
      'cd ~/AugmentAgent && ./target/release/augmentagent linkedin login --cookies-json ~/linkedin-cookies.json && rm ~/linkedin-cookies.json'

Or re-run with LINKEDIN_SSH_TARGET=<user>@<host> to do it in one shot.
EOF
    exit 0
fi

REMOTE_PATH="${LINKEDIN_REMOTE_PATH:-~/linkedin-cookies.json}"
REMOTE_REPO="${LINKEDIN_REMOTE_REPO:-~/AugmentAgent}"

echo ""
echo "shipping to $LINKEDIN_SSH_TARGET ..."
scp "$OUT_LOCAL" "$LINKEDIN_SSH_TARGET:$REMOTE_PATH"

echo "running linkedin login on $LINKEDIN_SSH_TARGET ..."
# shellcheck disable=SC2029
ssh "$LINKEDIN_SSH_TARGET" \
    "cd $REMOTE_REPO && ./target/release/augmentagent linkedin login --cookies-json $REMOTE_PATH && rm $REMOTE_PATH"

echo ""
echo "done. auto-updater will pick up the new binary + cookies on next cycle."
