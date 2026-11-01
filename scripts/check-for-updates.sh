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
# - #1571: a restart only counts when the unit comes back on
#   target/release/augmentagent. A systemd drop-in that overrides ExecStart
#   (never do that to pin a rollout) is reported as DEPLOY PINNED, alerted
#   once, and the restart is skipped until the override is retired.
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
PROVISION_PENDING="$LOG_DIR/autopr-provision-pending"
STAMP="$LOG_DIR/built-commit"   # SHA the deployed artifacts were last built from
LABEL="com.nolanmak.augmentagent"
SYSTEMD_UNIT="augmentagent.service"
DASHBOARD_LABEL="com.nolanmak.augmentagent-dashboard"
DASHBOARD_SYSTEMD_UNIT="augmentagent-dashboard.service"

stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }
log() { printf '%s [update] %s\n' "$(stamp)" "$*" >> "$LOG"; }

reconcile_native_runtime() {
  if [ "$(uname -s)" = Linux ] && [ -f "$REPO_ROOT/scripts/ensure-native-runtime.py" ]; then
    if ! /usr/bin/python3 "$REPO_ROOT/scripts/ensure-native-runtime.py" >> "$LOG" 2>&1; then
      log "NATIVE RUNTIME RECONCILIATION FAILED — provider admission remains blocked; retrying next tick"
      return 1
    fi
  fi
}

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
. "$REPO_ROOT/scripts/lib/update-build.sh"

# Artifact repair is independent of the executable's capability or health.
secure_release_artifacts() {
  python3 "$REPO_ROOT/scripts/secure-release-executables.py" "$REPO_ROOT/target/release" >> "$LOG" 2>&1
}

reconcile_worker_provisioning() {
  # Keep this retry state independent of the compiled/deployed build stamp.
  (umask 077; printf 'pending\n' > "$PROVISION_PENDING")
  secure_release_artifacts || return 1
  local worker_help=""
  worker_help=$("$DAEMON_BINARY" auto-pr-worker --help 2>/dev/null) || true
  case "$worker_help" in
    *'Usage: augmentagent auto-pr-worker'*)
      if ! bash "$REPO_ROOT/scripts/ensure-autopr-worker.sh" >> "$LOG" 2>&1; then
        log "AUTO-SHIP WORKER PROVISIONING FAILED — retry pending independently of build stamp"
        return 1
      fi ;;
    *) log "auto-ship worker capability unavailable; skipping provisioning" ;;
  esac
  rm -f "$PROVISION_PENDING"
}

# #1571 — the unit no longer runs what this script builds. A drop-in that
# overrides ExecStart (2026-10-20: a rollout pinned a frozen copy of the
# daemon) turns every tick into pull + rebuild + a verified bounce back onto
# the OLD binary. Say exactly that, once per distinct cause (the tick is five
# minutes; the key holds only stable facts, never systemd's pid/status
# fields), count it as a restart failure so the stamp stays honest, and do
# NOT restart while it holds: the bounce only relaunches the frozen binary
# and kills whatever the auto-PR loop was building. Checked before the
# deferral gates — it is a read-only `systemctl show`, safe under a live build.
DAEMON_LAUNCHER="$REPO_ROOT/scripts/run-rs.sh"
DAEMON_BINARY="$REPO_ROOT/target/release/augmentagent"
PINNED_ALERT_STAMP="$LOG_DIR/deploy-pinned-alerted"
report_pinned_deploy() {
  local cause="$1" dropins msg
  dropins=$(unit_exec_dropins "$SYSTEMD_UNIT") || dropins=""
  msg="DEPLOY PINNED: $SYSTEMD_UNIT $cause. ExecStart drop-ins: ${dropins:-none}. \
Merges to main are built but never run until the override is retired: move \
the drop-in out of ~/.config/systemd/user/$SYSTEMD_UNIT.d/, then \
systemctl --user daemon-reload && systemctl --user restart $SYSTEMD_UNIT (#1571)."
  log "$msg"
  if [ "$(cat "$PINNED_ALERT_STAMP" 2>/dev/null)" != "$cause|$dropins" ]; then
    printf '%s' "$cause|$dropins" > "$PINNED_ALERT_STAMP"
    notify_owner "$msg"
  fi
}
# 0 = the unit will not come back on our binary; reported, do not restart.
deploy_pinned_elsewhere() {
  if unit_exec_pinned "$SYSTEMD_UNIT" "$DAEMON_LAUNCHER"; then
    report_pinned_deploy "has ExecStart overridden to $(unit_exec_path "$SYSTEMD_UNIT") \
instead of $DAEMON_LAUNCHER; not restarting"
    return 0
  fi
  # A canonical ExecStart whose process still is not our binary (a launcher
  # that execs elsewhere, a relinked target/) is just as deterministic: the
  # last bounce proved it, restarting again proves it again at the loop's cost.
  if ! unit_runs_binary "$SYSTEMD_UNIT" "$DAEMON_BINARY"; then
    report_pinned_deploy "is running $SR_LAST_EXE instead of $DAEMON_BINARY; \
not restarting"
    return 0
  fi
  return 1
}

