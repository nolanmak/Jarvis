#!/usr/bin/env python3
"""Pure probe preflight arithmetic: never starts a service, VM, build or model."""
import importlib.util
from pathlib import Path
import unittest
import tempfile
import json
import os
import signal
import subprocess
import sys
import time

spec=importlib.util.spec_from_file_location('native_probe',Path(__file__).with_name('autoship-native-acceptance-probe.py'))
probe=importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
GIB=1024**3
MIB=1024**2


class Tests(unittest.TestCase):
    def test_claude_guarded_edit_requires_its_own_ignored_test_and_file_only_budget(self):
        expected='self_improve::native_route_acceptance::live_native_guarded_claude_file_edit'
        self.assertEqual(probe.TESTS.get('claude_file_edit'),expected,
                         'Claude must execute its guarded native edit test, not the Codex test or transport helper')
        with tempfile.TemporaryDirectory() as temporary:
            binary=Path(temporary)/'compiled-inventory-fixture'
            binary.write_text('#!/usr/bin/python3\nimport sys\nassert sys.argv[1:]=='+repr(['--list','--ignored','--exact',expected])+"\nprint(sys.argv[-1]+': test')\nprint('1 test, 0 benchmarks')\n")
            binary.chmod(0o700)
            self.assertEqual(len(probe.verify_test_inventory(binary,probe.TESTS['claude_file_edit'])),64)
            with self.assertRaises(ValueError):probe.verify_test_inventory(binary,probe.TESTS['file_edit'])
        self.assertEqual(probe.reachable_build_memory('claude_file_edit',4096),0)
        budget=probe.resource_budget(1024,probe.reachable_build_memory('claude_file_edit',4096),3*GIB)
        self.assertEqual(budget['run_memory_bytes'],2*GIB)
        self.assertEqual(budget['final_admission'],'RunScope')

    def test_service_stop_without_bound_test_completion_is_not_success(self):
        test=probe.TESTS['adoption'];nonce='fixture-generation'
        passed=f"test {test} ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n"
        receipt={'nonce':nonce,'exit_code':0}
        for value,transcript in [(None,'running 1 test\n'),(None,passed),({'nonce':'old','exit_code':0},passed),(receipt,'running 1 test\n'),(receipt,passed.replace('1 passed','0 passed'))]:
            with self.subTest(receipt=value,transcript=transcript):
                self.assertNotEqual(probe.completion_status(0,value,nonce,transcript,test),0)
        self.assertEqual(probe.completion_status(0,receipt,nonce,passed,test),0)
        self.assertNotEqual(probe.completion_status(1,receipt,nonce,passed,test),0)
        self.assertNotEqual(probe.completion_status(0,{'nonce':nonce,'exit_code':1},nonce,passed,test),0)

    def test_actual_wrapper_records_completion_but_not_interrupted_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);test=probe.TESTS['adoption'];nonce='actual-wrapper'
            for interrupted in (False,True):
                receipt=root/('stopped.json' if interrupted else 'completed.json')
                marker=root/'started'
                child="import pathlib,time;pathlib.Path("+repr(str(marker))+").touch();time.sleep(30)" if interrupted else "print("+repr(f"test {test} ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s")+")"
                process=subprocess.Popen([sys.executable,'-I','-c',probe.COMPLETION_WRAPPER,str(receipt),nonce,sys.executable,'-c',child],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
                try:
                    if interrupted:
                        deadline=time.monotonic()+5
                        while not marker.exists() and time.monotonic()<deadline:time.sleep(.01)
                        self.assertTrue(marker.exists(),'child must actually start before interruption')
                        os.killpg(process.pid,signal.SIGTERM)
                    stdout,stderr=process.communicate(timeout=5)
                    value=json.loads(receipt.read_text()) if receipt.exists() else None
                    self.assertEqual(probe.completion_status(0,value,nonce,stdout,test)==0,not interrupted,stderr)
                    self.assertEqual(receipt.exists(),not interrupted)
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid,signal.SIGKILL);process.communicate()

    def test_explicit_deferred_mode_preserves_full_limits_and_requires_controller_floor(self):
        available=6300*MIB
        with self.assertRaises(ValueError):probe.resource_budget(1024,4096,available)
        budget=probe.resource_budget(1024,4096,available,defer_run_admission=True)
        self.assertTrue(budget['deferred_run_admission'])
        self.assertFalse(budget['run_admitted_by_preflight'])
        self.assertEqual(budget['run_memory_bytes'],6*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],8*GIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+512*MIB)
        self.assertEqual(budget['startup_floor_bytes'],512*MIB)
        self.assertEqual(budget['final_admission'],'RunScope')
        self.assertEqual(probe.resource_budget(1024,4096,512*MIB,defer_run_admission=True)['startup_floor_bytes'],512*MIB)
        with self.assertRaises(ValueError):probe.resource_budget(1024,4096,512*MIB-1,defer_run_admission=True)
        with self.assertRaises(ValueError):probe.resource_budget(1024,4096,8*GIB,defer_run_admission='yes')

    def test_exact_ignored_inventory_required_before_launch(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);binary=root/'fixture-binary';mode=root/'mode'
            expected=probe.TESTS['routed_transport']
            binary.write_text("#!/usr/bin/python3\nimport pathlib,sys\nroot=pathlib.Path(__file__).parent\nassert sys.argv[1:]==['--list','--ignored','--exact',"+repr(expected)+"]\nmode=(root/'mode').read_text()\nif mode=='error':raise SystemExit(1)\nif mode=='exact':print(sys.argv[-1]+': test');print('1 test, 0 benchmarks')\nelif mode=='other':print('different_test: test')\nelse:print('0 tests, 0 benchmarks')\n")
            binary.chmod(0o700)
            for value in ['zero','other','error']:
                mode.write_text(value)
                with self.assertRaises(ValueError):probe.verify_test_inventory(binary,expected)
            mode.write_text('exact')
            self.assertEqual(len(probe.verify_test_inventory(binary,expected)),64)

    def test_reviewer_probes_do_not_reserve_unreachable_build_vm(self):
        for name in ('writer','adjudicator','routed_transport','file_edit'):
            build=probe.reachable_build_memory(name,4096)
            self.assertEqual(build,0)
            budget=probe.resource_budget(1024,build,3*GIB)
            self.assertEqual(budget['run_memory_bytes'],2*GIB)
            self.assertEqual(budget['aggregate_ceiling_bytes'],4*GIB)
        for name in ('issue','adoption'):
            build=probe.reachable_build_memory(name,4096)
            self.assertEqual(build,4096)
            self.assertEqual(probe.resource_budget(1024,build,7*GIB)['run_memory_bytes'],6*GIB)
            with self.assertRaises(ValueError):probe.reachable_build_memory(name,0)
        with self.assertRaises(ValueError):probe.reachable_build_memory('unclassified',4096)

    def test_full_guests_fit_at_six_and_half_gib_without_reserving_unit_ceiling(self):
        budget=probe.resource_budget(1024,4096,6*GIB+512*MIB)
        self.assertEqual(budget['run_memory_bytes'],6*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],8*GIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+512*MIB)
        self.assertEqual(budget['final_admission'],'RunScope')
        with self.assertRaisesRegex(ValueError,'insufficient host headroom'):
            probe.resource_budget(1024,4096,6*GIB+512*MIB-1)
        with self.assertRaisesRegex(ValueError,'insufficient host headroom'):
            probe.resource_budget(1024,4096,4400*MIB)

    def test_controller_allowance_is_explicit_and_cannot_reduce_minimum(self):
        budget=probe.resource_budget(1024,4096,7*GIB,512)
        self.assertEqual(budget['controller_allowance_bytes'],512*MIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+768*MIB)
        for value in [0,255,2049,True,'256',256.0]:
            with self.assertRaises(ValueError):
                probe.resource_budget(1024,4096,10*GIB,value)

    def test_review_reservation_and_malformed_inputs(self):
        budget=probe.resource_budget(1024,0,3*GIB)
        self.assertEqual(budget['run_memory_bytes'],2*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],4*GIB)
        for native,build,available in [(True,4096,10*GIB),(511,4096,10*GIB),
                (1024,True,10*GIB),(1024,-1,10*GIB),(1024,4097,10*GIB),
                (1024,4096,-1),(1024,4096,'available'),(1024,4096,True)]:
            with self.assertRaises(ValueError):probe.resource_budget(native,build,available)
        with self.assertRaises(ValueError):probe.resource_budget(4096,4096,20*GIB)


if __name__=='__main__':unittest.main()
