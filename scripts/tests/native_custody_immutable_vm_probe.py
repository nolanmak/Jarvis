"""Disposable guest-ext4 probe; run under managed disk build, no host root.

Accepts no paths/options. The host mounts nothing and production native state is
never selected. Root capability exists only inside the no-NIC test VM.
"""
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile


def main():
    if len(sys.argv) != 1 or os.getuid() == 0:
        raise RuntimeError('Use an unprivileged host and no paths/options')
    descriptors = tuple(int(value) for value in os.environ.get('JARVIS_STORAGE_LEASE_FDS', '').split(',') if value)
    if not descriptors or len(descriptors) > 64 or str(descriptors[0]) != os.environ.get('JARVIS_STORAGE_LEASE_FD') or any(fd < 3 for fd in descriptors):
        raise RuntimeError('Actual inherited managed storage descriptors required')
    for fd in descriptors:
        os.fstat(fd)
    lock = Path(os.readlink(f'/proc/self/fd/{descriptors[0]}'))
    if lock.name != 'disk-build.lock':
        raise RuntimeError('A normal managed build lease is required')
    with lock.open('rb') as independent:
        try:
            fcntl.flock(independent, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            raise RuntimeError('Managed build exclusion is not held')
    target = Path(os.environ['CARGO_TARGET_DIR']).resolve(strict=True)
    identity = target.stat()
    target_key = f'inode-{identity.st_dev}-{identity.st_ino}.lock'
    target_lock = next((Path(os.readlink(f'/proc/self/fd/{fd}')) for fd in descriptors
                        if Path(os.readlink(f'/proc/self/fd/{fd}')).name == target_key), None)
    if target_lock is None:
        raise RuntimeError('Inherited descriptors do not hold the actual target inode lock')
    with target_lock.open('rb') as independent:
        try:
            fcntl.flock(independent, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            raise RuntimeError('Physical target exclusion is not held')
    scratch = Path(os.environ['TMPDIR']).resolve(strict=True)
    if scratch.parent != target or scratch.name != 'tmp':
        raise RuntimeError('Managed target scratch required')
    scripts = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location('custody_probe_vm', scripts / 'codex-build-vm.py')
    vm = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(vm)
    runtime = vm.Runtime.load(Path.home() / '.local/share/augmentagent/build-vm/runtime.json')
    config = dict(runtime.config)
    config['memory_mb'] = 512
    fixture = Path(__file__).with_name('native_custody_immutable_probe.py').read_bytes()
    kernel_hash = hashlib.sha256(Path(config['kernel']).read_bytes()).hexdigest()
    temporary = tempfile.TemporaryDirectory(prefix='native-custody-kernel-', dir=scratch)
    retain = False
    try:
        root = Path(temporary.name)
        image = root / 'fixture.ext4'
        with image.open('xb') as output:
            output.truncate(64 * 1024 * 1024)
        subprocess.run(['/usr/sbin/mkfs.ext4', '-q', '-F', '-E', 'lazy_itable_init=0,lazy_journal_init=0', str(image)], check=True, timeout=30, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        entries = [(name, stat.S_IFDIR | (0o1777 if name == 'tmp' else 0o755), b'', 0, 0) for name in ('bootstrap', 'dev', 'proc', 'sys', 'usr', 'tmp', 'modules', 'fixture')]
        entries.append(('dev/console', stat.S_IFCHR | 0o600, b'', 5, 1))
        entries.append(('bootstrap/busybox', stat.S_IFREG | 0o755, Path(config['busybox']).read_bytes(), 0, 0))
        for name in ('sh', 'mount', 'insmod', 'poweroff'):
            entries.append(('bootstrap/' + name, stat.S_IFLNK | 0o777, b'busybox', 0, 0))
        for name in ('bin', 'sbin', 'lib', 'lib64'):
            entries.append((name, stat.S_IFLNK | 0o777, ('usr/' + name).encode(), 0, 0))
        module_commands = []
        for index, module in enumerate(config['modules']):
            name = f'modules/{index}.ko'
            entries.append((name, stat.S_IFREG | 0o600, Path(module).read_bytes(), 0, 0))
            module_commands.append(f'insmod /{name} || poweroff -f')
        prefix = b"""import os,json,atexit
os.environ['TMPDIR']='/fixture'
print('\\nJARVIS_GUEST_KERNEL:'+json.dumps({'kind':'guest_kernel','release':os.uname().release,'uid':os.geteuid()}),flush=True)
def prefaulted_mapping_case():
    original_mmap=mmap.mmap
    def warmed_mapping(*args,**kwargs):
        value=original_mmap(*args,**kwargs)
        value[0]=value[0]
        return value
    mmap.mmap=warmed_mapping
    try:
        observation=run_case('existing-mapping')
        observation['case']='prefaulted-shared-mapping'
        print('\\n'+json.dumps({'kind':'prefaulted_mapping','observation':observation}),flush=True)
    finally:
        mmap.mmap=original_mmap
atexit.register(prefaulted_mapping_case)
"""
        entries.append(('probe.py', stat.S_IFREG | 0o400, prefix + fixture, 0, 0))
        boot = '#!/bootstrap/sh\nexport PATH=/bootstrap\nmount -t devtmpfs devtmpfs /dev\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\n'
        boot += '\n'.join(module_commands) + '\n'
        boot += 'mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f\n'
        boot += 'mount -t ext4 -o nosuid,nodev /dev/vda /fixture || poweroff -f\n'
        boot += '/usr/bin/python3 -I /probe.py\n/bootstrap/busybox sync\n/bootstrap/busybox umount /fixture\npoweroff -f\n'
        entries.append(('init', stat.S_IFREG | 0o755, boot.encode(), 0, 0))
        initrd = root / 'initrd.gz'
        initrd.write_bytes(vm.initrd(entries))
        command = vm.qemu_command(config, initrd, [('runtime', Path('/usr'), True)], image)
        cleanup = root / 'cleanup-complete'
        environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'], 'QEMU_MODULE_DIR': config['module_dir'],
                       'JARVIS_STORAGE_LEASE_FD': str(descriptors[0]), 'JARVIS_STORAGE_LEASE_FDS': ','.join(map(str, descriptors)), 'JARVIS_SUPERVISOR_PARENT_PID': str(os.getpid())}
        with tempfile.TemporaryFile(dir=root) as log:
            retain = True
            process = subprocess.Popen([sys.executable, '-I', str(scripts / 'provider-supervisor.py'), str(cleanup), *command], env=environment,
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True, pass_fds=descriptors)
            try:
                status = process.wait(timeout=90)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    status = process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    retain = True
                    temporary._finalizer.detach()
                    raise RuntimeError(f'Original supervisor {process.pid} still live; scratch retained at {root}')
            except BaseException:
                process.terminate()
                try:
                    process.wait(timeout=15)
                finally:
                    if process.poll() is None or not cleanup.is_file() or cleanup.read_text() != 'all-descendants-reaped\n':
                        retain = True
                        temporary._finalizer.detach()
                raise
            if not cleanup.is_file() or cleanup.read_text() != 'all-descendants-reaped\n':
                retain = True
                temporary._finalizer.detach()
                raise RuntimeError(f'Cleanup unverified; original scratch retained at {root}')
            retain = False
            log.seek(0)
            output = log.read(8 * 1024 * 1024 + 1)
            if len(output) > 8 * 1024 * 1024:
                raise RuntimeError('VM probe output exceeds bound')
        records = []
        for line in output.decode('utf-8', 'replace').splitlines():
            if 'JARVIS_GUEST_KERNEL:' in line:
                line = line.split('JARVIS_GUEST_KERNEL:', 1)[1]
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if isinstance(value, dict):
                records.append(value)
        guest = next((row for row in records if row.get('kind') == 'guest_kernel'), None)
        observation = next((row for row in records if row.get('status') == 'observed'), None)
        prefaulted = next((row['observation'] for row in records if row.get('kind') == 'prefaulted_mapping'), None)
        complete = status == 0 and observation and guest and guest.get('uid') == 0 and prefaulted and prefaulted.get('supported')
        result = {'status': 'observed' if complete else 'guest-probe-failed', 'guest': guest,
                  'kernel_image_sha256': kernel_hash, 'fixture_sha256': hashlib.sha256(fixture).hexdigest(),
                  'observation': observation, 'prefaulted_mapping': prefaulted, 'supervisor_exit': status, 'all_descendants_reaped': True,
                  'host_root_used': False, 'host_kernel_verified': False, 'production_state_changed': False, 'grants_capacity': False}
        if not complete:
            result['diagnostic_tail'] = output.decode('utf-8', 'replace')[-4000:]
        print(json.dumps(result, sort_keys=True))
        return 0 if result['status'] == 'observed' and all(row['supported'] for row in observation['observations']) else 3
    finally:
        if retain:
            temporary._finalizer.detach()
        else:
            temporary.cleanup()


if __name__ == '__main__':
    raise SystemExit(main())
