#!/usr/bin/env bash
# #826 — restart a systemd --user unit and PROVE it bounced.
#
# The auto-updater used to treat "I ran the restart command" as success, and
# wrote its build stamp unconditionally afterwards. On 2026-08-27 it logged
# `daemon not registered under systemd (augmentagent.service)` — while the
# unit was enabled and `list-unit-files` found it — skipped the restart, and
# still recorded `update complete (build stamp written)`.
#
# That is the unrecoverable part. `built-commit` is what the updater's own
# staleness check compares against, so once it equals HEAD the guard
#
#     checkout up to date but artifacts last built from '$BUILT' — forcing rebuild
#
# can never fire again: every later tick logs `up to date` and does nothing,
# and the daemon serves old code indefinitely with every signal saying it is
# current. A restart skipped once was skipped permanently.
#
# The detection failure was not reproducible (0/200 on the same condition,
# with and without `pipefail`), so this does not try to out-guess it. It
# verifies the outcome instead: the unit must be queryable, the restart must
# succeed, MainPID must actually change, and the unit must be active
# afterwards. Anything else is a failure the caller must not paper over.
#
# Sourced by scripts/check-for-updates.sh; tested by
# scripts/tests/service-restart.test.sh.

# Log through the caller's `log()` when present, else stderr. Keeps the
# library usable from tests that have no update.log.
_sr_log() {
  if declare -F log >/dev/null 2>&1; then
    log "$*"
  else
    printf '[service-restart] %s\n' "$*" >&2
  fi
}

# Is `$1` a unit systemd knows about? Unlike the original inline check, a
# systemctl that cannot be reached at all is reported with its error rather
# than silently becoming "not registered" — that conflation is what made the
# 2026-08-27 skip so hard to read.
unit_is_registered() {
  local unit="$1" out
  if out=$(systemctl --user list-unit-files "$unit" 2>&1); then
    case "$out" in
      *"$unit"*) return 0 ;;
    esac
    _sr_log "unit $unit is not registered with systemd --user"
    return 1
  fi
  _sr_log "could not query systemd --user for $unit: ${out:-(no output)}"
  return 1
}

# MainPID of `$1`, or 0 when systemd has none / cannot be reached.
unit_main_pid() {
  local pid
  pid=$(systemctl --user show "$1" -p MainPID --value 2>/dev/null | head -n1 | tr -dc '0-9')
  printf '%s' "${pid:-0}"
}

# Restart `$1` and verify it actually bounced. 0 only on proof.
restart_unit() {
  local unit="$1" before after
  unit_is_registered "$unit" || return 1

  before=$(unit_main_pid "$unit")
  if ! systemctl --user restart "$unit" >>"${LOG:-/dev/null}" 2>&1; then
    _sr_log "systemctl --user restart $unit failed"
    return 1
  fi
  after=$(unit_main_pid "$unit")

  # A changed MainPID is the evidence the process really came back. Compare
  # PIDs rather than start timestamps: `ps -o lstart=` has 1-second
  # resolution and a fast rebuild restarts within the same second as the
  # binary write, which would false-alarm.
  if [ "$before" != "0" ] && [ "$before" = "$after" ]; then
    _sr_log "restart of $unit did not bounce the process (MainPID still $after)"
    return 1
  fi

  if ! systemctl --user is-active --quiet "$unit"; then
    _sr_log "unit $unit is not active after restart"
    return 1
  fi

  _sr_log "restarted $unit (MainPID $before -> $after)"
  return 0
}

# May the build stamp be advanced? Only when no required restart failed.
# Writing it regardless is what turned one skipped bounce into a permanently
# stale daemon.
should_write_stamp() {
  [ "${1:-0}" -eq 0 ]
}

# --- #844: don't kill an in-flight self-improve run -------------------------
#
# The auto-PR loop's build stage is ~20 minutes of agentic Opus on the owner's
# Claude subscription. The updater restarting augmentagent.service kills it
# mid-flight, silently: the process dies before `record_attempt`, so the spend
# is not even recorded, and the issue is re-picked to burn another build.
# Observed live 2026-08-28: a run selected #667 at 01:45:31 and the 01:49:12
# deploy restart destroyed it. Worse, an auto-merged PR *triggers* a rebuild +
# restart by design — the loop's own success can kill its next run.
#
# run_once holds an exclusive flock on the self-improve lock for the whole run
# (#816), which makes "a run is in flight" observable from outside the
# process. Defer the restart while it is held — bounded, because a stale
# binary is worse than one lost build.

