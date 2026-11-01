#!/usr/bin/env bash
# Return 75 for contention, 73 for capacity, 76 for unavailable admission,
# and 1 for an actual build failure. The caller leaves its stamp unchanged.
run_update_build() {
  local cli="$1" target="$2" help="" output="" code=0 legacy=0
  local repair="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/secure-release-executables.py"
  python3 "$repair" "$target/release" >> "$LOG" 2>&1 || return 1
  local driver=(cargo)
  if help=$("$cli" disk build --help 2>/dev/null); then
    case "$help" in
      *--update*) driver=("$cli" disk build --update --target-dir "$target" -- cargo) ;;
      *) driver=("$cli" disk build --target-dir "$target" -- cargo); legacy=1 ;;
    esac
  fi
  if output=$("${driver[@]}" build --release -p augmentagent-cli -p augmentagent-mcp-memory 2>&1); then
    code=0
  else
    code=$?
  fi
  printf '%s\n' "$output" >> "$LOG"
  # Bootstrap compatibility: older binaries used the low-disk prefix for
  # their own preflight contention. Never classify Cargo source diagnostics
  # merely because they quote one of these strings.
  if [ "$legacy" -eq 1 ] && [ "$code" -eq 1 ]; then
    case "$output" in
      "Error: gate refused: low disk:"*)
        case "$output" in
          *'build storage busy; retry admission'*|*'target lease busy'*|*'uncoordinated build is using'*) code=75 ;;
        esac ;;
    esac
  fi
  case "$code" in
    0) python3 "$repair" "$target/release" >> "$LOG" 2>&1 || return 1; return 0 ;;
    75) log "update deferred: storage busy; retry next tick"; return 75 ;;
    73) log "update deferred: low disk space or storage budget; retry after reclamation"; return 73 ;;
    76) log "update deferred: storage admission unavailable; retry next tick"; return 76 ;;
    *) log "RUST BUILD FAILED — not restarting; daemon stays on previous binary"; return 1 ;;
  esac
}
