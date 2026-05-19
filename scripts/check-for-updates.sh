#!/bin/bash
# Pull latest main from GitHub, rebuild if anything changed, restart the
# daemon so code updates take effect. Intended to be driven by a launchd job
# (macOS) or systemd user timer (Linux) every 5 minutes — see
# install-autoupdate.sh — but also runnable manually.
#
# Design notes:
# - We compare HEAD to origin/main. If they match, normally zero cost (just a
#   fetch) — UNLESS the deployed artifacts were not actually built from HEAD
#   (see "build stamp" below), in which case we force a rebuild.
# - We only rebuild Rust when something in crates/ changed, so wiki-only
#   sessions on the daemon side don't trigger expensive rebuilds.
# - On build failure we DO NOT restart, so a broken push doesn't take the
#   daemon down — it keeps running on the old binary until next pull fixes.
# - All output goes to a per-platform log dir for post-mortem.
#
# Build stamp (idempotency fix):
#   Idempotency used to be keyed purely on `HEAD == origin/main`. If the
#   checkout was reconciled to origin OUT OF BAND (e.g. a force-push / history
#   rewrite on main left the local copy "diverged", and a human or another
#   process later reset it to origin) the script would then see HEAD == origin
#   forever and NEVER rebuild — silently serving a stale binary indefinitely.
#   We now also record the commit the artifacts were last built from in
#   $STAMP. The invariant the script enforces is "the running artifacts were
#   built from the current HEAD", not merely "HEAD matches origin".

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
STAMP="$LOG_DIR/built-commit"   # SHA the deployed artifacts were last built from
LABEL="com.nolanmak.augmentagent"
SYSTEMD_UNIT="augmentagent.service"
DASHBOARD_LABEL="com.nolanmak.augmentagent-dashboard"
DASHBOARD_SYSTEMD_UNIT="augmentagent-dashboard.service"

stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }
log() { printf '%s [update] %s\n' "$(stamp)" "$*" >> "$LOG"; }

# Rebuild the side(s) flagged by NEEDS_REBUILD / NEEDS_DASHBOARD_REBUILD, then
# restart the corresponding services. On Rust build failure we exit non-zero
# WITHOUT restarting and WITHOUT advancing the stamp, so the daemon keeps
# running the previous good binary and the next tick retries. On success the
# stamp is advanced to $TARGET so a subsequent tick is a true no-op.
apply_update() {
  local TARGET="$1"

  if [ "$NEEDS_REBUILD" -eq 1 ]; then
    log "rebuilding rust (changed files touched crates/ or Cargo)"
    if ! cargo build --release -p augmentagent-cli >> "$LOG" 2>&1; then
      log "RUST BUILD FAILED — not restarting; daemon stays on previous binary"
      exit 1
    fi
    log "rust build ok"
  else
    log "no rust code changed; skipping rust rebuild"
  fi

  if [ "$NEEDS_DASHBOARD_REBUILD" -eq 1 ]; then
    log "rebuilding dashboard (changed files touched src/, views/, or package.json)"
    if ! command -v npm >/dev/null 2>&1; then
      log "npm not found on PATH; skipping dashboard rebuild — UI will be stale until manual rebuild"
    elif ! (npm install --production=false >> "$LOG" 2>&1 && npm run build >> "$LOG" 2>&1); then
      log "DASHBOARD BUILD FAILED — leaving previous build in place"
    else
      log "dashboard build ok"
    fi
  else
    log "no dashboard code changed; skipping dashboard rebuild"
  fi

  # Restart services so the new binary / config takes effect.
  case "$(uname -s)" in
    Darwin)
      if [ "$NEEDS_REBUILD" -eq 1 ]; then
        if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
          log "restarting daemon via launchctl kickstart"
          launchctl kickstart -k "gui/$(id -u)/$LABEL" >> "$LOG" 2>&1 || log "kickstart failed"
        else
          log "daemon not registered under launchd ($LABEL) — run install-autostart.sh manually"
        fi
      fi
      if [ "$NEEDS_DASHBOARD_REBUILD" -eq 1 ]; then
        if launchctl print "gui/$(id -u)/$DASHBOARD_LABEL" >/dev/null 2>&1; then
          log "restarting dashboard via launchctl kickstart"
          launchctl kickstart -k "gui/$(id -u)/$DASHBOARD_LABEL" >> "$LOG" 2>&1 || log "dashboard kickstart failed"
        else
          log "dashboard not registered under launchd ($DASHBOARD_LABEL) — run install-dashboard.sh manually"
        fi
      fi
      ;;
    Linux)
      if [ "$NEEDS_REBUILD" -eq 1 ]; then
        if systemctl --user list-unit-files "$SYSTEMD_UNIT" 2>/dev/null | grep -q "$SYSTEMD_UNIT"; then
          log "restarting daemon via systemctl --user restart $SYSTEMD_UNIT"
          systemctl --user restart "$SYSTEMD_UNIT" >> "$LOG" 2>&1 || log "systemctl restart failed"
        else
          log "daemon not registered under systemd ($SYSTEMD_UNIT) — run install-autostart.sh manually"
        fi
      fi
      if [ "$NEEDS_DASHBOARD_REBUILD" -eq 1 ]; then
        if systemctl --user list-unit-files "$DASHBOARD_SYSTEMD_UNIT" 2>/dev/null | grep -q "$DASHBOARD_SYSTEMD_UNIT"; then
          log "restarting dashboard via systemctl --user restart $DASHBOARD_SYSTEMD_UNIT"
          systemctl --user restart "$DASHBOARD_SYSTEMD_UNIT" >> "$LOG" 2>&1 || log "systemctl dashboard restart failed"
        else
          log "dashboard not registered under systemd ($DASHBOARD_SYSTEMD_UNIT) — run install-dashboard.sh manually"
        fi
      fi
      # Multi-tenant: tenant units run the same target/release/augmentagent
      # binary, so a Rust rebuild means they need a bounce too. Additive +
      # best-effort: prod restart above already completed; a tenant failure
      # here is logged, never fatal. Zero tenant units ⇒ loop no-ops (the
      # prod agent's behavior is unchanged).
      if [ "$NEEDS_REBUILD" -eq 1 ]; then
        while read -r tunit; do
          [ -n "$tunit" ] || continue
          log "restarting tenant unit $tunit"
          systemctl --user restart "$tunit" >> "$LOG" 2>&1 \
            || log "tenant restart $tunit failed (continuing)"
        done < <(systemctl --user list-unit-files 'augmentagent-tenant-*.service' --no-legend 2>/dev/null | awk '{print $1}')
      fi
      ;;
    *)
      log "no restart strategy for $(uname -s) — restart the daemon + dashboard manually"
      ;;
  esac

  # Record what we successfully built from so the next tick is a true no-op
  # (and so an out-of-band checkout move is detected, not silently ignored).
  printf '%s\n' "$TARGET" > "$STAMP"
  log "update complete: now at $TARGET (build stamp written)"
}

