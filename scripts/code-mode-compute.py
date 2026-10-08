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
import uuid

MAX_SOURCE = 256 * 1024
MAX_PACKAGES = 32
MAX_FILES = 32
MAX_INPUT_BYTES = 64 * 1024 * 1024
MAX_OUTPUT_BYTES = 64 * 1024 * 1024
MAX_FILE_BYTES = 32 * 1024 * 1024
FILENAME = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}', re.ASCII)


class ComputeError(ValueError):
    """Fixed diagnostics: never embed model source, secrets or package URLs."""
    def __init__(self, code, message, *, runner='none', retryable=False):
        self.code = code
        self.runner = runner
        self.retryable = retryable
        super().__init__(message)


def deny(code='bad_args', message='Invalid compute request.', **details):
    raise ComputeError(code, message, **details)


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


def run_execution(runtime_path, private, request, timeout=600, *, cache=None, environment_id=None, input_data=None):
    request = validate_request(request, 900)
    if request['dependencies'] and not environment_id:
        deny('dependency_unavailable', 'Dependency preparation is required before execution.')
    input_data = input_data or {}
    if set(input_data) != {entry['name'] for entry in request['inputs']}:
        deny('input_denied', 'Input capabilities must be resolved by the owning task.')
    if any(not isinstance(data, bytes) for data in input_data.values()):
        deny('input_denied', 'Input snapshots must be immutable bytes.')
    if sum(map(len, input_data.values())) > MAX_INPUT_BYTES:
        deny('resource_limit', 'Selected inputs exceed the byte limit.')
    return run_phase(runtime_path, private, request, timeout, cache=cache,
                     environment_id=environment_id, input_data=input_data)