# Policy selection can change without a Git or executable update. Use the
# ordinary daemon restart guards; the independent worker drains and reloads
# itself at its next completed tick/sweep boundary.
reconcile_native_selection() {
  [ "$(uname -s)" = Linux ] || return 0
  local helper="$REPO_ROOT/scripts/autoship-deployment-readiness.py" status=0
  [ -f "$helper" ] || return 0
  /usr/bin/python3 "$helper" --activation-needed \
    --selection "${XDG_CONFIG_HOME:-$HOME/.config}/augmentagent/autoship-native.env" \
    --receipts "$LOG_DIR/autoship-effective-config" || status=$?
  case "$status" in
    0) return 0 ;;
    10) ;;
    *) log "native policy activation evidence unavailable; keeping services and retrying next tick"; return 0 ;;
  esac
  if deploy_pinned_elsewhere || maybe_defer_restart || ! memory_pressure_ok || ! restart_budget_ok; then
    log "native policy activation deferred by deployment guards; retrying next tick"
    return 0
  fi
  if systemctl --user daemon-reload >> "$LOG" 2>&1 && restart_unit "$SYSTEMD_UNIT" "$DAEMON_BINARY"; then
    record_restart
    log "native policy activated for daemon; worker reloads after its active work drains; verify autoship readiness"
  else
    log "native policy activation failed; receipt remains pending for next tick"
  fi
}

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
    if ! python3 "$REPO_ROOT/scripts/pdf-runtime.py" >> "$LOG" 2>&1; then
      log "PDF RUNTIME INSTALL FAILED — not restarting; build stamp withheld"
      return 1
    fi
    # Build BOTH production binaries. `augmentagent-mcp-memory` is a separate
    # package that the daemon spawns as a stdio MCP server — ask_opts points
    # at `target/release/augmentagent-mcp-memory` (see reasoner.rs). It was
    # not in this build line, so it was never rebuilt by an update: on the
    # daemon host it was found ~3 weeks stale while the CLI was current, and
    # every change to that crate had silently never deployed.
    #
    # If a new binary is ever referenced from production code, add it here.
    # `grep -rn "target/release/" crates/*/src/*.rs` lists what is expected.
    local build_status=0
    if run_update_build "$DAEMON_BINARY" "$REPO_ROOT/target"; then
      :
    else
      build_status=$?
      case "$build_status" in
        73|75|76) return 0 ;; # A clean deferral; never advance the stamp.
        *) return 1 ;;
      esac
    fi
    log "rust build ok (augmentagent + augmentagent-mcp-memory)"
  else
    log "no rust code changed; skipping rust rebuild"
  fi

  # Installer/template fixes need provisioning even when Cargo output is
  # cached or only scripts changed. This reloads configuration without
  # starting a duplicate loop or forcing an active worker to restart.
  secure_release_artifacts || return 1
  local PROVISION_FAILURE=0
  reconcile_worker_provisioning || PROVISION_FAILURE=1

  # The pulled helper or selected profile may be new, and a provider binary
  # may have changed during compilation. Preserve the binary deployment's
  # independent status: failed runtime repair must not freeze code updates.
  reconcile_native_runtime || true

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
      { { [ "$(uname -s)" = Linux ] && systemctl --user cat augmentagent-computer-use.service >/dev/null 2>&1; } ||
        { [ "$(uname -s)" = Darwin ] && [ -f "$HOME/Library/LaunchAgents/com.nolanmak.augmentagent.computer-use.plist" ]; }; }; then
    if ! (cd "$REPO_ROOT/sidecars/computer-use" && npm ci >> "$LOG" 2>&1); then
      log "COMPUTER WORKER INSTALL FAILED — withholding build stamp"
      return 1
    fi
    if [ "$(uname -s)" = Linux ]; then
      systemctl --user daemon-reload >> "$LOG" 2>&1 || return 1
      restart_unit augmentagent-computer-use.service || RESTART_FAILURES=$((RESTART_FAILURES + 1))
    elif launchctl print "gui/$(id -u)/com.nolanmak.augmentagent.computer-use" >/dev/null 2>&1; then
      restart_agent com.nolanmak.augmentagent.computer-use || RESTART_FAILURES=$((RESTART_FAILURES + 1))
    else
      log "computer-use launchd plist is installed but not loaded; dependencies updated, restart required after loading"
      RESTART_FAILURES=$((RESTART_FAILURES + 1))
    fi
  fi

  # Optional sidecars are rebuilt one at a time, only on hosts with a
  # registered job. The setup scripts use pinned dependency locks. A failed
  # build or restart withholds the stamp so the next updater tick can retry.
  local sidecar need label unit installed
  for sidecar in browser renderer fetch wa-sidecar; do
    case "$sidecar" in
      browser) need="${NEEDS_BROWSER_REBUILD:-0}" ;;
      renderer) need="${NEEDS_RENDERER_REBUILD:-0}" ;;
      fetch) need="${NEEDS_FETCH_REBUILD:-0}" ;;
      wa-sidecar) need="${NEEDS_WA_SIDECAR_REBUILD:-0}" ;;
    esac
    [ "$need" -eq 1 ] || continue
    if [ "$sidecar" = browser ]; then
      label="com.nolanmak.augmentagent.browser-sidecar"
      unit="augmentagent-browser-sidecar.service"
    else
      label="com.nolanmak.augmentagent.$sidecar"
      unit="augmentagent-$sidecar.service"
    fi
    installed=false
    if [ "$(uname -s)" = Darwin ] && [ -f "$HOME/Library/LaunchAgents/$label.plist" ]; then
      installed=true
    elif [ "$(uname -s)" = Linux ] && systemctl --user cat "$unit" >/dev/null 2>&1; then
      installed=true
    fi
    [ "$installed" = true ] || continue
    log "rebuilding installed $sidecar sidecar"
    if ! (cd "$REPO_ROOT/sidecars/$sidecar" && bash setup.sh >> "$LOG" 2>&1); then
      log "$sidecar setup failed; withholding build stamp"
      return 1
    fi
    if [ "$(uname -s)" = Darwin ]; then
      if [ "$sidecar" = browser ] && [ "${NEEDS_BROWSER_REINSTALL:-0}" -eq 1 ]; then
        if ! "$REPO_ROOT/target/release/augmentagent" install browser-sidecar >> "$LOG" 2>&1; then
          log "browser LaunchAgent refresh failed; withholding build stamp"
          return 1
        fi
      fi
      if ! restart_agent "$label"; then RESTART_FAILURES=$((RESTART_FAILURES + 1)); fi
    elif ! restart_unit "$unit"; then
      RESTART_FAILURES=$((RESTART_FAILURES + 1))
    fi
  done

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
        if deploy_pinned_elsewhere; then
          # #1571 — reported above; a restart would relaunch the frozen binary.
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
        elif maybe_defer_restart; then
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
        elif ! memory_pressure_ok || ! restart_budget_ok; then
          # #903 — same deferral semantics as #844: counts as a restart
          # failure so the stamp is withheld and the next tick retries.
          RESTART_FAILURES=$((RESTART_FAILURES + 1))
        else
          log "restarting daemon via systemctl --user restart $SYSTEMD_UNIT"
          # #1571 — proof now includes "and it is running the binary we
          # built"; a bounce onto anything else is a pinned deploy.
          resource_status=0
          if [ -x "$REPO_ROOT/scripts/configure-run-resources.sh" ]; then
            AUGMENTAGENT_RESOURCE_UNIT="$SYSTEMD_UNIT" "$REPO_ROOT/scripts/configure-run-resources.sh" --install >> "$LOG" 2>&1 || resource_status=$?
          fi
          if [ "$resource_status" -eq 3 ]; then
            log "run-resource delegation unsupported; required Autoship stays disabled while unrelated service updates continue"
            notify_owner "Run-resource delegation unavailable for $SYSTEMD_UNIT; required Autoship remains disabled. Other daemon services continue updating."
          fi
          if [ "$resource_status" -ne 0 ] && [ "$resource_status" -ne 3 ]; then
            log "resource delegation setup failed; keeping daemon and withholding build stamp"
            RESTART_FAILURES=$((RESTART_FAILURES + 1))
          elif restart_unit "$SYSTEMD_UNIT" "$DAEMON_BINARY"; then
            record_restart
          else
            [ -n "$SR_LAST_EXE" ] && report_pinned_deploy "came back running \
$SR_LAST_EXE instead of $DAEMON_BINARY"
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
  reconcile_native_selection
  # #1571 — a verified deploy on our own binary closes any pinned-deploy
  # alert; the next pin is a new incident and gets a new Discord message.
  rm -f "$PINNED_ALERT_STAMP"
  # #1410 — a verified deploy is the moment older rollback material stops
  # mattering: keep the newest snapshots and rollback binaries, drop the rest.
  # Housekeeping only; it can never fail the update.
  if [ -x "$REPO_ROOT/target/release/augmentagent" ]; then
    "$REPO_ROOT/target/release/augmentagent" deploy prune >> "$LOG" 2>&1 || true
  fi
  return "$PROVISION_FAILURE"
}

