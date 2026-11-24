#!/bin/bash
# #2468 — launcher for the auto-ship worker when it is pinned. Same preflight
# as run-rs.sh, but executes the pinned build ($AUGMENTAGENT_AUTOSHIP_PINNED_BINARY,
# installed by scripts/lib/autoship-pin.sh) instead of target/release, which
# main deploys replace every few minutes.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

binary="${AUGMENTAGENT_AUTOSHIP_PINNED_BINARY:-}"
if [ -z "$binary" ] || [ ! -x "$binary" ]; then
  # Never leave auto-ship down over a missing pinned build; say so loudly.
  # Unset the pin variables so the worker, its reload watch and its readiness
  # receipt all describe the main build it actually runs.
  printf '%s\n' "WARNING: pinned auto-ship binary '${binary:-unset}' unavailable; running the main deploy build" >&2
  unset AUGMENTAGENT_AUTOSHIP_PINNED_BINARY AUGMENTAGENT_AUTOSHIP_DEPLOY_STAMP
  exec ./scripts/run-rs.sh "$@"
fi

./scripts/vault-mount.sh
if ! python3 ./scripts/pdf-runtime.py --check; then
  printf '%s\n' 'ERROR: PDF generation unavailable; run python3 scripts/pdf-runtime.py to repair.' >&2
fi
exec "$binary" "$@"
