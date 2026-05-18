#!/usr/bin/env bash
# uninstall-tenant.sh — tear down a multi-tenant AugmentAgent instance.
# Linux-only. Does NOT touch the prod agent or any other tenant.
#
#   ./scripts/uninstall-tenant.sh <tenant-name> [--purge]
#
# By default the tenant's data dir (db, wiki, tenant.env) is KEPT so you can
# re-install without re-provisioning. Pass --purge to also delete it.

set -euo pipefail

log() { printf '\033[1;36m[uninstall-tenant]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[uninstall-tenant] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

TENANT="${1:-}"
PURGE="${2:-}"
[ -n "$TENANT" ] || die "usage: ./scripts/uninstall-tenant.sh <tenant-name> [--purge]"

UNIT_NAME="augmentagent-tenant-${TENANT}.service"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
DATA_DIR="$HOME/.local/share/augmentagent-tenant-${TENANT}"

if [ "$(uname -s)" != "Linux" ] || ! command -v systemctl >/dev/null 2>&1; then
  log "systemd not available — nothing to do"
  exit 0
fi

if systemctl --user list-unit-files "$UNIT_NAME" 2>/dev/null | grep -q "$UNIT_NAME"; then
  log "Stopping + disabling $UNIT_NAME"
  systemctl --user disable --now "$UNIT_NAME" || true
else
  log "Unit $UNIT_NAME not loaded"
fi

if [ -f "$UNIT_DIR/$UNIT_NAME" ]; then
  log "Removing $UNIT_DIR/$UNIT_NAME"
  rm "$UNIT_DIR/$UNIT_NAME"
fi

systemctl --user daemon-reload

if [ "$PURGE" = "--purge" ]; then
  if [ -d "$DATA_DIR" ]; then
    log "Purging data dir $DATA_DIR (db, wiki, tenant.env)"
    rm -rf "$DATA_DIR"
  fi
else
  log "Kept data dir $DATA_DIR (re-install reuses it; pass --purge to delete)"
fi

log "Done."
