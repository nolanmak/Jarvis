"""Run the real unit policy under the unchanged original child supervisor."""
import os
import importlib.util
from pathlib import Path
import platform
import signal
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
PROGRAM = ROOT / 'crates/augmentagent-cli/src/ci_cargo_unit_policy_cases.py'
spec = importlib.util.spec_from_file_location('unit_policy_planner', ROOT / 'scripts/ci-cargo-planner.py')
planner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(planner)


class UnitPolicyLoaderTests(unittest.TestCase):
    def test_untrusted_inode_is_refused_before_dynamic_loading(self):
        for fault in ('owner', 'group-write', 'world-write', 'links', 'empty', 'large', 'type'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'policy.so'
                path.write_bytes(b'not executable')
                info = dict(st_mode=0o100444, st_uid=0, st_nlink=1, st_size=14)
                info.update({'owner': {'st_uid': 1000}, 'group-write': {'st_mode': 0o100464},
                    'world-write': {'st_mode': 0o100446}, 'links': {'st_nlink': 2},
                    'empty': {'st_size': 0}, 'large': {'st_size': 2 * 1024**2 + 1},
                    'type': {'st_mode': 0o40444}}[fault])
                with patch.object(planner, 'Path', return_value=path), \
                     patch.object(planner.os, 'fstat', return_value=SimpleNamespace(**info)), \
                     patch.object(planner.ctypes, 'CDLL') as load:
                    with self.assertRaises(planner.FreshnessHold):
                        planner.UnitPolicy()
                    load.assert_not_called()

    def test_symlink_is_refused_before_dynamic_loading(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'target'
            target.write_bytes(b'not executable')
            path = Path(directory) / 'policy.so'
            path.symlink_to(target)
            with patch.object(planner, 'Path', return_value=path), patch.object(planner.ctypes, 'CDLL') as load:
                with self.assertRaises(planner.FreshnessHold):
                    planner.UnitPolicy()
                load.assert_not_called()

    def test_checked_descriptor_is_loaded_and_activation_error_holds(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'policy.so'
            path.write_bytes(b'not executable')
            info = SimpleNamespace(st_mode=0o100444, st_uid=0, st_nlink=1, st_size=14)
            library = Mock()
            library.jarvis_ci_restrict_unit.return_value = 0
            checked = []
            def load(name, **options):
                descriptor = int(name.removeprefix('/proc/self/fd/'))
                self.assertEqual(os.pread(descriptor, 14, 0), b'not executable')
                self.assertEqual(options, {'use_errno': True})
                checked.append(descriptor)
                return library
            with patch.object(planner, 'Path', return_value=path), \
                 patch.object(planner.os, 'fstat', return_value=info), \
                 patch.object(planner.ctypes, 'CDLL', side_effect=load):
                policy = planner.UnitPolicy()
                library.jarvis_ci_restrict_unit.assert_not_called()
                policy.activate()
                library.jarvis_ci_restrict_unit.return_value = -1
                with self.assertRaises(planner.FreshnessHold):
                    policy.activate()
            self.assertEqual(len(checked), 1)
            with self.assertRaises(OSError):
                os.fstat(checked[0])


@unittest.skipUnless(sys.platform == 'linux' and platform.machine() == 'x86_64', 'requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'), 'requires managed native helper build')
class NativeUnitPolicyTests(unittest.TestCase):
    def run_mode(self, mode):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / 'quiescent'
            library = str(Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve())
            result = subprocess.run([sys.executable, '-I', str(ROOT / 'scripts/provider-supervisor.py'),
                str(receipt), sys.executable, '-I', str(PROGRAM), library, mode],
                capture_output=True, text=True, timeout=15,
                env={key: value for key, value in os.environ.items() if key in ('HOME', 'PATH', 'LANG', 'LC_ALL')})
            self.assertEqual(receipt.read_text(), 'all-descendants-reaped\n')
            return result

    def test_namespace_and_mount_changes_are_denied(self):
        self.assert_guarded('namespace')

    def test_both_kernel_async_io_interfaces_are_denied(self):
        self.assert_guarded('async')

    def test_namespace_clone_flags_are_denied_and_clone3_falls_back(self):
        self.assert_guarded('clone')

    def test_ordinary_fork_inherits_policy(self):
        self.assert_guarded('fork')

    def test_ordinary_exec_inherits_policy(self):
        self.assert_guarded('exec')

    def test_existing_and_new_threads_are_guarded(self):
        self.assert_guarded('threads')

    def test_x32_cannot_alias_native_syscall_numbers(self):
        result = self.run_mode('x32')
        self.assertEqual(result.returncode, 128 + signal.SIGSYS, result.stderr)

    def assert_guarded(self, mode):
        result = self.run_mode(mode)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'GUARDED\n')


if __name__ == '__main__':
    unittest.main()