SELF_IMPROVE_LOCK="${AUGMENTAGENT_SELFIMPROVE_LOCK:-$HOME/.local/state/augmentagent/self-improve.lock}"
RESTART_DEFER_STAMP="${AUGMENTAGENT_RESTART_DEFER_STAMP:-${LOG_DIR:-$HOME/.local/state/augmentagent}/restart-deferred-since}"
RESTART_DEFER_MAX_SECS="${AUGMENTAGENT_RESTART_DEFER_MAX_SECS:-2400}"

# Is a self-improve run holding the lock right now?
self_improve_run_in_flight() {
  [ -e "$SELF_IMPROVE_LOCK" ] || return 1
  ! flock -n "$SELF_IMPROVE_LOCK" -c true 2>/dev/null
}

# Should this restart be deferred? 0 = defer (a run is in flight and the
# deferral budget is not exhausted); 1 = proceed. Clears its own state on
# every proceed so one long-past deferral cannot poison a later deploy.
maybe_defer_restart() {
  if ! self_improve_run_in_flight; then
    rm -f "$RESTART_DEFER_STAMP"
    return 1
  fi
  local now since
  now=$(date +%s)
  since=$(cat "$RESTART_DEFER_STAMP" 2>/dev/null || true)
  case "$since" in
    ''|*[!0-9]*)
      printf '%s\n' "$now" > "$RESTART_DEFER_STAMP"
      _sr_log "self-improve run in flight; deferring restart (retry next tick)"
      return 0
      ;;
  esac
  if [ $((now - since)) -lt "$RESTART_DEFER_MAX_SECS" ]; then
    _sr_log "self-improve run still in flight ($((now - since))s); deferring restart"
    return 0
  fi
  _sr_log "self-improve run in flight for $((now - since))s (> ${RESTART_DEFER_MAX_SECS}s); restarting anyway — a stale binary is worse than one lost build"
  rm -f "$RESTART_DEFER_STAMP"
  return 1
}

# --- #891: cap the shared gate cache, only when NO lane is building ---------
#
# Parallel lanes key cargo debug artifacts by worktree path, so the shared
# gate cache doubled to 26 GB in one evening. Dropping `debug/` from inside a
# gate run would race the other lane's build; the updater runs every few
# minutes and can see both lane locks, so it is the safe place to trim.
GATE_CACHE_DIR="${AUGMENTAGENT_GATE_TARGET_DIR:-$HOME/.cache/augmentagent-gate-target}"
GATE_CACHE_MAX_MB="${AUGMENTAGENT_GATE_CACHE_MAX_MB:-20000}"
RESUME_LANE_LOCK="${AUGMENTAGENT_SELFIMPROVE_LOCK:-$HOME/.local/state/augmentagent/self-improve.lock}"
RESUME_LANE_LOCK="${RESUME_LANE_LOCK%.lock}-resume.lock"

any_lane_building() {
  for l in "$SELF_IMPROVE_LOCK" "$RESUME_LANE_LOCK"; do
    [ -e "$l" ] || continue
    flock -n "$l" -c true 2>/dev/null || return 0
  done
  return 1
}

# Trim the gate cache's debug/ when it is over the cap and every lane is idle.
# 0 = trimmed, 1 = left alone (under cap, or a lane is mid-build).
trim_gate_cache_if_idle() {
  [ -d "$GATE_CACHE_DIR/debug" ] || return 1
  local sz
  sz=$(du -sm "$GATE_CACHE_DIR" 2>/dev/null | cut -f1)
  [ "${sz:-0}" -gt "$GATE_CACHE_MAX_MB" ] || return 1
  if any_lane_building; then
    _sr_log "gate cache ${sz}MB over cap but a lane is building; trimming later"
    return 1
  fi
  _sr_log "gate cache ${sz}MB over ${GATE_CACHE_MAX_MB}MB cap and lanes idle; dropping debug/ (next gate cold-rebuilds once)"
  rm -rf "$GATE_CACHE_DIR/debug"
  return 0
}
