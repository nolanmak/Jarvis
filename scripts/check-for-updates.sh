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
# - We only rebuild Rust when something compiled into the binaries changed
#   (crates/, Cargo files, or a file production code embeds with
#   include_str!/include_bytes! — see RUST_REBUILD_PATHS), so wiki-only
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

# #1079 — macOS logs to the same state dir as Linux, so `autopr-health` and
# the restart library find update.log and the stamps in one place.
case "$(uname -s)" in
  Darwin|Linux) LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent" ;;
  *)            LOG_DIR="$HOME/.augmentagent/logs" ;;
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

# Tell the owner on Discord. Best effort and never fatal: a webhook that is
# unset or down must not stop a deploy. Anything this function reports is a
# state a human has to know about, not routine progress.
notify_owner() {
  [ -n "${DISCORD_WEBHOOK_URL:-}" ] || return 0
  curl -fsS -m 10 -X POST -H 'Content-Type: application/json' \
    --data "$(printf '{"content": %s}' "$(printf '%s' "⚙️ $*" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')")" \
    "$DISCORD_WEBHOOK_URL" >/dev/null 2>&1 || true
}

# #826 — restart_unit / should_write_stamp. Sourced after log() so the
# library's diagnostics land in update.log like everything else.
# shellcheck source=lib/service-restart.sh
. "$REPO_ROOT/scripts/lib/service-restart.sh"

# Rebuild the side(s) flagged by NEEDS_REBUILD / NEEDS_DASHBOARD_REBUILD, then
# restart the corresponding services. On Rust build failure we exit non-zero
# WITHOUT restarting and WITHOUT advancing the stamp, so the daemon keeps
# running the previous good binary and the next tick retries. On success the
# stamp is advanced to $TARGET so a subsequent tick is a true no-op.
apply_update() {
  local TARGET="$1"
  # #826 — how many restarts that were REQUIRED did not verifiably happen.
  local RESTART_FAILURES=0

  if [ "$NEEDS_REBUILD" -eq 1 ]; then
    # #903 — a release build is the second-largest memory user on the box;
    # under pressure it would tip a host that is already struggling. Not a
    # build failure: withhold the stamp so the next tick retries (#826).
    if ! memory_pressure_ok; then
      log "skipping rebuild + restart under memory pressure; stamp withheld, retry next tick"
      return 1
    fi
    log "rebuilding rust (changed files touched crates/ or Cargo)"
    # Build BOTH production binaries. `augmentagent-mcp-memory` is a separate
    # package that the daemon spawns as a stdio MCP server — ask_opts points
    # at `target/release/augmentagent-mcp-memory` (see reasoner.rs). It was
    # not in this build line, so it was never rebuilt by an update: on the
    # daemon host it was found ~3 weeks stale while the CLI was current, and
    # every change to that crate had silently never deployed.
    #
    # If a new binary is ever referenced from production code, add it here.
    # `grep -rn "target/release/" crates/*/src/*.rs` lists what is expected.
    if ! cargo build --release -p augmentagent-cli -p augmentagent-mcp-memory >> "$LOG" 2>&1; then
      log "RUST BUILD FAILED — not restarting; daemon stays on previous binary"
      exit 1
    fi
    log "rust build ok (augmentagent + augmentagent-mcp-memory)"
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

  # The browser worker is a separate long-running process with npm dependencies.
  # Deploy it only on hosts where the optional unit is installed.
  if [ "${NEEDS_COMPUTER_REBUILD:-1}" -eq 1 ] && [ -d "$REPO_ROOT/sidecars/computer-use" ] &&
      [ "$(uname -s)" = Linux ] && systemctl --user cat augmentagent-computer-use.service >/dev/null 2>&1; then
    if ! (cd "$REPO_ROOT/sidecars/computer-use" && npm ci >> "$LOG" 2>&1); then
      log "COMPUTER WORKER INSTALL FAILED — withholding build stamp"
      return 1
    fi
    systemctl --user daemon-reload >> "$LOG" 2>&1 || return 1
    restart_unit augmentagent-computer-use.service || RESTART_FAILURES=$((RESTART_FAILURES + 1))
  fi

  # Restart services so the new binary / config takes effect.
  case "$(uname -s)" in
    Darwin)
      if [ "$NEEDS_REBUILD" -eq 1 ]; then
        # #1079 — the Linux guarantees, on launchd: defer under an in-flight
        # auto-PR build (#844) or the restart budget (#903), and only count a
        # restart that verifiably bounced the agent (#826).
        if maybe_defer_restart; then
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
        elif ! memory_pressure_ok || ! restart_budget_ok; then
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
        elif launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
          log "restarting daemon via launchctl kickstart -k $LABEL"
          if restart_agent "$LABEL"; then
            record_restart
          else
            RESTART_FAILURES=$((RESTART_FAILURES + 1))
          fi
        else
          log "daemon not registered under launchd ($LABEL) — run install-autostart.sh manually"
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
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
        # #844 — an in-flight auto-PR build is ~20 min of agentic Opus; a
        # restart kills it unrecorded. Deferring counts as a restart failure,
        # so the stamp is withheld (#826) and the next tick retries — the
        # binary is already built, so the retry is cheap.
        if maybe_defer_restart; then
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
        elif ! memory_pressure_ok || ! restart_budget_ok; then
          # #903 — same deferral semantics as #844: counts as a restart
          # failure so the stamp is withheld and the next tick retries.
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
        else
          log "restarting daemon via systemctl --user restart $SYSTEMD_UNIT"
          if restart_unit "$SYSTEMD_UNIT"; then
            record_restart
          else
            RESTART_FAILURES=$((RESTART_FAILURES + 1))
          fi
        fi
      fi
      if [ "$NEEDS_DASHBOARD_REBUILD" -eq 1 ]; then
        # Dashboard failures stay non-fatal and do NOT block the stamp,
        # matching the existing severity split (a dashboard BUILD failure is
        # already log-only while a Rust build failure exits 1). It also must
        # not latch: `auto_register` below is what first registers this unit
        # on a fresh host, and it runs *after* apply_update — so an
        # unregistered dashboard here is an expected transient, not a fault.
        log "restarting dashboard via systemctl --user restart $DASHBOARD_SYSTEMD_UNIT"
        restart_unit "$DASHBOARD_SYSTEMD_UNIT" \
          || log "dashboard restart failed (non-fatal; UI stays on the previous build)"
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
      [ "$NEEDS_REBUILD" -eq 1 ] && RESTART_FAILURES=$((RESTART_FAILURES + 1))
      ;;
  esac

  # Record what we successfully built from so the next tick is a true no-op
  # (and so an out-of-band checkout move is detected, not silently ignored).
  #
  # #826 — ONLY when the daemon verifiably came back on the new binary. The
  # stamp is the updater's own staleness signal: advancing it after a skipped
  # restart satisfies the `artifacts last built from '$BUILT'` guard forever,
  # so the bounce is never retried and the daemon serves old code with every
  # signal claiming it is current. Leaving the stamp behind makes the next
  # tick take exactly that guard and try again.
  if ! should_write_stamp "$RESTART_FAILURES"; then
    log "NOT writing the build stamp: $RESTART_FAILURES required restart(s) could \
not be verified. The daemon may still be on the previous binary; the next tick \
will retry via the build-stamp mismatch path."
    return 1
  fi
  printf '%s\n' "$TARGET" > "$STAMP"
  log "update complete: now at $TARGET (build stamp written)"
}

