#!/bin/bash
# Restore an AugmentAgent runtime snapshot produced by migrate-export.sh.
#
# Usage: ./scripts/migrate-import.sh <path-to-migrate-*.tar.gz>

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

IN="${1:-}"
[ -n "$IN" ] || { echo "Usage: $0 <migrate-*.tar.gz>" >&2; exit 1; }
[ -f "$IN" ] || { echo "archive not found: $IN" >&2; exit 1; }

STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT

log() { printf '\033[1;36m[migrate-import]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[migrate-import ERR]\033[0m %s\n' "$*" >&2; exit 1; }

log "Extracting $IN"
tar -xzf "$IN" -C "$STAGING"

# Refuse to overwrite a populated data.db/wiki unless user confirms.
OVERWRITE_RISK=""
[ -f ./data.db ] && OVERWRITE_RISK+="./data.db "
[ -d ./wiki ] && [ -n "$(ls -A ./wiki 2>/dev/null)" ] && OVERWRITE_RISK+="./wiki/ "

if [ -n "$OVERWRITE_RISK" ]; then
  log "Warning: will overwrite: $OVERWRITE_RISK"
  printf '\nProceed? [y/N] '
  read -r ans
  [ "$ans" = "y" ] || [ "$ans" = "Y" ] || die "aborted by user"
fi

if [ -f "$STAGING/data.db" ]; then
  log "Restoring data.db"
  cp "$STAGING/data.db" ./data.db
fi

if [ -d "$STAGING/wiki" ]; then
  log "Restoring wiki/"
  rm -rf ./wiki
  cp -R "$STAGING/wiki" ./wiki
  log "  pages: $(find ./wiki -type f -name '*.md' | wc -l | tr -d ' ')"
fi

if [ -d "$STAGING/skills/email-triage/learned" ]; then
  log "Restoring skills/email-triage/learned/"
  mkdir -p ./skills/email-triage
  rm -rf ./skills/email-triage/learned
  cp -R "$STAGING/skills/email-triage/learned" ./skills/email-triage/learned
fi

if [ -f "$STAGING/skills/email-triage/config.json" ]; then
  log "Restoring skills/email-triage/config.json"
  cp "$STAGING/skills/email-triage/config.json" ./skills/email-triage/config.json
fi

log "Done. Next steps:"
log "  1. Copy .env to $REPO_ROOT/.env (secrets — use a secure channel)"
log "  2. claude login"
log "  3. cargo build --release -p augmentagent-cli"
log "  4. ./scripts/install-autostart.sh"
log "  5. ./scripts/install-autoupdate.sh  (optional, for GitHub auto-pull)"
