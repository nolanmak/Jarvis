#!/usr/bin/env bash
# Render/apply the production run-resource drop-in without changing ExecStart.
# Rollout: augmentagent deploy snapshot; this script --apply; then --check.
# --apply reloads/restarts the existing daemon; never run it during a build.
set -euo pipefail
mode="${1:---check}"
unit="augmentagent.service"
case "$mode" in --check|--apply|--install|--render) ;; *) echo 'usage: configure-run-resources.sh [--check|--render|--install|--apply]' >&2; exit 2;; esac
render() {
  cat <<EOF
[Service]
Delegate=cpu memory pids
DelegateSubgroup=daemon
CPUQuota=${AUGMENTAGENT_UNIT_CPU_QUOTA:-400%}
Environment=AUGMENTAGENT_RUN_RESOURCES=1
Environment=AUGMENTAGENT_RUN_MEMORY_BYTES=${AUGMENTAGENT_RUN_MEMORY_BYTES:-4294967296}
Environment=AUGMENTAGENT_RUN_PIDS=${AUGMENTAGENT_RUN_PIDS:-512}
Environment=AUGMENTAGENT_RUN_CPU_PERCENT=${AUGMENTAGENT_RUN_CPU_PERCENT:-200}
EOF
}
if [[ "$mode" == --render ]]; then render; exit; fi
version="$(systemd --version | head -1 | awk '{print $2}')"
[[ "$version" =~ ^[0-9]+$ ]] && (( version >= 254 )) || { echo 'systemd 254+ required for DelegateSubgroup' >&2; exit 1; }
# The existing aggregate ceilings remain authoritative; never move work to an
# unbounded sibling slice or silently weaken the original MemoryMax/TasksMax.
for property in MemoryMax TasksMax; do
  value="$(systemctl --user show "$unit" --value -p "$property")"
  [[ "$value" =~ ^[0-9]+$ ]] && (( value > 0 )) || { echo "finite existing $property required" >&2; exit 1; }
done
if [[ "$mode" == --apply || "$mode" == --install ]]; then
  destination="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$unit.d"
  mkdir -p "$destination"
  candidate="$(mktemp "$destination/.run-resources.XXXXXX")"
  trap 'rm -f "$candidate"' EXIT
  existing="$destination/60-run-resources.conf"
  if [[ "$mode" == --install && -f "$existing" ]] && grep -qx 'Delegate=cpu memory pids' "$existing" && grep -qx 'DelegateSubgroup=daemon' "$existing" && grep -qx 'Environment=AUGMENTAGENT_RUN_RESOURCES=1' "$existing"; then
    # Routine updates retain explicitly configured operator resource ceilings.
    rm -f "$candidate"
  else
    render > "$candidate"
    chmod 600 "$candidate"
    mv "$candidate" "$existing"
  fi
  systemctl --user daemon-reload
  if [[ "$mode" == --install ]]; then
    echo 'Run-resource drop-in installed; caller must use the normal guarded daemon restart.'
    exit 0
  fi
  systemctl --user restart "$unit"
fi
systemctl --user show "$unit" -p ControlGroup -p Delegate -p DelegateSubgroup -p MemoryMax -p TasksMax -p CPUQuotaPerSecUSec --no-pager
[[ "$(systemctl --user show "$unit" --value -p Delegate)" == yes ]] || { echo 'resource delegation not installed' >&2; exit 1; }
[[ "$(systemctl --user show "$unit" --value -p DelegateSubgroup)" == daemon ]] || { echo 'daemon leaf not installed' >&2; exit 1; }
echo 'Run-resource unit configured; verify protected per-run admission receipts before enabling Autoship.'
