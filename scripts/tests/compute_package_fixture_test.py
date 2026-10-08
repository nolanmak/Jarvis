"""Hermetic registry responses; real broker, pinned pip, and KVM preparation.

Only the broker's external fetch subprocess is replaced. No runtime option,
environment variable, public registry, or production helper is modified.
"""
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

SPEC = importlib.util.spec_from_file_location('fixture_compute', Path(__file__).parents[1] / 'code-mode-compute.py')
compute = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compute)


def request(**changes):
    return {'runtime': 'python', 'dependencies': [], 'code': 'print(60)', **changes}


def wheel(requirement=None, name='jarvis-probe', version='1.0'):
    buffer = io.BytesIO()
    module = name.replace('-', '_')
    info = module + '-' + version + '.dist-info/'
    with zipfile.ZipFile(buffer, 'w') as archive:
        metadata = f'Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n'
        if requirement:
            metadata += 'Requires-Dist: ' + requirement + '\n'
        archive.writestr(info + 'METADATA', metadata)
        archive.writestr(info + 'WHEEL',
                         'Wheel-Version: 1.0\nGenerator: harmless-fixture\nRoot-Is-Purelib: true\nTag: py3-none-any\n')
        archive.writestr(info + 'RECORD', '')
        archive.writestr(module + '.py', 'VALUE = 60\n')
    return buffer.getvalue()


class FixtureTransport:
    def __init__(self, mode):
        self.mode = mode
        self.calls = []
        self.original_run = subprocess.run
        self.name = 'packaging' if mode == 'shadow' else 'jarvis-probe'
        self.version = '1434.0' if mode == 'shadow' else '1.0'
        self.filename = self.name.replace('-', '_') + '-' + self.version + '-py3-none-any.whl'
        origin = 'files.pythonhosted.org' if mode == 'registry_url' else 'attacker.invalid'
        self.body = wheel('other @ https://' + origin + '/packages/other-1.0-py3-none-any.whl'
                          if mode in ('transitive_url', 'registry_url') else None, self.name, self.version)

    def run(self, argv, *args, **kwargs):
        if len(argv) < 6 or Path(argv[2]).name != 'build-dependency-proxy.py' or argv[3] != '--fetch':
            return self.original_run(argv, *args, **kwargs)
        route = json.loads(Path(argv[4]).read_text())['route']
        self.calls.append(route)
        if route == '/pypi/simple/' + self.name + '/':
            filename = 'jarvis-probe-1.0.tar.gz' if self.mode == 'source_only' else self.filename
            body = json.dumps({'meta': {'api-version': '1.0'}, 'name': self.name, 'files': [{
                'filename': filename, 'url': 'https://files.pythonhosted.org/packages/' + filename,
                'hashes': {'sha256': hashlib.sha256(self.body).hexdigest()}}]}).encode()
        elif route == '/pypi-files/packages/' + self.filename:
            body = self.body + (b'altered' if self.mode == 'altered_wheel' else b'')
        else:
            raise AssertionError('unexpected registry request: ' + route)
        Path(argv[5]).write_bytes(body)
        return subprocess.CompletedProcess(argv, 0, json.dumps({'status': 200, 'content_type': 'application/octet-stream'}).encode())


@unittest.skipUnless(os.environ.get('JARVIS_TEST_VM_CONFIG') and os.environ.get('JARVIS_TEST_COMPUTE_PIP'),
                     'requires KVM and pinned pip; no public network is used')
