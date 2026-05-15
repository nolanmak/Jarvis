#!/usr/bin/env bash
# Bootstrap the AugmentAgent browser sidecar venv.
#
# Idempotent: re-run safely after a `git pull` to pick up requirements.txt
# changes. Pinning playwright/browser-use happens in requirements.txt; this
# script only wires up the venv and downloads the matching Chromium build
# Playwright bundles (~500 MB). The downloaded Chromium is *separate* from
# the long-running headed Chromium that systemd manages — we only need
# Playwright's bundled one for the Python bindings to know which protocol
# rev to speak; CDP attach to the running browser doesn't use it.
#
# Usage:
#   sidecars/browser/setup.sh

set -euo pipefail

cd "$(dirname "$0")"

PYTHON="${PYTHON:-python3}"

if [[ ! -d .venv ]]; then
    "$PYTHON" -m venv .venv
fi

# shellcheck disable=SC1091
source .venv/bin/activate

pip install --upgrade pip
pip install -r requirements.txt

# Bundled Chromium for Playwright. Skip with SKIP_CHROMIUM=1 in dev.
if [[ "${SKIP_CHROMIUM:-0}" != "1" ]]; then
    playwright install chromium
fi

echo
echo "browser sidecar venv ready at: $(pwd)/.venv"
echo "next step: install systemd units (systemd/augmentagent-{xvfb,chromium,browser-sidecar}.service)"
