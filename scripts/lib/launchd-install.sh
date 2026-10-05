#!/usr/bin/env bash
# Shared helpers for user LaunchAgent installers. Call only from the Darwin path.

launchd_xml_escape() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' \
    -e 's/>/\&gt;/g' -e 's/"/\&quot;/g' -e "s/'/\&apos;/g"
}

launchd_candidate() {
  mktemp "${1}.new.XXXXXX"
}

# Include directories where the selected tools were found during setup. A
# graphical login does not inherit the interactive shell's PATH.
launchd_service_path() {
  local path="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/opt/coreutils/libexec/gnubin:/usr/local/opt/coreutils/libexec/gnubin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
  local tool found directory
  for tool in "$@"; do
    found="$(command -v "$tool" 2>/dev/null || PATH="$path:$PATH" command -v "$tool" 2>/dev/null || true)"
    if [ -z "$found" ] || [ ! -x "$found" ]; then
      printf 'required launchd tool %s is missing; install it and rerun this installer\n' "$tool" >&2
      return 1
    fi
    directory="$(dirname "$found")"
    case ":$path:" in
      *":$directory:"*) ;;
      *) path="$directory:$path" ;;
    esac
  done
  printf '%s\n' "$path"
}

launchd_reasoner_tools() {
  local repo="$1" chain="${AUGMENTAGENT_REASONER_CHAIN:-}" provider
  if [ -z "$chain" ] && [ -f "$repo/.env" ]; then
    chain="$(sed -n 's/^[[:space:]]*AUGMENTAGENT_REASONER_CHAIN[[:space:]]*=[[:space:]]*//p' "$repo/.env" | tail -n 1)"
    chain="${chain%%#*}"
    chain="${chain//[[:space:]\"\']/}"
  fi
  chain="${chain:-claude}"
  local -a providers
  IFS=, read -ra providers <<< "$chain"
  for provider in "${providers[@]}"; do
    case "$provider" in
      claude|codex|gemini) printf '%s\n' "$provider" ;;
      cerebras) ;;
      *) printf 'unsupported reasoner in AUGMENTAGENT_REASONER_CHAIN: %s\n' "$provider" >&2; return 1 ;;
    esac
  done
}

launchd_validate() {
  if command -v plutil >/dev/null 2>&1; then
    plutil -lint "$1" >/dev/null
  else
    python3 -c 'import plistlib,sys; plistlib.load(open(sys.argv[1], "rb"))' "$1"
  fi
}

# Replace a job only after a candidate is valid. If bootstrap/enable fails,
# restore the previous on-disk plist and the previously loaded job.
launchd_install() {
  local label="$1" plist="$2" candidate="$3" kick="${4:-false}"
  local domain="gui/$(id -u)" backup="" loaded=false
  if ! launchd_validate "$candidate"; then
    rm -f "$candidate"
    printf 'invalid launchd plist: %s\n' "$plist" >&2
    return 1
  fi
  if [ -e "$plist" ]; then
    backup="$(launchd_candidate "$plist")"
    cp -p "$plist" "$backup"
  fi
  if launchctl print "$domain/$label" >/dev/null 2>&1; then
    loaded=true
    if ! launchctl bootout "$domain/$label"; then
      rm -f "$candidate"
      [ -z "$backup" ] || rm -f "$backup"
      return 1
    fi
  fi
  if ! mv -f "$candidate" "$plist"; then
    rm -f "$candidate"
    if [ "$loaded" = true ]; then launchctl bootstrap "$domain" "$plist" || true; fi
    [ -z "$backup" ] || rm -f "$backup"
    return 1
  fi
  if launchctl bootstrap "$domain" "$plist" &&
      launchctl enable "$domain/$label" &&
      { [ "$kick" != true ] || launchctl kickstart -k "$domain/$label"; }; then
    [ -z "$backup" ] || rm -f "$backup"
    return 0
  fi
  launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
  if [ -n "$backup" ]; then mv -f "$backup" "$plist"; else rm -f "$plist"; fi
  if [ "$loaded" = true ]; then
    launchctl bootstrap "$domain" "$plist" || printf 'failed to restore previous launchd job: %s\n' "$label" >&2
  fi
  printf 'launchd install failed; previous plist restored: %s\n' "$label" >&2
  return 1
}
