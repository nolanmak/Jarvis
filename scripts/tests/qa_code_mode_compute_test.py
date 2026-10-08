import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('compute_qa', Path(__file__).parents[1] / 'qa-code-mode-compute.py')
qa = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qa)


class EvidenceContractTests(unittest.TestCase):
    def test_missing_evidence_cannot_complete_an_acceptance_row(self):
        result = qa.coverage_report({'AC01': ['disabled', 'unauthorized']},
                                    {'disabled': {'status': 'passed'}})
        self.assertFalse(result['AC01']['complete'], 'one passing case is not evidence for the whole criterion')

    def test_empty_or_failed_cases_cannot_complete_acceptance(self):
        for results in ({}, {'first': {'status': 'failed'}}, {'first': {'status': 'not_implemented'}}):
            with self.subTest(results=results):
                self.assertFalse(qa.coverage_report({'AC01': ['first']}, results)['AC01']['complete'])

    def test_required_suite_skips_and_zero_matching_tests_are_failures(self):
        for output in ('test result: ok. 0 passed; 0 failed; 0 ignored;',
                       'test result: ok. 1 passed; 0 failed; 1 ignored;',
                       'Ran 1 test in 0.01s\nOK (skipped=1)', 'ok | 3 passed | 0 failed | 1 ignored', ''):
            with self.subTest(output=output):
                self.assertFalse(qa.verify_suite_output(0, output))
        self.assertFalse(qa.verify_suite_output(1, 'Ran 1 test in 0.01s\nOK'))

    def test_completed_named_evidence_and_real_test_counts_are_accepted(self):
        rows = qa.coverage_report({'AC01': ['first', 'second']},
                                  {'first': {'status': 'passed'}, 'second': {'status': 'passed'}})
        self.assertTrue(rows['AC01']['complete'])
        self.assertTrue(qa.verify_suite_output(0, 'test result: ok. 3 passed; 0 failed; 0 ignored;'))
        self.assertTrue(qa.verify_suite_output(0, 'Ran 2 tests in 0.01s\n\nOK\n'))


class HarnessExecutionTests(unittest.TestCase):
    def test_missing_prerequisite_writes_failed_report_for_all_criteria(self):
        import contextlib
        import io
        import json
        import tempfile
        with tempfile.TemporaryDirectory() as tmp, contextlib.redirect_stdout(io.StringIO()):
            output = Path(tmp) / 'results'
            code = qa.main(['--bin', str(Path(tmp) / 'missing-cli'), '--vm-config', str(Path(tmp) / 'missing-runtime'),
                            '--scratch-root', tmp, '--output-dir', str(output), '--require-vm', '--cases', 'all'])
            report = json.loads((output / 'report.json').read_text())
            self.assertEqual(code, 1)
            self.assertFalse(report['ok'])
            self.assertFalse(report['acceptanceComplete'])
            self.assertEqual(len(report['acceptance']), 18)
            self.assertTrue(all(not row['complete'] for row in report['acceptance'].values()))
            self.assertIn('compiled CLI', report['prerequisiteFailure'])

    def test_noop_case_cannot_manufacture_passing_evidence(self):
        import contextlib
        import io
        from unittest.mock import patch
        harness = qa.Harness(None, None)
        with patch.dict(qa.CASES, {'noop_fixture': lambda *_: None}), contextlib.redirect_stdout(io.StringIO()):
            harness.run_case('noop_fixture')
        self.assertEqual(harness.results['noop_fixture']['status'], 'failed')
        self.assertIn('no command evidence', harness.results['noop_fixture']['reason'])

    def test_rust_suite_uses_build_cache_without_importing_runtime_secrets(self):
        import os
        import tempfile
        from types import SimpleNamespace
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args = SimpleNamespace(output_dir=root, public_packages=False,
                                   vm_config=root / 'runtime.json', scratch_root=root / 'scratch')
            observed = {}
            def run(argv, cwd, env, log, timeout):
                observed.update(env)
                log.write_text('test result: ok. 1 passed; 0 failed; 0 ignored;')
                return {'executed': True, 'exitCode': 0, 'failure': None}
            with patch.dict(os.environ, {'CARGO_TARGET_DIR':str(root / 'target'),
                                        'XDG_CACHE_HOME':str(root / 'build-cache'),
                                        'ORT_LIB_PATH':str(root / 'onnx'),
                                        'SYNTHETIC_PROVIDER_SECRET':'never-inherit'}, clear=True), patch.object(qa, 'command', run):
                qa.Harness(args, '/deno').suite('build', ['cargo','test'])
            self.assertEqual(observed.get('XDG_CACHE_HOME'), str(root / 'build-cache'))
            self.assertEqual(observed.get('ORT_LIB_PATH'), str(root / 'onnx'))
            self.assertNotIn('CARGO_NET_OFFLINE', observed)
            self.assertNotIn('SYNTHETIC_PROVIDER_SECRET', observed)
            self.assertEqual(observed['HOME'], str(root / 'build/home'))

    def test_command_deadline_and_log_limit_are_enforced(self):
        import os
        import sys
        import tempfile
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            env = {'PATH': os.defpath}
            limited = qa.command([sys.executable, '-c', 'import time; time.sleep(10)'], root, env, root / 'deadline.log', .05)
            self.assertIn('deadline', limited['failure'])
            self.assertLess(limited['elapsedSecs'], 2)
            with patch.object(qa, 'LOG_LIMIT', 64):
                flooded = qa.command([sys.executable, '-c', 'print("x"*1024)'], root, env, root / 'flood.log', 2)
            self.assertEqual(flooded['failure'], 'command log limit exceeded')
            self.assertEqual((root / 'flood.log').stat().st_size, 64)


if __name__ == '__main__':
    unittest.main()
