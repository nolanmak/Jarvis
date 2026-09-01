#!/usr/bin/env bash
# install-tenant.sh — register an ISOLATED multi-tenant AugmentAgent instance
# as a systemd user service. Linux-only (this host has no macOS counterpart).
#
#   ./scripts/install-tenant.sh <tenant-name>
#
# A tenant is a second+ daemon for ANOTHER Discord server, NOT hooked to email.
# It runs `serve --no-email true` against its OWN sqlite db + wiki + env, so it
# shares zero state with the production agent (augmentagent.service). The prod
# agent is never touched by this script.
#
# Idempotent: re-running rewrites the unit but never overwrites an existing
# tenant.env (your secrets are preserved).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

log() { printf '\033[1;34m[install-tenant]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-tenant] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

TENANT="${1:-}"
[ -n "$TENANT" ] || die "usage: ./scripts/install-tenant.sh <tenant-name>"
case "$TENANT" in
  *[!a-z0-9-]*|-*|*-) die "tenant name must be lowercase [a-z0-9-], not start/end with '-' (got: $TENANT)" ;;
esac

[ "$(uname -s)" = "Linux" ] || die "Linux-only (no macOS counterpart on this host)"
command -v systemctl >/dev/null 2>&1 || die "systemctl not found — this installer needs systemd"
BIN="$REPO_ROOT/target/release/augmentagent"
[ -x "$BIN" ] || die "release binary missing: $BIN (run: cargo build --release -p augmentagent-cli)"

UNIT_NAME="augmentagent-tenant-${TENANT}.service"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT="$UNIT_DIR/$UNIT_NAME"
DATA_DIR="$HOME/.local/share/augmentagent-tenant-${TENANT}"
LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent-tenant-${TENANT}"
ENV_FILE="$DATA_DIR/tenant.env"
SERVICE_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
NODE_BIN="$(command -v node 2>/dev/null || echo /usr/bin/node)"
# #902 — same cgroup + rlimit ceilings as the prod unit (see
# install-autostart.sh header); a tenant is the same binary with the same
# fan-out paths. Overridable per host at install time.
MEMORY_HIGH="${AUGMENTAGENT_UNIT_MEMORY_HIGH:-5G}"
MEMORY_MAX="${AUGMENTAGENT_UNIT_MEMORY_MAX:-6G}"
TASKS_MAX="${AUGMENTAGENT_UNIT_TASKS_MAX:-512}"
NOFILE="${AUGMENTAGENT_UNIT_NOFILE:-4096}"

mkdir -p "$UNIT_DIR" "$DATA_DIR" "$LOG_DIR" "$DATA_DIR/wiki"

FRESH_ENV=0
if [ ! -f "$ENV_FILE" ]; then
  FRESH_ENV=1
  log "Writing tenant.env SKELETON: $ENV_FILE (fill in the secrets)"
  cat > "$ENV_FILE" <<ENV_EOF
# Per-tenant config for AugmentAgent tenant "${TENANT}".
# This file holds secrets — it is NOT tracked by git and NOT overwritten on
# re-install. Fill the blanks, then: systemctl --user restart ${UNIT_NAME}

# Isolated sqlite store for this tenant (also passed via --db).
AUGMENTAGENT_DB=${DATA_DIR}/data.db

# Discord: REUSE the prod bot token; only the channel differs. Invite the
# existing bot to the other server and put one of ITS channel ids here.
DISCORD_BOT_TOKEN=
DISCORD_CHANNEL_ID=

# Composio: reuse the existing key (Composio isolates per tenant by entity id).
COMPOSIO_API_KEY=

# Meetup channel shells out to node; absolute path so the unit's PATH is moot.
AUGMENTAGENT_NODE_BIN=${NODE_BIN}
ENV_EOF
  chmod 600 "$ENV_FILE"
else
  log "Keeping existing tenant.env (secrets preserved): $ENV_FILE"
fi

log "Writing unit: $UNIT"
cat > "$UNIT" <<UNIT_EOF
[Unit]
Description=AugmentAgent tenant (${TENANT}) — no email; Discord/GitHub/Meetup/Drive
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$REPO_ROOT
EnvironmentFile=$ENV_FILE
Environment=PATH=$SERVICE_PATH
ExecStart=$BIN --db $DATA_DIR/data.db --wiki-dir $DATA_DIR/wiki serve --no-email true --dry-run false --interval-secs 120
Restart=on-failure
RestartSec=10
# #902 — die alone: killed inside this cgroup, never the desktop session.
MemoryHigh=$MEMORY_HIGH
MemoryMax=$MEMORY_MAX
MemorySwapMax=0
TasksMax=$TASKS_MAX
LimitNOFILE=$NOFILE
OOMPolicy=kill
StandardOutput=append:$LOG_DIR/stdout.log
StandardError=append:$LOG_DIR/stderr.log

[Install]
WantedBy=default.target
UNIT_EOF

log "Reloading systemd --user"
systemctl --user daemon-reload
systemctl --user enable "$UNIT_NAME" >/dev/null

if [ "$FRESH_ENV" -eq 1 ]; then
  log "NOT starting yet — tenant.env is a skeleton. Provision it first:"
else
  log "Restarting $UNIT_NAME"
  systemctl --user restart "$UNIT_NAME" || log "restart failed — check: journalctl --user -u $UNIT_NAME"
fi

if ! loginctl show-user "$USER" 2>/dev/null | grep -q "Linger=yes"; then
  log "NOTE: linger is OFF — tenant only runs while you're logged in."
  log "      Run 24/7: sudo loginctl enable-linger $USER"
fi

cat >&2 <<NEXT

[install-tenant] Provisioning checklist for "${TENANT}":
  1. Edit ${ENV_FILE}: set DISCORD_BOT_TOKEN (= prod bot), DISCORD_CHANNEL_ID
     (a channel in the other server — invite the existing bot there first),
     COMPOSIO_API_KEY (= existing key).
  2. GitHub repos:   $BIN --db $DATA_DIR/data.db github login --token <PAT> --login <user>
                      $BIN --db $DATA_DIR/data.db github subscribe owner/repo --mode priority
  3. Meetup:         $BIN --db $DATA_DIR/data.db meetup subscribe <group-urlname> --mode digest
  4. Google Drive:   run the dashboard with AUGMENTAGENT_DB=$DATA_DIR/data.db and click
                      "Connect Google Drive" (creates the tenant's drive_accounts row).
  5. Start it:       systemctl --user restart ${UNIT_NAME}
                      journalctl --user -u ${UNIT_NAME} -f
  Uninstall:         ./scripts/uninstall-tenant.sh ${TENANT}

This tenant shares NO state with the prod agent (separate db/env/unit).
NEXT
