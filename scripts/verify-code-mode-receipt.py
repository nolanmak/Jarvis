#!/usr/bin/env python3
"""Check a complete Code Mode acceptance report against the current contract."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import sys

SPEC = importlib.util.spec_from_file_location('compute_acceptance', Path(__file__).with_name('qa-code-mode-compute.py'))
qa = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qa)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def completed_command(command):
    return (isinstance(command, dict) and command.get('executed') is True
            and isinstance(command.get('argv'), list) and bool(command['argv'])
            and type(command.get('exitCode')) is int and command['exitCode'] >= 0
            and command.get('failure', 'missing') is None)


def validate(report, head):
    require(isinstance(report, dict), 'receipt must be an object')
    require(report.get('schemaVersion') == 1 and report.get('issue') == 1434, 'unsupported receipt schema')
    require(report.get('mode') == 'all' and report.get('requireVm') is True
            and report.get('publicPackages') is True, 'required VM/public-package all-mode run missing')
    require(report.get('ok') is True and report.get('acceptanceComplete') is True, 'acceptance is incomplete')
    require(report.get('git', {}).get('head') == head and report.get('git', {}).get('dirty') is False,
            'receipt is stale or source was dirty')
    require(report.get('binaryUnchangedDuringQa') is True, 'binary changed during QA')
    for name in ('binary', 'runtimeManifest'):
        require(re.fullmatch('[0-9a-f]{64}', report.get(name, {}).get('sha256', '')) is not None,
                'binary/runtime fingerprint missing')
    versions = report.get('versions', [])
    require(isinstance(versions, list) and len(versions) == 3
            and all(version.get('executed') is True and version.get('exitCode') == 0
                    and version.get('failure', 'missing') is None and bool(version.get('value'))
                    for version in versions), 'runtime version evidence missing')
    require(set(qa.REQUIREMENTS) == {f'AC{number:02}' for number in range(1, 19)},
            'acceptance contract must contain AC01 through AC18')
    cases = report.get('cases', {})
    require(isinstance(cases, dict), 'case evidence missing')
    for name in qa.GROUPS['all']:
        case = cases.get(name, {})
        command = case.get('command', {})
        require(case.get('status') == 'passed', f'required case incomplete: {name}')
        require(completed_command(command), f'command evidence missing or failed: {name}')
        if name in qa.MULTI_COMMAND_CASES:
            commands = case.get('commands')
            require(isinstance(commands, list) and len(commands) == qa.MULTI_COMMAND_CASES[name]
                    and all(completed_command(entry) for entry in commands),
                    f'multi-command evidence missing or failed: {name}')
    require(all(case.get('status') == 'passed' for case in cases.values()), 'report contains a failed or skipped case')
    expected = qa.coverage_report(qa.REQUIREMENTS, cases)
    require(report.get('acceptance') == expected and all(row['complete'] for row in expected.values()),
            'acceptance map does not match the current required cases')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('receipt', type=Path)
    parser.add_argument('--head', required=True)
    args = parser.parse_args()
    try:
        fd = os.open(args.receipt, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            metadata = os.fstat(stream.fileno())
            require(stat.S_ISREG(metadata.st_mode) and metadata.st_size <= 8 * 1024 * 1024,
                    'receipt must be a bounded regular file')
            data = stream.read(8 * 1024 * 1024 + 1)
            require(len(data) <= 8 * 1024 * 1024, 'receipt exceeds size limit')
        validate(json.loads(data), args.head)
    except (OSError, ValueError, TypeError, AttributeError, KeyError):
        # Reports can contain task paths. Do not copy their content into the
        # public hook diagnostic when JSON or a field is malformed.
        print('compute acceptance receipt missing, stale, malformed or incomplete', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
