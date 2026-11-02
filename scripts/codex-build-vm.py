#!/usr/bin/env python3
"""Disposable KVM builds. Callers supply a source snapshot, never a checkout.

Runtime configuration is owner-private operator state. No network device, host
home, daemon environment or host process namespace is made available to jobs.
"""
import gzip
import importlib.util
import json
import os
from pathlib import Path
import signal
import shutil
import shlex
import stat
import subprocess
import sys
import tempfile
import time

PROXY_SPEC = importlib.util.spec_from_file_location('jarvis_dependency_proxy',
    Path(__file__).with_name('build-dependency-proxy.py'))
dependency_proxy = importlib.util.module_from_spec(PROXY_SPEC)
PROXY_SPEC.loader.exec_module(dependency_proxy)

MAX_OUTPUT = 8 * 1024 * 1024


class Unavailable(ValueError):
    pass


class FreshnessUnavailable(Unavailable):
    def __init__(self, diagnostic):
        self.diagnostic = diagnostic
        super().__init__('shared CI prerequisite unavailable: source/cache freshness')


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
        optional = {'toolchain', 'registry', 'cargo_git', 'ort_cache'}
        if not expected <= set(config) or set(config) - expected - optional or type(config['memory_mb']) is not int or not 512 <= config['memory_mb'] <= 4096:
            raise Unavailable('invalid VM runtime configuration')
        if not isinstance(config['modules'], list) or not 1 <= len(config['modules']) <= 32:
            raise Unavailable('invalid VM kernel modules')
        for key in optional & set(config):
            # Dependency trees are untrusted guest inputs, not host executables.
            # Cargo commonly creates them group-writable; export them read-only.
            if not isinstance(config[key], str):
                raise Unavailable('invalid read-only dependency directory')
            candidate = Path(config[key])
            if not candidate.is_absolute() or not candidate.is_dir():
                raise Unavailable('invalid read-only dependency directory')
            if key == 'ort_cache' and candidate.resolve() != candidate:
                raise Unavailable('ONNX dependency cache must be a literal directory')
            if key == 'toolchain':
                info = candidate.stat()
                if info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
                    raise Unavailable('VM executable toolchain is not trusted')
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