log "checking for updates"
# #891 — housekeeping that must never run under a live build.
trim_gate_cache_if_idle || true

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
# non-ff pull would fail anyway.
#
# This used to bail and wait for a human — and nobody is watching this log, so
# it stalled SILENTLY: the daemon ran stale code for as long as the divergence
# lasted (2026-09-14, and before that). Divergence here is almost always
# benign and self-inflicted: a session committed on the deploy checkout's
# `main`, the PR was squash-merged upstream, and the local commits are the
# same work under different SHAs. `scripts/check-not-on-main.sh` now stops
# that at the source, but the updater must never be the thing that quietly
# stops deploying.
#
# So: preserve, then proceed. The local-only commits are saved on a rescue
# branch (nothing is ever lost — same discipline as `wiki sync`'s
# kb-conflict-<sha> branches) and the checkout is reset to origin/main. A
# DIRTY tree is the one case still left alone: uncommitted work is not ours
# to move, so that stays a loud no-op.
if ! git merge-base --is-ancestor "$LOCAL" "$REMOTE"; then
  AHEAD=$(git rev-list --count "$REMOTE".."$LOCAL" 2>/dev/null || echo "?")
  if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
    log "DIVERGED and the tree is dirty: LOCAL ($LOCAL, $AHEAD commit(s) not on origin/main) vs origin/main ($REMOTE)."
    log "  Not touching uncommitted work. Commit or stash, then this recovers itself on the next tick."
    notify_owner "Auto-updater stalled: the deploy checkout has diverged from origin/main and has uncommitted changes, so it will not deploy. Commit or stash in ~/AugmentAgent, and it recovers on the next tick."
    exit 0
  fi
  RESCUE="updater-rescue/$(date -u +%Y%m%d-%H%M%S)-$(git rev-parse --short "$LOCAL")"
  if git branch "$RESCUE" "$LOCAL" >/dev/null 2>&1; then
    log "DIVERGED: preserved $AHEAD local commit(s) on '$RESCUE'"
  else
    log "DIVERGED: could not create rescue branch '$RESCUE' — refusing to reset"
    exit 0
  fi
  if git reset --hard "$REMOTE" >/dev/null 2>&1; then
    log "DIVERGED: reset the checkout to origin/main ($REMOTE); deploying normally"
    notify_owner "Auto-updater recovered: the deploy checkout had diverged from origin/main. $AHEAD local commit(s) preserved on branch '$RESCUE'; the checkout is reset and deploying again."
    LOCAL=$(git rev-parse HEAD)
    # The reset is out-of-band relative to the stamp, so force a rebuild
    # rather than trusting a diff range that no longer applies.
    NEEDS_REBUILD=1
    NEEDS_NODE_REBUILD=1
    apply_update "$LOCAL"
    exit 0
  fi
  log "DIVERGED: reset failed — manual reconcile required (local commits are on '$RESCUE')"
  exit 0