log "checking for updates"
# This precedes every helper that can execute cached release artifacts.
secure_release_artifacts || { log "release artifact validation failed; retrying next tick"; exit 1; }
# Provider packages can move independently of Git, including when HEAD and
# the build stamp are current. This also runs after a pull/build below.
reconcile_native_runtime || true
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
    if [ -e "$PROVISION_PENDING" ]; then
      reconcile_worker_provisioning || exit 1
    fi
    reconcile_native_selection
    log "up to date ($LOCAL)"
    exit 0
  fi
  log "checkout up to date ($LOCAL) but artifacts last built from '${BUILT:-none}' — forcing rebuild/restart"
  NEEDS_REBUILD=1
  NEEDS_DASHBOARD_REBUILD=1
  NEEDS_RENDERER_REBUILD=1
  NEEDS_FETCH_REBUILD=1
  NEEDS_WA_SIDECAR_REBUILD=1
  NEEDS_BROWSER_REBUILD=1
  NEEDS_BROWSER_REINSTALL=1
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
    NEEDS_RENDERER_REBUILD=1
    NEEDS_FETCH_REBUILD=1
    NEEDS_WA_SIDECAR_REBUILD=1
    NEEDS_BROWSER_REBUILD=1
    NEEDS_BROWSER_REINSTALL=1
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
NEEDS_RENDERER_REBUILD=0
NEEDS_FETCH_REBUILD=0
NEEDS_WA_SIDECAR_REBUILD=0
NEEDS_BROWSER_REBUILD=0
NEEDS_BROWSER_REINSTALL=0
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(sidecars/browser/|scripts/start-sidecar\.py$|crates/augmentagent-browser-client/src/lib\.rs$|crates/augmentagent-cli/src/installers\.rs$)'; then
  NEEDS_BROWSER_REBUILD=1