def run_phase(runtime_path, private, request, timeout, *, cache=None, environment_id=None,
              input_data=None, pip_runtime=None):
    """Run a dependency-free workload through the compute VM profile.

    No writable host directory is shared with the guest. Artifacts cross only
    the bounded virtio protocol and are accepted after verified shutdown.
    The task service supplies the private scratch directory and validated policy.
    """
    prepare = pip_runtime is not None
    if environment_id is not None and not re.fullmatch('[0-9a-f]{32}', environment_id):
        deny('sandbox_unavailable', 'Invalid environment identity.')
    if (cache is None) != (environment_id is None):
        deny('sandbox_unavailable', 'Missing environment storage.')
    if cache is not None:
        descriptor = open_absolute(cache)
        info = os.fstat(descriptor)
        os.close(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_nlink != 1 or info.st_mode & 0o077):
            deny('sandbox_unavailable', 'Untrusted environment image.')
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
                     'home', 'home/worker', 'work', 'outputs', 'inputs', 'cache'):
            entries.append((name, stat.S_IFDIR | 0o755, b'', 0, 0))
        entries.append(('root', stat.S_IFDIR | 0o700, b'', 0, 0))
        entries.append(('dev/console', stat.S_IFCHR | 0o600, b'', 5, 1))
        entries.append(('bootstrap/busybox', stat.S_IFREG | 0o755, Path(config['busybox']).read_bytes(), 0, 0))
        for name in ('sh', 'mount', 'insmod', 'poweroff', 'ip', 'sync', 'umount'):
            entries.append(('bootstrap/' + name, stat.S_IFLNK | 0o777, b'busybox', 0, 0))
        for name in ('bin', 'sbin', 'lib', 'lib64'):
            entries.append((name, stat.S_IFLNK | 0o777, ('usr/' + name).encode(), 0, 0))
        commands = []
        for i, path in enumerate(config['modules']):
            name = f'modules/{i}.ko'
            entries.append((name, stat.S_IFREG | 0o400, Path(path).read_bytes(), 0, 0))
            commands.append(f'insmod /{name} || poweroff -f')
        extra_boot = ''
        shares = [('runtime', Path('/usr'), True)]
        broker = None
        if cache is not None:
            mode = 'rw' if prepare else 'ro,noload'
            extra_boot += f'mount -t ext4 -o {mode},nosuid,nodev /dev/vda /cache || poweroff -f\n'
        if prepare:
            pip_path = pip_runtime.get('path')
            pip_bytes = read_private_file(open_absolute(pip_path), 8 * 1024 * 1024)
            if hashlib.sha256(pip_bytes).hexdigest() != pip_runtime.get('sha256'):
                deny('sandbox_unavailable', 'Installer runtime integrity check failed.')
            control = root / 'control'; control.mkdir(mode=0o700)
            shares.append(('control', control, False))
            broker = vm.dependency_proxy.Broker(control, python=True)
            certificate, key = root / 'registry-ca.pem', root / 'registry-key.pem'
            subprocess.run(['/usr/bin/openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                '-keyout', str(key), '-out', str(certificate), '-days', '1',
                '-subj', '/CN=Jarvis compute package gateway', '-addext',
                'subjectAltName=DNS:pypi.org,DNS:files.pythonhosted.org'],
                env={'PATH': os.defpath}, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, timeout=max(0.001, min(10, deadline-time.monotonic())), check=True)
            entries.extend([
                ('root/control', stat.S_IFDIR | 0o700, b'', 0, 0),
                ('etc/jarvis-registry-ca.pem', stat.S_IFREG | 0o444, certificate.read_bytes(), 0, 0),
                ('root/registry-key.pem', stat.S_IFREG | 0o400, key.read_bytes(), 0, 0),
                ('etc/hosts', stat.S_IFREG | 0o444, b'127.0.0.1 localhost pypi.org files.pythonhosted.org\n', 0, 0),
                ('pip.whl', stat.S_IFREG | 0o444, pip_bytes, 0, 0),
                ('prepare.py', stat.S_IFREG | 0o444, Path(__file__).with_name('code-mode-compute-prepare.py').read_bytes(), 0, 0),
                ('prepare.json', stat.S_IFREG | 0o444, json.dumps({'requirements': request['dependencies'],
                    'environmentId': environment_id}).encode(), 0, 0),
            ])
            extra_boot += 'ip link set lo up\nmount -t 9p -o trans=virtio,version=9p2000.L,nosuid,nodev,noexec control /root/control || poweroff -f\n'
        for name, data in (input_data or {}).items():
            if not filename(name):
                deny('input_denied', 'Invalid input snapshot name.')
            entries.append(('inputs/' + name, stat.S_IFREG | 0o444, data, 0, 0))
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
""" + extra_boot + """
/usr/bin/python3 -I -S /guest.py
sync
umount /cache 2>/dev/null
poweroff -f
"""
        job = {'uid': os.getuid(), 'gid': os.getgid(), 'memory_mb': config['memory_mb'],
               'outputs': request['outputs'], 'prepare': prepare, 'environmentId': environment_id}
        guest = Path(__file__).with_name('code-mode-compute-guest.py').read_bytes()
        if prepare:
            guest = vm.dependency_proxy.GUEST_PROXY.encode() + b'\n' + guest
        entries.extend([('init', stat.S_IFREG | 0o700, boot.encode(), 0, 0),
                        ('guest.py', stat.S_IFREG | 0o400, guest, 0, 0),
                        ('program.py', stat.S_IFREG | 0o444, request['code'].encode(), 0, 0),
                        ('job.json', stat.S_IFREG | 0o400, json.dumps(job).encode(), 0, 0)])
        image = root / 'initrd.gz'
        image.write_bytes(vm.initrd(entries))
        command = vm.qemu_command(config, image, shares, cache)
        # The UART console is far too slow for bounded multi-MiB logs/exports.
        # A root-only virtio port feeds the same bounded host pipe; no writable
        # host share, file, socket listener, or workload RPC channel is added.
        command[command.index('-serial') + 1] = 'null'
        command.extend(['-device', 'virtio-serial-pci',
                        '-chardev', 'stdio,id=compute-result,signal=off',
                        '-device', 'virtserialport,chardev=compute-result,name=org.jarvis.compute.result'])
        if cache is not None and not prepare:
            index = command.index('-drive') + 1
            command[index] += ',readonly=on'
        environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'], 'QEMU_MODULE_DIR': config['module_dir']}
        cleanup = root / 'cleanup-complete'
        supervisor = Path(__file__).with_name('provider-supervisor.py')
        # Keep the unused input pipe open until shutdown: /dev/null sends EOF
        # immediately and makes QEMU disconnect its bidirectional virtio port.
        # No host command or data is ever written to this pipe.
        process = subprocess.Popen([sys.executable, '-I', str(supervisor), str(cleanup), *command],
            env=environment, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            start_new_session=True)
        captured = bytearray()
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        deny('timeout', 'Compute execution deadline expired.')
                    if broker is not None:
                        broker.poll(remaining)
                    for key, _ in selector.select(min(max(0, deadline-time.monotonic()), 0.05)):
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
        except BaseException as error:
            error.runner = 'vm'
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    deny('cleanup_unverified', 'Compute supervisor failed to stop.')
            process.stdin.close()
            process.stdout.close()
            if not cleanup.is_file() or cleanup.read_text() != 'all-descendants-reaped\n':
                deny('cleanup_unverified', 'Compute descendant cleanup was not verified.', runner='vm')
        prefix = b'JARVIS_COMPUTE_RESULT:'
        records = [line[len(prefix):] for line in captured.splitlines() if line.startswith(prefix)]
        if len(records) != 1:
            with tempfile.NamedTemporaryFile(prefix='compute-boot-', suffix='.log', dir=private, delete=False) as stream:
                stream.write(captured[-65536:])
            deny('sandbox_unavailable', 'Compute guest did not return a result.')
        result = json.loads(records[0])
        if not isinstance(result, dict) or type(result.get('ok')) is not bool:
            deny('sandbox_unavailable', 'Invalid compute guest result.')
        encoded_logs = result.pop('privateLogs', None)
        if not isinstance(encoded_logs, dict) or set(encoded_logs) != {'stdout', 'stderr'}:
            deny('sandbox_unavailable', 'Invalid compute log transfer.')
        raw_logs = {name: base64.b64decode(data, validate=True) for name, data in encoded_logs.items()}
        if sum(map(len, raw_logs.values())) > 8 * 1024 * 1024:
            deny('resource_limit', 'Compute logs exceed their transfer limit.')
        result['_logs'] = raw_logs
        for name, data in raw_logs.items():
            result[name] = data.decode(errors='replace')
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
        result.update(runner='vm', cleanupVerified=True, files=files,
                      downloads={'requests': broker.requests if broker else 0, 'bytes': broker.bytes if broker else 0})
        return result


def open_absolute(path, *, directory=False):
    """Open every component without following links; caller owns returned fd."""
    path = Path(path)
    if not path.is_absolute() or '..' in path.parts:
        deny('input_denied', 'Input must be an explicit absolute path.')
    parent = os.open('/', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        components = path.parts[1:]
        if not components:
            if directory:
                return os.dup(parent)
            deny('input_denied', 'Input must be an ordinary file.')
        for component in components[:-1]:
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
            os.close(parent)
            parent = child
        flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        if directory:
            flags |= os.O_DIRECTORY
        return os.open(components[-1], flags, dir_fd=parent)
    except OSError:
        deny('input_denied', 'Input path is unavailable or contains a link.')
    finally:
        os.close(parent)


def read_private_file(descriptor, limit=MAX_INPUT_BYTES):
    before = os.fstat(descriptor)
    if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1:
        os.close(descriptor)
        deny('input_denied', 'Input must be an ordinary single-link file.')
    with os.fdopen(descriptor, 'rb') as stream:
        if before.st_size > limit:
            deny('resource_limit', 'Input exceeds the byte limit.')
        data = stream.read(limit + 1)
        after = os.fstat(stream.fileno())
        if (len(data) > limit or after.st_size > limit):
            deny('resource_limit', 'Input exceeds the byte limit.')
        if (before.st_size != len(data) or before.st_size != after.st_size
                or before.st_mtime_ns != after.st_mtime_ns or after.st_nlink != 1):
            deny('input_denied', 'Input changed while creating its snapshot.')
        return data


class ArtifactStore:
    """One host task's capability map. Model input is never used as a path.

    The open directory descriptor pins the private storage inode. Inputs are
    copied at import and checked again against their digest before execution.
    """
    def __init__(self, root):
        self.root = Path(root)
        self.descriptor = None
        descriptor = open_absolute(root, directory=True)
        info = os.fstat(descriptor)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            os.close(descriptor)
            deny('input_denied', 'Artifact storage must be owner-private.')
        self.descriptor = descriptor
        self.entries = {}

    def close(self):
        if self.descriptor is not None:
            os.close(self.descriptor)
            self.descriptor = None

    def __del__(self):
        self.close()

    def _store(self, data, name):
        if sum(entry['bytes'] for entry in self.entries.values()) + len(data) > 128 * 1024 * 1024:
            deny('resource_limit', 'Task artifact storage is full.')
        identifier = uuid.uuid4().hex
        fd = os.open(identifier, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
                     0o400, dir_fd=self.descriptor)
        try:
            with os.fdopen(fd, 'wb') as stream:
                stream.write(data)
            entry = {'id': identifier, 'name': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
            self.entries[identifier] = entry
            return dict(entry)
        except BaseException:
            os.unlink(identifier, dir_fd=self.descriptor)
            raise

    def publish(self, files):
        if not isinstance(files, dict) or len(files) > MAX_FILES:
            deny('output_denied', 'Invalid output file set.')
        total = 0
        for name, data in files.items():
            if not filename(name) or not isinstance(data, bytes):
                deny('output_denied', 'Invalid output file.')
            total += len(data)
            if len(data) > MAX_FILE_BYTES or total > MAX_OUTPUT_BYTES:
                deny('resource_limit', 'Exports exceed their byte limit.')
        if sum(entry['bytes'] for entry in self.entries.values()) + total > 128 * 1024 * 1024:
            deny('resource_limit', 'Task artifact storage is full.')
        published = []
        try:
            for name, data in files.items():
                published.append(self._store(data, name))
            return published
        except BaseException:
            for entry in published:
                self.entries.pop(entry['id'], None)
                os.unlink(entry['id'], dir_fd=self.descriptor)
            raise

    def import_file(self, path, name):
        if not filename(name):
            deny()
        data = read_private_file(open_absolute(path))
        return self._store(data, name)

    def resolve_inputs(self, inputs):
        validate_request({'runtime': 'python', 'dependencies': [], 'code': '', 'inputs': inputs})
        resolved, total = {}, 0
        for item in inputs:
            entry = self.entries.get(item['artifactId'])
            if entry is None:
                deny('input_denied', 'Artifact is not owned by this task.')
            try:
                descriptor = os.open(entry['id'], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                                     dir_fd=self.descriptor)
                data = read_private_file(descriptor)
            except OSError:
                deny('input_denied', 'Artifact snapshot is unavailable.')
            total += len(data)
            if total > MAX_INPUT_BYTES:
                deny('resource_limit', 'Selected inputs exceed the byte limit.')
            if len(data) != entry['bytes'] or hashlib.sha256(data).hexdigest() != entry['sha256']:
                deny('input_denied', 'Artifact snapshot failed its integrity check.')
            resolved[item['name']] = data
        return resolved


def run_preparation(runtime_path, private, cache, pip_runtime, requirements, environment_id, timeout=600):
    request = validate_request({'runtime': 'python', 'dependencies': requirements, 'code': ''}, 900)
    if not request['dependencies']:
        deny('bad_args', 'Preparation requires a nonempty dependency set.')
    return run_phase(runtime_path, private, request, timeout, cache=cache,
                     environment_id=environment_id, pip_runtime=pip_runtime)


def validate_lock(value):
    from packaging.utils import canonicalize_name
    from packaging.version import Version, InvalidVersion
    if not isinstance(value, list) or len(value) > 256:
        deny('dependency_integrity', 'Invalid dependency lock.')
    seen = set()
    for entry in value:
        if (not isinstance(entry, dict) or set(entry) != {'name', 'version', 'sha256'}
                or not all(isinstance(v, str) for v in entry.values())
                or not re.fullmatch('[a-z0-9]+(?:-[a-z0-9]+)*', entry['name'])
                or not re.fullmatch('[0-9a-f]{64}', entry['sha256'])
                or entry['name'] in seen):
            deny('dependency_integrity', 'Invalid dependency lock.')
        try:
            Version(entry['version'])
        except InvalidVersion:
            deny('dependency_integrity', 'Invalid dependency version.')
        seen.add(canonicalize_name(entry['name']))
    return value


class ComputeTask:
    """Host-owned task identity, monotonic deadline and immutable resolution map."""
    def __init__(self, backend, artifacts, *, enabled, call_timeout=600, task_timeout=1800, clock=time.monotonic):
        validate_request({'runtime': 'python', 'dependencies': [], 'code': ''}, call_timeout)
        if type(enabled) is not bool or type(task_timeout) is not int or not 1 <= task_timeout <= 3600:
            deny('sandbox_unavailable', 'Invalid compute task policy.')
        self.backend = backend
        self.artifacts = artifacts
        self.enabled = enabled
        self.call_timeout = call_timeout
        self.clock = clock
        self.deadline = clock() + task_timeout
        self.environments = {}
        self.records = []
        self.task_id = uuid.uuid4().hex
        self.calls = 0
        self.active = None

    def _private_write(self, name, data, *, replace=False):
        # Never follow an entry supplied by another process. Atomic replacement
        # ensures crash recovery sees either complete journal snapshot.
        temporary = 'audit-tmp-' + uuid.uuid4().hex
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
                     0o600, dir_fd=self.artifacts.descriptor)
        try:
            with os.fdopen(fd, 'wb') as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            if replace:
                os.replace(temporary, name, src_dir_fd=self.artifacts.descriptor,
                           dst_dir_fd=self.artifacts.descriptor)
            else:
                os.link(temporary, name, src_dir_fd=self.artifacts.descriptor,
                        dst_dir_fd=self.artifacts.descriptor, follow_symlinks=False)
                os.unlink(temporary, dir_fd=self.artifacts.descriptor)
            os.fsync(self.artifacts.descriptor)
        finally:
            try:
                os.unlink(temporary, dir_fd=self.artifacts.descriptor)
            except FileNotFoundError:
                pass

    def _audit(self, **terminal):
        snapshot = {'schemaVersion': 1, 'taskId': self.task_id, 'ownerPid': os.getpid(),
                    'ownerStartTime': Path('/proc/self/stat').read_text().rsplit(')', 1)[1].split()[19],
                    'active': self.active, 'records': self.records, **terminal}
        data = json.dumps(snapshot, separators=(',', ':')).encode()
        if len(data) > 2 * 1024 * 1024:
            deny('resource_limit', 'Compute audit metadata limit exceeded.')
        self._private_write('audit.json', data, replace=True)

    def _phase(self, name):
        self.active['phase'] = name
        self._audit()

    def _logs(self, execution_id, executed):
        metadata = {}
        total = 0
        for name in ('stdout', 'stderr'):
            raw = executed.get('_logs', {}).get(name)
            if raw is None:
                raw = executed.get(name, '').encode()
            total += len(raw)
            if total > 8 * 1024 * 1024:
                deny('resource_limit', 'Compute private logs exceed their limit.')
            filename = 'audit-' + execution_id + '.' + name
            self._private_write(filename, raw)
            metadata[name] = {'file': filename, 'bytes': len(raw),
                              'sha256': hashlib.sha256(raw).hexdigest(),
                              'responseTruncated': len(raw.decode(errors='replace').encode()) > 65536}
        return metadata

    def finish(self, *, cancelled=False):
        # The caller must first verify backend.close(); cancellation alone is
        # never evidence of successful descendant cleanup.
        for record in self.records:
            if record['error'] and record['error']['code'] == 'cancelled':
                record['cleanupVerified'] = True
        receipt = {'closed': True, 'cleanupVerified': True, 'cancelled': cancelled,
                   'records': self.records}
        self._audit(**receipt)
        return receipt

    def execute(self, value):
        # Schema failures are distinguishable from execution results, and occur
        # before artifacts, admission, resolution or VM startup.
        request = validate_request(value, self.call_timeout)
        if self.calls >= 25:
            deny('resource_limit', 'Compute task call limit exceeded.')
        self.calls += 1
        execution_id = uuid.uuid4().hex
        started = self.clock()
        result = {'ok': False, 'runner': 'none', 'executionId': execution_id, 'exitCode': None,
                  'stdout': '', 'stderr': '', 'error': None, 'dependencyLock': [],
                  'environmentReused': False, 'artifacts': []}
        downloads = {'requests': 0, 'bytes': 0}
        fingerprint = None
        cleanup = True
        logs = {}
        self.active = {'executionId': execution_id, 'phase': 'admission', 'startedMonotonic': started}
        self._audit()
        try:
            if not self.enabled:
                deny('compute_disabled', 'Compute is disabled for this context.')
            if self.clock() >= self.deadline:
                deny('timeout', 'Compute task deadline expired.')
            selected = self.artifacts.resolve_inputs(request['inputs'])
            deadline = min(self.deadline, started + request['timeoutSecs'])
            fingerprint = self.backend.fingerprint
            key = json.dumps([request['dependencies'], fingerprint, 1], separators=(',', ':'))
            with self.backend.admit():
                environment = self.environments.get(key)
                if environment is None:
                    if request['dependencies']:
                        self._phase('prepare')
                        environment = self.backend.prepare(request['dependencies'], deadline)
                        validate_lock(environment['dependencyLock'])
                        if not environment['dependencyLock']:
                            deny('dependency_integrity', 'Preparation returned an empty lock.')
                        downloads = environment['downloads']
                    else:
                        environment = {'environmentId': None, 'dependencyLock': []}
                    # Commit reuse only after preparation and shutdown succeed.
                    self.environments[key] = environment
                else:
                    result['environmentReused'] = True
                result['dependencyLock'] = environment['dependencyLock']
                if self.clock() >= deadline:
                    deny('timeout', 'Compute call deadline expired.')
                self._phase('execute')
                executed = self.backend.execute(request, selected, environment['environmentId'], deadline)
                result['runner'] = executed.get('runner', 'none')
                cleanup = executed.get('cleanupVerified') is True
                if not cleanup:
                    deny('cleanup_unverified', 'Compute cleanup could not be verified.')
                result['exitCode'] = executed.get('exitCode')
                logs = self._logs(execution_id, executed)
                for name in ('stdout', 'stderr'):
                    result[name] = executed.get(name, '').encode()[:65536].decode(errors='ignore')
                if not executed['ok']:
                    code = executed.get('error')
                    if code not in ('dependency_policy_denied', 'dependency_integrity', 'dependency_unavailable',
                                    'resource_limit', 'output_denied', 'timeout', 'cancelled', 'execution_failed'):
                        code = 'execution_failed'
                    deny(code, 'Compute execution failed; inspect the private execution record.')
                if set(executed['files']) != set(request['outputs']):
                    deny('output_denied', 'Compute output set does not match the request.')
                if self.clock() >= deadline:
                    deny('timeout', 'Compute call deadline expired before export.')
                self._phase('export')
                result['artifacts'] = self.artifacts.publish(executed['files'])
                result['ok'] = True
        except ComputeError as error:
            if error.runner == 'vm':
                result['runner'] = 'vm'
            cleanup = error.code != 'cleanup_unverified'
            result['error'] = {'code': error.code, 'message': str(error), 'retryable': error.retryable}
        except BaseException as error:
            if getattr(error, 'runner', None) == 'vm':
                result['runner'] = 'vm'
            cleanup = False
            code = 'cancelled' if not isinstance(error, Exception) else 'execution_failed'
            result['error'] = {'code': code, 'message': 'Compute execution interrupted.', 'retryable': False}
            raise
        finally:
            self.records.append({'executionId': execution_id, 'taskId': self.task_id,
                                 'runner': result['runner'], 'runtimeFingerprint': fingerprint,
                                 'dependencyLock': result['dependencyLock'], 'environmentReused': result['environmentReused'],
                                 'downloads': downloads, 'elapsedSecs': self.clock() - started,
                                 'cleanupVerified': cleanup, 'error': result['error'],
                                 'logs': logs, 'artifacts': result['artifacts']})
            self.active = None
            self._audit()
        return result


class VMBackend:
    """Operator-configured transport; task arguments cannot select host paths."""
    def __init__(self, runtime_path, scratch_root, pip_runtime=None, scratch_limits=None):
        self.runtime_path = str(runtime_path)
        self.pip_runtime = pip_runtime
        spec = importlib.util.spec_from_file_location('compute_bridge', Path(__file__).with_name('codex-tool-bridge.py'))
        bridge = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(bridge)
        self.scratch = bridge.BuildScratch(str(scratch_root), limits=scratch_limits)
        self.bridge = bridge
        self._formatted = False

    @property
    def fingerprint(self):
        vm = vm_module()
        try:
            config = vm.Runtime.load(self.runtime_path).config
        except (OSError, ValueError):
            deny('sandbox_unavailable', 'Compute VM runtime is unavailable.')
        digest = hashlib.sha256(json.dumps(config, sort_keys=True).encode())
        # Hash actual executable/runtime bytes, not just a mutable path name.
        for raw in ['/usr/bin/python3', config['kernel'], config['qemu'], *config['modules']]:
            with open(raw, 'rb') as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                    digest.update(chunk)
        if self.pip_runtime is not None:
            digest.update(self.pip_runtime['sha256'].encode())
        return digest.hexdigest()

    def admit(self):
        from contextlib import contextmanager
        import fcntl

        @contextmanager
        def admission():
            root = lock = None
            try:
                root = self.scratch._open_root()
                lock = os.open('.compute.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC,
                               0o600, dir_fd=root)
                info = os.fstat(lock)
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.getuid() or info.st_mode & 0o077:
                    deny('sandbox_unavailable', 'Compute admission lock is not private.')
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    deny('resource_limit', 'Another compute execution is active.', retryable=True)
                self.scratch.open()
                if not self._formatted:
                    # Keep the shared accounting image's full logical size,
                    # but limit its guest filesystem to cap minus 2 GiB. The
                    # reservation covers host-side inputs, logs, broker staging
                    # and exports as well as guest-writable data.
                    blocks = (self.scratch.cache_bytes - 2 * 1024**3) // 4096
                    subprocess.run(['/usr/sbin/mke2fs', '-q', '-F', '-t', 'ext4', '-b', '4096', '-m', '0',
                                    '-E', 'lazy_itable_init=1,nodiscard', str(self.scratch.cache), str(blocks)],
                                   env={'PATH': os.defpath}, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30, check=True)
                    self._formatted = True
                yield
            except self.bridge.Readiness as error:
                if error.category == 'build_scratch_space':
                    deny('resource_limit', 'Compute scratch capacity is exhausted.')
                deny('sandbox_unavailable', 'Compute scratch is unavailable.')
            finally:
                if lock is not None:
                    os.close(lock)
                if root is not None:
                    os.close(root)
        return admission()

    def prepare(self, requirements, deadline):
        if self.pip_runtime is None:
            deny('sandbox_unavailable', 'A pinned guest installer runtime is required.')
        environment_id = uuid.uuid4().hex
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            deny('timeout', 'Compute preparation deadline expired.')
        result = run_preparation(self.runtime_path, self.scratch.tmp, self.scratch.cache,
                                 self.pip_runtime, requirements, environment_id, remaining)
        if not result['ok']:
            deny(result['error'], 'Dependency preparation failed; inspect the private execution record.', runner='vm')
        return {'environmentId': environment_id, 'dependencyLock': validate_lock(result['dependencyLock']),
                'downloads': result['downloads']}

    def execute(self, request, selected, environment_id, deadline):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            deny('timeout', 'Compute execution deadline expired.')
        return run_execution(self.runtime_path, self.scratch.tmp, request, remaining,
                             cache=self.scratch.cache if environment_id else None,
                             environment_id=environment_id, input_data=selected)

    def close(self):
        session = self.scratch.session
        self.scratch.close()
        if session is not None and session.exists():
            deny('cleanup_unverified', 'Compute scratch cleanup failed.')


def serve(policy_path):
    """Private host protocol. The model sees only compute.run, never this pipe."""
    import ctypes
    import signal

    class Cancelled(BaseException):
        pass

    def cancel(_signum, _frame):
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        raise Cancelled()

    def respond(value):
        sys.stdout.write(json.dumps(value, separators=(',', ':')) + '\n')
        sys.stdout.flush()

    libc = ctypes.CDLL(None)
    parent = os.getppid()
    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGINT, cancel)
    if libc.prctl(1, signal.SIGTERM, 0, 0, 0) != 0 or os.getppid() != parent:
        deny('sandbox_unavailable', 'Compute helper lost its owner.')
    policy = vm_module().private_json(Path(policy_path))
    required = {'runtime', 'scratch', 'artifactRoot', 'enabled', 'callTimeoutSecs', 'taskTimeoutSecs', 'inputFiles'}
    if not required <= set(policy) or set(policy) - required - {'pip', 'scratchLimits'}:
        deny('sandbox_unavailable', 'Invalid compute host policy.')
    if not isinstance(policy['inputFiles'], dict) or len(policy['inputFiles']) > MAX_FILES:
        deny('bad_args', 'Invalid input file mapping.')
    backend = VMBackend(policy['runtime'], policy['scratch'], policy.get('pip'), policy.get('scratchLimits'))
    artifacts = ArtifactStore(policy['artifactRoot'])
    task = ComputeTask(backend, artifacts, enabled=policy['enabled'], call_timeout=policy['callTimeoutSecs'],
                       task_timeout=policy['taskTimeoutSecs'])
    aliases = {}
    try:
        # Recovery must know the helper identity before any input snapshot exists.
        task._audit()
        total = 0
        for name, path in policy['inputFiles'].items():
            entry = artifacts.import_file(path, name)
            total += entry['bytes']
            if total > MAX_INPUT_BYTES:
                deny('resource_limit', 'Selected inputs exceed the byte limit.')
            aliases[name] = entry['id']
        respond({'ready': True, 'taskId': task.task_id, 'inputs': aliases})
        while True:
            line = sys.stdin.buffer.readline(2 * 1024 * 1024 + 1)
            if not line:
                break
            if len(line) > 2 * 1024 * 1024 or not line.endswith(b'\n'):
                deny('bad_args', 'Compute request frame exceeds its limit.')
            try:
                frame = json.loads(line)
                if not isinstance(frame, dict):
                    deny()
                if set(frame) == {'execute'}:
                    respond({'result': task.execute(frame['execute'])})
                elif frame == {'close': True}:
                    backend.close()
                    respond(task.finish())
                    break
                elif frame == {'report': True}:
                    respond({'records': task.records})
                else:
                    deny()
            except (json.JSONDecodeError, UnicodeError):
                respond({'error': {'code': 'bad_args', 'message': 'Invalid compute request JSON.'}})
            except ComputeError as error:
                respond({'error': {'code': error.code, 'message': str(error)}})
    except Cancelled:
        backend.close()
        respond(task.finish(cancelled=True))
        return 130
    finally:
        backend.close()
        artifacts.close()
    return 0


if __name__ == '__main__':
    if len(sys.argv) != 3 or sys.argv[1] != '--serve':
        raise SystemExit(2)
    try:
        raise SystemExit(serve(sys.argv[2]))
    except ComputeError as error:
        print(json.dumps({'error': {'code': error.code, 'message': str(error)}}), flush=True)
        raise SystemExit(2)
    except (OSError, ValueError, subprocess.SubprocessError):
        print(json.dumps({'error': {'code': 'sandbox_unavailable', 'message': 'Compute runtime could not start.'}}), flush=True)
        raise SystemExit(2)
