#!/usr/bin/env bash
# One-time bootstrap for the private knowledge-base mirror (epic #474 / KB
# sync #1). Turns wiki/ into its own git repo backed by a PRIVATE GitHub
# repo, so the owner can browse/edit the KB from anywhere. Idempotent:
# re-running is safe.
#
#   Usage: scripts/kb-mirror-bootstrap.sh [repo-name]
#          repo-name defaults to "agent-knowledge-base".
#
# Auth is the ambient `gh` login (no token in .env). The repo is created
# PRIVATE and the guard below refuses to proceed if it is ever public —
# the KB contains people's emails and personal facts.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WIKI_DIR="$REPO_ROOT/wiki"
REPO_NAME="${1:-agent-knowledge-base}"
GH="${AUGMENTAGENT_GH_BIN:-gh}"

OWNER="$("$GH" api user -q .login)"
SLUG="$OWNER/$REPO_NAME"

echo "==> KB mirror bootstrap: $SLUG (private)"

[ -d "$WIKI_DIR" ] || { echo "no wiki dir at $WIKI_DIR" >&2; exit 1; }

# 1. Create the private repo if it doesn't exist yet.
if "$GH" repo view "$SLUG" >/dev/null 2>&1; then
  echo "    repo exists"
else
  echo "    creating $SLUG ..."
  "$GH" repo create "$SLUG" --private \
    --description "AugmentAgent knowledge base (private mirror of wiki/)" >/dev/null
fi

# 2. Privacy guard — refuse to touch a public repo.
VIS="$("$GH" repo view "$SLUG" --json visibility -q .visibility)"
if [ "$VIS" != "PRIVATE" ]; then
  echo "REFUSING: $SLUG is $VIS, not PRIVATE. The KB must never be public." >&2
  exit 1
fi

# 3. Content boundary: everything under wiki/ syncs EXCEPT the daemon's
#    SQLite store, secrets, lock files, and scratch. Mirrors the hard guard
#    in `augmentagent wiki sync`.
cat > "$WIKI_DIR/.gitignore" <<'IGNORE'
# AugmentAgent private KB mirror — exclude everything that is NOT
# owner-facing markdown. The KB is markdown; anything else here is the
# daemon's store, a secret, or scratch, and must NEVER reach the mirror.
data.db
data.db-wal
data.db-shm
data.db-journal
*.lock
.env
# scratch / temp / backups the daemon leaves behind (all non-content)
*.txt
*.tmp
*.swp
log_temp.md
.scratch/
IGNORE

# 4. git init (default branch main) + ambient-gh credential helper, repo-local
#    so we don't touch the intentionally-unset global git config.
if [ ! -d "$WIKI_DIR/.git" ]; then
  git -C "$WIKI_DIR" init -b main
fi
git -C "$WIKI_DIR" config "credential.https://github.com.helper" "!$GH auth git-credential"

REMOTE_URL="https://github.com/$SLUG.git"
if git -C "$WIKI_DIR" remote get-url origin >/dev/null 2>&1; then
  git -C "$WIKI_DIR" remote set-url origin "$REMOTE_URL"
else
  git -C "$WIKI_DIR" remote add origin "$REMOTE_URL"
fi

# 5. First commit + push via the daemon's own sync command (exercises the
#    real guard). --no-pull because origin/main doesn't exist yet.
echo "==> initial sync (first push)"
"$REPO_ROOT/target/release/augmentagent" --wiki-dir "$WIKI_DIR" wiki sync --no-pull

echo "==> done. Browse it at: https://github.com/$SLUG"
