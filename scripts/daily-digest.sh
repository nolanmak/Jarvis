#!/bin/bash
# Wrapper invoked by launchd/systemd each morning. Mounts the vault if
# applicable, then runs `augmentagent digest --post-discord`.
#
# Safe to run manually to preview/regenerate a digest at any time.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Best-effort vault mount (macOS sparse bundle). No-op everywhere else.
./scripts/vault-mount.sh || true

# Load .env (secrets: COMPOSIO_API_KEY, DISCORD_*). The `set -a` exports
# everything we source so the binary inherits it.
if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

exec ./target/release/augmentagent \
  --wiki-dir ./wiki \
  digest --since 24 --post-discord true
