"""The CI switch that turns a missing sandbox from a skip into a failure (#1048).

Hosts that cannot enforce the command sandbox skip those tests locally. CI sets
REQUIRE_ENFORCEABLE_SANDBOX=1 so a runner that loses Landlock, libseccomp or
Poppler fails the job instead of passing with zero sandbox coverage. These
tests fake a non-enforceable report; no probe of this host is involved.
"""
import importlib.util
import io
import os
from pathlib import Path
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    'host_capabilities', Path(__file__).with_name('host_capabilities.py'))
capabilities = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(capabilities)

FAKE_REASON = 'kernel Landlock ABI is 3; the command sandbox needs ABI 6+ (Linux 6.12+)'


def run_case(case):
    """Run one synthetic TestCase class and return its result."""
    result = unittest.TestResult()
    unittest.defaultTestLoader.loadTestsFromTestCase(case).run(result)
    return result


def switch(value):
    environment = {key: item for key, item in os.environ.items() if key != capabilities.REQUIRE_ENV}
    if value is not None:
        environment[capabilities.REQUIRE_ENV] = value
    return mock.patch.dict(os.environ, environment, clear=True)


class RequirementTests(unittest.TestCase):
    def synthetic_cases(self, reason):
        ran = []

        class Method(unittest.TestCase):
            @capabilities.requirement(reason)
            def test_confined(self):
                ran.append('method')

        @capabilities.requirement(reason)
        class Whole(unittest.TestCase):
            def test_confined(self):
                ran.append('class')

        return Method, Whole, ran

    def test_unavailable_capability_skips_with_its_reason_when_the_switch_is_off(self):
        for value in (None, '', '0'):
            with self.subTest(switch=value), switch(value):
                for case in self.synthetic_cases(FAKE_REASON)[:2]:
                    result = run_case(case)
                    self.assertTrue(result.wasSuccessful())
                    self.assertEqual([reason for _, reason in result.skipped], [FAKE_REASON])

    def test_unavailable_capability_fails_when_the_switch_is_on(self):
        with switch('1'):
            *cases, ran = self.synthetic_cases(FAKE_REASON)
            for case in cases:
                result = run_case(case)
                self.assertFalse(result.wasSuccessful())
                self.assertEqual(result.skipped, [])
                self.assertEqual(len(result.failures), 1)
                message = result.failures[0][1]
                self.assertIn(capabilities.REQUIRE_ENV + '=1', message)
                self.assertIn(FAKE_REASON, message)
            self.assertEqual(ran, [], 'a failing requirement must not run the confined body')

    def test_available_capability_runs_regardless_of_the_switch(self):
        for value in (None, '1'):
            with self.subTest(switch=value), switch(value):
                *cases, ran = self.synthetic_cases(None)
                for case in cases:
                    self.assertTrue(run_case(case).wasSuccessful())
                self.assertEqual(sorted(ran), ['class', 'method'])


class ReportTests(unittest.TestCase):
    def report(self, value, sandbox=None, poppler=None):
        out, err = io.StringIO(), io.StringIO()
        with switch(value), \
                mock.patch.object(capabilities, 'landlock_abi', return_value=3), \
                mock.patch.object(capabilities, 'sandbox_unavailable_reason', return_value=sandbox), \
                mock.patch.object(capabilities, 'poppler_unavailable_reason', return_value=poppler), \
                mock.patch('sys.stdout', out), mock.patch('sys.stderr', err):
            return capabilities.main(), out.getvalue(), err.getvalue()

    def test_report_fails_only_when_the_switch_is_on_and_enforcement_is_missing(self):
        code, out, _ = self.report(None, sandbox=FAKE_REASON)
        self.assertEqual(code, 0)
        self.assertIn('command sandbox: ' + FAKE_REASON, out)

        code, _, err = self.report('1', sandbox=FAKE_REASON)
        self.assertEqual(code, 1)
        self.assertIn(capabilities.REQUIRE_ENV + '=1', err)
        self.assertIn(FAKE_REASON, err)

        missing = 'Poppler is not installed (apt install poppler-utils): missing /usr/bin/pdftoppm'
        code, _, err = self.report('1', poppler=missing)
        self.assertEqual(code, 1)
        self.assertIn(missing, err)

    def test_report_passes_with_the_switch_on_when_everything_is_enforceable(self):
        code, out, err = self.report('1')
        self.assertEqual(code, 0)
        self.assertIn('command sandbox: enforceable', out)
        self.assertIn('PDF renderer: installed', out)
        self.assertEqual(err, '')


if __name__ == '__main__':
    unittest.main()
