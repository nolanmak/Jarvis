#!/usr/bin/env bash
# Bootstrap the AugmentAgent WhatsApp sidecar.
#
# Idempotent: re-run after a `git pull` to pick up go.mod changes.
# Requires a Go toolchain (>= 1.22). This host currently has none — the
# Rust side + JSON-RPC contract + mock-socket tests are complete and green;
# this build step is deferred until Go is installed (see #74).
#
# Usage:
#   sidecars/wa-sidecar/setup.sh

set -euo pipefail

cd "$(dirname "$0")"

if ! command -v go >/dev/null 2>&1; then
    echo "go toolchain not found. install Go >= 1.22, then re-run." >&2
    echo "  (Rust side is complete; only the sidecar binary build is pending.)" >&2
    exit 1
fi

# Resolve exact dependency versions + generate go.sum.
go mod tidy

# Build the static-ish sidecar binary next to this script.
go build -o wa-sidecar .

echo
echo "wa-sidecar built at: $(pwd)/wa-sidecar"
echo "next: pair a device with  augmentagent whatsapp login --phone <number>"
echo "then install systemd unit  systemd/augmentagent-wa-sidecar.service"
