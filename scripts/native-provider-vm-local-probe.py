#!/usr/bin/env python3
"""Explicit synthetic native VM lifecycle probe; never invokes a model."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--execute', action='store_true', required=True)
    parser.add_argument('--runtime-config', type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='native-vm-synthetic-', dir='/tmp') as name:
        root = Path(name)
        session = root / 'session'
        session.mkdir(mode=0o700)
        environment = {'PATH': '/usr/bin:/bin', 'JARVIS_NATIVE_VM_READY': str(root / 'ready'),
                       'JARVIS_NATIVE_VM_CONTINUE': str(root / 'continue')}
        policy = {'version': 1, 'authorities': [{'authority': 'api.example.com:443',
            'routes': [{'path_prefix': '/v1/', 'methods': ['POST'], 'websocket': False}],
            'tls': True, 'allow_loopback': False}], 'limits': {'max_connections': 2,
            'max_request_bytes': 1024, 'max_response_bytes': 4096,
            'max_frame_bytes': 65536, 'idle_timeout_seconds': 30}}
        script = """import os,json,socket
from pathlib import Path
assert os.getuid() != 0
try: Path('/root/job.json').read_bytes(); raise AssertionError('root config readable')
except PermissionError: pass
home=Path.home()
prior=(home/'count').read_text() if (home/'count').exists() else '0'
(home/'count').write_text(str(int(prior)+1))
print(json.dumps({'type':'synthetic','count':int(prior)+1,'interfaces':os.listdir('/sys/class/net')}),flush=True)
"""
        spec = {'version': 1, 'runtime_config': str(args.runtime_config), 'memory_mb': 512,
            'session_directory': str(session), 'resume': False, 'executable': '/usr/bin/python3.12',
            'argv': ['-I', '-S', '-c', script], 'environment': {'LANG': 'C.UTF-8'},
            'auth_file': None, 'auth_relative': '.codex/auth.json', 'gateway': policy,
            'mcp_argv': None, 'assets': []}
        for iteration in [1, 2]:
            spec['resume'] = iteration == 2
            spec_path = root / 'spec.json'
            spec_path.write_text(json.dumps(spec))
            spec_path.chmod(0o600)
            for filename in ['ready', 'continue', 'vm-dispatch-intent']:
                (root / filename).unlink(missing_ok=True)
            process = subprocess.Popen([sys.executable, '-I', str(Path(__file__).with_name('native-provider-vm.py')),
                str(spec_path)], env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 45
                while not (root / 'ready').exists():
                    if process.poll() is not None:
                        stdout, stderr = process.communicate()
                        raise RuntimeError(f'VM startup failed: {stderr.decode()}')
                    if time.monotonic() > deadline:
                        raise TimeoutError('VM startup deadline')
                    time.sleep(.02)
                (root / 'continue').write_text('admitted\n')
                (root / 'continue').chmod(0o600)
                stdout, stderr = process.communicate(timeout=30)
                if process.returncode != 0:
                    raise RuntimeError(f'VM worker failed: {stderr.decode()}')
                value = json.loads(stdout)
                assert value == {'type': 'synthetic', 'count': iteration, 'interfaces': ['lo']}, value
                assert (session / 'complete.json').is_file()
                print(f'PASS synthetic launch {iteration}: isolated UID, root config denied, no NIC, runtime continuation')
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()


if __name__ == '__main__':
    main()
