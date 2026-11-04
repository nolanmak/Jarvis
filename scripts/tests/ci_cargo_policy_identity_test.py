"""Additional policy identity cases; preserve the original ten native cases."""
import hashlib
import os
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import Mock,patch
from ci_cargo_unit_policy_test import planner


class PolicyIdentityTests(unittest.TestCase):
    def test_activation_observation_requires_success_and_binds_checked_library_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'policy.so';data=b'not executable';path.write_bytes(data)
            info=SimpleNamespace(st_mode=0o100444,st_uid=0,st_nlink=1,st_size=len(data))
            library=Mock();library.jarvis_ci_restrict_unit.return_value=0
            with patch.object(planner,'Path',return_value=path), \
                    patch.object(planner.os,'fstat',return_value=info), \
                    patch.object(planner.ctypes,'CDLL',return_value=library):
                policy=planner.UnitPolicy()
                with self.assertRaises(planner.FreshnessHold):policy.activation_observation()
                policy.activate();record=policy.activation_observation()
                self.assertEqual(record['sha256'],hashlib.sha256(data).hexdigest())
                self.assertEqual(record['size_bytes'],len(data))
                self.assertFalse(record['input_closure_complete']);self.assertFalse(record['shipping_authority'])
                library.jarvis_ci_restrict_unit.return_value=-1
                with self.assertRaises(planner.FreshnessHold):policy.activate()
                with self.assertRaises(planner.FreshnessHold):policy.activation_observation()

    def test_policy_measurement_uses_held_inode_across_path_replacement(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'policy.so';path.write_bytes(b'original bytes')
            info=SimpleNamespace(st_mode=0o100444,st_uid=0,st_nlink=1,st_size=14)
            library=Mock();library.jarvis_ci_restrict_unit.return_value=0
            def load(name,**options):
                replacement=path.with_suffix('.new');replacement.write_bytes(b'replaced bytes');replacement.replace(path)
                self.assertEqual(os.pread(int(name.rsplit('/',1)[1]),14,0),b'original bytes')
                return library
            with patch.object(planner,'Path',return_value=path), \
                    patch.object(planner.os,'fstat',return_value=info), \
                    patch.object(planner.ctypes,'CDLL',side_effect=load):
                policy=planner.UnitPolicy();policy.activate()
                self.assertEqual(policy.activation_observation()['sha256'],hashlib.sha256(b'original bytes').hexdigest())

    def test_policy_changed_during_dynamic_loading_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'policy.so';path.write_bytes(b'original bytes')
            info=SimpleNamespace(st_mode=0o100444,st_uid=0,st_nlink=1,st_size=14)
            def load(name,**options):
                descriptor=int(name.rsplit('/',1)[1]);writer=os.open(f'/proc/self/fd/{descriptor}',os.O_WRONLY)
                try:os.pwrite(writer,b'replaced bytes',0)
                finally:os.close(writer)
                return Mock()
            with patch.object(planner,'Path',return_value=path), \
                    patch.object(planner.os,'fstat',return_value=info), \
                    patch.object(planner.ctypes,'CDLL',side_effect=load),self.assertRaises(planner.FreshnessHold):
                planner.UnitPolicy()


if __name__=='__main__':unittest.main()
