#!/usr/bin/env python3
"""Observe current provisioning interrupted at real replace boundaries.

Disposable files, scripted systemctl, no production units/state, no VM/model.
Executes the unchanged embedded Python body from ensure-autopr-worker.sh; the
shell Linux/cwd wrapper is not the subject of this probe. Runtime fault injection
kills the original child immediately after a selected actual os.replace.
This is incident evidence, not verification of a future durable publisher.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
HEADER = '# Managed by ensure-autopr-worker.sh\n'
TARGETS = ('augmentagent-autopr.service', 'augmentagent-runtime.slice',
           'augmentagent.service.d/autoship-worker.conf')


def probe():
    source_path = ROOT / 'scripts/ensure-autopr-worker.sh'
    source = source_path.read_bytes()
    body = source.decode().split("python3 - <<'PY'\n", 1)[1].rsplit('\nPY', 1)[0]
    observed = []
    for cut in (1, 2, 3):
        with tempfile.TemporaryDirectory(prefix='capacity-publication-crash-') as name:
            root = Path(name)
            repo = root / 'repo'
            scripts = repo / 'scripts'
            (scripts / 'systemd').mkdir(parents=True)
            for filename in ('ensure-autopr-worker.sh', 'secure-release-executables.py',
                             'systemd/augmentagent-autopr.service'):
                shutil.copyfile(ROOT / 'scripts' / filename, scripts / filename)
            release = repo / 'target/release'; release.mkdir(parents=True)
            for filename in ('augmentagent', 'augmentagent-mcp-memory'):
                binary = release / filename
                binary.write_text('#!/bin/sh\nexit 0\n'); binary.chmod(0o755)
            tools = root / 'bin'; tools.mkdir()
            manager = tools / 'systemctl'
            manager.write_text('''#!/usr/bin/python3
import os,sys
from pathlib import Path
with Path(os.environ['PROBE_CALLS']).open('a') as stream:
    stream.write(' '.join(sys.argv[1:])+'\\n')
if 'show' in sys.argv:
    key=sys.argv[sys.argv.index('-p')+1]
    print({'ExecStart':'{ argv[]=/fixture/serve --dry-run false ; }'}.get(key,''))
''')
            manager.chmod(0o755)
            units = root / 'config/systemd/user'
            original = {}
            for target in TARGETS:
                path = units / target; path.parent.mkdir(parents=True, exist_ok=True)
                data = (HEADER + '# original ' + target + '\n').encode()
                path.write_bytes(data); path.chmod(0o600)
                original[target] = data
            body_path = root / 'original-provisioner.py'; body_path.write_text(body)
            runner = root / 'kill-after-replace.py'
            runner.write_text('''import os,signal,sys
from pathlib import Path
real_replace=os.replace
count=0
units=Path(os.environ['PROBE_UNITS'])
def interrupted_replace(source,target,*args,**kwargs):
    global count
    result=real_replace(source,target,*args,**kwargs)
    if Path(target).is_relative_to(units):
        count+=1
        if count==int(os.environ['PROBE_CUT']):
            with open(os.environ['PROBE_MARKER'],'w') as stream:
                stream.write(str(count));stream.flush();os.fsync(stream.fileno())
            os.kill(os.getpid(),signal.SIGKILL)
    return result
os.replace=interrupted_replace
exec(compile(Path(sys.argv[1]).read_text(),sys.argv[1],'exec'),{'__name__':'__main__'})
''')
            env = {'HOME': str(root), 'XDG_CONFIG_HOME': str(root / 'config'),
                   'XDG_STATE_HOME': str(root / 'state'), 'REPO_ROOT': str(repo),
                   'PATH': str(tools) + ':/usr/bin:/bin', 'PROBE_CALLS': str(root / 'calls'),
                   'PROBE_UNITS': str(units), 'PROBE_CUT': str(cut),
                   'PROBE_MARKER': str(root / 'cut-observed')}
            result = subprocess.run([sys.executable, '-I', str(runner), str(body_path)],
                                    env=env, capture_output=True, timeout=15)
            assert result.returncode == -signal.SIGKILL, (cut, result.returncode, result.stderr)
            assert (root / 'cut-observed').read_text() == str(cut)
            changed = [target for target in TARGETS if (units / target).read_bytes() != original[target]]
            assert changed == list(TARGETS[:cut]), (cut, changed)
            calls = (root / 'calls').read_text()
            assert 'daemon-reload' not in calls
            assert 'enable augmentagent-autopr.service' not in calls
            # Ordinary rerun repairs the simple same-source fixture; do not
            # misrepresent lack of a durable original plan as universal failure.
            rerun = subprocess.run(['bash', str(scripts / 'ensure-autopr-worker.sh')],
                                   env=env, capture_output=True, timeout=15)
            assert rerun.returncode == 0, (cut, rerun.stderr)
            assert all((units / target).read_bytes() != original[target] for target in TARGETS)
            calls = (root / 'calls').read_text()
            assert 'daemon-reload' in calls and 'enable augmentagent-autopr.service' in calls
            observed.append({'kill_after_real_replace': cut, 'verified_sigkill_marker': True,
                             'changed_targets': cut, 'untouched_targets': len(TARGETS)-cut,
                             'manager_not_reloaded_at_crash': True,
                             'ordinary_same_source_rerun_completed': True})
    return {'source_sha256': hashlib.sha256(source).hexdigest(), 'cases': observed,
            'production_mutated': False, 'future_publisher_verified': False,
            'physical_power_loss_verified': False, 'native_custody_proved': False}


if __name__ == '__main__':
    print(json.dumps(probe(), sort_keys=True, indent=2))
