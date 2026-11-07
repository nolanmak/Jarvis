"""Disposable multi-inode immutable experiments, never a custody producer.

Run only from the no-NIC ext4 test guest. All paths are allocated internally
below its disposable /fixture mount. No journal, accounting or runtime grant
exists here. Explicit fixture cleanup restores only flags on its own held FDs.
"""
import array
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

IMMUTABLE = 0x10
GETFLAGS = 0x80086601
SETFLAGS = 0x40086602


def flags(fd):
    value = array.array('l', [0])
    fcntl.ioctl(fd, GETFLAGS, value, True)
    return value[0]


def change(fd, value):
    fcntl.ioctl(fd, SETFLAGS, array.array('l', [value]))


def identity(fd):
    value = os.fstat(fd)
    return value.st_dev, value.st_ino


class CaptureChanged(Exception):
    pass


class Experiment:
    """Own fixture descriptors; no production input or persistent capability."""
    def __init__(self, path):
        self.path = path
        self.directory = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        self.directory_flags = flags(self.directory)
        self.files = {}
        self.original = None

    def close_fixture(self):
        for fd, original_flags in self.files.values():
            change(fd, original_flags)
            os.close(fd)
        change(self.directory, self.directory_flags)
        os.close(self.directory)

    def inventory(self):
        result = {}
        for name in sorted(os.listdir(self.directory)):
            fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=self.directory)
            try:
                info = os.fstat(fd)
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > 4096:
                    raise CaptureChanged('unsupported fixture inode')
                result[name] = (info.st_dev, info.st_ino, info.st_uid,
                                info.st_nlink, info.st_size,
                                hashlib.sha256(os.pread(fd, 4097, 0)).hexdigest())
            finally:
                os.close(fd)
        return result

    def require_original_inventory(self):
        named = os.stat(self.path, follow_symlinks=False)
        if (not stat.S_ISDIR(named.st_mode)
                or (named.st_dev, named.st_ino) != identity(self.directory)
                or not flags(self.directory) & IMMUTABLE
                or self.inventory() != self.original):
            raise CaptureChanged('original directory or inventory changed')
        for name, (fd, _) in self.files.items():
            info = os.fstat(fd)
            value = (info.st_dev, info.st_ino, info.st_uid, info.st_nlink,
                     info.st_size, hashlib.sha256(os.pread(fd, 4097, 0)).hexdigest())
            if value != self.original[name]:
                raise CaptureChanged('captured file description changed')

    def capture(self, hook=lambda _phase: None):
        self.original = self.inventory()
        if set(self.original) != {'lock', 'owned.json'}:
            raise CaptureChanged('unsupported fixture inventory')
        for name in self.original:
            fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=self.directory)
            self.files[name] = fd, flags(fd)
        hook('captured')
        change(self.directory, self.directory_flags | IMMUTABLE)
        hook('directory-fenced')
        self.require_original_inventory()
        for name, (fd, original_flags) in self.files.items():
            change(fd, original_flags | IMMUTABLE)
            hook('file-fenced:' + name)
        hook('all-files-fenced')
        self.require_original_inventory()
        if not all(flags(fd) & IMMUTABLE for fd, _ in self.files.values()):
            raise CaptureChanged('incomplete original file fence')
        return {'consistent': True, 'grants_capacity': False}


