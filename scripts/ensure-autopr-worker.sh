#!/usr/bin/env bash
# Provision independently of daemon restart. Starting the worker is ordered
# after the daemon so a legacy embedded loop is not duplicated at migration.
set -euo pipefail
[ "$(uname -s)" = Linux ] || exit 0
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export REPO_ROOT
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
def write(path,text):
 path.parent.mkdir(parents=True,exist_ok=True)
 if path.exists():
  previous=path.read_text()
  if not previous.startswith('# Managed by ensure-autopr-worker.sh'):
   raise RuntimeError('preserving unmanaged unit '+str(path))
  if previous==text:return
 with tempfile.NamedTemporaryFile(mode='w',dir=path.parent,prefix='.autopr-',delete=False) as f:
  f.write(text); temporary=pathlib.Path(f.name)
 temporary.replace(path)
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
 extra.append('EnvironmentFile='+('-' if optional=='yes' else '')+'"'+escaped(name)+'"')
 raw=raw[match.end():]
# Selection is explicit; merely staging a profile never activates it. Keep a
# shared final EnvironmentFile so both units use the same choice on rollout.
# This validates safe configuration storage, not live provider readiness.
selection=unit_dir.parent.parent/'augmentagent/autoship-native.env'
selected=os.environ.get('AUGMENTAGENT_NATIVE_VM_POLICY')
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
 write(selection,'# Managed by ensure-autopr-worker.sh\nAUGMENTAGENT_NATIVE_VM_POLICY="'+value+'"\n')
shared_environment='EnvironmentFile=-"'+escaped(str(selection))+'"'
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
Environment=AUGMENTAGENT_AUTOPR_EXTERNAL=1
{shared_environment}
Slice=augmentagent-runtime.slice
''')
log_dir.mkdir(parents=True,exist_ok=True)
PY
systemctl --user daemon-reload
systemctl --user enable augmentagent-autopr.service >/dev/null