GUEST_RUNNER = dependency_proxy.GUEST_PROXY + r'''
import ctypes,json,os,re,selectors,signal,subprocess
from pathlib import Path
job=json.loads(Path('/job.json').read_text())
ci_cache={}
if job.get('ci_context'):
    exec(compile(Path('/ci-tools/cache-generation.py').read_bytes(),'/ci-tools/cache-generation.py','exec'),ci_cache)
uid=job['uid'];gid=job['gid']
os.makedirs('/home/worker',mode=0o700,exist_ok=True)
os.chown('/home/worker',uid,gid)
os.chown('/cargo',uid,gid)
Path('/etc/passwd').write_text(f'root:x:0:0:root:/root:/bin/sh\nworker:x:{uid}:{gid}:worker:/home/worker:/bin/sh\n')
Path('/etc/group').write_text(f'root:x:0:\nworker:x:{gid}:\n')
Path('/etc/hosts').write_text('127.0.0.1 localhost registry.npmjs.org index.crates.io static.crates.io\n::1 localhost\n')
environment={'PATH':'/toolchain/bin:/usr/bin:/bin','HOME':'/home/worker','USER':'worker','LOGNAME':'worker',
    'LANG':'C.UTF-8','TMPDIR':'/tmp','CARGO_HOME':'/cargo','CARGO_NET_OFFLINE':'true','CARGO_TARGET_DIR':'/workspace/target'}
def seed_build_cache(ci_target=None):
    # Session build cache (#1036): persistent target and Cargo home. Seed the
    # Cargo home once per image from the read-only operator caches.
    import shutil
    cache=Path('/build-cache')
    if ci_target is None:
        for name in ('target','cargo-home'):
            (cache/name).mkdir(exist_ok=True); os.chown(cache/name,uid,gid)
        home=cache/'cargo-home'
    else:
        home=ci_target.parent/'cargo-home'
        home.mkdir(exist_ok=True);os.chown(home,uid,gid)
        environment.update({'CARGO_HOME':str(home),'CARGO_TARGET_DIR':'/workspace/target'})
    if not (home/'.jarvis-seeded').exists():
        for source,destination in (('/cargo/registry/cache','registry/cache'),('/cargo/registry/index','registry/index'),('/cargo/git','git')):
            if Path(source).is_dir() and any(Path(source).iterdir()):
                shutil.copytree(source,home/destination,symlinks=True,dirs_exist_ok=True)
        for base,dirs,files in os.walk(home):
            for name in [*dirs,*files]:
                os.lchown(os.path.join(base,name),uid,gid)
        (home/'.jarvis-seeded').touch(); os.chown(home/'.jarvis-seeded',uid,gid)
if job.get('build_cache'):
    environment.update({'CARGO_HOME':'/build-cache/cargo-home','CARGO_TARGET_DIR':'/build-cache/target'})
environment.update(job['environment'])
if job.get('ort_cache'):
    # ort-sys uses its target/distribution hash beneath this cache. Export only
    # the explicit operator dependency tree; never the host's cache or HOME.
    environment['ORT_CACHE_DIR']='/ort-cache'
if job.get('ci_context'):
    environment.update({'GIT_ALTERNATE_OBJECT_DIRECTORIES':'/ci-git-objects','GIT_CONFIG_NOSYSTEM':'1','RUSTUP_TOOLCHAIN':'/toolchain','CARGO_NET_OFFLINE':'false',
        'CARGO_BUILD_JOBS':'1','CARGO_PROFILE_DEV_DEBUG':'0','CARGO_PROFILE_TEST_DEBUG':'0'})
registry=make_proxy('/root/control')
environment.update({'NODE_EXTRA_CA_CERTS':'/etc/jarvis-registry-ca.pem',
    'CARGO_HTTP_CAINFO':'/etc/jarvis-registry-ca.pem', 'NPM_CONFIG_AUDIT':'false',
    'NPM_CONFIG_UPDATE_NOTIFIER':'false'})
# Use the matching, read-only system headers for native addons. No package code
# runs here: this probes only the trusted runtime's Node executable and headers.
headers=Path('/usr/include/node/node_version.h')
if headers.is_file() and Path('/usr/bin/node').is_file():
    try:
        text=headers.read_text()
        version='v'+'.'.join(re.search(r'^#define NODE_'+part+r'_VERSION\s+(\d+)',text,re.M).group(1)
            for part in ('MAJOR','MINOR','PATCH'))
        installed=subprocess.check_output(['/usr/bin/node','--version'],env={'PATH':'/usr/bin:/bin'},
            stderr=subprocess.DEVNULL,timeout=5,text=True).strip()
        if version==installed:
            environment.update({'npm_config_nodedir':'/usr','npm_config_build_from_source':'true'})
    except (OSError,AttributeError,subprocess.SubprocessError):
        pass
libc=ctypes.CDLL(None)
def worker():
    if libc.prctl(38,1,0,0,0)!=0: raise OSError('cannot enforce no-new-privileges')
    os.setgroups([]); os.setgid(gid); os.setuid(uid)
    os.umask(0o022)
try:
    if job.get('build_cache'):
        if job.get('ci_context'):
            if os.geteuid()!=0:raise OSError('CI cache preparation requires the guest controller')
            generation=job['ci_generation']
            ci_target=ci_cache['prepare']('/build-cache',generation['binding'],generation['command'],uid,gid,
                predecessor=generation['predecessor'])
            subprocess.run(['/usr/bin/mount','--bind',str(ci_target),'/workspace/target'],check=True)
            seed_build_cache(ci_target)
        else:
            seed_build_cache()
    process=subprocess.Popen(job['argv'],cwd='/workspace',env=environment,
        stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        start_new_session=True,preexec_fn=worker)
    threading.Thread(target=registry.serve_forever,daemon=True).start()
    outputs={'stdout':bytearray(),'stderr':bytearray()}
    size=0; exceeded=False
    with selectors.DefaultSelector() as streams:
        streams.register(process.stdout,selectors.EVENT_READ,'stdout')
        streams.register(process.stderr,selectors.EVENT_READ,'stderr')
        while streams.get_map() and not exceeded:
            for key,_ in streams.select():
                chunk=os.read(key.fileobj.fileno(),65536)
                if not chunk:
                    streams.unregister(key.fileobj); key.fileobj.close(); continue
                size+=len(chunk)
                if size>8*1024*1024:
                    exceeded=True; break
                outputs[key.data].extend(chunk)
    if exceeded:
        # The entire guest is shut down after this receipt, including detached
        # descendants. Kill the foreground group promptly to stop its output.
        try: os.killpg(process.pid,signal.SIGKILL)
        except ProcessLookupError: pass
        process.wait()
        result={'error':'VM command exceeded output limit'}
    else:
        result={'exit_code':process.wait(),**{key:value.decode('utf-8','replace') for key,value in outputs.items()}}
        if len(result['stdout'].encode())+len(result['stderr'].encode())>8*1024*1024:
            result={'error':'VM command exceeded output limit'}
except Exception as error:
    import errno
    # copytree aggregates per-file failures into shutil.Error without an errno.
    if job.get('ci_context') and isinstance(error,ci_cache['FreshnessHold']):
        result={'error':'CI source/cache freshness unavailable','freshness_hold':error.diagnostic()}
    else:
        full=getattr(error,'errno',None) in (errno.ENOSPC,errno.EDQUOT) or 'No space left on device' in str(error)
        result={'error':'VM build cache is full' if full else 'VM command could not start'}
path=Path('/root/control/outcome.json')
with path.open('w') as receipt:
    os.chmod(path,0o600);json.dump(result,receipt);receipt.flush();os.fsync(receipt.fileno())

'''


