#!/usr/bin/env python3
"""Disposable KVM builds. Callers supply a source snapshot, never a checkout.

Runtime configuration is owner-private operator state. No network device, host
home, daemon environment or host process namespace is made available to jobs.
"""
import gzip
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile

MAX_OUTPUT = 8 * 1024 * 1024


class Unavailable(ValueError):
    pass


def private_json(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > MAX_OUTPUT:
            raise Unavailable('VM configuration or receipt is not owner-private')
        return json.load(stream)


class Runtime:
    @classmethod
    def load(cls, path):
        config = private_json(path)
        expected = {'qemu', 'kernel', 'busybox', 'firmware', 'data_dir', 'library_dir', 'module_dir', 'modules', 'memory_mb'}
        optional = {'toolchain', 'registry', 'cargo_git'}
        if not expected <= set(config) or set(config) - expected - optional or type(config['memory_mb']) is not int or not 512 <= config['memory_mb'] <= 4096:
            raise Unavailable('invalid VM runtime configuration')
        if not isinstance(config['modules'], list) or not 1 <= len(config['modules']) <= 32:
            raise Unavailable('invalid VM kernel modules')
        for key in optional & set(config):
            # Dependency trees are untrusted guest inputs, not host executables.
            # Cargo commonly creates them group-writable; export them read-only.
            candidate = Path(config[key])
            if not candidate.is_absolute() or not candidate.is_dir():
                raise Unavailable('invalid read-only dependency directory')
        for raw in [config[key] for key in expected - {'modules', 'memory_mb'}] + config['modules']:
            candidate = Path(raw)
            info = candidate.stat()
            if not candidate.is_absolute() or info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
                raise Unavailable('VM runtime artifact is not trusted')
        instance = cls()
        instance.config = config
        return instance


def initrd(entries):
    """Build newc directly: no root-owned device nodes needed on the host."""
    archive = bytearray()
    for inode, (name, mode, data, major, minor) in enumerate([*entries, ('TRAILER!!!', 0, b'', 0, 0)], 1):
        encoded = name.encode() + b'\0'
        fields = [inode, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(encoded), 0]
        archive.extend(b'070701' + ''.join(f'{value:08x}' for value in fields).encode() + encoded)
        archive.extend(b'\0' * (-len(archive) % 4))
        archive.extend(data)
        archive.extend(b'\0' * (-len(archive) % 4))
    return gzip.compress(bytes(archive), mtime=0)


GUEST_RUNNER = r'''import ctypes,json,os,subprocess
from pathlib import Path
job=json.loads(Path('/job.json').read_text())
uid=job['uid'];gid=job['gid']
os.makedirs('/home/worker',mode=0o700,exist_ok=True)
os.chown('/home/worker',uid,gid)
os.chown('/cargo',uid,gid)
Path('/etc/passwd').write_text(f'root:x:0:0:root:/root:/bin/sh\nworker:x:{uid}:{gid}:worker:/home/worker:/bin/sh\n')
Path('/etc/group').write_text(f'root:x:0:\nworker:x:{gid}:\n')
Path('/etc/hosts').write_text('127.0.0.1 localhost\n::1 localhost\n')
environment={'PATH':'/toolchain/bin:/usr/bin:/bin','HOME':'/home/worker','USER':'worker','LOGNAME':'worker',
    'LANG':'C.UTF-8','TMPDIR':'/tmp','CARGO_HOME':'/cargo','CARGO_NET_OFFLINE':'true','CARGO_TARGET_DIR':'/workspace/target'}
environment.update(job['environment'])
libc=ctypes.CDLL(None)
def worker():
    if libc.prctl(38,1,0,0,0)!=0: raise OSError('cannot enforce no-new-privileges')
    os.setgroups([]); os.setgid(gid); os.setuid(uid)
    os.umask(0o022)
with open('/stdout','w+b') as stdout,open('/stderr','w+b') as stderr:
    try:
        process=subprocess.Popen(job['argv'],cwd='/workspace',env=environment,
            stdin=subprocess.DEVNULL,stdout=stdout,stderr=stderr,start_new_session=True,preexec_fn=worker)
        status=process.wait()
        stdout.seek(0); stderr.seek(0)
        result={'exit_code':status,'stdout':stdout.read(8*1024*1024+1).decode('utf-8','replace'),
                'stderr':stderr.read(8*1024*1024+1).decode('utf-8','replace')}
        if len(result['stdout'].encode())+len(result['stderr'].encode())>8*1024*1024:
            result={'error':'VM command exceeded output limit'}
    except OSError:
        result={'error':'VM command could not start'}
    path=Path('/root/control/outcome.json')
    with path.open('w') as receipt:
        os.chmod(path,0o600);json.dump(result,receipt);receipt.flush();os.fsync(receipt.fileno())
'''


def run(runtime, workspace, argv, environment, timeout=120):
    """Execute argv in a disposable source snapshot; return after verified VM exit."""
    workspace = Path(workspace)
    if os.getuid() == 0:
        raise Unavailable('VM builds require an unprivileged host account')
    if not workspace.is_absolute() or workspace.is_symlink() or not workspace.is_dir():
        raise Unavailable('VM workspace must be a disposable absolute directory')
    if not argv or not all(isinstance(arg, str) and '\0' not in arg for arg in argv):
        raise Unavailable('invalid VM command arguments')
    allowed_environment = {'PATH', 'HOME', 'LANG', 'LC_ALL', 'TERM', 'CARGO_HOME', 'RUSTUP_HOME',
        'RUSTUP_TOOLCHAIN', 'CARGO_TARGET_DIR', 'CARGO_NET_OFFLINE', 'NPM_CONFIG_CACHE',
        'NPM_CONFIG_USERCONFIG', 'NPM_CONFIG_GLOBALCONFIG'}
    if set(environment) - allowed_environment:
        raise Unavailable('unexpected VM environment variable')
    for base, dirs, files in os.walk(workspace, followlinks=False):
        for name in files:
            info = (Path(base) / name).lstat()
            if stat.S_ISREG(info.st_mode) and info.st_nlink != 1:
                raise Unavailable('VM snapshot must not contain host hard links')
    config = runtime.config
    with tempfile.TemporaryDirectory(prefix='jarvis-build-vm-') as temporary:
        private = Path(temporary)
        control = private / 'control'; control.mkdir(mode=0o700)
        entries = []
        for name in ('bootstrap', 'dev', 'proc', 'sys', 'tmp', 'modules', 'usr', 'workspace', 'etc', 'home',
                     'cargo', 'cargo/registry', 'cargo/git', 'toolchain', 'etc/alternatives'):
            entries.append((name, stat.S_IFDIR | (0o1777 if name == 'tmp' else 0o755), b'', 0, 0))
        entries.extend([('root', stat.S_IFDIR | 0o700, b'', 0, 0), ('root/control', stat.S_IFDIR | 0o700, b'', 0, 0)])
        entries.append(('dev/console', stat.S_IFCHR | 0o600, b'', 5, 1))
        entries.append(('bootstrap/busybox', stat.S_IFREG | 0o755, Path(config['busybox']).read_bytes(), 0, 0))
        for name in ('sh', 'mount', 'ip', 'insmod', 'poweroff'):
            entries.append(('bootstrap/' + name, stat.S_IFLNK | 0o777, b'busybox', 0, 0))
        for name in ('bin', 'sbin', 'lib', 'lib64'):
            entries.append((name, stat.S_IFLNK | 0o777, ('usr/' + name).encode(), 0, 0))
        for name, target in [('cc', '/usr/bin/gcc'), ('c++', '/usr/bin/g++'), ('cpp', '/usr/bin/cpp')]:
            entries.append(('etc/alternatives/' + name, stat.S_IFLNK | 0o777, target.encode(), 0, 0))
        module_commands = []
        for index, path in enumerate(config['modules']):
            name = f'modules/{index}.ko'
            entries.append((name, stat.S_IFREG | 0o600, Path(path).read_bytes(), 0, 0))
            module_commands.append(f'insmod /{name} || poweroff -f')
        dependency_mounts = []
        shares = [('runtime', Path('/usr'), True), ('workspace', workspace, False), ('control', control, False)]
        for key, destination in [('toolchain', '/toolchain'), ('registry', '/cargo/registry'), ('cargo_git', '/cargo/git')]:
            if key in config:
                shares.append((key, Path(config[key]), True))
                dependency_mounts.append(f'mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev {key} {destination} || poweroff -f')
        boot = '''#!/bootstrap/sh
export PATH=/bootstrap
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
ip link set lo up
''' + '\n'.join(module_commands) + '''
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f
mount -t 9p -o trans=virtio,version=9p2000.L,nosuid,nodev workspace /workspace || poweroff -f
mount -t 9p -o trans=virtio,version=9p2000.L,nosuid,nodev,noexec control /root/control || poweroff -f
''' + '\n'.join(dependency_mounts) + '''
/usr/bin/python3 -I /guest.py
poweroff -f
'''
        entries.extend([('init', stat.S_IFREG | 0o755, boot.encode(), 0, 0),
            ('guest.py', stat.S_IFREG | 0o400, GUEST_RUNNER.encode(), 0, 0),
            ('job.json', stat.S_IFREG | 0o400, json.dumps({'argv': argv, 'environment': environment,
                'uid': os.getuid(), 'gid': os.getgid()}).encode(), 0, 0)])
        image = private / 'initrd.gz'; image.write_bytes(initrd(entries))
        command = [config['qemu'], '-no-user-config', '-nodefaults', '-machine', 'pc,accel=kvm',
            '-cpu', 'host', '-m', str(config['memory_mb']), '-smp', '2', '-nographic', '-serial', 'stdio',
            '-monitor', 'none', '-nic', 'none', '-no-reboot', '-L', config['data_dir'], '-bios', config['firmware'],
            '-kernel', config['kernel'], '-initrd', str(image), '-append', 'rdinit=/init console=ttyS0 panic=-1 loglevel=3',
            '-sandbox', 'on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny']
        for tag, path, readonly in shares:
            escaped = str(path).replace(',', ',,')
            command.extend(['-fsdev', f'local,id={tag},path={escaped},security_model=none,readonly={"on" if readonly else "off"}',
                '-device', f'virtio-9p-pci,fsdev={tag},mount_tag={tag}'])
        host_environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'], 'QEMU_MODULE_DIR': config['module_dir']}
        supervisor = Path(__file__).with_name('provider-supervisor.py')
        cleanup = private / 'cleanup-complete'
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen([sys.executable, '-I', str(supervisor), str(cleanup), *command],
                env=host_environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
            try:
                try:
                    status = process.wait(timeout=min(max(timeout, 1), 900))
                except subprocess.TimeoutExpired as exc:
                    raise Unavailable('VM command timed out') from exc
                if status or not cleanup.is_file() or cleanup.read_text() != 'all-descendants-reaped\n':
                    raise Unavailable('VM execution or cleanup failed')
                try:
                    result = private_json(control / 'outcome.json')
                except (OSError, ValueError) as exc:
                    raise Unavailable('VM did not produce a trusted command result') from exc
                if 'error' in result:
                    raise Unavailable(result['error'])
                if set(result) != {'exit_code', 'stdout', 'stderr'} or type(result['exit_code']) is not int:
                    raise Unavailable('invalid VM result')
                return result
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
