#!/usr/bin/env bash
# Bootstrap the AugmentAgent renderer sidecar.
#
# Idempotent: re-run safely after a `git pull` to pick up package.json
# changes. Pinning Remotion happens in package.json; this script installs
# node_modules and downloads the Chrome Headless Shell Remotion uses for
# headless frame extraction (~150 MB, separate from the browser sidecar's
# Playwright/system Chromium — Remotion manages its own).
#
# Usage:
#   sidecars/renderer/setup.sh

set -euo pipefail

cd "$(dirname "$0")"

# npm ci needs a lockfile; on a fresh checkout there isn't one (it's
# gitignored), so fall back to `npm install` which generates it.
if [[ -f package-lock.json ]]; then
    npm ci
else
    npm install
fi

# Chrome Headless Shell for Remotion's renderer. Skip with SKIP_CHROMIUM=1
# in dev (renders will fail until it's present). We call `ensureBrowser()`
# from @remotion/renderer directly rather than the `remotion` CLI — the CLI
# lives in the separate @remotion/cli package which this sidecar doesn't
# depend on (we only need bundler + renderer).
if [[ "${SKIP_CHROMIUM:-0}" != "1" ]]; then
    node --input-type=module -e \
        "import('@remotion/renderer').then(m => m.ensureBrowser()).then(() => console.log('Chrome Headless Shell ready'))"
fi

echo
echo "renderer sidecar ready at: $(pwd)"
echo "next step: install systemd unit (systemd/augmentagent-renderer.service)"
