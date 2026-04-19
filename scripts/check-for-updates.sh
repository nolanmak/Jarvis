#!/bin/bash
# Pull latest main from GitHub, rebuild if anything changed, restart the
# daemon so code updates take effect. Intended to be driven by a launchd job
# (macOS) or systemd user timer (Linux) every 5 minutes — see
# install-autoupdate.sh — but also runnable manually.
#
# Design notes:
# - We compare HEAD to origin/main. If they match, zero cost (just a fetch).
# - We only rebuild when something in crates/ changed, so wiki-only sessions
#   on the daemon side don't trigger expensive rebuilds.
# - On build failure we DO NOT restart, so a broken push doesn't take the
#   daemon down — it keeps running on the old binary until next pull fixes.
# - All output goes to a per-platform log dir for post-mortem.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

case "$(uname -s)" in
  Darwin) LOG_DIR="$HOME/Library/Logs/augmentagent" ;;
  Linux)  LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent" ;;
  *)      LOG_DIR="$HOME/.augmentagent/logs" ;;
esac
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/update.log"
LABEL="com.nolanmak.augmentagent"
SYSTEMD_UNIT="augmentagent.service"

stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }
log() { printf '%s [update] %s\n' "$(stamp)" "$*" >> "$LOG"; }

log "checking for updates"

git fetch origin main --quiet || {
  log "fetch failed"
  exit 0  # silently no-op; retry next tick
}

LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/main)

if [ "$LOCAL" = "$REMOTE" ]; then
  log "up to date ($LOCAL)"
  exit 0
fi

# If origin/main is an ancestor of HEAD (including equal), local is up-to-date
# or ahead — nothing to pull. This guards against restarting for no reason on
# a dev machine with unpushed commits.
if git merge-base --is-ancestor "$REMOTE" "$LOCAL"; then
  log "local ahead of or equal to origin/main ($LOCAL, origin at $REMOTE) — nothing to do"
  exit 0
fi

# If HEAD is not an ancestor of origin/main, the branches have diverged. A
# non-ff pull would fail anyway; bail cleanly so a developer can reconcile.
if ! git merge-base --is-ancestor "$LOCAL" "$REMOTE"; then
  log "LOCAL ($LOCAL) and origin/main ($REMOTE) have diverged — manual reconcile required"
  exit 0
fi

log "update available: $LOCAL -> $REMOTE"

# What changed? Decide whether a rebuild is needed.
CHANGED_FILES=$(git diff --name-only "$LOCAL" "$REMOTE")
NEEDS_REBUILD=0
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(crates/|Cargo\.(toml|lock)|rust-toolchain\.toml)'; then
  NEEDS_REBUILD=1
fi

log "pulling"
if ! git pull --ff-only origin main >> "$LOG" 2>&1; then
  log "pull failed (non-fast-forward or conflict) — abandoning update"
  exit 0
fi

if [ "$NEEDS_REBUILD" -eq 1 ]; then
  log "rebuilding (changed files touched crates/ or Cargo)"
  if ! cargo build --release -p augmentagent-cli >> "$LOG" 2>&1; then
    log "BUILD FAILED — not restarting; daemon stays on previous binary"
    exit 1
  fi
  log "build ok"
else
  log "no rust code changed; skipping rebuild"
fi

# Restart daemon so the new binary / config takes effect.
case "$(uname -s)" in
  Darwin)
    if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
      log "restarting daemon via launchctl kickstart"
      launchctl kickstart -k "gui/$(id -u)/$LABEL" >> "$LOG" 2>&1 || log "kickstart failed"
    else
      log "daemon not registered under launchd ($LABEL) — run install-autostart.sh manually"
    fi
    ;;
  Linux)
    if systemctl --user list-unit-files "$SYSTEMD_UNIT" 2>/dev/null | grep -q "$SYSTEMD_UNIT"; then
      log "restarting daemon via systemctl --user restart $SYSTEMD_UNIT"
      systemctl --user restart "$SYSTEMD_UNIT" >> "$LOG" 2>&1 || log "systemctl restart failed"
    else
      log "daemon not registered under systemd ($SYSTEMD_UNIT) — run install-autostart.sh manually"
    fi
    ;;
  *)
    log "no restart strategy for $(uname -s) — restart the daemon manually"
    ;;
esac

log "update complete: now at $REMOTE"
