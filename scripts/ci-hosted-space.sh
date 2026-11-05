#!/usr/bin/env bash
set -euo pipefail

# Snapshot/recovery tests retain the production free-space reserve. The hosted
# Ubuntu image includes large mobile/.NET/Haskell/CodeQL SDKs this Rust job
# never uses. Targeted runs retain several crates' test executables, so reclaim
# these before compilation as well as preserving the snapshot reserve itself.
# Never remove host software when this command is replayed by local CI.
if [[ "${GITHUB_ACTIONS:-}" != true || "${RUNNER_ENVIRONMENT:-}" != github-hosted || "${RUNNER_OS:-}" != Linux ]]; then
  exit 0
fi

df -h .
sudo rm -rf -- /usr/local/lib/android /usr/share/dotnet \
  /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL
df -h .
