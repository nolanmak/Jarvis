#!/usr/bin/env bash
# Shared opt-in macOS scheduler for wiki and finance sync. Sourced by the
# install/uninstall entrypoints; it never runs during the ordinary daemon setup.

scheduled_sync() {
  local action="$1" name="$2" repo_root="$3"
  local label="com.nolanmak.augmentagent.${name}-sync"
  local plist="$HOME/Library/LaunchAgents/$label.plist"
  local domain="gui/$(id -u)"

  [ "$(uname -s)" = Darwin ] || { echo "macOS only; Linux uses scripts/systemd/augmentagent-${name}-sync.*" >&2; return 1; }
  case "$name" in wiki|finance) ;; *) echo "unknown scheduled sync: $name" >&2; return 1 ;; esac

  if [ "$action" = uninstall ]; then
    if launchctl print "$domain/$label" >/dev/null 2>&1; then
      launchctl bootout "$domain/$label" || return 1
    fi
    rm -f "$plist"
    echo "Removed $label" >&2
    return 0
  fi
  [ "$action" = install ] || { echo "unknown action: $action" >&2; return 1; }

  local binary="$repo_root/target/release/augmentagent"
  [ -x "$binary" ] || { echo "release binary missing: $binary" >&2; return 1; }
  local wiki_dir="${AUGMENTAGENT_WIKI_DIR:-$repo_root/wiki}"
  case "$wiki_dir" in /*) ;; *) echo "AUGMENTAGENT_WIKI_DIR must be an absolute path" >&2; return 1 ;; esac
  [ -d "$wiki_dir" ] || { echo "wiki directory missing: $wiki_dir" >&2; return 1; }

  if [ "$name" = wiki ]; then
    local origins origin private slug
    origins="$(git -C "$wiki_dir" remote get-url --push --all origin 2>/dev/null)" || {
      echo "wiki origin is missing; configure a private GitHub remote first" >&2; return 1;
    }
    [ -n "$origins" ] || { echo "wiki origin has no push target" >&2; return 1; }
    while IFS= read -r origin; do
      case "$origin" in
        git@github.com:*|ssh://git@github.com/*|https://github.com/*)
          slug="${origin#git@github.com:}"
          slug="${slug#ssh://git@github.com/}"
          slug="${slug#https://github.com/}"
          slug="${slug%.git}"
          [[ "$slug" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
            echo "wiki origin is not a GitHub owner/repository path" >&2; return 1;
          }
          private="$(GH_HOST=github.com gh repo view "$slug" --json isPrivate --jq .isPrivate 2>/dev/null)" || {
            echo "could not verify that wiki origin is private" >&2; return 1;
          }
          [ "$private" = true ] || { echo "wiki origin must be private" >&2; return 1; }
          ;;
        /*|file://*) ;; # Disposable local remotes for testing/offline use.
        *) echo "wiki origin must be a private GitHub repo or local filesystem remote" >&2; return 1 ;;
      esac
    done <<< "$origins"
  fi

  # Preserve relative ./wiki for the default layout, while allowing a
  # checkout to point at a separate private wiki directory.
  if [ "$wiki_dir" = "$repo_root/wiki" ]; then wiki_dir=./wiki; fi

  source "$repo_root/scripts/lib/launchd-install.sh"
  local launch_path
  if [ "$name" = wiki ]; then
    launch_path="$(launchd_service_path git gh)" || return 1
  else
    launch_path="$(launchd_service_path)" || return 1
  fi

  local log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
  (umask 077; mkdir -p "$log_dir" "$(dirname "$plist")")
  chmod 700 "$log_dir"
  local candidate
  candidate="$(launchd_candidate "$plist")" || return 1
  local root_xml binary_xml wiki_xml home_xml path_xml log_xml
  root_xml="$(launchd_xml_escape "$repo_root")"
  binary_xml="$(launchd_xml_escape "$binary")"
  wiki_xml="$(launchd_xml_escape "$wiki_dir")"
  home_xml="$(launchd_xml_escape "$HOME")"
  path_xml="$(launchd_xml_escape "$launch_path")"
  log_xml="$(launchd_xml_escape "$log_dir")"

  cat > "$candidate" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>WorkingDirectory</key><string>$root_xml</string>
  <key>ProgramArguments</key><array>
    <string>$binary_xml</string><string>--wiki-dir</string><string>$wiki_xml</string>
    <string>$name</string><string>sync</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>$home_xml</string>
    <key>PATH</key><string>$path_xml</string>
  </dict>
  <key>Umask</key><integer>63</integer>
  <key>StartCalendarInterval</key>
PLIST_EOF

  if [ "$name" = wiki ]; then
    {
      echo '  <array>'
      local minute
      for minute in 0 10 20 30 40 50; do
        printf '    <dict><key>Minute</key><integer>%s</integer></dict>\n' "$minute"
      done
      echo '  </array>'
    } >> "$candidate"
  else
    {
      echo '  <array>'
      local hour
      for hour in 0 6 12 18; do
        printf '    <dict><key>Hour</key><integer>%s</integer><key>Minute</key><integer>17</integer></dict>\n' "$hour"
      done
      echo '  </array>'
    } >> "$candidate"
  fi

  cat >> "$candidate" <<PLIST_EOF
  <key>StandardOutPath</key><string>$log_xml/${name}-sync.log</string>
  <key>StandardErrorPath</key><string>$log_xml/${name}-sync.log</string>
</dict>
</plist>
PLIST_EOF
  launchd_install "$label" "$plist" "$candidate" false
  echo "Installed $label; status: $binary service --unit augmentagent-${name}-sync.timer status; logs: $log_dir/${name}-sync.log" >&2
}
