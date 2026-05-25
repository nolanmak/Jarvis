#!/usr/bin/env bash
# Install grocery sidecar deps + Playwright Chromium.
set -euo pipefail
cd "$(dirname "$0")"
npm install
npx playwright install chromium
echo "grocery sidecar deps installed."