class InventoryCases(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='inventory-probe-', dir='/fixture')
        self.addCleanup(self.temporary.cleanup)
        self.outer = Path(self.temporary.name)
        self.state = self.outer / 'state'
        self.state.mkdir(mode=0o700)
        (self.state / 'owned.json').write_bytes(b'original metadata')
        (self.state / 'lock').write_bytes(b'')
        self.probe = Experiment(self.state)
        self.addCleanup(self.probe.close_fixture)

    def writer(self, operation, fd=None):
        body = """import errno,os,sys
try:
    exec(sys.argv[1])
except OSError as error:
    raise SystemExit(80 if error.errno in (errno.EPERM,errno.EACCES) else 81)
"""
        result = subprocess.run([sys.executable, '-I', '-c', body, operation],
                                pass_fds=(() if fd is None else (fd,)),
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=5)
        self.assertIn(result.returncode, (0, 80), result.stderr.decode())
        return result.returncode

    def test_stable_inventory_fences_both_original_inodes(self):
        result = self.probe.capture()
        self.assertTrue(result['consistent'])
        self.assertFalse(result['grants_capacity'])
        self.assertTrue(flags(self.probe.directory) & IMMUTABLE)
        self.assertTrue(all(flags(fd) & IMMUTABLE for fd, _ in self.probe.files.values()))
        self.assertEqual(self.probe.inventory(), self.probe.original)

    def test_new_entry_before_directory_fence_is_detected(self):
        def race(phase):
            if phase == 'captured':
                self.assertEqual(self.writer(f"open({str(self.state / 'new')!r},'wb').write(b'new')"), 0)
        with self.assertRaises(CaptureChanged):
            self.probe.capture(race)

    def test_same_bytes_replacement_inode_is_detected(self):
        def race(phase):
            if phase == 'captured':
                original = self.state / 'owned.json'
                original.rename(self.outer / 'parked-file')
                original.write_bytes(b'original metadata')
        with self.assertRaises(CaptureChanged):
            self.probe.capture(race)

    def test_replacement_directory_is_detected(self):
        def race(phase):
            if phase == 'captured':
                self.state.rename(self.outer / 'parked-directory')
                self.state.mkdir(mode=0o700)
                (self.state / 'owned.json').write_bytes(b'original metadata')
                (self.state / 'lock').write_bytes(b'')
        with self.assertRaises(CaptureChanged):
            self.probe.capture(race)

    def test_existing_fd_write_between_directory_and_file_fences_is_detected(self):
        writable = os.open(self.state / 'owned.json', os.O_WRONLY)
        self.addCleanup(os.close, writable)
        def race(phase):
            if phase == 'directory-fenced':
                self.assertEqual(self.writer(f'os.write({writable},b"changed! metadata")', writable), 0)
        with self.assertRaises(CaptureChanged):
            self.probe.capture(race)

    def test_directory_fence_denies_new_entries(self):
        def race(phase):
            if phase == 'directory-fenced':
                self.assertEqual(self.writer(f"open({str(self.state / 'new')!r},'wb').write(b'new')"), 80)
        self.assertTrue(self.probe.capture(race)['consistent'])

    def test_write_after_first_file_fence_is_detected_by_final_recensus(self):
        writable = os.open(self.state / 'owned.json', os.O_WRONLY)
        self.addCleanup(os.close, writable)
        def race(phase):
            if phase == 'file-fenced:lock':
                self.assertEqual(self.writer(f'os.write({writable},b"changed! metadata")', writable), 0)
        with self.assertRaises(CaptureChanged):
            self.probe.capture(race)

    def test_inode_owner_without_capability_cannot_clear_immutable_flag(self):
        os.chown(self.state / 'owned.json', 1000, 1000)
        def race(phase):
            if phase == 'all-files-fenced':
                fd = self.probe.files['owned.json'][0]
                operation = (f'import array,fcntl;os.setgroups([]);os.setresgid(1000,1000,1000);'
                             f'os.setresuid(1000,1000,1000);assert os.fstat({fd}).st_uid==os.geteuid();'
                             f'fcntl.ioctl({fd},{SETFLAGS},array.array("l",[{flags(fd) & ~IMMUTABLE}]))')
                self.assertEqual(self.writer(operation, fd), 80)
                self.assertTrue(flags(fd) & IMMUTABLE)
        self.assertTrue(self.probe.capture(race)['consistent'])

    def test_full_fence_denies_preexisting_writer_and_unlink(self):
        writable = os.open(self.state / 'owned.json', os.O_WRONLY)
        self.addCleanup(os.close, writable)
        def race(phase):
            if phase == 'all-files-fenced':
                self.assertEqual(self.writer(f'os.write({writable},b"changed! metadata")', writable), 80)
                self.assertEqual(self.writer(f'os.unlink({str(self.state / "lock")!r})'), 80)
        self.assertTrue(self.probe.capture(race)['consistent'])

    def test_partial_transition_keeps_restrictions_until_explicit_fixture_cleanup(self):
        class Interrupted(Exception):
            pass
        def interrupt(phase):
            if phase == 'file-fenced:lock':
                raise Interrupted()
        with self.assertRaises(Interrupted):
            self.probe.capture(interrupt)
        self.assertTrue(flags(self.probe.directory) & IMMUTABLE)
        self.assertTrue(flags(self.probe.files['lock'][0]) & IMMUTABLE)
        self.assertFalse(flags(self.probe.files['owned.json'][0]) & IMMUTABLE)


def main():
    if len(sys.argv) != 1 or os.geteuid() != 0 or os.environ.get('JARVIS_CUSTODY_DISPOSABLE_GUEST') != '1':
        raise SystemExit('Only internally allocated disposable guest fixtures are supported')
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(InventoryCases))
    print(json.dumps({'kind': 'inventory_transition', 'tests_run': result.testsRun,
                      'failures': len(result.failures), 'errors': len(result.errors),
                      'skipped': len(result.skipped), 'success': result.wasSuccessful(),
                      'production_state_changed': False, 'grants_capacity': False}), flush=True)
    return 0 if result.wasSuccessful() else 3


if __name__ == '__main__':
    raise SystemExit(main())
