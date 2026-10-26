#!/usr/bin/env python3
"""Controller-owned native CLI launcher: no guest NIC or host home export.

The enclosing provider supervisor owns resource admission and descendant cleanup.
This launcher streams framed guest output, delegates tool calls to the original
host policy bridge, and services only the declared provider gateway protocol.
"""
import base64
import contextlib
import hashlib
import fcntl
import uuid
import importlib.util
import json
import os
import queue
from pathlib import Path
import selectors
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time

MAX_FRAME = 2 * 1024 * 1024
MAX_HOME = 64 * 1024 * 1024



class BoundedWriter:
    """Never block the shared control/gateway dispatcher on an application pipe."""
    def __init__(self, write, finish, failed):
        self.write, self.finish, self.failed = write, finish, failed
        self.pending = queue.Queue(16)
        self.closed = threading.Event()
        self.ended = False
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def submit(self, data):
        if self.closed.is_set() or self.ended or (data is not None and len(data) > 65536):
            raise RuntimeError('invalid or closed native stream')
        if data is None:
            self.ended = True
        try:
            self.pending.put_nowait(data)
        except queue.Full:
            raise RuntimeError('native stream backpressure limit') from None

    def run(self):
        try:
            while True:
                data = self.pending.get()
                if data is None:
                    self.finish()
                    return
                self.write(data)
        except BaseException:
            # Pipe/transport exceptions can embed private protocol payloads.
            self.failed()
        finally:
            self.closed.set()



def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def private_json(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd) as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or info.st_size > MAX_FRAME):
            raise ValueError('native VM launch input is not private')
        return json.load(stream)


def private_bytes(path, maximum):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or info.st_size > maximum):
            raise ValueError('native VM credential/runtime input is not private')
        return stream.read(maximum + 1)


def atomic(path, data):
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
        name = Path(stream.name)
        os.fchmod(stream.fileno(), 0o600)
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(name, path)
    parent_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def safe_relative(raw):
    path = Path(raw)
    if (not isinstance(raw, str) or not raw or path.is_absolute()
            or any(part in ('..', '.') for part in path.parts)
            or '\0' in raw or len(raw) > 1024 or len(path.parts) > 16):
        raise ValueError('invalid native runtime export path')
    return path


@contextlib.contextmanager
def short_channel_endpoint():
    """Socket only: AF_UNIX names cannot inherit arbitrarily long state roots.

    All durable artifacts remain in the registered session. This private,
    ephemeral directory contains only the live transport socket and is removed
    after QEMU cleanup, including startup failure.
    """
    with tempfile.TemporaryDirectory(prefix='jv-', dir='/tmp') as directory:
        root = Path(directory)
        metadata = root.stat()
        if root.is_symlink() or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
            raise ValueError('native channel directory is not private')
        endpoint = root / 'channel'
        if len(os.fsencode(endpoint)) > 107:
            raise ValueError('native channel path exceeds AF_UNIX bound')
        yield endpoint


class Channel:
    def __init__(self, connection):
        self.connection = connection
        self.lock = threading.Lock()

    def send(self, frame):
        data = json.dumps(frame, separators=(',', ':')).encode() + b'\n'
        if len(data) > MAX_FRAME:
            raise ValueError('native VM frame exceeds limit')
        with self.lock:
            self.connection.sendall(data)

    def receive(self):
        with self.connection.makefile('rb') as stream:
            while True:
                raw = stream.readline(MAX_FRAME + 1)
                if not raw:
                    return
                if len(raw) > MAX_FRAME or not raw.endswith(b'\n'):
                    raise ValueError('invalid native VM frame')
                frame = json.loads(raw)
                if not isinstance(frame, dict) or not isinstance(frame.get('type'), str):
                    raise ValueError('invalid native VM frame type')
                yield frame


def jwt_identity(token):
    parts = token.split('.')
    if len(parts) != 3 or any(not part for part in parts):
        raise ValueError('credential identity token is not a JWT')
    raw = parts[1].encode()
    payload = json.loads(base64.b64decode(raw + b'=' * (-len(raw) % 4), altchars=b'-_', validate=True))
    if not isinstance(payload, dict) or not all(key in payload for key in ('sub', 'iss', 'aud')):
        raise ValueError('credential identity claims unavailable')
    identity = {key: payload[key] for key in ('sub', 'iss', 'aud')}
    account = payload.get('https://api.openai.com/auth', {})
    if not isinstance(account, dict):
        raise ValueError('credential account claims malformed')
    identity['account'] = {key: account[key] for key in
        ('chatgpt_account_id', 'user_id', 'organization_id') if key in account}
    return identity