# #1036: a session build cache is a raw ext4 image attached as a virtio disk
# and mounted here. It holds the Cargo target directory and Cargo home. It is
# not a 9p share: the guest's 9p client stores timestamps to the second, which
# makes Cargo rebuild every dependency. The host never mounts the image.
BUILD_CACHE_MOUNT = '/build-cache'


def qemu_command(config, image, shares, build_cache):
    """The qemu argv for one build VM."""
    command = [config['qemu'], '-no-user-config', '-nodefaults', '-machine', 'pc,accel=kvm',
        '-cpu', 'host', '-m', str(config['memory_mb']), '-smp', '2', '-nographic', '-serial', 'stdio',
        '-monitor', 'none', '-nic', 'none', '-no-reboot', '-L', config['data_dir'], '-bios', config['firmware'],
        '-kernel', config['kernel'], '-initrd', str(image), '-append', 'rdinit=/init console=ttyS0 panic=-1 loglevel=3',
        '-sandbox', 'on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny']
    if build_cache is not None:
        escaped = str(build_cache).replace(',', ',,')
        # werror/rerror=report: a host ENOSPC or I/O error fails the guest's
        # write. QEMU's default (werror=enospc) pauses the VM, and with no
        # monitor nothing resumes it, so the build would hang to its deadline.
        command.extend(['-drive', f'if=none,id=buildcache,format=raw,discard=unmap,werror=report,rerror=report,file={escaped}',
            '-device', 'virtio-blk-pci,drive=buildcache'])
    for tag, path, readonly in shares:
        escaped = str(path).replace(',', ',,')
        command.extend(['-fsdev', f'local,id={tag},path={escaped},security_model=none,readonly={"on" if readonly else "off"}',
            '-device', f'virtio-9p-pci,fsdev={tag},mount_tag={tag}'])
    return command


