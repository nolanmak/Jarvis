#!/usr/bin/env python3
"""Boot archive contract tests; no credentials, VM/model or production changes."""
import gzip
import importlib.util
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


native = load('native_archive_test', 'native-provider-vm.py')
codec = load('shared_archive_test', 'codex-build-vm.py')


def encoded_size(entries):
    # Independent sizing, verified against actual gzip decompression below.
    total = 0
    for name, mode, data, major, minor in [*entries, ('TRAILER!!!', 0, b'', 0, 0)]:
        total = (total + 110 + len(name.encode()) + 1 + 3) // 4 * 4
        total = (total + len(data) + 3) // 4 * 4
    return total


class NativeBootBudgetTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.busybox = self.root / 'busybox'
        self.busybox.write_bytes(b'bootstrap')
        self.config = {'busybox': str(self.busybox), 'modules': []}

    def entries(self, **changes):
        values = dict(config=self.config, job={'argv': ['synthetic']}, seed=[],
                      guest_source=b'guest', gateway_source=b'gateway', cert_entries=[])
        values.update(changes)
        return native.boot_entries(**values)

    def test_existing_boot_path_refuses_oversized_busybox(self):
        self.busybox.write_bytes(b'x' * 8193)
        with mock.patch.object(native, 'MAX_BOOT_ARCHIVE_BYTES', 8192, create=True):
            with self.assertRaisesRegex(ValueError, 'budget'):
                self.entries()

    def test_existing_boot_path_refuses_aggregate_modules_seed_and_metadata(self):
        module = self.root / 'module'; module.write_bytes(b'm' * 4096)
        self.config['modules'] = [str(module)]
        args = {'seed': [('nested/home', b'h' * 4096)],
                'cert_entries': [('etc/cert', stat.S_IFREG | 0o400, b'cert', 0, 0)]}
        size = encoded_size(self.entries(**args))
        with mock.patch.object(native, 'MAX_BOOT_ARCHIVE_BYTES', size - 1, create=True):
            with self.assertRaisesRegex(ValueError, 'budget'):
                self.entries(**args)

    def test_shared_codec_enforces_exact_raw_archive_boundary(self):
        for count in range(6):
            entries = [('utf8-λ', stat.S_IFREG | 0o600, b'a' * count, 0, 0)]
            raw = gzip.decompress(codec.initrd(entries))
            self.assertEqual(len(raw), encoded_size(entries))
            self.assertEqual(gzip.decompress(codec.initrd(entries, max_bytes=len(raw))), raw)
            with self.assertRaisesRegex(ValueError, 'budget'):
                codec.initrd(entries, max_bytes=len(raw) - 1)
        for invalid in (True, False, 0, -1, 1.5, '128'):
            with self.assertRaises(ValueError):
                codec.initrd([], max_bytes=invalid)

    def test_native_exact_total_boundary_includes_seed_and_helpers(self):
        args = {'seed': [('nested/home', b'home')], 'guest_source': b'g' * 33}
        size = encoded_size(self.entries(**args))
        with mock.patch.object(native, 'MAX_BOOT_ARCHIVE_BYTES', size, create=True):
            self.assertEqual(encoded_size(self.entries(**args)), size)
        with mock.patch.object(native, 'MAX_BOOT_ARCHIVE_BYTES', size - 1, create=True):
            with self.assertRaisesRegex(ValueError, 'budget'):
                self.entries(**args)

    def test_real_128_mib_sparse_source_refuses_before_payload_read(self):
        with self.busybox.open('wb') as stream:
            stream.truncate(128 * 1024 * 1024 + 1)
        with mock.patch.object(native.os, 'read', side_effect=AssertionError('oversized source was read')):
            with self.assertRaisesRegex(ValueError, 'budget'):
                self.entries()

    def test_held_source_reader_rejects_observed_mutation_and_shrink(self):
        original_read = os.read
        for replacement in (b'changed!!', b'x', b'growth' * 32):
            self.busybox.write_bytes(b'bootstrap')
            touched = False
            def changing(fd, amount):
                nonlocal touched
                self.assertLessEqual(amount, 65536)
                data = original_read(fd, amount)
                if not touched:
                    touched = True
                    self.busybox.write_bytes(replacement)
                return data
            with mock.patch.object(native.os, 'read', side_effect=changing):
                with self.assertRaisesRegex(ValueError, 'changed|budget'):
                    native.boot_source_bytes(self.busybox, 64)
        self.assertFalse(self.busybox.read_bytes() == b'bootstrap')

    def test_short_read_and_io_failure_refuse_without_private_paths(self):
        with mock.patch.object(native.os, 'read', side_effect=[b'boot', b'']):
            with self.assertRaisesRegex(ValueError, 'changed'):
                native.boot_source_bytes(self.busybox, 64)
        with mock.patch.object(native.os, 'read', side_effect=OSError('/private/synthetic/credential')):
            with self.assertRaisesRegex(ValueError, '^native initrd source unavailable$'):
                native.boot_source_bytes(self.busybox, 64)

    def test_fifo_refuses_without_waiting_for_a_writer(self):
        fifo = self.root / 'fifo'; os.mkfifo(fifo)
        child = '''import importlib.util,sys
s=importlib.util.spec_from_file_location('native',sys.argv[1]); n=importlib.util.module_from_spec(s);s.loader.exec_module(n)
try:n.boot_source_bytes(sys.argv[2],4096)
except ValueError:print('nonregular-refused')
else:raise SystemExit('FIFO was accepted')
'''
        result = subprocess.run([sys.executable, '-I', '-c', child,
                                 str(HERE / 'native-provider-vm.py'), str(fifo)],
                                capture_output=True, timeout=3)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(result.stdout, b'nonregular-refused\n')

    def test_image_bounds_precede_exclusive_private_output(self):
        entries = self.entries(); target = self.root / 'initrd.gz'
        with mock.patch.object(native, 'MAX_BOOT_IMAGE_BYTES', 16):
            with self.assertRaisesRegex(ValueError, 'budget'):
                native.write_boot_image(target, entries, lambda *a, **k: b'x' * 17)
        self.assertFalse(target.exists())
        native.write_boot_image(target, entries, codec.initrd)
        original = target.read_bytes()
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)
        self.assertEqual(len(gzip.decompress(original)), encoded_size(entries))
        with self.assertRaises(FileExistsError):
            native.write_boot_image(target, entries, codec.initrd)
        self.assertEqual(target.read_bytes(), original)

    def test_output_fsync_failure_preserves_original_failure_and_owned_file(self):
        target = self.root / 'initrd.gz'
        with mock.patch.object(native.os, 'fsync', side_effect=OSError('synthetic fsync refusal')):
            with self.assertRaisesRegex(OSError, 'synthetic fsync refusal'):
                native.write_boot_image(target, self.entries(), codec.initrd)
        self.assertTrue(target.exists())
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)
        # It is the caller's original private temporary-directory lifecycle,
        # not an inferred permission to delete/release an unknown artifact.

    def test_each_materialized_payload_category_refuses_one_byte_short(self):
        module = self.root / 'module'; module.write_bytes(b'module')
        variants = [dict(config={'busybox': str(self.busybox), 'modules': [str(module)]}),
                    dict(seed=[('nested/home', b'home')]), dict(guest_source=b'g' * 64),
                    dict(gateway_source=b'w' * 64), dict(job={'argv': ['z' * 64]}),
                    dict(cert_entries=[('etc/cert', stat.S_IFREG | 0o400, b'cert', 0, 0)])]
        for variant in variants:
            with self.subTest(category=next(iter(variant))):
                size = encoded_size(self.entries(**variant))
                with mock.patch.object(native, 'MAX_BOOT_ARCHIVE_BYTES', size - 1):
                    with self.assertRaisesRegex(ValueError, 'budget'):
                        self.entries(**variant)

    def test_main_uses_bounded_encoder_and_script_paths_before_qemu(self):
        binary = self.root / 'cli'; binary.write_bytes(b'synthetic executable'); binary.chmod(0o755)
        state = self.root / 'session'; state.mkdir(mode=0o700)
        request = {'version': 1, 'guest_runtime': str(self.root / 'runtime'), 'runtime_config': 'fixture',
                   'memory_mb': 1024, 'session_directory': str(state), 'auth_relative': 'auth.json',
                   'executable': str(binary), 'gateway': {}, 'argv': ['synthetic'], 'environment': {}}
        class StoppedBeforeVm(Exception):
            pass
        original_image = native.write_boot_image
        produced = []
        def image(path, entries, encoder):
            original_image(path, entries, encoder)
            produced.append(len(gzip.decompress(path.read_bytes())))
            raise StoppedBeforeVm()
        original_open = os.open
        own_locks = []
        def opening(path, *args, **kwargs):
            fd = original_open(path, *args, **kwargs)
            if Path(path) == state / 'lock':
                own_locks.append(fd)
            return fd
        gateway = SimpleNamespace(provision=lambda *a: ({}, []))
        try:
            with mock.patch.object(native, 'module', side_effect=lambda name, filename: codec if filename == 'codex-build-vm.py' else gateway), \
                 mock.patch.object(native, 'private_json', return_value=request), \
                 mock.patch.object(native, 'validate_runtime'), \
                 mock.patch.object(codec.Runtime, 'load', return_value=SimpleNamespace(config=self.config)), \
                 mock.patch.object(native, 'credential_use', return_value=None), \
                 mock.patch.object(native, 'runtime_seed', return_value=([], None)), \
                 mock.patch.object(native, 'stage_native_companions'), \
                 mock.patch.object(native.os, 'open', side_effect=opening), \
                 mock.patch.object(native, 'boot_entries', wraps=native.boot_entries) as boot, \
                 mock.patch.object(native, 'write_boot_image', side_effect=image), \
                 mock.patch.object(native.subprocess, 'Popen', side_effect=AssertionError('QEMU launched')):
                with self.assertRaises(StoppedBeforeVm):
                    native.main(self.root / 'synthetic-spec')
                self.assertIsInstance(boot.call_args.args[3], Path)
                self.assertIsInstance(boot.call_args.args[4], Path)
            self.assertEqual(len(produced), 1)
            self.assertLessEqual(produced[0], native.MAX_BOOT_ARCHIVE_BYTES)
        finally:
            for fd in own_locks:
                os.close(fd)

    def test_all_payload_categories_charge_archive_metadata(self):
        guest = self.root / 'guest.py'; guest.write_bytes(b'g' * 33)
        gateway = self.root / 'gateway.py'; gateway.write_bytes(b'w' * 35)
        entries = self.entries(guest_source=guest, gateway_source=gateway)
        raw = gzip.decompress(codec.initrd(entries))
        self.assertEqual(len(raw), encoded_size(entries))
        self.assertTrue(any(e[0] == 'root/guest.py' and e[2] == b'g' * 33 for e in entries))
        self.assertTrue(any(e[0] == 'root/native_provider_gateway.py' and e[2] == b'w' * 35 for e in entries))


if __name__ == '__main__':
    unittest.main()