def refreshed_credential(original, refreshed, provider):
    """Only provider-declared rotating fields may change; identity stays fixed."""
    if not isinstance(original, dict) or not isinstance(refreshed, dict):
        raise ValueError('credential schema is not an object')
    if provider == 'claude':
        container = 'claudeAiOauth'
        rotating = {'accessToken': str, 'refreshToken': str, 'expiresAt': int}
        timestamps = set()
    elif provider == 'codex':
        container = 'tokens'
        rotating = {'access_token': str, 'refresh_token': str, 'id_token': str}
        timestamps = {'last_refresh'}
    else:
        raise ValueError('provider does not declare OAuth refresh')
    before, after = original.get(container), refreshed.get(container)
    if not isinstance(before, dict) or not isinstance(after, dict) or set(before) != set(after):
        raise ValueError('credential token schema changed')
    if set(original) != set(refreshed):
        raise ValueError('credential outer schema changed')
    for key in original:
        if key == container:
            continue
        if key in timestamps:
            if not isinstance(refreshed[key], str) or len(refreshed[key]) > 128:
                raise ValueError('invalid credential refresh timestamp')
        elif original[key] != refreshed[key]:
            raise ValueError('credential identity or configuration changed')
    for key in before:
        if key not in rotating:
            if before[key] != after[key]:
                raise ValueError('credential account identity or scope changed')
        elif type(after[key]) is not rotating[key]:
            raise ValueError('invalid refreshed token shape')
        elif isinstance(after[key], str) and not 1 <= len(after[key]) <= 128 * 1024:
            raise ValueError('invalid refreshed token length')
        elif isinstance(after[key], int) and after[key] <= 0:
            raise ValueError('invalid credential expiry')
    if not set(rotating) <= set(before):
        raise ValueError('required rotating credential fields unavailable')
    if provider == 'codex' and jwt_identity(before['id_token']) != jwt_identity(after['id_token']):
        raise ValueError('credential identity token account changed')
    return refreshed


