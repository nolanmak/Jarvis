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
exec ./target/release/augmentagent "$@"
