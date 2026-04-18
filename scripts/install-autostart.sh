#!/bin/bash
# Register AugmentAgent Rust daemon as a macOS LaunchAgent so it:
#   - auto-starts on user login
#   - auto-restarts if it crashes
#   - survives Terminal close
#
# Writes ~/Library/LaunchAgents/com.nolanmak.augmentagent.plist and bootstraps it.
# Idempotent: running twice just reloads the agent with the current plist.
#
# Usage: ./scripts/install-autostart.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/augmentagent"

log() { printf '\033[1;36m[install-autostart]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-autostart ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(uname)" = "Darwin" ] || die "macOS only (uses launchd)"
[ -x "$REPO_ROOT/scripts/run-rs.sh" ] || die "run-rs.sh not executable — run chmod +x scripts/*.sh"
[ -x "$REPO_ROOT/target/release/augmentagent" ] \
  || die "release binary missing — run: cargo build --release -p augmentagent-cli"

mkdir -p "$LOG_DIR"
mkdir -p "$(dirname "$PLIST")"

# Build PATH that works for a graphical login (launchd inherits /usr/bin:/bin by default).
# Include the usual suspects so `claude`, `cargo`, `node`, Homebrew, etc. are found.
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
        <string>$REPO_ROOT/scripts/run-rs.sh</string>
        <string>--wiki-dir</string>
        <string>./wiki</string>
        <string>serve</string>
        <string>--dry-run</string>
        <string>false</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$LAUNCH_PATH</string>
        <key>RUST_LOG</key>
        <string>info</string>
        <key>HOME</key>
        <string>$HOME</string>
    </dict>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
        <key>Crashed</key>
        <true/>
    </dict>

    <key>ThrottleInterval</key>
    <integer>10</integer>

    <key>StandardOutPath</key>
    <string>$LOG_DIR/stdout.log</string>

    <key>StandardErrorPath</key>
    <string>$LOG_DIR/stderr.log</string>
</dict>
</plist>
PLIST_EOF

# Reload the agent so the new plist takes effect.
UID_NUM="$(id -u)"
DOMAIN="gui/$UID_NUM"

if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
  log "Previous agent found — bootout first"
  launchctl bootout "$DOMAIN/$LABEL" || true
  sleep 1
fi

log "Bootstrapping agent"
launchctl bootstrap "$DOMAIN" "$PLIST"
launchctl enable "$DOMAIN/$LABEL"
launchctl kickstart -k "$DOMAIN/$LABEL"

log "Installed."
log "  Label:     $LABEL"
log "  Plist:     $PLIST"
log "  Logs:      $LOG_DIR/"
log "  Status:    launchctl print $DOMAIN/$LABEL | head -30"
log "  Uninstall: ./scripts/uninstall-autostart.sh"
