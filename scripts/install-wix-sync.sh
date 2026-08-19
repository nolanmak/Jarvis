#!/bin/bash
# Register the daily Wix Events calendar mirror (#633).
#
# Cross-platform: launchd on macOS (StartCalendarInterval), systemd user timer
# on Linux (OnCalendar). Idempotent — rerunning replaces the prior
# registration. Runs at 07:15 local by default.
#
# The job is CREATE ONLY and idempotent, so a missed or doubled run is safe.
# With AUGMENTAGENT_WIX_SYNC_REQUIRE_APPROVAL=1 (the default) the scheduled run
# only ever prints a plan — publishing needs a human with --yes.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent.wix-sync"
HOUR="${AUGMENTAGENT_WIX_SYNC_HOUR:-7}"
MINUTE="${AUGMENTAGENT_WIX_SYNC_MINUTE:-15}"

log() { printf '\033[1;36m[install-wix-sync]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-wix-sync ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ -f "$REPO_ROOT/scripts/wix-events-sync.mjs" ] || die "scripts/wix-events-sync.mjs missing"
command -v node >/dev/null 2>&1 || die "node not on PATH"

for v in AUGMENTAGENT_WIX_API_KEY AUGMENTAGENT_WIX_SITE_ID AUGMENTAGENT_WIX_MEETUP_GROUPS; do
  grep -qE "^${v}=.+" "$REPO_ROOT/.env" 2>/dev/null \
    || log "warning: $v is not set in .env — the job will refuse to run until it is"
done

case "$HOUR" in ''|*[!0-9]*) die "hour must be 0-23 (got '$HOUR')";; esac
case "$MINUTE" in ''|*[!0-9]*) die "minute must be 0-59 (got '$MINUTE')";; esac

install_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local LOG_DIR="$HOME/Library/Logs/augmentagent"
  mkdir -p "$LOG_DIR" "$(dirname "$PLIST")"

  log "Writing plist: $PLIST (daily at ${HOUR}:$(printf '%02d' "$MINUTE"))"
  cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>WorkingDirectory</key>
    <string>$REPO_ROOT</string>
    <key>ProgramArguments</key>
    <array>
        <string>$(command -v node)</string>
        <string>$REPO_ROOT/scripts/wix-events-sync.mjs</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key><integer>$HOUR</integer>
        <key>Minute</key><integer>$MINUTE</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>$LOG_DIR/wix-sync.log</string>
    <key>StandardErrorPath</key>
    <string>$LOG_DIR/wix-sync.log</string>
</dict>
</plist>
PLIST_EOF

  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
  log "Installed. Log: $LOG_DIR/wix-sync.log"
}

install_linux() {
  local UNIT_DIR="$HOME/.config/systemd/user"
  mkdir -p "$UNIT_DIR" "$HOME/.local/state/augmentagent"

  log "Installing user units into $UNIT_DIR"
  install -m 644 "$REPO_ROOT/systemd/augmentagent-wix-sync.service" "$UNIT_DIR/"
  install -m 644 "$REPO_ROOT/systemd/augmentagent-wix-sync.timer" "$UNIT_DIR/"

  if [ "$HOUR:$MINUTE" != "7:15" ]; then
    log "Overriding schedule to ${HOUR}:$(printf '%02d' "$MINUTE")"
    mkdir -p "$UNIT_DIR/augmentagent-wix-sync.timer.d"
    cat > "$UNIT_DIR/augmentagent-wix-sync.timer.d/override.conf" <<OVERRIDE_EOF
[Timer]
OnCalendar=
OnCalendar=*-*-* ${HOUR}:$(printf '%02d' "$MINUTE"):00
OVERRIDE_EOF
  fi

  systemctl --user daemon-reload
  systemctl --user enable --now augmentagent-wix-sync.timer
  log "Installed. Next run: $(systemctl --user list-timers augmentagent-wix-sync.timer --no-pager | sed -n 2p)"
  log "Log: ~/.local/state/augmentagent/wix-sync.log"
}

case "$(uname -s)" in
  Darwin) install_macos ;;
  Linux)  install_linux ;;
  *)      die "unsupported platform: $(uname -s)" ;;
esac

log "Dry run now:  node $REPO_ROOT/scripts/wix-events-sync.mjs"
log "Publish:      node $REPO_ROOT/scripts/wix-events-sync.mjs --yes"