def credential_use(spec):
    if not spec.get('refresh_auth'):
        return None
    auth = Path(spec['auth_file'])
    controlled = Path(spec['credential_directory'])
    if auth != controlled / (spec['provider'] + '.json'):
        raise ValueError('refresh credential is outside controlled writer domain')
    for parent in auth.parents:
        info = parent.stat()
        if info.st_uid not in (0, os.getuid()) or (info.st_mode & 0o022 and not (info.st_uid == 0 and info.st_mode & stat.S_ISVTX)):
            raise ValueError('credential ancestor is writable by other principals')
    if auth.resolve() != auth or auth.parent.stat().st_uid != os.getuid() or auth.parent.stat().st_mode & 0o077:
        raise ValueError('credential refresh parent is not private')
    lock_fd = os.open(auth.with_name(auth.name + '.native-vm-refresh.lock'),
                      os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        info = os.fstat(lock_fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            raise ValueError('credential refresh lock is not private')
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return lock_fd
    except BaseException:
        os.close(lock_fd)
        raise


def refresh_auth(spec, generation, original_digest, held_lock=None):
    if not spec.get('refresh_auth'):
        return
    lock_fd = held_lock if held_lock is not None else credential_use(spec)
    auth = Path(spec['auth_file'])
    try:
        current = private_bytes(auth, 1024 * 1024)
        if hashlib.sha256(current).hexdigest() != original_digest:
            return  # Independently refreshed controller credentials always win.
        candidate = private_bytes(generation / safe_relative(spec['auth_relative']), 1024 * 1024)
        updated = refreshed_credential(json.loads(current), json.loads(candidate), spec['provider'])
        if updated != json.loads(current):
            atomic(auth, json.dumps(updated, separators=(',', ':')).encode())
    finally:
        if held_lock is None:
            os.close(lock_fd)


def runtime_seed(runtime, auth_file, auth_relative):
    entries = []
    total = 0
    generation = private_json(runtime / 'complete.json')['generation'] if (runtime / 'complete.json').exists() else None
    if generation and (not generation.startswith('generation-') or len(safe_relative(generation).parts) != 1):
        raise ValueError('invalid native runtime generation')
    home = runtime / safe_relative(generation) if generation else runtime / 'empty'
    if home.is_symlink():
        raise ValueError('native runtime generation is a symlink')
    if home.exists():
        for base, directories, files in os.walk(home, followlinks=False):
            for name in directories:
                item = Path(base) / name
                if item.is_symlink():
                    raise ValueError('native runtime directory is a symlink')
            for name in files:
                item = Path(base) / name
                data = private_bytes(item, MAX_HOME - total)
                total += len(data)
                if total > MAX_HOME:
                    raise ValueError('native runtime exceeds budget')
                entries.append((str(item.relative_to(home)), data))
    original = None
    if auth_file:
        data = private_bytes(Path(auth_file), 1024 * 1024)
        json.loads(data)
        original = hashlib.sha256(data).hexdigest()
        previous = private_json(runtime / 'complete.json') if (runtime / 'complete.json').exists() else {}
        # Preserve an OAuth refresh produced inside this isolated conversation.
        # A separately refreshed controller credential supersedes the seed.
        if previous.get('auth_generation') != original or not any(name == auth_relative for name, _ in entries):
            entries = [(name, contents) for name, contents in entries if name != auth_relative]
            entries.append((auth_relative, data))
    return entries, original


def boot_entries(config, job, seed, guest_source, gateway_source, cert_entries):
    entries = []
    directories = ('bootstrap', 'dev', 'proc', 'sys', 'tmp', 'modules', 'usr', 'etc', 'native',
                   'home', 'home/worker', 'workspace', 'seed', 'root', 'root/control',
                   'run', 'run/native-provider-gateway')
    for name in directories:
        mode = 0o700 if name.startswith('root') else 0o1777 if name == 'tmp' else 0o755
        entries.append((name, stat.S_IFDIR | mode, b'', 0, 0))
    entries.append(('dev/console', stat.S_IFCHR | 0o600, b'', 5, 1))
    entries.append(('bootstrap/busybox', stat.S_IFREG | 0o755, Path(config['busybox']).read_bytes(), 0, 0))
    for name in ('sh', 'mount', 'ip', 'insmod', 'poweroff', 'mkdir'):
        entries.append(('bootstrap/' + name, stat.S_IFLNK | 0o777, b'busybox', 0, 0))
    for name in ('bin', 'sbin', 'lib', 'lib64'):
        entries.append((name, stat.S_IFLNK | 0o777, ('usr/' + name).encode(), 0, 0))
    commands = []
    for index, raw in enumerate(config['modules']):
        entries.append((f'modules/{index}.ko', stat.S_IFREG | 0o400, Path(raw).read_bytes(), 0, 0))
        commands.append(f'insmod /modules/{index}.ko || poweroff -f')
    # Only this process-specific seed enters the guest; no host directory is
    # exported for writable home state. Runtime exports are bounded frames.
    seen = set(directories)
    for relative, data in seed:
        path = Path('seed') / safe_relative(relative)
        for parent in reversed(path.parents):
            if str(parent) not in ('.', '') and str(parent) not in seen:
                entries.append((str(parent), stat.S_IFDIR | 0o700, b'', 0, 0))
                seen.add(str(parent))
        entries.append((str(path), stat.S_IFREG | 0o600, data, 0, 0))
    entries.extend(cert_entries)
    entries.extend([
        ('root/job.json', stat.S_IFREG | 0o400, json.dumps(job).encode(), 0, 0),
        ('root/guest.py', stat.S_IFREG | 0o400, guest_source, 0, 0),
        ('root/native_provider_gateway.py', stat.S_IFREG | 0o400, gateway_source, 0, 0),
    ])
    boot = '#!/bootstrap/sh\nexport PATH=/bootstrap:/usr/bin:/bin\nmount -t devtmpfs devtmpfs /dev\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\n' + '\n'.join(commands) + '''
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev provider /native || poweroff -f
ip link set lo up
/usr/bin/python3 -I -S /root/guest.py
poweroff -f
'''
    entries.append(('init', stat.S_IFREG | 0o700, boot.encode(), 0, 0))
    return entries


def validate_runtime(path):
    guest_runtime = Path(path)
    if guest_runtime.is_symlink() or not guest_runtime.is_absolute() or guest_runtime.stat().st_mode & 0o077:
        raise ValueError('native guest runtime is not private')
    manifest = json.loads((guest_runtime / 'manifest.json').read_text())
    if manifest.get('version') != 1 or not manifest.get('files'):
        raise ValueError('native guest runtime manifest unavailable')
    declared = set()
    for entry in manifest['files']:
        relative = safe_relative(entry['path'])
        path = guest_runtime / relative
        if path.is_symlink() or not path.resolve().is_relative_to(guest_runtime):
            raise ValueError('runtime manifest contains a link')
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != entry['sha256']:
            raise ValueError('runtime artifact digest changed')
        declared.add(str(relative))
    inventory = list(guest_runtime.rglob('*'))
    if any(path.is_symlink() for path in inventory):
        raise ValueError('runtime symlink is undeclared')
    actual = {str(path.relative_to(guest_runtime)) for path in inventory if path.is_file()}
    if actual != declared | {'manifest.json'}:
        raise ValueError('undeclared runtime file')


def main(spec_path):
    vm = module('native_vm_runtime', 'codex-build-vm.py')
    gateway_module = module('native_provider_gateway', 'native-provider-gateway.py')
    spec = private_json(spec_path)
    if spec.get('version') != 1:
        raise ValueError('unsupported native VM launch version')
    validate_runtime(spec['guest_runtime'])
    runtime = vm.Runtime.load(Path(spec['runtime_config']))
    config = dict(runtime.config)
    config['memory_mb'] = spec['memory_mb']
    if not 512 <= config['memory_mb'] <= 4096:
        raise ValueError('native VM memory policy invalid')
    state = Path(spec['session_directory'])
    if state.is_symlink() or state.resolve() != state or state.stat().st_mode & 0o077:
        raise ValueError('native session directory is not private')
    if spec.get('resume') and not (state / 'complete.json').is_file():
        raise ValueError('native conversation recovery requires completed runtime export')
    lock_fd = os.open(state / 'lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    try:
        credential_lock = credential_use(spec)
    except BlockingIOError:
        ready = os.environ.get('JARVIS_NATIVE_VM_READY')
        if ready:
            atomic(Path(ready).with_name('vm-startup-failure.json'), b'{"phase":"native_credential_in_use"}')
        raise RuntimeError('native credential is in use; retry after capacity is available') from None
    seed, original_auth = runtime_seed(state, spec.get('auth_file'), spec['auth_relative'])
    with tempfile.TemporaryDirectory(prefix='native-vm-launch-', dir=state) as temporary, short_channel_endpoint() as endpoint:
        private = Path(temporary)
        binary = private / 'provider'
        binary.mkdir()
        executable = Path(spec['executable'])
        info = executable.stat()
        if not executable.is_absolute() or info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022 or info.st_size > 512 * 1024 * 1024:
            raise ValueError('native executable is not trusted or exceeds budget')
        shutil.copyfile(executable, binary / 'cli')
        (binary / 'cli').chmod(0o555)
        assets = spec.get('assets', [])
        if len(assets) > 32:
            raise ValueError('native asset count exceeds budget')
        asset_bytes = 0
        for asset in assets:
            name = safe_relative(asset['name'])
            if len(name.parts) != 1 or not str(name).startswith('asset-'):
                raise ValueError('invalid native input asset')
            source = Path(asset['source'])
            fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            with os.fdopen(fd, 'rb') as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_size > 16 * 1024 * 1024:
                    raise ValueError('native input asset exceeds policy')
                data = stream.read(16 * 1024 * 1024 + 1)
                asset_bytes += len(data)
                if asset_bytes > 16 * 1024 * 1024:
                    raise ValueError('native total asset budget exceeded')
                (binary / name).write_bytes(data)
                (binary / name).chmod(0o444)
        # The guest receives only a relay executable, never the host bridge
        # policy, settings, hooks, source checkout or daemon credential store.
        shutil.copyfile(Path(__file__).with_name('native-provider-mcp-relay.py'), binary / 'mcp-relay.py')
        guest_config, certificates = gateway_module.provision(spec['gateway'], private)
        job = {'argv': spec['argv'], 'environment': spec['environment'], 'uid': os.getuid(), 'gid': os.getgid(),
               'gateway': guest_config, 'mcp': bool(spec.get('mcp_argv')), 'auth_relative': spec['auth_relative']}
        entries = boot_entries(config, job, seed, Path(__file__).with_name('native-provider-vm-guest.py').read_bytes(),
                               Path(__file__).with_name('native-provider-gateway.py').read_bytes(), certificates)
        image = private / 'initrd.gz'
        image.write_bytes(vm.initrd(entries))
        command = vm.qemu_command(config, image, [('runtime', Path(spec['guest_runtime']), True), ('provider', binary, True)], None)
        command[command.index('-serial') + 1] = 'null'
        command.extend(['-device', 'virtio-serial-pci', '-chardev',
                        f'socket,id=native,path={endpoint},server=on,wait=off', '-device',
                        'virtserialport,chardev=native,name=org.jarvis.native.control'])
        environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'], 'QEMU_MODULE_DIR': config['module_dir']}
        process = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        bridge = None
        gateway = None
        channel_socket = socket.socket(socket.AF_UNIX)
        phase = 'channel_startup'
        frame_counts = {}
        admitted = False
        exported = 0
        received = set()
        active_export = None
        quiescence = None
        exit_code = None
        try:
            deadline = time.monotonic() + 30
            while True:
                if process.poll() is not None:
                    raise RuntimeError('native VM could not boot')
                try:
                    channel_socket.connect(str(endpoint))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    if time.monotonic() > deadline:
                        raise TimeoutError('native VM channel admission timed out')
                    time.sleep(0.02)
            channel = Channel(channel_socket)
            diagnostic_path = Path(spec_path).parent / 'gateway-route-denials.jsonl' if spec.get('probe_route_diagnostics') is True else None
            gateway = gateway_module.HostGateway(spec['gateway'], channel.send, diagnostic_path=diagnostic_path)
            if spec.get('mcp_argv'):
                bridge = subprocess.Popen(spec['mcp_argv'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                          stderr=subprocess.DEVNULL, env={'PATH': os.defpath, 'HOME': str(Path.home())})
                def bridge_output():
                    for raw in iter(lambda: bridge.stdout.read1(65536), b''):
                        channel.send({'type': 'mcp_output', 'data': base64.b64encode(raw).decode()})
                    channel.send({'type': 'mcp_end'})
                threading.Thread(target=bridge_output, daemon=True).start()
                def bridge_failed():
                    try:
                        channel_socket.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                def bridge_write(data):
                    bridge.stdin.write(data)
                    bridge.stdin.flush()
                bridge_writer = BoundedWriter(bridge_write, bridge.stdin.close, bridge_failed)
            else:
                bridge_writer = None
            def stdin_input():
                for raw in iter(lambda: sys.stdin.buffer.read1(65536), b''):
                    channel.send({'type': 'stdin', 'data': base64.b64encode(raw).decode()})
                channel.send({'type': 'stdin_end'})
            admitted = False
            exported = 0
            received = set()
            export_directories = set()
            staging = private / 'export'
            staging.mkdir(mode=0o700)
            exit_code = None
            active_export = None
            quiescence = None
            phase = 'receive_frames'
            for frame in channel.receive():
                kind = frame['type']
                safe_kind = kind if kind in ('ready', 'stdout', 'stderr', 'mcp_input', 'worker_quiescent', 'runtime_start', 'runtime_data', 'runtime_end', 'completion_failure', 'guest_fatal', 'exit') else 'gateway' if kind.startswith('gateway_') else 'unknown'
                frame_counts[safe_kind] = frame_counts.get(safe_kind, 0) + 1
                if kind == 'ready' and not admitted:
                    ready = os.environ.get('JARVIS_NATIVE_VM_READY')
                    proceed = os.environ.get('JARVIS_NATIVE_VM_CONTINUE')
                    if not ready or not proceed:
                        raise RuntimeError('native VM controller admission missing')
                    atomic(Path(ready), b'ready\n')
                    deadline = time.monotonic() + 15
                    while not Path(proceed).exists():
                        if time.monotonic() >= deadline:
                            raise TimeoutError('native VM admission timed out')
                        time.sleep(0.005)
                    if private_bytes(Path(proceed), 32) != b'admitted\n':
                        raise ValueError('invalid native VM admission')
                    # Durable controller dispatch intent precedes releasing the
                    # guest. An interrupted handshake must not imply no call.
                    atomic(Path(ready).with_name('vm-dispatch-intent'),
                           json.dumps({'version': 1, 'phase': 'dispatch_uncertain'}).encode())
                    channel.send({'type': 'start'})
                    admitted = True
                    threading.Thread(target=stdin_input, daemon=True).start()
                elif kind in ('stdout', 'stderr') and admitted:
                    data = base64.b64decode(frame['data'], validate=True)
                    stream = sys.stdout.buffer if kind == 'stdout' else sys.stderr.buffer
                    stream.write(data)
                    stream.flush()
                elif kind == 'mcp_input':
                    if bridge is None:
                        raise ValueError('undeclared tool channel')
                    bridge_writer.submit(base64.b64decode(frame['data'], validate=True))
                elif kind.startswith('gateway_'):
                    gateway.receive(frame)
                elif kind == 'worker_quiescent' and admitted and quiescence is None:
                    keys = ('scans', 'signalled', 'reaped', 'remaining_workers', 'quiet_scans')
                    if any(type(frame.get(key)) is not int or not 0 <= frame[key] <= 100000 for key in keys):
                        raise ValueError('invalid native quiescence receipt')
                    if frame['remaining_workers'] != 0 or frame['quiet_scans'] < 2 or frame['scans'] < 2:
                        raise ValueError('native workers remain before runtime export')
                    quiescence = {key: frame[key] for key in keys}
                elif kind == 'runtime_start' and admitted and quiescence is not None:
                    relative = safe_relative(frame['path'])
                    size = frame['size']
                    if active_export is not None or type(size) is not int or size < 0:
                        raise ValueError('invalid runtime export start')
                    exported += size
                    if exported > MAX_HOME or str(relative) in received or len(received) >= 4096:
                        raise ValueError('native runtime export exceeds budget or duplicates file')
                    received.add(str(relative))
                    target = staging / relative
                    export_directories.update(str(parent) for parent in relative.parents if str(parent) != '.')
                    if len(export_directories) > 2048:
                        raise ValueError('native runtime directory budget exceeded')
                    target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                    stream = open(target, 'xb')
                    os.fchmod(stream.fileno(), 0o600)
                    active_export = [stream, size, 0]
                elif kind == 'runtime_data' and active_export is not None:
                    data = base64.b64decode(frame['data'], validate=True)
                    stream, size, offset = active_export
                    if frame['offset'] != offset or offset + len(data) > size:
                        raise ValueError('invalid runtime export chunk')
                    stream.write(data)
                    active_export[2] += len(data)
                elif kind == 'runtime_end' and active_export is not None:
                    stream, size, offset = active_export
                    if size != offset:
                        raise ValueError('truncated runtime export')
                    stream.flush()
                    os.fsync(stream.fileno())
                    stream.close()
                    active_export = None
                elif kind == 'guest_fatal':
                    reason = frame.get('reason')
                    if reason not in ('stdin_write_failed', 'mcp_write_failed', 'oversized_or_truncated_frame', 'stdin_submit_failed', 'mcp_submit_failed', 'unknown_frame', 'controller_eof', 'receive_exception'):
                        raise ValueError('invalid guest fatal diagnostic')
                    print(json.dumps({'native_guest_fatal': reason}), file=sys.stderr, flush=True)
                    raise RuntimeError('guest failed before completion')
                elif kind == 'completion_failure':
                    if frame.get('phase') not in ('quiescence', 'output_drain', 'runtime_export', 'exit_receipt') or frame.get('category') not in ('ValueError', 'RuntimeError', 'OSError', 'PermissionError', 'FileNotFoundError', 'BoundaryFailure'):
                        raise ValueError('invalid completion diagnostic')
                    diagnostic = {'phase': frame['phase'], 'category': frame['category']}
                    if type(frame.get('errno')) is int:
                        diagnostic['errno'] = frame['errno']
                    if frame.get('export_kind') in ('symlink', 'directory_symlink', 'open_failure') and frame.get('export_area') in ('codex_arg0', 'other'):
                        diagnostic.update(export_kind=frame['export_kind'], export_area=frame['export_area'])
                    print(json.dumps({'native_guest_completion_failure': diagnostic}), file=sys.stderr, flush=True)
                    raise RuntimeError('guest completion failed')
                elif kind == 'exit':
                    exit_code = frame['code']
                    if type(exit_code) is not int or not 0 <= exit_code <= 255:
                        raise ValueError('invalid native exit status')
                    break
                else:
                    raise ValueError('unknown native guest frame')
            if exit_code is None or active_export is not None or not admitted or quiescence is None:
                raise RuntimeError('native VM disconnected without completion')
            phase = 'qemu_shutdown'
            process.wait(timeout=10)
            if process.returncode:
                raise RuntimeError('native VM shutdown failed')
            phase = 'publish_generation'
            generation = 'generation-' + uuid.uuid4().hex
            os.replace(staging, state / generation)
            refresh_auth(spec, state / generation, original_auth, credential_lock)
            atomic(state / 'complete.json', json.dumps({'version': 1, 'exit_code': exit_code,
                   'generation': generation, 'export_bytes': exported,
                   'auth_generation': original_auth, 'worker_quiescence': quiescence,
                   'launch_sha256': hashlib.sha256(private_bytes(Path(spec_path), 2 * 1024 * 1024)).hexdigest(),
                   'gateway_transport': gateway.transport_receipt()}).encode())
            # Only controller-generated complete exports are retained. Keep the
            # previous generation through pointer publication for crash safety.
            previous = sorted(state.glob('generation-*'), key=lambda path: path.stat().st_mtime)
            for obsolete in previous[:-2]:
                if obsolete.name != generation and not obsolete.is_symlink():
                    shutil.rmtree(obsolete)
            if exit_code and (gateway.guest_failures or gateway.host_failures):
                print(json.dumps({'native_gateway_failures': gateway.guest_failures, 'native_gateway_host_failures': gateway.host_failures}), file=sys.stderr, flush=True)
            return exit_code
        except Exception:
            print(json.dumps({'native_host_failure_state': {'phase': phase, 'frames': frame_counts,
                'admitted': admitted, 'quiescent': quiescence is not None, 'export_bytes': exported,
                'export_files': len(received), 'partial_export': active_export is not None,
                'guest_exit_code': exit_code, 'qemu_exit_code': process.poll()}}), file=sys.stderr, flush=True)
            if gateway is not None:
                # Retained observation is diagnostic only: no generation publication,
                # complete.json, or successful launch authority follows this path.
                atomic(Path(spec_path).parent / 'failed-gateway-observation.json', json.dumps({
                    'version': 1, 'diagnostic_only': True,
                    'launch_sha256': hashlib.sha256(private_bytes(Path(spec_path), 2 * 1024 * 1024)).hexdigest(),
                    'gateway_transport': gateway.transport_receipt()}).encode())
            if gateway is not None and (gateway.guest_failures or gateway.host_failures):
                print(json.dumps({'native_gateway_failures': gateway.guest_failures, 'native_gateway_host_failures': gateway.host_failures}), file=sys.stderr, flush=True)
            raise
        finally:
            if gateway is not None:
                gateway.close()
            channel_socket.close()
            os.close(lock_fd)
            if bridge is not None:
                bridge.terminate()
            if process.poll() is None:
                process.terminate()
            for child in (bridge, process):
                if child is None:
                    continue
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            if credential_lock is not None:
                os.close(credential_lock)


if __name__ == '__main__':
    try:
        raise SystemExit(main(sys.argv[1]))
    except Exception as error:
        # Only a fixed exception category is safe; messages may contain secrets.
        category = type(error).__name__
        if category not in {'JSONDecodeError','ValueError','RuntimeError','TimeoutError','OSError','BrokenPipeError','FileNotFoundError','PermissionError','ChildProcessError','CalledProcessError'}:
            category = 'BoundaryFailure'
        diagnostic = {'native_vm_failure': category}
        if isinstance(error, OSError) and isinstance(error.errno, int):
            diagnostic['errno'] = error.errno
        print(json.dumps(diagnostic), file=sys.stderr)
        print('Native VM boundary unavailable; launch terminated without host fallback.', file=sys.stderr)
        raise SystemExit(126)
