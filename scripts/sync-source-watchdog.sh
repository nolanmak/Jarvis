#!/usr/bin/env bash
# Relaunch a data-source app when its database has gone cold.
#
# The exporters can only export what their source app has written. In October
# 2026 Notes.app had not run on this Mac for ~8 weeks and WhatsApp for ~2, so
# NoteStore.sqlite and ChatStorage.sqlite stopped advancing and the sync jobs
# reported "0 changed" every two minutes — indefinitely, with no error. 95 notes
# and 1,523 messages were missing from the archives by the time it was noticed.
#
# Freshness of the store is the signal, NOT whether a process is up: WhatsApp's
# ServiceExtension and Notes' Spotlight extension were both alive the whole
# time while the main apps were gone and nothing was being written.
#
# Launching is best effort. A relaunch that fails must not fail the tick, or
# one unavailable app would stop the other from being checked.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# The launchd plist exports only PATH, so the webhook has to come from .env the
# way the other standalone jobs load it. `set +u` while sourcing because an
# .env line referencing an unset variable would otherwise abort the whole tick,
# and a missing webhook must never cost us a relaunch.
if [ -f "$REPO_ROOT/.env" ]; then
  set -a
  set +u
  # shellcheck disable=SC1091
  . "$REPO_ROOT/.env" || true
  set +a
  set -u
fi

NOTIFY_OWNER_PREFIX="🧊"
if [ -f "$REPO_ROOT/scripts/lib/notify-owner.sh" ]; then
  # shellcheck source=lib/notify-owner.sh
  . "$REPO_ROOT/scripts/lib/notify-owner.sh"
else
  # 2 = "nothing to deliver with", matching the real helper, so a missing
  # library never stamps the throttle marker for a message nobody got.
  notify_owner() { return 2; }
fi

MAX_AGE_S="${WATCHDOG_MAX_AGE_S:-21600}"        # 6 h: longer than any normal gap
ALARM_AGE_S="${WATCHDOG_ALARM_AGE_S:-172800}"   # 48 h: relaunching is not working
BACKOFF_S="${WATCHDOG_BACKOFF_S:-1800}"         # 30 min between relaunch attempts
ALARM_NOTIFY_INTERVAL_S="${WATCHDOG_ALARM_NOTIFY_INTERVAL_S:-86400}"  # 1 post/day
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
mkdir -p "$STATE_DIR"

NOTES_DB="${WATCHDOG_NOTES_DB:-$HOME/Library/Group Containers/group.com.apple.notes/NoteStore.sqlite}"
WHATSAPP_DB="${WATCHDOG_WHATSAPP_DB:-$HOME/Library/Group Containers/group.net.whatsapp.WhatsApp.shared/ChatStorage.sqlite}"
MESSAGES_DB="${WATCHDOG_MESSAGES_DB:-$HOME/Library/Messages/chat.db}"

stamp() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }
say()   { printf '%s [watchdog] %s\n' "$(stamp)" "$*"; }

# Seconds since <file> was last written, or non-zero when it cannot be read.
# `stat -f %m` is BSD/macOS; `stat -c %Y` is GNU coreutils. The watchdog only
# ACTS on macOS, but the freshness logic is exercised by CI on Linux, so both
# have to work or the tests pass vacuously against a never-cold store.
age_of() {
  [ -f "$1" ] || return 1
  local mtime now
  mtime=$(stat -f %m "$1" 2>/dev/null) \
    || mtime=$(stat -c %Y "$1" 2>/dev/null) \
    || return 1
  now=$(date +%s)
  printf '%s' "$(( now - mtime ))"
}

# True at most once per BACKOFF_S per app: a cold store stays cold for a few
# ticks while the app launches, and `open` on every tick is its own bug.
may_relaunch() {
  local key="$1" marker="$STATE_DIR/watchdog-$1.last" last now
  now=$(date +%s)
  if [ -f "$marker" ]; then
    last=$(cat "$marker" 2>/dev/null || echo 0)
    [ $(( now - last )) -ge "$BACKOFF_S" ] || return 1
  fi
  printf '%s' "$now" > "$marker"
  return 0
}

alarm_marker() { printf '%s' "$STATE_DIR/watchdog-$1.alarm-notified"; }

# True when the last *delivered* alarm post for this app is older than
# ALARM_NOTIFY_INTERVAL_S. An alarming store stays alarming until a human
# fixes it, so at a 15-minute tick an unthrottled post is ~96 Discord messages
# a day for the same fact.
alarm_notify_due() {
  local marker last now
  marker=$(alarm_marker "$1")
  [ -f "$marker" ] || return 0
  now=$(date +%s)
  last=$(cat "$marker" 2>/dev/null || echo 0)
  [ $(( now - last )) -ge "$ALARM_NOTIFY_INTERVAL_S" ]
}

# Stamps the throttle window. Called only after the webhook accepted the post:
# a marker written on a failed delivery would silence the next day of retries
# while the owner was never told anything.
alarm_notify_recorded() {
  # 2>/dev/null comes first so the shell's own message about a failed `>`
  # redirect is suppressed too, not just printf's.
  printf '%s' "$(date +%s)" 2>/dev/null > "$(alarm_marker "$1")" \
    || say "state dir is not writable; alarm notifications cannot be throttled"
}

# check <key> <app name> <database>
check() {
  local key="$1" app="$2" db="$3" age
  if ! age=$(age_of "$db"); then
    say "$(basename "$db") is missing; cannot judge $app freshness"
    return 0
  fi
  if [ "$age" -le "$MAX_AGE_S" ]; then
    return 0
  fi
  say "$app store is $(( age / 3600 ))h cold (threshold $(( MAX_AGE_S / 3600 ))h)"
  # Past the alarm threshold the relaunching has demonstrably not worked —
  # signed out, uninstalled, or needing a device re-pair. Say so loudly rather
  # than retrying in silence forever, which is the outage this script exists
  # to end.
  if [ "$age" -ge "$ALARM_AGE_S" ]; then
    say "ALARM: $app store has been cold for $(( age / 86400 ))d; relaunching is not fixing it — check that $app is installed and signed in"
    # A line in a log file nobody reads is how the original outage lasted two
    # weeks. The alarm is the one state the watchdog cannot fix itself, so it
    # has to reach a human channel too.
    if alarm_notify_due "$key"; then
      notify_owner "$app store has been cold for $(( age / 86400 ))d ($(( age / 3600 ))h); its archive is frozen and relaunching is not fixing it — check that $app is installed and signed in"
      case $? in
        0) alarm_notify_recorded "$key"
           say "alarm posted to the owner channel" ;;
        1) say "alarm post to the owner channel failed; retrying on the next tick" ;;
        *) ;;  # no webhook configured; the log line above is the only channel
      esac
    else
      say "alarm notification suppressed; already posted within the notify window"
    fi
  fi
  if ! may_relaunch "$key"; then
    say "$app relaunch suppressed; already attempted within the backoff window"
    return 0
  fi
  say "relaunching $app"
  open -a "$app" >/dev/null 2>&1 || say "could not launch $app; will retry after the backoff"
  return 0
}

check notes    "Notes"    "$NOTES_DB"
check whatsapp "WhatsApp" "$WHATSAPP_DB"
# Messages is included for symmetry: it is the one source that has stayed
# healthy, and a cold chat.db would be the same class of outage.
check messages "Messages" "$MESSAGES_DB"
exit 0
