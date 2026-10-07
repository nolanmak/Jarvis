#!/bin/bash
# pm2 wrapper for the Rust daemon. Ensures the encrypted vault is mounted
# before exec'ing augmentagent.
#
# Usage (directly): ./scripts/run-rs.sh serve --dry-run false --wiki-dir ./wiki
# Usage (pm2):       script: "./scripts/run-rs.sh", args: "serve ..."

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

./scripts/vault-mount.sh
# Offline, full render preflight. Surface damage before the first PDF request,
# without turning a PDF outage into a restart loop for every other channel.
# Deployment fails closed on provisioning errors; startup never installs.
if ! python3 ./scripts/pdf-runtime.py --check; then
  printf '%s\n' 'ERROR: PDF generation unavailable; run python3 scripts/pdf-runtime.py to repair.' >&2
fi
exec ./target/release/augmentagent "$@"
