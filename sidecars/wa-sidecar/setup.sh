#!/usr/bin/env bash
# Bootstrap the AugmentAgent WhatsApp sidecar.
#
# Idempotent: re-run after a `git pull` to pick up go.mod changes.
# Requires the Go toolchain pinned by go.mod.
#
# Usage:
#   sidecars/wa-sidecar/setup.sh

set -euo pipefail

cd "$(dirname "$0")"

if ! command -v go >/dev/null 2>&1; then
    echo "go toolchain not found. install the version pinned in go.mod, then re-run." >&2
    exit 1
fi

# Build with the committed module checksums and without changing dependencies.
go mod verify
go build -mod=readonly -trimpath -o wa-sidecar .

echo
echo "wa-sidecar built at: $(pwd)/wa-sidecar"
echo "next: run 'augmentagent whatsapp login --phone <number> --self-chat' or use --owner-jid for a dedicated account"
