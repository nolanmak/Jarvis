#!/usr/bin/env bash
set -euo pipefail

# Snapshot/recovery tests retain the production free-space reserve. The hosted
# Ubuntu image includes large mobile/.NET SDKs that this Rust job never uses.
# Never remove host software when this command is replayed by local CI.
if [[ "${GITHUB_ACTIONS:-}" != true || "${RUNNER_ENVIRONMENT:-}" != github-hosted || "${RUNNER_OS:-}" != Linux ]]; then
  exit 0
fi

sudo rm -rf -- /usr/local/lib/android /usr/share/dotnet
df -h .