fi
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(scripts/start-sidecar\.py$|crates/augmentagent-browser-client/src/lib\.rs$|crates/augmentagent-cli/src/installers\.rs$)'; then
  NEEDS_BROWSER_REINSTALL=1
fi
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(sidecars/renderer/|scripts/start-sidecar\.py$)'; then
  NEEDS_RENDERER_REBUILD=1
fi
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(sidecars/fetch/|scripts/start-sidecar\.py$)'; then
  NEEDS_FETCH_REBUILD=1
fi
if printf '%s\n' "$CHANGED_FILES" | grep -q '^sidecars/wa-sidecar/'; then
  NEEDS_WA_SIDECAR_REBUILD=1
fi
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(sidecars/computer-use/|systemd/augmentagent-computer-use.service$)'; then
  NEEDS_COMPUTER_REBUILD=1
fi
# Rust rebuild: crates/ and Cargo files, plus every file outside crates/ that
# production code compiles in with include_str!/include_bytes! — agent prompts
# under schema/, the embedded .env.example key list, and the Codex tool bridge,
# command sandbox, build VM runner, dependency gateway, provider supervisor,
# Python compute helpers and the embedded Deno Code Mode runner.
# Missing one means the PR merges but the daemon keeps the stale embedded copy.
# scripts/tests/updater-rebuild-trigger.test.sh fails when a new include target
# outside crates/ is added without being classified here.
RUST_REBUILD_PATHS='^(crates/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|schema/|\.env\.example$|docs/slack-app-manifest\.json$|scripts/(ci-build-vm|native-provider-(vm(-guest)?|mcp-relay|gateway)|run-resource-scope|autoship-deployment-readiness|autoship-verifier-checkout|autoship-registry-integrity|autoship-scope-guard|codex-tool-bridge|codex-command-sandbox|codex-build-vm|build-dependency-proxy|provider-supervisor|pdf-runtime|code-mode-compute(-guest|-prepare)?)\.py$|scripts/ci-contract\.(py|json)$|scripts/run-rs\.sh$|sidecars/code-mode-runner/runner\.ts$)'
if printf '%s\n' "$CHANGED_FILES" | grep -qE "$RUST_REBUILD_PATHS"; then
  NEEDS_REBUILD=1
