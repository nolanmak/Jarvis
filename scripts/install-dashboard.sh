#!/bin/bash
# Register the Node dashboard as a user-level service so it runs alongside
# the Rust daemon. Provides the resume-upload UI at http://<host>:3000/resume
# and the dashboard at http://<host>:3000/dashboard (tailnet-accessible on
# Linux).
#
# Cross-platform: launchd plist on macOS, systemd user unit on Linux.
# Idempotent — re-running reloads with the current config.
#
# Usage: ./scripts/install-dashboard.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="com.nolanmak.augmentagent-dashboard"
UNIT_NAME="augmentagent-dashboard.service"

log() { printf '\033[1;36m[install-dashboard]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install-dashboard ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[ -x "$REPO_ROOT/scripts/run-dashboard.sh" ] \
  || die "run-dashboard.sh not executable — run chmod +x scripts/*.sh"

# Make sure the TS has been built. node + npm may live under ~/.local/bin or
# /usr/bin/node depending on install method, so prefer PATH lookup.
if [ ! -f "$REPO_ROOT/dist/dashboard-server.js" ]; then
  if command -v npm >/dev/null 2>&1; then
    log "Building dashboard (dist/dashboard-server.js missing)"
    (cd "$REPO_ROOT" && npm install --production=false && npm run build) \
      || die "npm build failed"
  else
    die "npm not found and dist/ missing — install node + run 'npm install && npm run build'"
  fi
fi

install_macos() {
  local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  local LOG_DIR="$HOME/Library/Logs/augmentagent-dashboard"
  mkdir -p "$LOG_DIR"
  mkdir -p "$(dirname "$PLIST")"

  local LAUNCH_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

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
        <string>$REPO_ROOT/scripts/run-dashboard.sh</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$LAUNCH_PATH</string>
        <key>DASHBOARD_PORT</key>
        <string>3000</string>
        <key>AUGMENTAGENT_WIKI_DIR</key>
        <string>$REPO_ROOT/wiki</string>
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

  local UID_NUM
  UID_NUM="$(id -u)"
  local DOMAIN="gui/$UID_NUM"

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
  log "  URL:       http://localhost:3000/resume"
  log "  Logs:      $LOG_DIR/"
}

install_linux() {
  local UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  local UNIT="$UNIT_DIR/$UNIT_NAME"
  local LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent-dashboard"
  mkdir -p "$UNIT_DIR" "$LOG_DIR"

  command -v systemctl >/dev/null 2>&1 \
    || die "systemctl not found — this installer needs systemd"

  local SERVICE_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

  log "Writing unit: $UNIT"
  cat > "$UNIT" <<UNIT_EOF
[Unit]
Description=AugmentAgent dashboard (Node/Express)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$REPO_ROOT
ExecStart=$REPO_ROOT/scripts/run-dashboard.sh
Environment=PATH=$SERVICE_PATH
Environment=DASHBOARD_PORT=3000
Environment=AUGMENTAGENT_WIKI_DIR=$REPO_ROOT/wiki
Restart=on-failure
RestartSec=10
StandardOutput=append:$LOG_DIR/stdout.log
StandardError=append:$LOG_DIR/stderr.log

[Install]
WantedBy=default.target
UNIT_EOF

  log "Reloading systemd --user"
  systemctl --user daemon-reload

  log "Enabling + (re)starting $UNIT_NAME"
  systemctl --user enable "$UNIT_NAME" >/dev/null
  systemctl --user restart "$UNIT_NAME"

  log "Installed."
  log "  Unit:      $UNIT"
  log "  URL:       http://localhost:3000/resume  (tailnet: http://<this-host>:3000/resume)"
  log "  Logs:      $LOG_DIR/  (also: journalctl --user -u $UNIT_NAME -f)"
  log "  Status:    systemctl --user status $UNIT_NAME"
}

case "$(uname -s)" in
  Darwin) install_macos ;;
  Linux)  install_linux ;;
  *)      die "unsupported platform: $(uname -s)" ;;
esac