log "checking for updates"

git fetch origin main --quiet || {
  log "fetch failed"
  exit 0  # silently no-op; retry next tick
}

LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/main)
BUILT=$(cat "$STAMP" 2>/dev/null || true)

if [ "$LOCAL" = "$REMOTE" ]; then
  # Checkout is at origin/main. Normally nothing to do — UNLESS the deployed
  # artifacts were never actually built from this commit (out-of-band
  # reconcile after a divergence). In that case force a full rebuild: we have
  # no pulled range to diff, so rebuild both sides unconditionally
  # (correctness over cost — this path only fires when something already went
  # wrong, not on the steady-state hot path).
  if [ "$BUILT" = "$LOCAL" ]; then
    log "up to date ($LOCAL)"
    exit 0
  fi
  log "checkout up to date ($LOCAL) but artifacts last built from '${BUILT:-none}' — forcing rebuild/restart"
  NEEDS_REBUILD=1
  NEEDS_DASHBOARD_REBUILD=1
  apply_update "$LOCAL"
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
# (Once reconciled, the build-stamp check above guarantees the binary is
# rebuilt even if the reconcile lands HEAD == origin out of band.)
if ! git merge-base --is-ancestor "$LOCAL" "$REMOTE"; then
  log "LOCAL ($LOCAL) and origin/main ($REMOTE) have diverged — manual reconcile required"
  exit 0
fi

log "update available: $LOCAL -> $REMOTE"

# What changed? Decide whether each side needs a rebuild.
CHANGED_FILES=$(git diff --name-only "$LOCAL" "$REMOTE")
NEEDS_REBUILD=0
NEEDS_DASHBOARD_REBUILD=0
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(crates/|Cargo\.(toml|lock)|rust-toolchain\.toml)'; then
  NEEDS_REBUILD=1
fi
# Dashboard rebuild needed when TS sources, EJS views, or package.json change.
# tsconfig.json and tailwind.config.js also gate compiled output.
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(src/|views/|package(-lock)?\.json|tsconfig\.json|tailwind\.config\.js)'; then
  NEEDS_DASHBOARD_REBUILD=1
fi

log "pulling"
if ! git pull --ff-only origin main >> "$LOG" 2>&1; then
  log "pull failed (non-fast-forward or conflict) — abandoning update"
  exit 0
fi

apply_update "$REMOTE"

# --- Auto-register optional scheduled jobs once ----------------------------
# When a new install-*.sh ships with the pull, check whether its
# corresponding unit is registered. If not, run the install script to wire
# it up. Idempotent: after the first run the unit is registered and the
# subsequent auto-update passes skip.
#
# This is how the remote machine automatically adopts new schedules the
# operator pushed, without needing an SSH session to opt in.
auto_register() {
  local script_name="$1"  # e.g. install-digest.sh
  local unit_id="$2"      # Linux: systemd unit (augmentagent-digest.timer)
                          # macOS: launchd label (com.nolanmak.augmentagent.digest)
  local script_path="$REPO_ROOT/scripts/$script_name"

  [ -x "$script_path" ] || return 0

  case "$(uname -s)" in
    Darwin)
      if ! launchctl print "gui/$(id -u)/$unit_id" >/dev/null 2>&1; then
        log "auto-registering $unit_id via $script_name"
        "$script_path" >> "$LOG" 2>&1 || log "auto-register $unit_id failed (continuing)"
      fi
      ;;
    Linux)
      if ! systemctl --user list-unit-files "$unit_id" 2>/dev/null | grep -q "$unit_id"; then
        log "auto-registering $unit_id via $script_name"
        "$script_path" >> "$LOG" 2>&1 || log "auto-register $unit_id failed (continuing)"
      fi
      ;;
  esac
}

# Enumerate here. Adding a new optional schedule = one-line entry below +
# the install-*.sh script in the repo.
auto_register "install-digest.sh" "augmentagent-digest.timer"  # Linux
auto_register "install-digest.sh" "com.nolanmak.augmentagent.digest"  # macOS label (uname gate inside)
auto_register "install-dashboard.sh" "augmentagent-dashboard.service"  # Linux
auto_register "install-dashboard.sh" "com.nolanmak.augmentagent-dashboard"  # macOS label (uname gate inside)