fi
# Dashboard rebuild needed when TS sources, EJS views, or package.json change.
# tsconfig.json and tailwind.config.js also gate compiled output.
if printf '%s\n' "$CHANGED_FILES" | grep -qE '^(src/|views/|package(-lock)?\.json|tsconfig\.json|tailwind\.config\.js)'; then
  NEEDS_DASHBOARD_REBUILD=1
fi
# The data-sync cron jobs execute their Python straight out of this checkout, so
# the pull alone deploys them — there is nothing to rebuild and no daemon to
# bounce. Report it instead: these jobs are scheduled outside the service this
# script restarts, so a silent pull leaves the operator unable to tell whether a
# merged fix is live. Gated on the paths the jobs actually run, because most
# pushes to main touch none of them and a notice on every push gets ignored.
# scripts/tests/updater-sync-scripts.test.sh pins both directions.
SYNC_SCRIPT_PATHS='^scripts/(apple-notes|imessage)/[^/]+\.py$'
CHANGED_SYNC_SCRIPTS=$(printf '%s\n' "$CHANGED_FILES" | grep -E "$SYNC_SCRIPT_PATHS" || true)

log "pulling"
if ! git pull --ff-only origin main >> "$LOG" 2>&1; then
  log "pull failed (non-fast-forward or conflict) — abandoning update"
  exit 0
fi

apply_update "$REMOTE"

if [ -n "$CHANGED_SYNC_SCRIPTS" ]; then
  SYNC_SCRIPT_LIST=$(printf '%s' "$CHANGED_SYNC_SCRIPTS" | tr '\n' ' ')
  log "data sync script changed in this pull; live for the next scheduled run: $SYNC_SCRIPT_LIST"
  notify_owner "data sync script updated: $SYNC_SCRIPT_LIST — live on the next scheduled run"
fi

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
