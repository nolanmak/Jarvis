#!/usr/bin/env bash
# One-time bootstrap for the private ops archive (#1414, epic #1406): rotated
# daemon logs and audit sinks, pushed to a PRIVATE GitHub repo so past
# incidents stay readable after local retention deletes them. Idempotent.
#
#   Usage: scripts/ops-archive-bootstrap.sh [repo-name]
#          repo-name defaults to "agent-ops-archive".
#
# Auth is the ambient `gh` login (no token in .env). The repo is created
# PRIVATE and the guard below refuses to proceed if it is ever public — the
# logs contain message content. `augmentagent ops-archive sync` repeats the
# same check before every push.
set -euo pipefail

REPO_NAME="${1:-agent-ops-archive}"
GH="${AUGMENTAGENT_GH_BIN:-gh}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/augmentagent"
ARCHIVE_DIR="${AUGMENTAGENT_OPS_ARCHIVE_DIR:-$DATA_DIR/ops-archive}"
OWNER="$("$GH" api user -q .login)"
SLUG="$OWNER/$REPO_NAME"

echo "==> ops archive bootstrap: $SLUG (private) in $ARCHIVE_DIR"

# 1. Create the private repo if it doesn't exist yet.
if "$GH" repo view "$SLUG" >/dev/null 2>&1; then
  echo "    repo exists"
else
  echo "    creating $SLUG ..."
  "$GH" repo create "$SLUG" --private \
    --description "AugmentAgent ops archive (private): rotated logs and audit trails" >/dev/null
fi

# 2. Privacy guard — refuse to touch a public repo.
VIS="$("$GH" repo view "$SLUG" --json visibility -q .visibility)"
if [ "$VIS" != "PRIVATE" ]; then
  echo "REFUSING: $SLUG is $VIS, not PRIVATE. The ops archive must never be public." >&2
  exit 1
fi

# 3. Local clone: its own repo, with the ambient-gh credential helper set
#    repo-locally so the intentionally-unset global git config stays untouched.
mkdir -p "$ARCHIVE_DIR"
chmod 700 "$ARCHIVE_DIR"
if [ ! -d "$ARCHIVE_DIR/.git" ]; then
  git -C "$ARCHIVE_DIR" init -q -b main
fi
git -C "$ARCHIVE_DIR" config "credential.https://github.com.helper" "!$GH auth git-credential"
REMOTE_URL="https://github.com/$SLUG.git"
if git -C "$ARCHIVE_DIR" remote get-url origin >/dev/null 2>&1; then
  git -C "$ARCHIVE_DIR" remote set-url origin "$REMOTE_URL"
else
  git -C "$ARCHIVE_DIR" remote add origin "$REMOTE_URL"
fi

# 4. The archive only ever holds logs/<year>/<name>.<date>.gz. Belt and
#    braces with the allowlist in `ops-archive sync`: ignore everything else.
cat > "$ARCHIVE_DIR/.gitignore" <<'IGNORE'
# AugmentAgent ops archive — only rotated, compressed logs belong here.
/*
!/logs/
!/.gitignore
IGNORE

echo "==> done. Turn it on by adding this line to .env:"
echo "    AUGMENTAGENT_OPS_ARCHIVE_REMOTE=$SLUG"
echo "    Then: augmentagent ops-archive sync --dry-run"
