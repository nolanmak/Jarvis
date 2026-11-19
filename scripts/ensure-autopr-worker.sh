#!/usr/bin/env bash
# Provision the auto-ship worker (`augmentagent auto-pr-worker`) independently
# of daemon restart. It is auto-ship's only execution path (#2275): systemd
# runs it as augmentagent-autopr.service on Linux and launchd runs it as the
# com.nolanmak.augmentagent.autopr LaunchAgent on macOS. The daemon never
# hosts the loop, whatever AUGMENTAGENT_AUTOPR_EXTERNAL says.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export REPO_ROOT

# macOS mirror of scripts/systemd/augmentagent-autopr.service: same command,
# working directory, environment (the daemon job's, plus the repo .env the
# binary loads itself), logs and restart policy. KeepAlive restarts on any
# non-zero exit, including 75 (load the updated executable), and leaves a
# disabled worker's clean exit 0 stopped. ThrottleInterval mirrors RestartSec.
ensure_macos() {
  local label="com.nolanmak.augmentagent.autopr"
  local agents="$HOME/Library/LaunchAgents"
  local plist="$agents/$label.plist"
  local log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent"
  # shellcheck source=lib/launchd-install.sh
  source "$REPO_ROOT/scripts/lib/launchd-install.sh"
  mkdir -p "$agents" "$log_dir"
  python3 "$REPO_ROOT/scripts/secure-release-executables.py" "$REPO_ROOT/target/release"
  local candidate status=0
  candidate="$(launchd_candidate "$plist")"
  python3 - "$plist" "$agents/com.nolanmak.augmentagent.plist" "$candidate" "$log_dir" "$label" <<'PY' || status=$?
import os, plistlib, sys
plist, daemon, candidate, log_dir, label = sys.argv[1:]
repo, home = os.environ['REPO_ROOT'], os.environ['HOME']
marker = b'<!-- Managed by ensure-autopr-worker.sh -->'
try:
    with open(daemon, 'rb') as f: job = plistlib.load(f)
except FileNotFoundError:
    job = {}
arguments = job.get('ProgramArguments', [])
dry = os.environ.get('AUGMENTAGENT_AUTOSTART_DRY_RUN')
if dry is None:
    dry = next((arguments[i + 1] for i, a in enumerate(arguments[:-1]) if a == '--dry-run'), 'true')
if dry not in ('true', 'false'):
    sys.exit('invalid worker dry-run setting')
if os.environ.get('AUGMENTAGENT_NATIVE_VM_POLICY'):
    print('native VM policy selection is Linux-only; not applied to the macOS worker', file=sys.stderr)
environment = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'RUST_LOG': 'info', 'HOME': home}
environment.update({k: str(v) for k, v in job.get('EnvironmentVariables', {}).items()})
environment.pop('AUGMENTAGENT_AUTOPR_EXTERNAL', None)
data = plistlib.dumps({
    'Label': label,
    'WorkingDirectory': repo,
    'ProgramArguments': [repo + '/scripts/run-rs.sh', 'auto-pr-worker', '--dry-run', dry],
    'EnvironmentVariables': environment,
    'RunAtLoad': True,
    'KeepAlive': {'SuccessfulExit': False, 'Crashed': True},
    'ThrottleInterval': 3,
    'StandardOutPath': log_dir + '/stdout.log',
    'StandardErrorPath': log_dir + '/stderr.log',
})
declaration, rest = data.split(b'\n', 1)
data = declaration + b'\n' + marker + b'\n' + rest
if os.path.lexists(plist):
    with open(plist, 'rb') as f: previous = f.read()
    if marker not in previous:
        sys.exit('preserving unmanaged launchd job ' + plist)
    if previous == data:
        sys.exit(10)
with open(candidate, 'wb') as f: f.write(data)
PY
  case "$status" in
    0) ;;
    10)
      rm -f "$candidate"
      # Unchanged and loaded: leave the running worker alone. It reloads a
      # new executable itself at its next clean boundary (exit 75).
      if launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then return 0; fi
      cp -p "$plist" "$candidate" ;;
    *) rm -f "$candidate"; return 1 ;;
  esac
  launchd_install "$label" "$plist" "$candidate" false
}

case "$(uname -s)" in
  Linux) ;;
  Darwin) ensure_macos; exit ;;
  *) exit 0 ;;
