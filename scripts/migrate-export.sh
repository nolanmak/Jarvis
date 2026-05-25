#!/bin/bash
# Export a portable snapshot of the AugmentAgent runtime state for moving
# to a different machine.
#
# What goes in the tarball:
#   data.db                                        (VACUUM INTO snapshot)
#   wiki/                                          (Claude-maintained pages)
#   skills/email-triage/learned/*.json             (learned skip/flag patterns)
#   skills/email-triage/config.json (if present)   (runtime skill config)
#
# What is NOT included (deliberately):
#   .env                        — transfer via a secure channel yourself
#   target/, node_modules/      — rebuilt on the new machine
#   ~/augmentagent-vault.sparsebundle — if you use the encrypted vault, copy
#                                        it separately (it lives outside the repo)
#
# Usage: ./scripts/migrate-export.sh [output_path.tar.gz]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="${1:-./migrate-${STAMP}.tar.gz}"
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT

log() { printf '\033[1;36m[migrate-export]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[migrate-export ERR]\033[0m %s\n' "$*" >&2; exit 1; }

# 1. data.db (via VACUUM INTO — consistent snapshot even if daemon is live)
if [ -f ./data.db ]; then
  log "Snapshotting data.db"
  sqlite3 ./data.db "VACUUM INTO '$STAGING/data.db'" || die "sqlite VACUUM INTO failed"
  log "  size: $(du -h "$STAGING/data.db" | cut -f1)"
else
  log "No data.db found — skipping"
fi

# 2. wiki/
if [ -d ./wiki ]; then
  log "Copying wiki/"
  cp -R ./wiki "$STAGING/wiki"
  log "  pages: $(find "$STAGING/wiki" -type f -name '*.md' | wc -l | tr -d ' ')"
else
  log "No wiki/ found — skipping"
fi

# 3. skills/email-triage/learned/
if [ -d ./skills/email-triage/learned ]; then
  log "Copying skills/email-triage/learned/"
  mkdir -p "$STAGING/skills/email-triage"
  cp -R ./skills/email-triage/learned "$STAGING/skills/email-triage/learned"
fi

# 4. skills/email-triage/config.json (if present)
if [ -f ./skills/email-triage/config.json ]; then
  log "Copying skills/email-triage/config.json"
  mkdir -p "$STAGING/skills/email-triage"
  cp ./skills/email-triage/config.json "$STAGING/skills/email-triage/config.json"
fi

# 5. Meta: a README explaining what this archive is.
cat > "$STAGING/MIGRATE_README.txt" <<EOF
AugmentAgent runtime snapshot

Exported:   $STAMP
Host:       $(hostname -s)
Repo HEAD:  $(git rev-parse HEAD 2>/dev/null || echo "n/a")
Branch:     $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "n/a")

Contents:
$(cd "$STAGING" && find . -type f | sed 's|^\./|  |' | sort)

Restore on the target machine:
  1. Clone this repo: git clone https://github.com/nolanmak/MyAgentAssistant.git AugmentAgent
  2. cd AugmentAgent
  3. ./scripts/migrate-import.sh <path-to-this-tarball>
  4. Copy .env over separately (secure channel — Tailscale file cp, AirDrop, 1Password, etc.)
  5. claude login  (Max session auth required by the daemon)
  6. cargo build --release -p augmentagent-cli
  7. ./scripts/install-autostart.sh   (optional: autostart on login)
  8. ./scripts/install-autoupdate.sh  (optional: auto-pull from GitHub)

See docs/MIGRATION.md for the full walkthrough.
EOF

# 6. Tar it up.
log "Packing tarball → $OUT"
tar -czf "$OUT" -C "$STAGING" .
log "Exported $(du -h "$OUT" | cut -f1) to $OUT"
log "Next: transfer to target machine, then run ./scripts/migrate-import.sh $OUT on the other side."
