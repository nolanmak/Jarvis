"""Actual production controller with real kernel notifications and supervision."""
import errno
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
class ExecControllerTests(unittest.TestCase):
    def run_case(self,mode,expected):
        library=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        early=Path(os.environ.get('AUGMENTAGENT_CARGO_EXEC_EARLY_LIBRARY',library.parent/'libci_cargo_exec_early.so'))
        executable=Path(os.environ.get('AUGMENTAGENT_CARGO_EXEC_STATIC_BINARY',library.parent/'ci_cargo_exec_static'))
        controller=os.environ.get('AUGMENTAGENT_TEST_EXEC_CONTROLLER',str(ROOT/'scripts/ci-cargo-planner.py'))
        with tempfile.TemporaryDirectory() as directory:
            receipt=Path(directory)/'cleanup'
            result=subprocess.run([sys.executable,'-I',str(ROOT/'crates/augmentagent-cli/src/ci_cargo_exec_mediation_cases.py'),
                '--controller',controller,str(ROOT/'scripts/provider-supervisor.py'),str(library),str(early),str(executable),mode,str(receipt)],
                capture_output=True,text=True,timeout=15,
                env={key:value for key,value in os.environ.items() if key in ('HOME','PATH','LANG','LC_ALL')})
            self.assertEqual(result.returncode,0,result.stderr)
            lines=result.stdout.splitlines();self.assertEqual(lines[:-1],expected)
            record=json.loads(lines[-1]);self.assertEqual(record['controller_hold'],None if mode in ('cancel','clone3-fallback') else 'input_census_unknown')
            self.assertTrue(record['original_cleanup']);self.assertFalse(record['shipping_authority'])
            for row in record.get('policy_records',[]):
                self.assertEqual(row['response_errno'],errno.ENOSYS)
                self.assertFalse(row['allowed']);self.assertTrue(row['after_ack'])
            return record

    def test_constructor_denial_reaches_typed_controller_hold(self):self.run_case('early',['EARLY_DENIED','BODY_DENIED'])
    def test_static_denial_reaches_typed_controller_hold(self):self.run_case('static',['STATIC_DENIED'])
    def test_path_search_preserves_first_exec_and_later_hold(self):self.run_case('search',['BODY_DENIED'])
    def test_descendant_denial_reaches_typed_controller_hold(self):self.run_case('fork',['BODY_DENIED'])
    def test_cancelled_staging_preserves_original_cleanup_without_exec_ack(self):self.run_case('cancel',[])

    def test_kernel_vfork_refusal_is_an_unsupported_route_even_when_child_exits_zero(self):
        record=self.run_case('vfork-policy',['POLICY_DENIED'])
        self.assertEqual([r['disposition'] for r in record['policy_records']],['unsupported_vfork'])

    def test_clone_value_flags_distinguish_vfork_and_separate_shared_mm(self):
        record=self.run_case('clone-policy',['POLICY_DENIED'])
        self.assertEqual([r['disposition'] for r in record['policy_records']],
            ['unsupported_vfork','unsupported_shared_mm'])
        self.assertEqual([r['clone_flags'] for r in record['policy_records']],[0x4111,0x111])

    def test_expected_clone3_fallback_is_observed_without_becoming_an_unsupported_route(self):
        record=self.run_case('clone3-fallback',['POLICY_DENIED'])
        self.assertGreaterEqual(len(record['policy_records']),1)
        self.assertTrue(all(r['disposition']=='clone3_fallback' for r in record['policy_records']))

    def test_original_listener_observes_descendant_vfork_refusal_and_reaps_both(self):
        record=self.run_case('vfork-descendant',['POLICY_DENIED'])
        self.assertEqual([r['disposition'] for r in record['policy_records']],['unsupported_vfork'])


if __name__=='__main__':unittest.main()