def validate_snapshot_links(workspace):
    # Native package builds may link their own artifacts inside the disposable
    # snapshot. Account for every alias by inode; an unaccounted link may expose
    # a host file and must refuse. Directory symlinks never expand the census.
    groups = {}
    for base, dirs, files in os.walk(workspace, followlinks=False):
        for name in files:
            path = Path(base) / name
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode):
                continue
            key = (info.st_dev, info.st_ino)
            expected, paths = groups.setdefault(key, (info.st_nlink, []))
            if info.st_nlink != expected:
                raise Unavailable('VM snapshot hard links changed during validation')
            paths.append(path)
    for identity, (expected, paths) in groups.items():
        if len(paths) != expected:
            raise Unavailable('VM snapshot must not contain host hard links')
        for path in paths:
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode) or (info.st_dev, info.st_ino, info.st_nlink) != (*identity, expected):
                raise Unavailable('VM snapshot hard links changed during validation')


def run(runtime, workspace, argv, environment, timeout=120, node_modules=None, node_workspaces=(), download_info=None,
        scratch_dir=None, build_cache=None, storage_lease=None, ci_context=None):
    """Execute argv in a disposable source snapshot; return after verified VM exit.

    `scratch_dir` holds the VM's private control files (default temp dir when
    omitted). `build_cache` is an ext4 image, owned by the caller, that outlives
    this command: its target directory and Cargo home make later builds
    incremental.
    """
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
    if scratch_dir is not None:
        scratch = Path(scratch_dir)
        if not scratch.is_absolute() or scratch.is_symlink() or not scratch.is_dir():
            raise Unavailable('VM scratch directory must be an absolute directory')
    if build_cache is not None:
        cache = Path(build_cache)
        try:
            info = cache.lstat()
        except OSError:
            info = None
        if (not cache.is_absolute() or info is None or not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
                or info.st_uid != os.getuid() or info.st_mode & 0o077):
            raise Unavailable('invalid build cache image')
    validate_snapshot_links(workspace)
    node_mounts = list(node_workspaces)
    if node_modules is not None:
        node_mounts.append(('node_modules', node_modules))
    if len(node_mounts) > 16:
        raise Unavailable('too many npm workspace dependency roots')
    seen = set()
    for relative, source in node_mounts:
        relative, source = Path(relative), Path(source)
        if (relative.is_absolute() or '..' in relative.parts or relative.name != 'node_modules'
                or 'node_modules' in relative.parts[:-1] or relative in seen):
            raise Unavailable('invalid npm dependency mount destination')
        seen.add(relative)
        for index in range(1, len(relative.parts) + 1):
            target = workspace.joinpath(*relative.parts[:index])
            if target.is_symlink() or (target.exists() and not target.is_dir()):
                raise Unavailable('npm dependency mount traverses a non-directory or symlink')
        if not source.is_absolute() or source.resolve() != source or not source.is_dir():
            raise Unavailable('invalid read-only npm dependency directory')
    if ci_context is not None:
        if set(ci_context)!={'state','git_objects','runner','contract','cache_program','generation'}:
            raise Unavailable('invalid controller CI context')
        for key in ('state','git_objects'):
            path=Path(ci_context[key]);info=path.lstat()
            if (not path.is_absolute() or path.resolve()!=path or not stat.S_ISDIR(info.st_mode)
                    or info.st_uid!=os.getuid() or info.st_mode&0o002 or (key=='state' and info.st_mode&0o022) or path.is_relative_to(workspace)
                    or workspace.is_relative_to(path)):
                raise Unavailable('CI state and Git objects require separate owned directories')
            if key=='state' and info.st_mode&0o077:raise Unavailable('CI state is not private')
        for key in ('runner','contract','cache_program'):
            if not isinstance(ci_context[key],bytes) or not 0<len(ci_context[key])<=2*1024*1024:
                raise Unavailable('invalid controller CI bundle')
    config = runtime.config
    with tempfile.TemporaryDirectory(prefix='jarvis-build-vm-', dir=scratch_dir) as temporary:
        private = Path(temporary)
        control = private / 'control'; control.mkdir(mode=0o700)
        openssl = shutil.which('openssl', path=os.defpath)
        if not openssl:
            raise Unavailable('registry gateway requires the trusted OpenSSL executable')
        info = Path(openssl).stat()
        if info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
            raise Unavailable('registry gateway OpenSSL executable is untrusted')
        certificate, key = private / 'registry-ca.pem', private / 'registry-key.pem'
        try:
            subprocess.run([openssl, 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                '-keyout', str(key), '-out', str(certificate), '-days', '1',
                '-subj', '/CN=Jarvis build registry gateway', '-addext',
                'subjectAltName=DNS:registry.npmjs.org,DNS:index.crates.io,DNS:static.crates.io'],
                env={'PATH': os.defpath}, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, timeout=10, check=True)
        except (OSError, subprocess.SubprocessError) as error:
            raise Unavailable('registry gateway certificate setup failed') from error
        entries = []
        for name in ('bootstrap', 'dev', 'proc', 'sys', 'tmp', 'modules', 'usr', 'workspace', 'etc', 'home',
                     'cargo', 'cargo/registry', 'cargo/git', 'toolchain', 'ort-cache', 'etc/alternatives'):
            entries.append((name, stat.S_IFDIR | (0o1777 if name == 'tmp' else 0o755), b'', 0, 0))
        entries.extend([('root', stat.S_IFDIR | 0o700, b'', 0, 0), ('root/control', stat.S_IFDIR | 0o700, b'', 0, 0)])
        entries.extend([('etc/jarvis-registry-ca.pem', stat.S_IFREG | 0o444, certificate.read_bytes(), 0, 0),
                        ('root/registry-key.pem', stat.S_IFREG | 0o400, key.read_bytes(), 0, 0)])
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
        shares = [('runtime', Path('/usr'), True), ('workspace', workspace, ci_context is not None), ('control', control, False)]
        if ci_context is not None:
            entries.extend((name,stat.S_IFDIR|0o755,b'',0,0) for name in ('ci-runtime','ci-git-objects','ci-tools'))
            entries.extend([('ci-tools/runner.py',stat.S_IFREG|0o444,ci_context['runner'],0,0),
                            ('ci-tools/contract.json',stat.S_IFREG|0o444,ci_context['contract'],0,0),
                            ('ci-tools/cache-generation.py',stat.S_IFREG|0o444,ci_context['cache_program'],0,0)])
            shares.extend([('ci_runtime',Path(ci_context['state']),False),('ci_objects',Path(ci_context['git_objects']),True)])
            dependency_mounts.extend([
                'mount -t 9p -o trans=virtio,version=9p2000.L,nosuid,nodev ci_runtime /ci-runtime || poweroff -f',
                'mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev ci_objects /ci-git-objects || poweroff -f'])
        if build_cache is not None:
            dependency_mounts.append(f'/bootstrap/busybox mkdir -p {BUILD_CACHE_MOUNT} || poweroff -f')
            dependency_mounts.append(f'mount -t ext4 -o nosuid,nodev,discard /dev/vda {BUILD_CACHE_MOUNT} || poweroff -f')
        for key, destination in [('toolchain', '/toolchain'), ('registry', '/cargo/registry'), ('cargo_git', '/cargo/git'), ('ort_cache', '/ort-cache')]:
            if key in config:
                shares.append((key, Path(config[key]), True))
                dependency_mounts.append(f'mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev {key} {destination} || poweroff -f')
        for index, (relative, source) in enumerate(node_mounts):
            tag = f'node_{index}'
            destination = shlex.quote(str(Path('/workspace') / relative))
            shares.append((tag, Path(source), True))
            dependency_mounts.append(f'/bootstrap/busybox mkdir -p {destination} || poweroff -f')
            dependency_mounts.append(f'mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev {tag} {destination} || poweroff -f')
        boot = '''#!/bootstrap/sh
export PATH=/bootstrap
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
ip link set lo up
''' + '\n'.join(module_commands) + '''
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f
mount -t 9p -o trans=virtio,version=9p2000.L,''' + ('ro,' if ci_context is not None else '') + '''nosuid,nodev workspace /workspace || poweroff -f
mount -t 9p -o trans=virtio,version=9p2000.L,nosuid,nodev,noexec control /root/control || poweroff -f
''' + '\n'.join(dependency_mounts) + '''
/usr/bin/python3 -I /guest.py
/bootstrap/busybox sync
/bootstrap/busybox umount /build-cache 2>/dev/null
poweroff -f
'''
        entries.extend([('init', stat.S_IFREG | 0o755, boot.encode(), 0, 0),
            ('guest.py', stat.S_IFREG | 0o400, GUEST_RUNNER.encode(), 0, 0),
            ('job.json', stat.S_IFREG | 0o400, json.dumps({'argv': argv, 'environment': environment,
                'uid': os.getuid(), 'gid': os.getgid(), 'build_cache': build_cache is not None,'ci_context':ci_context is not None,
                'ci_generation':ci_context['generation'] if ci_context is not None else None,
                'ort_cache':'ort_cache' in config}).encode(), 0, 0)])
        image = private / 'initrd.gz'; image.write_bytes(initrd(entries))
        command = qemu_command(config, image, shares, build_cache)
        host_environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'], 'QEMU_MODULE_DIR': config['module_dir']}
        supervisor = Path(__file__).with_name('provider-supervisor.py')
        cleanup = private / 'cleanup-complete'
        broker = dependency_proxy.Broker(control)
        if storage_lease is not None:
            storage_lease.check()
            host_environment["JARVIS_STORAGE_LEASE_FD"] = str(storage_lease.fd)
            host_environment["JARVIS_STORAGE_LEASE_FDS"] = ','.join(str(fd) for fd in getattr(storage_lease,'fds',[storage_lease.fd]))
        with tempfile.TemporaryFile(dir=private) as log:
            process = subprocess.Popen([sys.executable, '-I', str(supervisor), str(cleanup), *command],
                env=host_environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True,
                pass_fds=() if storage_lease is None else tuple(getattr(storage_lease,'fds',[storage_lease.fd])))
            try:
                deadline = None if timeout is None else time.monotonic() + min(max(timeout, 1), 900)
                while True:
                    if storage_lease is not None:
                        storage_lease.check()
                    remaining = 1 if deadline is None else deadline - time.monotonic()
                    if remaining <= 0:
                        raise Unavailable('VM command timed out')
                    broker.poll(remaining)
                    try:
                        status = process.wait(timeout=min(0.05, max(0.001, remaining)))
                        break
                    except subprocess.TimeoutExpired:
                        continue
                if status or not cleanup.is_file() or cleanup.read_text() != 'all-descendants-reaped\n':
                    raise Unavailable('VM execution or cleanup failed')
                try:
                    result = private_json(control / 'outcome.json')
                except (OSError, ValueError) as exc:
                    raise Unavailable('VM did not produce a trusted command result') from exc
                if 'error' in result:
                    if ci_context is not None and set(result)=={'error','freshness_hold'}:
                        # The CI adapter validates this bounded diagnostic.
                        # Controller bundle programs execute only in the guest.
                        raise FreshnessUnavailable(result['freshness_hold'])
                    raise Unavailable(result['error'])
                if download_info is not None:
                    download_info.update(requests=broker.requests, bytes=broker.bytes)
                if set(result) != {'exit_code', 'stdout', 'stderr'} or type(result['exit_code']) is not int:
                    raise Unavailable('invalid VM result')
                return result
            finally:
                if process.poll() is None:
                    process.terminate()
                    if storage_lease is not None:
                        # The subreaper owns detached descendants. Killing it
                        # on an arbitrary cleanup timeout could release storage
                        # while QEMU still writes; retain admission until reaped.
                        process.wait()
                    else:
                        try:
                            process.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid, signal.SIGKILL)
                            process.wait()