fi

log "update available: $LOCAL -> $REMOTE"

# What changed? Decide whether each side needs a rebuild.
CHANGED_FILES=$(git diff --name-only "$LOCAL" "$REMOTE")
NEEDS_REBUILD=0
NEEDS_DASHBOARD_REBUILD=0
NEEDS_COMPUTER_REBUILD=0
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(sidecars/computer-use/|systemd/augmentagent-computer-use.service$)'; then
  NEEDS_COMPUTER_REBUILD=1
fi
# Rust rebuild: crates/ and Cargo files, plus every file outside crates/ that
# production code compiles in with include_str!/include_bytes! — agent prompts
# under schema/, the embedded .env.example key list, and the Codex tool bridge,
# command sandbox, build VM runner, dependency gateway and provider supervisor.
# Missing one means the PR merges but the daemon keeps the stale embedded copy.
# scripts/tests/updater-rebuild-trigger.test.sh fails when a new include target
# outside crates/ is added without being classified here.
RUST_REBUILD_PATHS='^(crates/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|schema/|\.env\.example$|scripts/(codex-tool-bridge|codex-command-sandbox|codex-build-vm|build-dependency-proxy|provider-supervisor)\.py$)'
if printf '%s\n' "$CHANGED_FILES" | grep -qE "$RUST_REBUILD_PATHS"; then
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
      # Each schedule is listed once per platform; only launchd labels apply
      # here. A systemd unit name is never loaded in launchd, so without this
      # guard it re-ran the installer on every tick (#1079).
      case "$unit_id" in com.*) ;; *) return 0 ;; esac
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
auto_register "install-research.sh" "augmentagent-research.timer"  # Linux
auto_register "install-research.sh" "com.nolanmak.augmentagent.research"  # macOS label (uname gate inside)
auto_register "install-dashboard.sh" "augmentagent-dashboard.service"  # Linux
auto_register "install-dashboard.sh" "com.nolanmak.augmentagent-dashboard"  # macOS label (uname gate inside)
