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
    def test_policy_timing_audit_rejects_inconsistent_metadata(self):
        import copy
        import hashlib
        import json
        import tempfile
        from types import SimpleNamespace
        from unittest.mock import patch
        policy = {'enabled': True, 'callTimeoutSecs': 90, 'taskTimeoutSecs': 180,
                  'implementation': {name: qa.digest(qa.REPO / 'scripts' / name) for name in
                    ('code-mode-compute.py', 'code-mode-compute-guest.py', 'code-mode-compute-prepare.py', 'build-dependency-proxy.py',
                     'codex-build-vm.py', 'codex-tool-bridge.py', 'provider-supervisor.py')}}
        fingerprint = hashlib.sha256(json.dumps(policy, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        rows = []
        for names in [('admission', 'prepare'), ('admission', 'prepare', 'execute', 'export'), ('admission', 'execute', 'export')]:
            rows.append({'policy': policy, 'policyFingerprint': fingerprint, 'startedMonotonic': 0,
                         'deadlineMonotonic': 90, 'taskDeadlineMonotonic': 180, 'elapsedSecs': len(names),
                         'phases': [{'phase': name, 'startedMonotonic': i, 'endedMonotonic': i+1, 'elapsedSecs': 1}
                                    for i, name in enumerate(names)]})
        with tempfile.TemporaryDirectory() as tmp, patch.object(qa, 'preparation_audit', lambda *_: None):
            report = Path(tmp) / 'report.json'
            h = SimpleNamespace(results={'fixture': {'report': str(report)}})
            report.write_text(json.dumps({'records': rows}))
            qa.policy_timings_audit(h, 'fixture')
            for mutation in ('fingerprint', 'gap', 'elapsed', 'deadline'):
                bad = copy.deepcopy(rows)
                if mutation == 'fingerprint': bad[0]['policyFingerprint'] = '0' * 64
                if mutation == 'gap': bad[0]['phases'][1]['startedMonotonic'] += 1
                if mutation == 'elapsed': bad[1]['elapsedSecs'] += 1
                if mutation == 'deadline': bad[2]['taskDeadlineMonotonic'] += 1
                report.write_text(json.dumps({'records': bad}))
                with self.subTest(mutation=mutation), self.assertRaises(AssertionError):
                    qa.policy_timings_audit(h, 'fixture')

    def test_bridge_regression_requires_full_suite_and_public_prerequisites(self):
        self.assertIn('bridge_regression', qa.CASES)
        from types import SimpleNamespace
        from unittest.mock import Mock
        h = SimpleNamespace(suite=Mock())
        qa.CASES['bridge_regression'](h, 'bridge_regression')
        args, options = h.suite.call_args
        self.assertIn('scripts.tests.codex_tool_bridge_test', args[1])
        self.assertTrue(options['public'])

    def test_cli_case_cannot_pass_after_mutating_unselected_host_sentinel(self):
        import tempfile
        from types import SimpleNamespace
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args = SimpleNamespace(output_dir=root, bin='/fixture-cli', vm_config=root / 'runtime', scratch_root=root)
            def mutate(argv, cwd, env, log, timeout):
                (cwd / 'unselected-host-sentinel').write_bytes(b'changed')
                return {'failure': None, 'exitCode': 2}
            with patch.object(qa, 'command', mutate), self.assertRaisesRegex(AssertionError, 'host sentinel changed'):
                qa.Harness(args, '/deno').cli('fixture', 'pass', expect_exit=2)

    def test_preparation_audit_is_registered_for_real_cli_acceptance(self):
        self.assertIn('preparation_audit', qa.CASES)

    def test_native_wheel_is_required_and_drives_the_cli(self):
        self.assertIn('native_wheel', qa.REQUIREMENTS['AC02'])
        self.assertIn('native_wheel', qa.CASES)
        from types import SimpleNamespace
        from unittest.mock import Mock
        h = SimpleNamespace(args=SimpleNamespace(public_packages=False), cli=Mock())
        with self.assertRaises(AssertionError):
            qa.native_wheel(h, 'native_wheel')
        h.cli.assert_not_called()

    def test_dependency_lifecycle_cases_require_real_cli_evidence(self):
        for name in ('fresh_task', 'changed_constraints', 'failed_preparation'):
            with self.subTest(name=name):
                self.assertIn(name, qa.CASES)

    def test_failed_preparation_check_rejects_cached_failure_and_execution(self):
        from types import SimpleNamespace
        import copy
        class FakeHarness:
            args = SimpleNamespace(public_packages=True)
            def cli(self, name, source, **kwargs):
                kwargs['verify'](self.value, None)
        h = FakeHarness()
        failure = {'ok': False, 'runner': 'vm', 'error': {'code': 'dependency_unavailable'},
                   'artifacts': [], 'dependencyLock': [], 'environmentReused': False, 'stdout': ''}
        success = {'ok': True, 'runner': 'vm', 'stdout': '60', 'environmentReused': False}
        records = [{'runner': 'vm', 'cleanupVerified': True, 'environmentReused': False,
                    'dependencyLock': [], 'error': failure['error']} for _ in range(2)]
        records.append({'runner': 'vm', 'cleanupVerified': True, 'error': None})
        good = {'final': [failure, copy.deepcopy(failure), success], 'records': records, 'artifacts': []}
        h.value = good
        qa.failed_preparation(h, 'fixture')
        for field, value in [('environmentReused', True), ('stdout', 'EXECUTION_MUST_NOT_RUN'),
                             ('dependencyLock', [{'name': 'unexpected'}]), ('ok', True)]:
            h.value = copy.deepcopy(good)
            h.value['final'][1][field] = value
            with self.subTest(field=field), self.assertRaises(AssertionError):
                qa.failed_preparation(h, 'fixture')

    def test_changed_constraints_check_rejects_reuse_and_hidden_network(self):
        from types import SimpleNamespace
        import copy
        class FakeHarness:
            args = SimpleNamespace(public_packages=True)
            def cli(self, name, source, **kwargs):
                kwargs['verify'](self.value, None)
        h = FakeHarness()
        records = [{'environmentReused': reused, 'downloads': {'requests': requests, 'bytes': requests * 100},
                    'dependencyLock': [{'name': 'fixture', 'sha256': 'a' * 64}], 'cleanupVerified': True}
                   for reused, requests in [(False, 1), (False, 1), (True, 0)]]
        good = {'final': [{'ok': True, 'runner': 'vm', 'stdout': '60'} for _ in records], 'records': records}
        h.value = good
        qa.changed_constraints(h, 'fixture')
        for index, field, value in [(1, 'environmentReused', True), (1, 'downloads', {'requests': 0, 'bytes': 0}),
                                     (2, 'downloads', {'requests': 1, 'bytes': 100}), (2, 'dependencyLock', [])]:
            h.value = copy.deepcopy(good)
            h.value['records'][index][field] = value
            with self.subTest(index=index, field=field), self.assertRaises(AssertionError):
                qa.changed_constraints(h, 'fixture')

    def test_concurrent_admission_requires_both_real_cli_commands(self):
        self.assertIn('concurrent_admission', qa.CASES)
        self.assertEqual(qa.MULTI_COMMAND_CASES['concurrent_admission'], 2)

    def test_byte_boundaries_cannot_pass_without_all_cli_commands(self):
        self.assertIn('byte_boundaries', qa.CASES)
        from types import SimpleNamespace
        from unittest.mock import patch
        class Probe:
            args = SimpleNamespace()
            deno = '/deno'
            def __init__(self):
                self.results = {}
        parent = Probe()
        class MissingEvidence:
            def __init__(self, *_):
                self.results = {}
            def cli(self, *_args, **_kwargs):
                pass
        with patch.object(qa, 'Harness', MissingEvidence), self.assertRaises((AssertionError, KeyError)):
            qa.byte_boundaries(parent, 'fixture')

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
