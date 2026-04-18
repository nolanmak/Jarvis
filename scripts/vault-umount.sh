#!/bin/bash
# Cleanly detach the encrypted vault.
#
# Usage: ./scripts/vault-umount.sh

set -euo pipefail

MOUNT_POINT="${AUGMENTAGENT_MOUNT_POINT:-/Volumes/augmentagent}"

log() { printf '\033[1;36m[vault-umount]\033[0m %s\n' "$*" >&2; }

if ! mount | grep -q " on $MOUNT_POINT "; then
  log "Not mounted at $MOUNT_POINT"
  exit 0
fi

log "Detaching $MOUNT_POINT"
hdiutil detach "$MOUNT_POINT"
log "Detached"
