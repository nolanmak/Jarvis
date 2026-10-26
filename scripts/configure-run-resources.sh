#!/usr/bin/env bash
# Render/apply the production run-resource drop-in without changing ExecStart.
# Rollout: augmentagent deploy snapshot; this script --apply; then --check.
# --apply reloads/restarts the existing daemon; never run it during a build.
set -euo pipefail
mode="${1:---check}"
unit="${AUGMENTAGENT_RESOURCE_UNIT:-augmentagent.service}"
[[ "$unit" =~ ^[A-Za-z0-9_.@-]+\.service$ ]] || { echo 'invalid resource service unit name' >&2; exit 2; }
aggregate_cpu_quota="${AUGMENTAGENT_UNIT_CPU_QUOTA:-400%}"
run_cpu_percent="${AUGMENTAGENT_RUN_CPU_PERCENT:-200}"
case "$mode" in --check|--apply|--install|--render) ;; *) echo 'usage: configure-run-resources.sh [--check|--render|--install|--apply]' >&2; exit 2;; esac
render() {
  cat <<EOF
[Service]
Delegate=cpu memory pids
DelegateSubgroup=daemon
CPUQuota=$aggregate_cpu_quota
Environment=AUGMENTAGENT_RUN_RESOURCES=1
Environment=AUGMENTAGENT_RUN_MEMORY_BYTES=${AUGMENTAGENT_RUN_MEMORY_BYTES:-4294967296}
Environment=AUGMENTAGENT_RUN_PIDS=${AUGMENTAGENT_RUN_PIDS:-512}
Environment=AUGMENTAGENT_RUN_CPU_PERCENT=$run_cpu_percent
EOF
}
if [[ "$mode" == --render ]]; then render; exit; fi
unsupported() {
  # A clean unsupported host may keep serving unrelated interactive calls.
  # Never classify an already enabled incompatible resource policy as safe.
  local environment
  environment="$(systemctl --user show "$unit" --value -p Environment)" || { echo 'cannot verify current resource enrollment' >&2; exit 1; }
  if [[ "$environment" == *AUGMENTAGENT_RUN_RESOURCES=1* ]]; then
    echo "unsupported resource configuration is already enabled: $1" >&2
    exit 1
  fi
  echo "run resources unavailable; required Autoship remains disabled: $1" >&2
  exit 3
}
version="$(systemctl --version 2>/dev/null | awk 'NR==1 {print $2}')" || version=""
[[ "$version" =~ ^[0-9]+$ ]] && (( version >= 254 )) || unsupported 'systemd 254+ required for DelegateSubgroup'
# The existing aggregate ceilings remain authoritative; never move work to an
# unbounded sibling slice or silently weaken the original MemoryMax/TasksMax.
for property in MemoryMax TasksMax; do
  value="$(systemctl --user show "$unit" --value -p "$property")"
  [[ "$value" =~ ^[0-9]+$ ]] && (( value > 0 )) || unsupported "finite existing $property required"
done
# Preserve a stricter existing aggregate CPU ceiling. systemctl presents this
# duration in human units; parse only that public property, never Environment.
existing_cpu="$(systemctl --user show "$unit" --value -p CPUQuotaPerSecUSec)"
limits="$(python3 - "$existing_cpu" "$aggregate_cpu_quota" "$run_cpu_percent" <<'PY_LIMITS'
import decimal,re,sys
existing,requested,run=sys.argv[1:]
D=decimal.Decimal
if not re.fullmatch(r'[0-9]+(?:\.[0-9]+)?%',requested): raise SystemExit('invalid aggregate CPU quota')
quota=D(requested[:-1])
if not existing: raise SystemExit('cannot verify existing aggregate CPU quota')
if existing!='infinity':
 units={'us':D('0.000001'),'µs':D('0.000001'),'ms':D('.001'),'s':D(1),'min':D(60),'h':D(3600),'d':D(86400)}
 parts=re.findall(r'([0-9]+(?:\.[0-9]+)?)(us|µs|ms|min|s|h|d)',existing)
 if not parts or ''.join(a+b for a,b in parts)!=existing.replace(' ',''): raise SystemExit('cannot verify existing aggregate CPU quota')
 quota=min(quota,sum(D(a)*units[b] for a,b in parts)*100)
if quota<1 or not run.isdigit() or int(run)<1: raise SystemExit('unsupported CPU resource quota')
print(f'{quota.normalize():f}% {min(int(run),int(quota))}')
PY_LIMITS
)"
read -r aggregate_cpu_quota run_cpu_percent <<< "$limits"
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
