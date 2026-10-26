#!/usr/bin/env bash
# Provision independently of daemon restart. Starting the worker is ordered
# after the daemon so a legacy embedded loop is not duplicated at migration.
set -euo pipefail
[ "$(uname -s)" = Linux ] || exit 0
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export REPO_ROOT
python3 - <<'PY'
import os, pathlib, re, shlex, subprocess, tempfile
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
dry=os.environ.get('AUGMENTAGENT_AUTOSTART_DRY_RUN')
if dry is None:
 match=re.search(r'--dry-run\s+(true|false)',show('ExecStart'))
 dry=match.group(1) if match else 'true'
if dry not in ('true','false'):raise RuntimeError('invalid worker dry-run setting')
# Path-valued directives consume a whole path, not a shell-style quoted
# word. ExecStart and Environment use the quoted-word parser instead.
values={'REPO_ROOT':escaped(str(repo)), 'WORKING_DIR':str(repo).replace('%','%%'),
 'LOG_DIR':str(log_dir).replace('%','%%'),
 'SERVICE_PATH':escaped(os.environ.get('PATH','/usr/bin:/bin')), 'DRY_RUN':dry,
 'EXTRA_ENV':'\n'.join(extra),
 'NOFILE':setting('LimitNOFILE','AUGMENTAGENT_UNIT_NOFILE','4096')}
template=(repo/'scripts/systemd/augmentagent-autopr.service').read_text()
template=re.sub(r'@([A-Z_]+)@',lambda match:values[match.group(1)],template)
write(unit_dir/'augmentagent-autopr.service',template)
high=setting('MemoryHigh','AUGMENTAGENT_UNIT_MEMORY_HIGH','8G')
maximum=setting('MemoryMax','AUGMENTAGENT_UNIT_MEMORY_MAX','10G')
tasks=setting('TasksMax','AUGMENTAGENT_UNIT_TASKS_MAX','2048')
write(unit_dir/'augmentagent-runtime.slice',f'''# Managed by ensure-autopr-worker.sh
[Unit]
Description=AugmentAgent daemon and auto-ship shared resource budget
[Slice]
MemoryHigh={high}
MemoryMax={maximum}
MemorySwapMax=0
TasksMax={tasks}
''')
write(unit_dir/'augmentagent.service.d/autoship-worker.conf','''# Managed by ensure-autopr-worker.sh
[Unit]
Wants=augmentagent-autopr.service
[Service]
Environment=AUGMENTAGENT_AUTOPR_EXTERNAL=1
Slice=augmentagent-runtime.slice
''')
log_dir.mkdir(parents=True,exist_ok=True)
PY
systemctl --user daemon-reload
systemctl --user enable augmentagent-autopr.service >/dev/null
