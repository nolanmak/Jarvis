#!/bin/bash
# Register a periodic LaunchAgent that checks GitHub every 5 minutes for
# new commits on main, pulls + rebuilds + restarts the daemon if there are.
#
# Paired with install-autostart.sh (which runs the daemon itself). They're
# separate agents so updater failures don't take the daemon down.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent.updater"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/augmentagent"
INTERVAL_SECS=300

log() { printf '\033[1;36m[install-autoupdate]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-autoupdate ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(uname)" = "Darwin" ] || die "macOS only"
[ -x "$REPO_ROOT/scripts/check-for-updates.sh" ] \
  || die "check-for-updates.sh not executable"

mkdir -p "$LOG_DIR"
mkdir -p "$(dirname "$PLIST")"

LAUNCH_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

log "Writing plist: $PLIST"
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
        <string>$REPO_ROOT/scripts/check-for-updates.sh</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$LAUNCH_PATH</string>
        <key>HOME</key>
        <string>$HOME</string>
    </dict>

    <key>StartInterval</key>
    <integer>$INTERVAL_SECS</integer>

    <key>RunAtLoad</key>
    <true/>

    <key>StandardOutPath</key>
    <string>$LOG_DIR/update.stdout.log</string>

    <key>StandardErrorPath</key>
    <string>$LOG_DIR/update.stderr.log</string>
</dict>
</plist>
PLIST_EOF

UID_NUM="$(id -u)"
DOMAIN="gui/$UID_NUM"

if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
  log "Previous updater found — bootout first"
  launchctl bootout "$DOMAIN/$LABEL" || true
  sleep 1
fi

log "Bootstrapping updater (runs every ${INTERVAL_SECS}s)"
launchctl bootstrap "$DOMAIN" "$PLIST"
launchctl enable "$DOMAIN/$LABEL"

log "Installed."
log "  Label:     $LABEL"
log "  Interval:  ${INTERVAL_SECS}s"
log "  Logs:      $LOG_DIR/update.log"
log "  Uninstall: ./scripts/uninstall-autoupdate.sh"
