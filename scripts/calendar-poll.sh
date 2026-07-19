#!/bin/bash
# Wrapper invoked by launchd/systemd on an interval. Runs one Google
# Calendar -> wiki Meeting log poll cycle (#82) and exits.
#
# Safe to run manually at any time — poll-once is idempotent per event.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Best-effort vault mount (macOS sparse bundle). No-op everywhere else.
./scripts/vault-mount.sh || true

# Load .env (secrets: COMPOSIO_API_KEY). The `set -a` exports everything we
# source so the binary inherits it.
if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

# Live/dry toggle. poll-once defaults to dry-run; set
# AUGMENTAGENT_CALENDAR_DRY_RUN=false in .env to enable real wiki ingest
# and Discord alerts (#396/#397).
exec ./target/release/augmentagent \
  --wiki-dir ./wiki \
  calendar poll-once --dry-run "${AUGMENTAGENT_CALENDAR_DRY_RUN:-true}"
