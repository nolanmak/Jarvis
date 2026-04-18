#!/bin/bash
# pm2 wrapper for the Node dashboard. Ensures the encrypted vault is mounted
# before exec'ing the dashboard (which reads data.db through the symlink).
#
# Usage (directly): ./scripts/run-dashboard.sh
# Usage (pm2):       script: "./scripts/run-dashboard.sh"

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

./scripts/vault-mount.sh
exec node dist/dashboard-server.js
