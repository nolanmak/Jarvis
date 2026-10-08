#!/usr/bin/env python3
"""Task-scoped Python compute policy and VM execution boundary.

All request validation is pure: no guest, filesystem write or network activity
may occur until it succeeds. The host supplies policy separately from requests.
"""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import stat
import subprocess
import sys
import tempfile
import time

MAX_SOURCE = 256 * 1024
MAX_PACKAGES = 32
MAX_FILES = 32
MAX_INPUT_BYTES = 64 * 1024 * 1024
MAX_OUTPUT_BYTES = 64 * 1024 * 1024
MAX_FILE_BYTES = 32 * 1024 * 1024
FILENAME = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}', re.ASCII)


class ComputeError(ValueError):
    """Fixed diagnostics: never embed model source, secrets or package URLs."""
    def __init__(self, code, message):
        self.code = code
        super().__init__(message)


def deny(code='bad_args', message='Invalid compute request.'):
    raise ComputeError(code, message)


def filename(value):
    return isinstance(value, str) and FILENAME.fullmatch(value) is not None


def dependencies(values):
    # packaging is a trusted operator-provisioned runtime dependency, never
    # bootstrapped by installing packages into the host interpreter.
    try:
        from packaging.requirements import Requirement, InvalidRequirement
        from packaging.utils import canonicalize_name
    except ImportError:
        deny('sandbox_unavailable', 'Compute runtime requires the packaging parser.')
    result = {}
    for value in values:
        if not isinstance(value, str) or len(value) > 512 or any(c in value for c in '\r\n\0'):
            deny('dependency_policy_denied', 'Unsupported dependency specification.')
        try:
            parsed = Requirement(value)
        except InvalidRequirement:
            deny('dependency_policy_denied', 'Unsupported dependency specification.')
        name = canonicalize_name(parsed.name)
        if (parsed.url or parsed.extras or parsed.marker or name in result
                or any(spec.operator == '===' for spec in parsed.specifier)):
            deny('dependency_policy_denied', 'Unsupported dependency specification.')
        result[name] = name + str(parsed.specifier)
    return sorted(result.values())


def validate_request(value, maximum_timeout=600):
    if type(maximum_timeout) is not int or not 1 <= maximum_timeout <= 900:
        deny('sandbox_unavailable', 'Invalid compute timeout policy.')
    required = {'runtime', 'dependencies', 'code'}
    allowed = required | {'inputs', 'outputs', 'timeoutSecs'}
    if type(value) is not dict or not required <= value.keys() or value.keys() - allowed:
        deny()
    if value['runtime'] != 'python' or not isinstance(value['code'], str) or '\0' in value['code']:
        deny()
    try:
        size = len(value['code'].encode('utf-8'))
    except UnicodeError:
        deny()
    if size > MAX_SOURCE:
        deny()
    for key, maximum in (('dependencies', MAX_PACKAGES), ('inputs', MAX_FILES), ('outputs', MAX_FILES)):
        items = value.get(key, [])
        if type(items) is not list or len(items) > maximum:
            deny()
    timeout = value.get('timeoutSecs', maximum_timeout)
    if type(timeout) is not int or not 1 <= timeout <= maximum_timeout:
        deny()
    inputs, outputs = [], list(value.get('outputs', []))
    seen = set()
    for entry in value.get('inputs', []):
        if (type(entry) is not dict or set(entry) != {'artifactId', 'name'}
                or not isinstance(entry['artifactId'], str) or not 1 <= len(entry['artifactId']) <= 128
                or not filename(entry['name']) or entry['name'] in seen):
            deny()
        seen.add(entry['name'])
        inputs.append(dict(entry))
    seen = set()
    for name in outputs:
        if not filename(name) or name in seen:
            deny()
        seen.add(name)
    return {'runtime': 'python', 'dependencies': dependencies(value['dependencies']),
            'code': value['code'], 'inputs': inputs, 'outputs': outputs, 'timeoutSecs': timeout}


