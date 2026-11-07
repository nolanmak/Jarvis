"""Four disposable ext4 crash/reboot experiments under real managed leases.

No paths/options, host privilege, network or production native-state access.
Each fresh image is booted twice; only original verified cleanup permits reuse
or deletion. This is a test oracle, never custody or capacity authority.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import select
import stat
import subprocess
import sys
import tempfile
import time


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def records(data):
    result = []
    for line in data.decode('utf-8', 'replace').splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if isinstance(row, dict):
            result.append(row)
    return result


def run_boot(scripts, config, command, directory, descriptors, nonce, boundary, crash):
    directory.mkdir()
    receipt = directory / 'cleanup'
    environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'],
                   'QEMU_MODULE_DIR': config['module_dir'],
                   'JARVIS_STORAGE_LEASE_FD': str(descriptors[0]),
                   'JARVIS_STORAGE_LEASE_FDS': ','.join(map(str, descriptors)),
                   'JARVIS_SUPERVISOR_PARENT_PID': str(os.getpid())}
    ready, requested, qemu_fd = None, False, None
    with (directory / 'console.log').open('w+b') as log:
        process = subprocess.Popen([sys.executable, '-I', str(scripts / 'provider-supervisor.py'), str(receipt), *command],
                                   env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                   start_new_session=True, pass_fds=descriptors)
        try:
            deadline = time.monotonic() + 90
            while process.poll() is None:
                if time.monotonic() >= deadline:
                    raise TimeoutError('original guest supervisor deadline')
                data = os.pread(log.fileno(), 8 * 1024 * 1024 + 1, 0)
                if len(data) > 8 * 1024 * 1024:
                    raise RuntimeError('guest output exceeds bound')
                if crash and not requested:
                    ready = next((row for row in records(data)
                                  if row.get('kind') == 'persistence_ready' and row.get('nonce') == nonce
                                  and row.get('boundary') == boundary and row.get('fsync_complete') is True), None)
                    if ready is not None:
                        children = Path(f'/proc/{process.pid}/task/{process.pid}/children').read_text().split()
                        if len(children) != 1:
                            raise RuntimeError('original QEMU child identity unavailable')
                        qemu_pid = int(children[0])
                        qemu_fd = os.pidfd_open(qemu_pid)
                        actual = Path(f'/proc/{qemu_pid}/exe').stat()
                        expected = Path(config['qemu']).stat()
                        if ((actual.st_dev, actual.st_ino) != (expected.st_dev, expected.st_ino)
                                or select.select([qemu_fd], [], [], 0)[0]):
                            raise RuntimeError('original QEMU is not live')
                        # The existing original supervisor handles TERM by
                        # SIGKILLing its owned QEMU tree, without guest shutdown.
                        # Keep observing this same handle through final cleanup.
                        process.terminate()
                        requested = True
                time.sleep(0.01)
            status = process.wait()
        except BaseException:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=15)
            raise
        finally:
            if qemu_fd is not None:
                qemu_exited = bool(select.select([qemu_fd], [], [], 0)[0])
                os.close(qemu_fd)
            else:
                qemu_exited = False
        if not receipt.is_file() or receipt.read_text() != 'all-descendants-reaped\n':
            raise RuntimeError('original guest cleanup unverified')
        data = os.pread(log.fileno(), 8 * 1024 * 1024 + 1, 0)
        if len(data) > 8 * 1024 * 1024:
            raise RuntimeError('guest output exceeds bound')
    result = {'supervisor_exit': status, 'all_descendants_reaped': True,
              'crash_requested_after_ready': requested, 'qemu_pidfd_observed_exit': qemu_exited,
              'records': records(data)}
    expected_status = 143 if crash else 0
    recovered = any(row.get('kind') == 'persistence_recovered' and row.get('nonce') == nonce
                    and row.get('boundary') == boundary for row in result['records'])
    if (status != expected_status or crash and (not requested or not qemu_exited)
            or not crash and not recovered):
        result['diagnostic_tail'] = data.decode('utf-8', 'replace')[-4000:]
    return result


def boot_entries(config, fixture, inventory, nonce, phase, boundary):
    entries = [(name, stat.S_IFDIR | (0o1777 if name == 'tmp' else 0o755), b'', 0, 0)
               for name in ('bootstrap', 'dev', 'proc', 'sys', 'usr', 'tmp', 'modules', 'fixture')]
    entries.append(('dev/console', stat.S_IFCHR | 0o600, b'', 5, 1))
    entries.append(('bootstrap/busybox', stat.S_IFREG | 0o755, Path(config['busybox']).read_bytes(), 0, 0))
    for name in ('sh', 'mount', 'insmod', 'poweroff'):
        entries.append(('bootstrap/' + name, stat.S_IFLNK | 0o777, b'busybox', 0, 0))
    for name in ('bin', 'sbin', 'lib', 'lib64'):
        entries.append((name, stat.S_IFLNK | 0o777, ('usr/' + name).encode(), 0, 0))
    modules = []
    for index, module in enumerate(config['modules']):
        name = f'modules/{index}.ko'
        entries.append((name, stat.S_IFREG | 0o600, Path(module).read_bytes(), 0, 0))
        modules.append(f'insmod /{name} || poweroff -f')
    entries.extend([('persistence-probe.py', stat.S_IFREG | 0o400, fixture, 0, 0),
                    ('inventory-probe.py', stat.S_IFREG | 0o400, inventory, 0, 0),
                    ('nonce', stat.S_IFREG | 0o400, nonce.encode(), 0, 0),
                    ('phase', stat.S_IFREG | 0o400, json.dumps({'phase': phase, 'boundary': boundary}).encode(), 0, 0)])
    boot = '#!/bootstrap/sh\nexport PATH=/bootstrap\nmount -t devtmpfs devtmpfs /dev\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\n'
    boot += '\n'.join(modules) + '\n'
    boot += 'mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f\n'
    boot += 'mount -t ext4 -o nosuid,nodev /dev/vda /fixture || poweroff -f\n'
    boot += 'JARVIS_CUSTODY_DISPOSABLE_GUEST=1 /usr/bin/python3 -I /persistence-probe.py\n/bootstrap/busybox sync\n/bootstrap/busybox umount /fixture\npoweroff -f\n'
    entries.append(('init', stat.S_IFREG | 0o755, boot.encode(), 0, 0))
    return entries


def main():
    if len(sys.argv) != 1 or os.getuid() == 0:
        raise RuntimeError('Requires unprivileged host and no paths/options')
    scripts = Path(__file__).resolve().parents[1]
    storage = load('persistence_managed_storage', Path(__file__).with_name('native_custody_immutable_vm_probe.py'))
    descriptors, _target, scratch = storage.managed_storage()
    vm = load('persistence_vm_runtime', scripts / 'codex-build-vm.py')
    runtime = vm.Runtime.load(Path.home() / '.local/share/augmentagent/build-vm/runtime.json')
    config = dict(runtime.config, memory_mb=512)
    fixture = Path(__file__).with_name('native_custody_persistence_guest_probe.py').read_bytes()
    inventory = Path(__file__).with_name('native_custody_inventory_guest_probe.py').read_bytes()
    temporary = tempfile.TemporaryDirectory(prefix='custody-persistence-', dir=scratch)
    retain = False
    results = []
    try:
        root = Path(temporary.name)
        for index, boundary in enumerate(('captured', 'directory-fenced', 'file-fenced:lock', 'complete')):
            directory = root / str(index)
            directory.mkdir()
            image = directory / 'fixture.ext4'
            with image.open('xb') as output:
                output.truncate(64 * 1024 * 1024)
            image_identity = image.stat()
            subprocess.run(['/usr/sbin/mkfs.ext4', '-q', '-F', '-E', 'lazy_itable_init=0,lazy_journal_init=0', str(image)],
                           check=True, timeout=30, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            nonce = secrets.token_hex(16)
            boots = []
            for phase in ('prepare', 'recover'):
                initrd = directory / f'{phase}.gz'
                initrd.write_bytes(vm.initrd(boot_entries(config, fixture, inventory, nonce, phase, boundary)))
                command = vm.qemu_command(config, initrd, [('runtime', Path('/usr'), True)], image)
                retain = True
                result = run_boot(scripts, config, command, directory / phase, descriptors, nonce, boundary, phase == 'prepare')
                current_image = image.stat()
                if (current_image.st_dev, current_image.st_ino) != (image_identity.st_dev, image_identity.st_ino):
                    raise RuntimeError('original fixture image identity changed')
                retain = False  # Original cleanup and original image both verified.
                boots.append(result)
                if phase == 'prepare' and (result['supervisor_exit'] != 143 or not result['crash_requested_after_ready']
                                          or not result['qemu_pidfd_observed_exit']):
                    break
            recovery = next((row for row in boots[-1]['records'] if row.get('kind') == 'persistence_recovered'
                             and row.get('nonce') == nonce and row.get('boundary') == boundary), None) if len(boots) == 2 else None
            complete = bool(recovery and boots[-1]['supervisor_exit'] == 0
                            and recovery.get('original_identity_content_preserved') is True
                            and recovery.get('all_fixture_flags_set') is (boundary == 'complete')
                            and recovery.get('partial_or_unfenced') is (boundary != 'complete')
                            and recovery.get('grants_capacity') is False)
            results.append({'boundary': boundary, 'success': complete, 'same_image_inode_verified': True, 'boots': boots})
            if not complete:
                break
        success = len(results) == 4 and all(row['success'] for row in results)
        report = {'status': 'observed' if success else 'persistence-probe-failed', 'cases': results,
                  'kernel_image_sha256': hashlib.sha256(Path(config['kernel']).read_bytes()).hexdigest(),
                  'fixture_sha256': hashlib.sha256(fixture).hexdigest(), 'inventory_fixture_sha256': hashlib.sha256(inventory).hexdigest(),
                  'host_root_used': False, 'host_kernel_verified': False, 'production_state_changed': False, 'grants_capacity': False,
                  'scope': 'Fsync+syncfs then supervisor SIGKILL of QEMU; fresh boot of same test image. Not host power-loss testing or a production journal/accounting proof.'}
        print(json.dumps(report, sort_keys=True))
        return 0 if success else 3
    finally:
        if retain:
            temporary._finalizer.detach()
        else:
            temporary.cleanup()


if __name__ == '__main__':
    raise SystemExit(main())
