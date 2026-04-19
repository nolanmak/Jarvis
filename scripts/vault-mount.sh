#!/bin/bash
# Idempotent vault mount. Intended to run:
#   - manually after a reboot
#   - as a pm2 pre-start hook (via wrapper) before augmentagent-rs and dashboard
#
# Exits 0 if already mounted. Fails loudly if mount cannot complete.
#
# Usage: ./scripts/vault-mount.sh

set -euo pipefail

VAULT_PATH="${AUGMENTAGENT_VAULT_PATH:-$HOME/augmentagent-vault.sparsebundle}"
MOUNT_POINT="${AUGMENTAGENT_MOUNT_POINT:-/Volumes/augmentagent}"
VAULT_SERVICE="${AUGMENTAGENT_VAULT_SERVICE:-augmentagent-vault}"

log() { printf '\033[1;36m[vault-mount]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[vault-mount ERR]\033[0m %s\n' "$*" >&2; exit 1; }

# Vault is a macOS-only sparsebundle. On other platforms the daemon runs
# against plaintext ./wiki and ./data.db — same as the unconfigured-vault
# path on macOS — so this is a clean no-op.
if [ "$(uname)" != "Darwin" ]; then
  exit 0
fi

# Vault not configured = no-op. This is the "I haven't run vault-init yet"
# state; let the daemon boot against plaintext ./wiki and ./data.db.
if [ ! -e "$VAULT_PATH" ]; then
  log "vault not configured ($VAULT_PATH not found) — continuing with plaintext storage"
  exit 0
fi

# Already mounted? Exit clean.
if mount | grep -q " on $MOUNT_POINT "; then
  log "Already mounted at $MOUNT_POINT"
  exit 0
fi

# Fetch passphrase from keychain.
PASS=$(security find-generic-password -a "$USER" -s "$VAULT_SERVICE" -w 2>/dev/null || true)
if [ -z "$PASS" ]; then
  die "Passphrase not in keychain under service '$VAULT_SERVICE'. If the Mac just booted, unlock login keychain and retry."
fi

log "Attaching $VAULT_PATH"
printf '%s' "$PASS" | hdiutil attach \
  -mountpoint "$MOUNT_POINT" \
  -stdinpass \
  -nobrowse \
  "$VAULT_PATH" >/dev/null \
  || die "hdiutil attach failed"

unset PASS
log "Mounted at $MOUNT_POINT"