esac
python3 - <<'PY'
import os, pathlib, re, shlex, subprocess, tempfile, json, stat
repo=pathlib.Path(os.environ['REPO_ROOT'])
home=pathlib.Path.home()
unit_dir=pathlib.Path(os.environ.get('XDG_CONFIG_HOME',str(home/'.config')))/'systemd/user'
log_dir=pathlib.Path(os.environ.get('XDG_STATE_HOME',str(home/'.local/state')))/'augmentagent'
def show(key):
 r=subprocess.run(['systemctl','--user','show','augmentagent.service','-p',key,'--value'],capture_output=True,text=True)
 return r.stdout.strip() if r.returncode==0 else ''
def escaped(value):
 if '\n' in value or '\r' in value: raise RuntimeError('newline in service configuration')
 return value.replace('\\','\\\\').replace('"','\\"').replace('%','%%')
def environment_file(path,optional=False,literal=False):
 # This directive consumes the whole path; embedded shell quotes become
 # literal filename characters. Preserve inherited glob patterns below.
 if not pathlib.Path(path).is_absolute() or any(c in path for c in '\n\r\0'):
  raise RuntimeError('environment file must be an absolute single-line path')
 if path.endswith('\\'):
  raise RuntimeError('environment file path cannot end in a line continuation')
 if literal:
  # EnvironmentFile uses libc glob patterns. Inherited daemon directives keep
  # their patterns; our shared selection names one exact file instead.
  path=''.join('\\'+c if c in '\\*?[]' else c for c in path)
 return 'EnvironmentFile='+('-' if optional else '')+path.replace('%','%%')
plan={}
def write(path,text):
 path.parent.mkdir(parents=True,exist_ok=True)
 if path.exists():
  previous=path.read_text()
  if not previous.startswith('# Managed by ensure-autopr-worker.sh'):
   raise RuntimeError('preserving unmanaged unit '+str(path))
  if previous==text:return
 plan[path]=text.encode()
def replace(path,data,mode=0o600):
 with tempfile.NamedTemporaryFile(dir=path.parent,prefix='.autopr-',delete=False) as f:
  temporary=pathlib.Path(f.name)
  try:
   f.write(data);f.flush();os.fsync(f.fileno());os.fchmod(f.fileno(),mode)
   temporary.replace(path)
  finally:temporary.unlink(missing_ok=True)
def setting(name,override,fallback):
 value=os.environ.get(override) or show(name) or fallback
 if not re.fullmatch(r'(?:[0-9]+[KMGTPE]?|infinity)',value):
  raise RuntimeError('invalid resource setting '+name)
 return value

env=shlex.split(show('Environment'))
extra=['Environment="'+escaped(value)+'"' for value in env]
# systemctl expands EnvironmentFile specifiers; preserve the resolved paths,
# optional-file semantics, and order without reading credential contents.
raw=show('EnvironmentFiles')
while raw:
 match=re.match(r'(.*?) \(ignore_errors=(yes|no)\)(?:\s+|$)',raw,re.S)
 if not match:raise RuntimeError('cannot preserve daemon EnvironmentFiles')
 name,optional=match.groups()
 extra.append(environment_file(name,optional=='yes'))
 raw=raw[match.end():]
# Selection is explicit; merely staging a profile never activates it. Keep a
# shared final EnvironmentFile so both units use the same choice on rollout.
# This validates safe configuration storage, not live provider readiness.
selection=unit_dir.parent.parent/'augmentagent/autoship-native.env'
selected=os.environ.get('AUGMENTAGENT_NATIVE_VM_POLICY')
selection_text=None
if selected:
 path=pathlib.Path(selected)
 if not path.is_absolute() or '\n' in selected or '\r' in selected:
  raise RuntimeError('native policy selection must be an absolute single-line path')
 with os.fdopen(os.open(path,os.O_RDONLY|os.O_NOFOLLOW),'rb') as f:
  meta=os.fstat(f.fileno())
  if not stat.S_ISREG(meta.st_mode) or meta.st_uid!=os.geteuid() or meta.st_mode & 0o077 or meta.st_nlink!=1 or meta.st_size>2*1024*1024:
   raise RuntimeError('native policy must be an owner-private regular file')
  policy=json.load(f)
 if policy.get('version')!=1 or not isinstance(policy.get('providers'),dict) or not policy['providers']:
  raise RuntimeError('invalid native policy declaration')
 # EnvironmentFile does not expand systemd specifiers or shell variables.
 value=selected.replace('\\','\\\\').replace('"','\\"')
 selection_text='# Managed by ensure-autopr-worker.sh\nAUGMENTAGENT_NATIVE_VM_POLICY="'+value+'"\n'
