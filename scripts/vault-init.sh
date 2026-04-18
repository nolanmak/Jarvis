#!/bin/bash
# One-time vault setup.
#
# Creates an AES-256 APFS sparse bundle at $VAULT_PATH and stores the passphrase
# in the macOS keychain under service $VAULT_SERVICE. Then (if requested) moves
# ./wiki and ./data.db into the mounted volume and replaces them with symlinks
# so existing CLI/dashboard paths keep working.
#
# Idempotency: if the bundle already exists, this script refuses to proceed —
# use vault-mount.sh to attach an existing bundle. Deliberate; we do not want
# silent re-creation that could orphan an encrypted volume.
#
# Usage: ./scripts/vault-init.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VAULT_PATH="${AUGMENTAGENT_VAULT_PATH:-$HOME/augmentagent-vault.sparsebundle}"
MOUNT_POINT="${AUGMENTAGENT_MOUNT_POINT:-/Volumes/augmentagent}"
VOLNAME="${AUGMENTAGENT_VOLNAME:-augmentagent}"
VAULT_SERVICE="${AUGMENTAGENT_VAULT_SERVICE:-augmentagent-vault}"
SIZE="${AUGMENTAGENT_VAULT_SIZE:-2g}"

log() { printf '\033[1;36m[vault-init]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[vault-init ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(uname)" = "Darwin" ] || die "macOS only (uses hdiutil + keychain)"

if [ -e "$VAULT_PATH" ]; then
  die "Vault already exists at $VAULT_PATH. Use vault-mount.sh or remove it first."
fi

log "Vault path:    $VAULT_PATH"
log "Mount point:   $MOUNT_POINT"
log "Keychain item: $VAULT_SERVICE"
log "Size (sparse): $SIZE (grows on demand)"
echo

# 1. Passphrase — prompt once, confirm once.
printf 'Enter passphrase for new vault: '
stty -echo; read -r PASS1; stty echo; echo
printf 'Confirm passphrase:              '
stty -echo; read -r PASS2; stty echo; echo

[ "$PASS1" = "$PASS2" ] || die "Passphrases do not match"
[ -n "$PASS1" ] || die "Empty passphrase rejected"

# 2. Store in keychain (replace if present).
log "Storing passphrase in keychain (service=$VAULT_SERVICE)"
security delete-generic-password -a "$USER" -s "$VAULT_SERVICE" >/dev/null 2>&1 || true
security add-generic-password -a "$USER" -s "$VAULT_SERVICE" -w "$PASS1" \
  || die "security add-generic-password failed"

# 3. Create encrypted sparse bundle.
log "Creating sparse bundle (AES-256, APFS)"
printf '%s' "$PASS1" | hdiutil create \
  -encryption AES-256 \
  -type SPARSEBUNDLE \
  -fs "APFS" \
  -size "$SIZE" \
  -volname "$VOLNAME" \
  -stdinpass \
  "$VAULT_PATH" >/dev/null \
  || die "hdiutil create failed"

# 4. Mount it.
log "Mounting at $MOUNT_POINT"
mkdir -p "$(dirname "$MOUNT_POINT")"
printf '%s' "$PASS1" | hdiutil attach \
  -mountpoint "$MOUNT_POINT" \
  -stdinpass \
  -nobrowse \
  "$VAULT_PATH" >/dev/null \
  || die "hdiutil attach failed"

unset PASS1 PASS2

# 5. Migrate existing wiki/ and data.db (if present as regular files/dirs, not symlinks).
cd "$REPO_ROOT"

migrate() {
  local src="$1"
  local dest="$MOUNT_POINT/$src"
  if [ -L "./$src" ]; then
    log "./$src is already a symlink — skipping"
    return
  fi
  if [ ! -e "./$src" ]; then
    log "./$src does not exist yet — skipping (will be created inside vault on first run)"
    return
  fi
  log "Migrating ./$src → $dest"
  mv "./$src" "$dest"
  ln -s "$dest" "./$src"
}

migrate "wiki"
migrate "data.db"

log "Done. Vault is mounted at $MOUNT_POINT."
log "To detach:  ./scripts/vault-umount.sh"
log "To re-mount after reboot:  ./scripts/vault-mount.sh"
