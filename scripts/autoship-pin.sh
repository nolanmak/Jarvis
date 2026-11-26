#!/usr/bin/env bash
# #2468 — move the commit the auto-ship worker runs. Main deploys never move it.
#
#   scripts/autoship-pin.sh <sha|tag|ref>   pin the worker to that commit (must be on origin/main)
#   scripts/autoship-pin.sh main            unpin: the worker follows main deploys again
#   scripts/autoship-pin.sh status          show the pin, the installed build and how far main is ahead
#   scripts/autoship-pin.sh auto on|off     let the updater advance the pin to green main commits (default on)
#
# With auto on, each updater tick moves the pin forward to the newest
# origin/main commit whose required checks are green (lib/autoship-pin.sh).
# Pinning backwards with this script turns auto off, so the next tick does
# not undo a rollback; `auto on` resumes it. AUTOSHIP_PIN_AUTO=off in the
# updater's environment overrides the file.
#
# Setting a pin records it and starts one updater run (Linux), which builds
# the pinned commit on the build volume, installs it, and lets the worker
# reload at its next safe point. --no-apply only records it; the next
# five-minute tick applies it.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
LOG="$LOG_DIR/update.log"
log() { mkdir -p "$LOG_DIR"; printf '%s [update] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >> "$LOG"; }
# shellcheck source=lib/autoship-pin.sh
. "$REPO_ROOT/scripts/lib/autoship-pin.sh"

usage() { sed -n '4,7p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

status() {
  local pin installed ahead=""
  pin=$(autoship_pin_read) || { echo "pin: INVALID ($(autoship_pin_file))"; return 1; }
  installed=$(autoship_pin_installed)
  if [[ "$pin" =~ ^[0-9a-f]{40}$ ]]; then
    ahead=$(git -C "$REPO_ROOT" rev-list --count "$pin..origin/main" 2>/dev/null || echo unknown)
  fi
  echo "pin: ${pin:-none (bootstraps on the next deploy)}"
  echo "installed: ${installed:-none}"
  [ -z "$ahead" ] || echo "main ahead of pin: $ahead commit(s)"
  echo "pinned binary: $(autoship_pin_binary)"
  echo "auto-advance: $(autoship_pin_auto_mode)$( [ -z "${AUTOSHIP_PIN_AUTO:-}" ] || echo ' (AUTOSHIP_PIN_AUTO in this environment)')"
  if [[ "$pin" =~ ^[0-9a-f]{40}$ ]] && [ "$installed" != "$pin" ]; then
    echo "pending: the updater installs $pin on its next run"
  fi
}

apply=1
args=()
for arg in "$@"; do
  case "$arg" in
    --no-apply) apply=0 ;;
    -h|--help) usage ;;
    *) args+=("$arg") ;;
  esac
done
if [ "${#args[@]}" -eq 2 ] && [ "${args[0]}" = auto ]; then
  case "${args[1]}" in
    on|off)
      autoship_pin_write "$(autoship_pin_auto_file)" "${args[1]}"
      log "AUTOSHIP PIN AUTO set ${args[1]} by owner"
      echo "autoship pin auto-advance: ${args[1]}"
      exit 0 ;;
    *) usage ;;
  esac
fi
[ "${#args[@]}" -eq 1 ] || usage
ref="${args[0]}"

if [ "$ref" = status ]; then status; exit; fi
case "$ref" in
  auto)
    echo "usage: scripts/autoship-pin.sh auto on|off" >&2; exit 2 ;;
esac

old=$(autoship_pin_read 2>/dev/null || echo invalid)
if [ "$ref" = main ]; then
  new=main
else
  git -C "$REPO_ROOT" fetch origin main --tags --quiet || { echo "fetch failed; not moving the pin" >&2; exit 1; }
  new=$(git -C "$REPO_ROOT" rev-parse --verify --quiet "$ref^{commit}") || { echo "unknown ref: $ref" >&2; exit 1; }
  if ! git -C "$REPO_ROOT" merge-base --is-ancestor "$new" origin/main; then
    echo "$ref ($new) is not on origin/main; only merged commits can be pinned" >&2
    exit 1
  fi
fi
autoship_pin_write "$(autoship_pin_file)" "$new"
log "AUTOSHIP PIN MOVED by owner: ${old:-none} -> $new"
echo "autoship pin: ${old:-none} -> $new"
# A move that is not forward is a rollback or hold: stop the updater from
# advancing past it on its next tick.
if [[ "$old" =~ ^[0-9a-f]{40}$ ]] && [[ "$new" =~ ^[0-9a-f]{40}$ ]] && [ "$old" != "$new" ] &&
   ! git -C "$REPO_ROOT" merge-base --is-ancestor "$old" "$new" 2>/dev/null &&
   [ "$(autoship_pin_auto_mode)" = on ]; then
  autoship_pin_write "$(autoship_pin_auto_file)" off
  log "AUTOSHIP PIN AUTO set off: the owner pinned backwards to $new"
  echo "auto-advance turned off so the next tick keeps this pin; resume with: scripts/autoship-pin.sh auto on"
fi

if [ "$apply" -eq 1 ] && [ "$(uname -s)" = Linux ] && command -v systemctl >/dev/null 2>&1; then
  # One updater run applies it: build, install, and the worker reloads itself.
  if systemctl --user start --no-block augmentagent-update.service; then
    echo "started augmentagent-update.service; follow it in $LOG"
  else
    echo "could not start augmentagent-update.service; the next timer tick applies the pin" >&2
  fi
else
  echo "recorded; the next updater tick applies it"
fi