shared_environment=environment_file(str(selection),True,literal=True)
extra.append(shared_environment)
dry=os.environ.get('AUGMENTAGENT_AUTOSTART_DRY_RUN')
if dry is None:
 match=re.search(r'--dry-run\s+(true|false)',show('ExecStart'))
 dry=match.group(1) if match else 'true'
if dry not in ('true','false'):raise RuntimeError('invalid worker dry-run setting')
high=setting('MemoryHigh','AUGMENTAGENT_UNIT_MEMORY_HIGH','8G')
maximum=setting('MemoryMax','AUGMENTAGENT_UNIT_MEMORY_MAX','10G')
tasks=setting('TasksMax','AUGMENTAGENT_UNIT_TASKS_MAX','2048')
# Keep the existing aggregate CPU ceiling across both services. systemctl
# exposes a human duration per second, e.g. 4s means 400 percent.
quota=show('CPUQuotaPerSecUSec') or 'infinity'
cpu=''
if quota!='infinity':
 import decimal
 units={'us':decimal.Decimal('.000001'),'µs':decimal.Decimal('.000001'),'ms':decimal.Decimal('.001'),'s':decimal.Decimal(1),'min':decimal.Decimal(60),'h':decimal.Decimal(3600)}
 parts=re.findall(r'([0-9]+(?:\.[0-9]+)?)(us|µs|ms|min|s|h)',quota)
 if not parts or ''.join(a+b for a,b in parts)!=quota.replace(' ',''):raise RuntimeError('cannot preserve CPU quota')
 cpu='CPUQuota='+str(sum(decimal.Decimal(a)*units[b] for a,b in parts)*100)+'%'
delegation=''
if show('Delegate')=='yes':
 if show('DelegateSubgroup')!='daemon':raise RuntimeError('cannot preserve run-resource delegation')
 delegation='Delegate=cpu memory pids\nDelegateSubgroup=daemon'
# Path-valued directives consume a whole path, not a shell-style quoted
# word. ExecStart and Environment use the quoted-word parser instead.
values={'REPO_ROOT':escaped(str(repo)), 'WORKING_DIR':str(repo).replace('%','%%'),
 'LOG_DIR':str(log_dir).replace('%','%%'),
 'SERVICE_PATH':escaped(os.environ.get('PATH','/usr/bin:/bin')), 'DRY_RUN':dry,
 'EXTRA_ENV':'\n'.join(extra),
 'MEMORY_HIGH':high,'MEMORY_MAX':maximum,'TASKS_MAX':tasks,
 'DELEGATION':delegation,'CPU_QUOTA':cpu,
 'NOFILE':setting('LimitNOFILE','AUGMENTAGENT_UNIT_NOFILE','4096')}
template=(repo/'scripts/systemd/augmentagent-autopr.service').read_text()
template=re.sub(r'@([A-Z_]+)@',lambda match:values[match.group(1)],template)
write(unit_dir/'augmentagent-autopr.service',template)
write(unit_dir/'augmentagent-runtime.slice',f'''# Managed by ensure-autopr-worker.sh
[Unit]
Description=AugmentAgent daemon and auto-ship shared resource budget
[Slice]
MemoryHigh={high}
MemoryMax={maximum}
MemorySwapMax=0
TasksMax={tasks}
{cpu}
''')
write(unit_dir/'augmentagent.service.d/autoship-worker.conf',f'''# Managed by ensure-autopr-worker.sh
[Unit]
Wants=augmentagent-autopr.service
[Service]
{shared_environment}
Slice=augmentagent-runtime.slice
''')
# Validate every destination before changing any managed file. Selection is
# committed last; manager failures restore the prior units and selection.
if selection_text is not None:write(selection,selection_text)
subprocess.run(['python3',str(repo/'scripts/secure-release-executables.py'),str(repo/'target/release')],check=True)
log_dir.mkdir(parents=True,exist_ok=True)
previous={path:(path.read_bytes(),stat.S_IMODE(path.stat().st_mode)) if path.exists() else None for path in plan}
written=[]
try:
 for path,data in plan.items():
  replace(path,data);written.append(path)
 subprocess.run(['systemctl','--user','daemon-reload'],check=True)
 subprocess.run(['systemctl','--user','enable','augmentagent-autopr.service'],check=True,stdout=subprocess.DEVNULL)
except BaseException:
 for path in reversed(written):
  if path.read_bytes()!=plan[path]:raise RuntimeError('preserving concurrently changed unit '+str(path))
  old=previous[path]
  if old is None:path.unlink()
  else:replace(path,*old)
 subprocess.run(['systemctl','--user','daemon-reload'],check=False)
 raise
PY
