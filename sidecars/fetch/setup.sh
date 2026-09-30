#!/usr/bin/env bash
# Reproducible fetch sidecar build for macOS and Linux.
set -euo pipefail
cd "$(dirname "$0")"
npm ci
npm run build
if [ "${SKIP_CHROMIUM:-0}" != 1 ]; then
  npx playwright install chromium
fi
printf 'fetch sidecar built; on macOS run python3 scripts/install-sidecar.py fetch from the repo root\n'
