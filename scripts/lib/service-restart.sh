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