def vm_module():
    spec = importlib.util.spec_from_file_location('compute_build_vm', Path(__file__).with_name('codex-build-vm.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run_execution(runtime_path, private, request, timeout=600):
    """Run a dependency-free workload through the compute VM profile.

    No writable host directory is shared with the guest. Artifacts cross only
    the bounded serial protocol and are accepted after verified shutdown.
    The task service supplies the private scratch directory and validated policy.
    """
    request = validate_request(request)
    if request['dependencies']:
        deny('dependency_unavailable', 'Dependency preparation is required before execution.')
    if request['inputs']:
        deny('input_denied', 'Input capabilities must be resolved by the owning task.')
    deadline = time.monotonic() + timeout
    vm = vm_module()
    runtime = vm.Runtime.load(runtime_path)
    config = runtime.config
    if os.getuid() == 0:
        deny('sandbox_unavailable', 'Compute requires an unprivileged host account.')
    with tempfile.TemporaryDirectory(prefix='compute-execution-', dir=private) as temporary:
        root = Path(temporary)
        entries = []
        for name in ('bootstrap', 'dev', 'proc', 'sys', 'tmp', 'modules', 'usr', 'etc',
                     'home', 'home/worker', 'work', 'outputs', 'inputs'):
            entries.append((name, stat.S_IFDIR | 0o755, b'', 0, 0))
        entries.append(('root', stat.S_IFDIR | 0o700, b'', 0, 0))
        entries.append(('dev/console', stat.S_IFCHR | 0o600, b'', 5, 1))
        entries.append(('bootstrap/busybox', stat.S_IFREG | 0o755, Path(config['busybox']).read_bytes(), 0, 0))
        for name in ('sh', 'mount', 'insmod', 'poweroff'):
            entries.append(('bootstrap/' + name, stat.S_IFLNK | 0o777, b'busybox', 0, 0))
        for name in ('bin', 'sbin', 'lib', 'lib64'):
            entries.append((name, stat.S_IFLNK | 0o777, ('usr/' + name).encode(), 0, 0))
        commands = []
        for i, path in enumerate(config['modules']):
            name = f'modules/{i}.ko'
            entries.append((name, stat.S_IFREG | 0o400, Path(path).read_bytes(), 0, 0))
            commands.append(f'insmod /{name} || poweroff -f')
        boot = """#!/bootstrap/sh
export PATH=/bootstrap
mount -t devtmpfs devtmpfs /dev
mount -t proc -o hidepid=2 proc /proc
mount -t sysfs sysfs /sys
""" + '\n'.join(commands) + """
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f
mount -t tmpfs -o size=128m,mode=1777,nosuid,nodev tmpfs /tmp || poweroff -f
mount -t tmpfs -o size=256m,mode=0700,nosuid,nodev tmpfs /work || poweroff -f
mount -t tmpfs -o size=64m,mode=0700,nosuid,nodev tmpfs /outputs || poweroff -f
mount -t tmpfs -o size=16m,mode=0700,nosuid,nodev tmpfs /home/worker || poweroff -f
/usr/bin/python3 -I -S /guest.py
poweroff -f
"""
        job = {'uid': os.getuid(), 'gid': os.getgid(), 'memory_mb': config['memory_mb'],
               'outputs': request['outputs']}
        guest = Path(__file__).with_name('code-mode-compute-guest.py').read_bytes()
        entries.extend([('init', stat.S_IFREG | 0o700, boot.encode(), 0, 0),
                        ('guest.py', stat.S_IFREG | 0o400, guest, 0, 0),
                        ('program.py', stat.S_IFREG | 0o444, request['code'].encode(), 0, 0),
                        ('job.json', stat.S_IFREG | 0o400, json.dumps(job).encode(), 0, 0)])
        image = root / 'initrd.gz'
        image.write_bytes(vm.initrd(entries))
        command = vm.qemu_command(config, image, [('runtime', Path('/usr'), True)], None)
        environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'], 'QEMU_MODULE_DIR': config['module_dir']}
        cleanup = root / 'cleanup-complete'
        supervisor = Path(__file__).with_name('provider-supervisor.py')
        process = subprocess.Popen([sys.executable, '-I', str(supervisor), str(cleanup), *command],
            env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            start_new_session=True)
        captured = bytearray()
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        deny('timeout', 'Compute execution deadline expired.')
                    for key, _ in selector.select(min(remaining, 0.05)):
                        data = os.read(key.fileobj.fileno(), 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                            break
                        captured.extend(data)
                        if len(captured) > 100 * 1024 * 1024:
                            deny('resource_limit', 'Compute result exceeded its transfer limit.')
            status = process.wait(timeout=max(0.001, deadline - time.monotonic()))
            if status or not cleanup.is_file() or cleanup.read_text() != 'all-descendants-reaped\n':
                deny('cleanup_unverified', 'Compute cleanup could not be verified.')
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    deny('cleanup_unverified', 'Compute supervisor failed to stop.')
            process.stdout.close()
        prefix = b'JARVIS_COMPUTE_RESULT:'
        records = [line[len(prefix):] for line in captured.splitlines() if line.startswith(prefix)]
        if len(records) != 1:
            with tempfile.NamedTemporaryFile(prefix='compute-boot-', suffix='.log', dir=private, delete=False) as stream:
                stream.write(captured[-65536:])
            deny('sandbox_unavailable', 'Compute guest did not return a result.')
        result = json.loads(records[0])
        if not isinstance(result, dict) or type(result.get('ok')) is not bool:
            deny('sandbox_unavailable', 'Invalid compute guest result.')
        files, total = {}, 0
        if result['ok']:
            if set(result.get('files', {})) != set(request['outputs']):
                deny('output_denied', 'Compute output set does not match the request.')
            for name, entry in result['files'].items():
                data = base64.b64decode(entry['data'], validate=True)
                total += len(data)
                if len(data) > MAX_FILE_BYTES or total > MAX_OUTPUT_BYTES:
                    deny('resource_limit', 'Compute exports exceed their byte limit.')
                if len(data) != entry['bytes'] or hashlib.sha256(data).hexdigest() != entry['sha256']:
                    deny('output_denied', 'Compute export integrity check failed.')
                files[name] = data
        result.update(runner='vm', cleanupVerified=True, files=files)
        return result
