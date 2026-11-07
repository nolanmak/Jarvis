"""Composition of real vfork, static V3 component and supervised delegated tool."""
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native helper build')
class StatusHandoffTests(unittest.TestCase):
    def run_case(self,mode):
        lib=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve();out=lib.parent
        with tempfile.TemporaryDirectory() as directory:
            receipt=Path(directory)/'cleanup'
            r=subprocess.run([sys.executable,'-I',str(ROOT/'crates/augmentagent-cli/src/ci_cargo_status_handoff_cases.py'),
                str(ROOT/'scripts/ci-cargo-planner.py'),str(ROOT/'scripts/provider-supervisor.py'),
                str(ROOT/'crates/augmentagent-cli/src/ci_cargo_vfork_exec_cases.py'),str(lib),
                str(out/'ci_cargo_status_vfork_producer'),str(out/'ci_cargo_status_proxy'),str(out/'ci_cargo_exec_static'),mode,str(receipt)],
                capture_output=True,text=True,timeout=18,
                env={k:v for k,v in os.environ.items() if k in ('HOME','PATH','LANG','LC_ALL')})
            self.assertEqual(r.returncode,0,r.stderr)
            lines=r.stdout.splitlines();actual=mode in ('finish','fd-finish','signal','cancel-after-start')
            self.assertEqual(lines[:-1],['DELEGATED_TOOL_OUTPUT'] if actual else [])
            row=json.loads(lines[-1]);self.assertTrue(row['original_cleanup'])
            self.assertFalse(row['root_role_admission_proved']);self.assertFalse(row['production_native_handoff'])
            self.assertFalse(row['full_ci_pass']);self.assertFalse(row['shipping_authority'])
            self.assertEqual(row['tool_calls'],int(actual))
            if actual:
                self.assertTrue(row['tool_exec_observed']);self.assertTrue(row['tool_cleanup'])
                self.assertTrue(row['continuation_accepted']);self.assertTrue(row['parent_released_before_proxy_termination'])
                self.assertTrue(row['original_connection_closed_at_exec'])
            if mode=='inherited-exec-deny':
                self.assertTrue(row['inherited_exec_denial_observed'])
                self.assertFalse(row['native_exec_stop'])
                self.assertFalse(row['parent_released_before_proxy_termination'])
                self.assertEqual(row['original_parent_wait_status'],93<<8)
            if mode=='swapped-channel':
                self.assertTrue(row['image_accepted']);self.assertFalse(row['continuation_accepted'])
            self.assertEqual(receipt.read_text(),'all-descendants-reaped\n')

    def test_real_vfork_releases_parent_and_returns_actual_delegated_exit(self):self.run_case('finish')
    def test_real_vfork_descriptor_exec_releases_parent_and_returns_delegated_exit(self):self.run_case('fd-finish')
    def test_proxy_signal_reaches_actual_tool_and_native_parent_wait(self):self.run_case('signal')
    def test_cancellation_after_tool_start_retains_original_cleanup(self):self.run_case('cancel-after-start')
    def test_wrong_image_never_launches_a_tool(self):self.run_case('wrong-image')
    def test_valid_continuation_for_wrong_operation_is_rejected_before_body(self):self.run_case('swapped-channel')
    def test_pre_exec_cancellation_never_launches_a_tool(self):self.run_case('cancel-before-exec')
    def test_inherited_constructor_filter_denies_exec_before_listener(self):self.run_case('inherited-exec-deny')
    def test_production_listener_keeps_unadmitted_vfork_refused(self):self.run_case('production-deny')


if __name__=='__main__':unittest.main()
