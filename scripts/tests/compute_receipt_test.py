"""Synthetic receipt validation; these fixtures are not execution evidence."""
import copy
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('compute_receipt', Path(__file__).parents[1] / 'verify-code-mode-receipt.py')
receipt = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(receipt)


def fixture(head='a' * 40):
    cases = {name: {'status': 'passed', 'command': {'executed': True, 'argv': ['synthetic-fixture'],
              'exitCode': 0, 'failure': None}} for name in receipt.qa.GROUPS['all']}
    for name, count in receipt.qa.MULTI_COMMAND_CASES.items():
        cases[name]['commands'] = [copy.deepcopy(cases[name]['command']) for _ in range(count)]
    return {'schemaVersion': 1, 'issue': 1434, 'mode': 'all', 'requireVm': True,
            'publicPackages': True, 'ok': True, 'acceptanceComplete': True,
            'git': {'head': head, 'dirty': False}, 'binaryUnchangedDuringQa': True,
            'binary': {'sha256': 'b' * 64}, 'runtimeManifest': {'sha256': 'c' * 64},
            'versions': [{'executed': True, 'exitCode': 0, 'failure': None, 'value': 'synthetic'} for _ in range(3)],
            'cases': cases, 'acceptance': receipt.qa.coverage_report(receipt.qa.REQUIREMENTS, cases)}


class ReceiptTests(unittest.TestCase):
    def test_complete_report_is_accepted_and_expected_cli_refusals_are_allowed(self):
        report = fixture()
        report['cases']['disabled']['command']['exitCode'] = 1
        receipt.validate(report, 'a' * 40)

    def test_stale_dirty_partial_and_unenforced_reports_are_rejected(self):
        for key, value in [('mode', 'smoke'), ('requireVm', False), ('publicPackages', False),
                           ('ok', False), ('acceptanceComplete', False), ('binaryUnchangedDuringQa', False),
                           ('git', {'head': 'd' * 40, 'dirty': False}),
                           ('git', {'head': 'a' * 40, 'dirty': True}), ('versions', [])]:
            with self.subTest(key=key, value=value):
                report = fixture()
                report[key] = value
                with self.assertRaises(ValueError):
                    receipt.validate(report, 'a' * 40)

    def test_missing_or_skipped_cases_cannot_hide_behind_green_summary(self):
        for mutation in ('missing', 'skipped', 'no-command', 'not-executed', 'failed-command'):
            report = fixture()
            case = report['cases']['xlsx']
            if mutation == 'missing': del report['cases']['xlsx']
            if mutation == 'skipped': case['status'] = 'skipped'
            if mutation == 'no-command': del case['command']
            if mutation == 'not-executed': case['command']['executed'] = False
            if mutation == 'failed-command': case['command']['failure'] = 'timed out'
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                receipt.validate(report, 'a' * 40)

    def test_multi_command_cases_require_every_completed_command(self):
        for name in receipt.qa.MULTI_COMMAND_CASES:
            for mutation in ('missing', 'short', 'not-executed', 'failure'):
                report = fixture()
                case = report['cases'][name]
                if mutation == 'missing': del case['commands']
                if mutation == 'short': case['commands'].pop(0)
                if mutation == 'not-executed': case['commands'][0]['executed'] = False
                if mutation == 'failure': case['commands'][0]['failure'] = 'timed out'
                with self.subTest(name=name, mutation=mutation), self.assertRaises(ValueError):
                    receipt.validate(report, 'a' * 40)

    def test_acceptance_map_must_match_current_contract(self):
        report = fixture()
        report['acceptance'] = copy.deepcopy(report['acceptance'])
        del report['acceptance']['AC18']
        with self.assertRaises(ValueError):
            receipt.validate(report, 'a' * 40)
