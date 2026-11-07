"""Real kernel vfork, native exec stops and original-parent wait outcomes."""
import json
import os
from pathlib import Path
import platform
import struct
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native helper build')
class VforkExecBoundaryTests(unittest.TestCase):
    def run_case(self,mode):
        library=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        producer=library.parent/'ci_cargo_vfork_producer';proxy=library.parent/'ci_cargo_vfork_proxy'
        wrong=Path(os.environ.get('AUGMENTAGENT_CARGO_EXEC_STATIC_BINARY',library.parent/'ci_cargo_exec_static'))
        data=proxy.read_bytes();self.assertEqual(data[:6],b'\x7fELF\x02\x01')
        offset,=struct.unpack_from('<Q',data,32);size,count=struct.unpack_from('<HH',data,54)
        self.assertNotIn(3,[struct.unpack_from('<I',data,offset+i*size)[0] for i in range(count)])
        with tempfile.TemporaryDirectory() as directory:
            receipt=Path(directory)/'cleanup'
            result=subprocess.run([sys.executable,'-I',str(ROOT/'crates/augmentagent-cli/src/ci_cargo_vfork_exec_cases.py'),
                str(ROOT/'scripts/ci-cargo-planner.py'),str(ROOT/'scripts/provider-supervisor.py'),str(library),
                str(producer),str(proxy),str(wrong),mode,str(receipt)],capture_output=True,text=True,timeout=15,
                env={k:v for k,v in os.environ.items() if k in ('HOME','PATH','LANG','LC_ALL')})
            self.assertEqual(result.returncode,0,result.stderr)
            lines=result.stdout.splitlines()
            self.assertEqual(lines[:-1],['PROXY_OUTPUT'] if mode=='finish' else [])
            self.assertEqual(result.stderr,'PROXY_ERROR\n' if mode=='finish' else '')
            row=json.loads(lines[-1]);self.assertTrue(row['original_cleanup']);self.assertFalse(row['shipping_authority'])
            self.assertFalse(row['full_ci_pass'])
            self.assertEqual(receipt.read_text(),'all-descendants-reaped\n')
            if mode in ('finish','signal','wrong-image'):
                self.assertTrue(row['parent_released_before_proxy_termination'])
                self.assertTrue(row['shared_memory_observed'])
                self.assertTrue(row['cloexec_closed_at_exec_stop'])
                self.assertEqual(row['image_accepted'],mode!='wrong-image')
            else:self.assertFalse(row['parent_released_before_proxy_termination'])
            return row

    def test_native_exec_releases_vfork_parent_before_proxy_runs_or_exits(self):self.run_case('finish')
    def test_original_parent_observes_real_proxy_signal_status(self):self.run_case('signal')
    def test_wrong_image_is_killed_at_exec_stop_before_its_first_instruction(self):self.run_case('wrong-image')
    def test_cancelled_exec_never_runs_proxy_or_claims_exec_release(self):self.run_case('cancel')
    def test_listener_loss_releases_waiting_parent_only_after_child_exits(self):self.run_case('listener-loss')
    def test_production_controller_still_refuses_unadmitted_vfork(self):self.run_case('production-deny')


if __name__=='__main__':unittest.main()