class PackageFixtureVMTests(unittest.TestCase):
    def test_locked_packages_override_runtime_packages_and_are_absent_without_dependencies(self):
        self.assertIsNotNone(importlib.util.find_spec('packaging'), 'host parser is a required runtime prerequisite')
        transport = FixtureTransport('shadow')
        with tempfile.TemporaryDirectory() as tmp:
            backend = compute.VMBackend(os.environ['JARVIS_TEST_VM_CONFIG'],
                os.environ['JARVIS_TEST_COMPUTE_SCRATCH'],
                json.loads(Path(os.environ['JARVIS_TEST_COMPUTE_PIP']).read_text()))
            try:
                task = compute.ComputeTask(backend, compute.ArtifactStore(Path(tmp)), enabled=True)
                with patch.object(subprocess, 'run', transport.run):
                    result = task.execute(request(dependencies=['packaging==1434.0'], code=
                        "import packaging\nassert packaging.__file__.startswith('/cache/envs/'), packaging.__file__\nprint(packaging.VALUE)", timeoutSecs=30))
                    self.assertTrue(result['ok'], result)
                    self.assertEqual(result['stdout'].strip(), '60')
                    self.assertEqual(result['dependencyLock'], [{'name': 'packaging', 'version': '1434.0',
                        'sha256': hashlib.sha256(transport.body).hexdigest()}])
                    empty = task.execute(request(code="import importlib.util\nassert importlib.util.find_spec('packaging') is None\nprint(60)", timeoutSecs=15))
                    self.assertTrue(empty['ok'], empty)
                    self.assertEqual(empty['dependencyLock'], [])
                    self.assertEqual(empty['stdout'].strip(), '60')
                    self.assertEqual(len(transport.calls), 2)
            finally:
                backend.close()

    def check_refusal(self, mode, expected):
        transport = FixtureTransport(mode)
        scratch = Path(os.environ['JARVIS_TEST_COMPUTE_SCRATCH'])
        before = set(scratch.glob('jarvis-vm-session-*'))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sentinel = root / 'unselected-host-sentinel'
            sentinel.write_bytes(b'unchanged')
            store = root / 'artifacts'; store.mkdir(mode=0o700)
            backend = compute.VMBackend(os.environ['JARVIS_TEST_VM_CONFIG'], scratch,
                json.loads(Path(os.environ['JARVIS_TEST_COMPUTE_PIP']).read_text()))
            try:
                task = compute.ComputeTask(backend, compute.ArtifactStore(store), enabled=True)
                with patch.object(subprocess, 'run', transport.run):
                    result = task.execute(request(dependencies=['jarvis-probe==1.0'],
                        code="raise AssertionError('task code must not run')", outputs=['never'], timeoutSecs=30))
                self.assertFalse(result['ok'], result)
                diagnostic = '\n'.join((store / log['file']).read_text(errors='replace')
                    for log in task.records[-1]['preparation']['logs'].values())
                self.assertEqual(result['error']['code'], expected, (result, diagnostic))
                self.assertEqual(result['runner'], 'vm', result)
                self.assertEqual(result['artifacts'], [])
                self.assertEqual(result['dependencyLock'], [])
                self.assertFalse(result['environmentReused'])
                self.assertEqual(task.environments, {})
                record = task.records[-1]
                self.assertEqual([p['phase'] for p in record['phases']], ['admission', 'prepare'])
                self.assertTrue(record['cleanupVerified'])
                self.assertTrue(record['preparation']['cleanupVerified'])
                self.assertEqual(transport.calls, ['/pypi/simple/jarvis-probe/'] +
                    ([] if mode == 'source_only' else ['/pypi-files/packages/' + transport.filename]))
                self.assertEqual(sentinel.read_bytes(), b'unchanged')
            finally:
                backend.close()
        self.assertEqual(set(scratch.glob('jarvis-vm-session-*')), before)

    def test_source_only_is_unavailable_without_downloading_source(self):
        self.check_refusal('source_only', 'dependency_unavailable')

    def test_transitive_url_is_policy_denied_without_requesting_url(self):
        for mode in ('transitive_url', 'registry_url'):
            with self.subTest(mode=mode):
                self.check_refusal(mode, 'dependency_policy_denied')

    def test_altered_wheel_has_integrity_failure_before_install_or_execution(self):
        self.check_refusal('altered_wheel', 'dependency_integrity')

    def test_valid_fixture_installs_and_executes_offline(self):
        transport = FixtureTransport('valid')
        with tempfile.TemporaryDirectory() as tmp:
            backend = compute.VMBackend(os.environ['JARVIS_TEST_VM_CONFIG'],
                os.environ['JARVIS_TEST_COMPUTE_SCRATCH'],
                json.loads(Path(os.environ['JARVIS_TEST_COMPUTE_PIP']).read_text()))
            try:
                task = compute.ComputeTask(backend, compute.ArtifactStore(Path(tmp)), enabled=True)
                with patch.object(subprocess, 'run', transport.run):
                    result = task.execute(request(dependencies=['jarvis-probe==1.0'],
                        code='import jarvis_probe; print(jarvis_probe.VALUE)', timeoutSecs=30))
                self.assertTrue(result['ok'], result)
                self.assertEqual(result['runner'], 'vm')
                self.assertEqual(result['stdout'].strip(), '60')
                self.assertEqual(result['dependencyLock'], [{'name': 'jarvis-probe', 'version': '1.0',
                    'sha256': hashlib.sha256(transport.body).hexdigest()}])
                self.assertTrue(task.records[-1]['cleanupVerified'])
                self.assertEqual(len(transport.calls), 2)
            finally:
                backend.close()
