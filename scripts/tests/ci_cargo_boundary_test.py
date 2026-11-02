"""Capability observations cannot stand in for an enforced execution boundary."""
import importlib.util
import json
from pathlib import Path
import platform
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

PROGRAM=Path(__file__).resolve().parents[1]/'ci-cargo-boundary.py'


class CapabilityTests(unittest.TestCase):
    def load(self):
        spec=importlib.util.spec_from_file_location('ci_cargo_boundary',PROGRAM)
        module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        return module

    def elf(self,root,interpreter=b'/lib64/ld-linux-x86-64.so.2\0'):
        header=bytearray(64);header[:7]=b'\x7fELF\x02\x01\x01'
        struct.pack_into('<HHI',header,16,3,62,1)
        struct.pack_into('<H',header,52,64)
        struct.pack_into('<Q',header,32,64)
        struct.pack_into('<HH',header,54,56,1)
        program=struct.pack('<IIQQQQQQ',3,4,120,0,0,len(interpreter),len(interpreter),1)
        path=root/'cargo';path.write_bytes(header+program+interpreter)
        return path

    @unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64 kernel ABI')
    def test_real_kernel_observation_grants_no_execution_or_ci_authority(self):
        module=self.load()
        with tempfile.TemporaryDirectory() as directory:
            result=module.observe(self.elf(Path(directory)))
        self.assertEqual(result['kind'],'ci-cargo-execution-capabilities')
        self.assertTrue(result['seccomp_user_notification'])
        self.assertTrue(result['process_vm_self_read'])
        self.assertTrue(result['pidfd'])
        self.assertFalse(result['enforcement_established'])
        self.assertFalse(result['shipping_authority'])

    def test_unknown_architecture_and_unavailable_kernel_features_refuse(self):
        module=self.load()
        with tempfile.TemporaryDirectory() as directory:
            cargo=self.elf(Path(directory))
            with patch.object(module.platform,'machine',return_value='i686'):
                with self.assertRaises(module.FreshnessHold):module.observe(cargo)
            with patch.object(module,'_kernel_capabilities',side_effect=OSError(38,'unavailable')):
                with self.assertRaises(module.FreshnessHold):module.observe(cargo)

    def test_missing_static_malformed_or_aliased_program_cannot_claim_support(self):
        module=self.load()
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);cargo=self.elf(root);alias=root/'alias';alias.symlink_to(cargo)
            for path in (root/'missing',alias):
                with self.assertRaises(module.FreshnessHold):module.observe(path)
            data=bytearray(cargo.read_bytes());struct.pack_into('<H',data,18,183);cargo.write_bytes(data)
            with self.assertRaises(module.FreshnessHold):module.observe(cargo)
            for data in (b'',b'not-elf',cargo.read_bytes()[:64],b'\x7fELF\x01\x01'+bytes(128)):
                cargo.write_bytes(data)
                with self.assertRaises(module.FreshnessHold):module.observe(cargo)

    def test_cli_failure_is_bounded_and_does_not_echo_private_paths(self):
        marker='PRIVATE_CAPABILITY_PROBE_PATH'
        result=subprocess.run([sys.executable,'-I',str(PROGRAM),'--cargo','/missing/'+marker],capture_output=True,text=True)
        self.assertEqual(result.returncode,2)
        self.assertNotIn(marker,result.stdout+result.stderr)
        self.assertEqual(json.loads(result.stdout)['freshness_hold']['reason'],'freshness_unknown')


if __name__=='__main__':unittest.main()
