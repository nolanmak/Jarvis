"""Checkpoint admission through the real driver; GitHub is a refusing fixture."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

DRIVER = Path(__file__).with_name('autoship-github-acceptance.py')
SPEC = importlib.util.spec_from_file_location('github_acceptance', DRIVER)
driver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(driver)


class ArtifactAdmissionTests(unittest.TestCase):
    def check_resume(self, mutation, expected, github=False):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / 'test-binary'
            binary.write_bytes(b'#!/bin/sh\nexit 99\n')
            binary.chmod(0o700)
            artifact = {'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                        'bytes': binary.stat().st_size}
            checkpoint = {'version': 2, 'artifact': artifact, 'prs': {}, 'done': {}}
            mutation(checkpoint, root)
            (root / 'checkpoint.json').write_text(json.dumps(checkpoint))
            (root / 'repository.json').write_text(json.dumps({
                'repository': 'owner/autoship-acceptance-fixture',
                'marker': 'Synthetic autoship acceptance fixture; no production source or credentials.'}))
            tools = root / 'bin'
            tools.mkdir()
            gh = tools / 'gh'
            gh.write_text('#!/bin/sh\n: > "$FIXTURE_GITHUB_CALLED"\necho fixture-github-refused >&2\nexit 71\n')
            gh.chmod(0o700)
            called = root / 'github-called'
            before = (root / 'checkpoint.json').read_bytes()
            result = subprocess.run([sys.executable, str(DRIVER), '--execute',
                                     '--resume-root', str(root), '--test-binary', str(binary)],
                                    env={'PATH': str(tools) + ':/usr/bin:/bin',
                                         'FIXTURE_GITHUB_CALLED': str(called)},
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(expected, result.stderr)
            self.assertEqual(called.exists(), github,
                             'invalid evidence must refuse before any GitHub operation')
            self.assertEqual((root / 'checkpoint.json').read_bytes(), before)

    def test_unbound_historical_checkpoint_is_not_current_build_evidence(self):
        def legacy(checkpoint, root):
            checkpoint.pop('version')
            checkpoint.pop('artifact')
        self.check_resume(legacy, 'checkpoint-artifact-unbound')

    def test_different_binary_cannot_reuse_success(self):
        self.check_resume(lambda checkpoint, root: checkpoint['artifact'].update(sha256='0' * 64),
                          'checkpoint-artifact-mismatch')

    def test_legacy_completed_stage_is_not_upgraded_from_a_success_log(self):
        def stage(checkpoint, root):
            (root / 'old.log').write_text('test result: ok. 1 passed\n')
            checkpoint['done']['1-accepted'] = {'head': 'a' * 40, 'log': 'old.log'}
        self.check_resume(stage, 'checkpoint-stage-unbound')

    def test_matching_artifact_may_reach_the_github_boundary(self):
        self.check_resume(lambda checkpoint, root: None, 'fixture-github-refused', github=True)

    def test_modified_completed_log_is_not_reused(self):
        def stage(checkpoint, root):
            original = b'test result: ok. 1 passed\n'
            checkpoint['done']['1-accepted'] = {
                'head': 'a' * 40, 'artifact': checkpoint['artifact'],
                'log': 'stage.log', 'log_sha256': hashlib.sha256(original).hexdigest()}
            (root / 'stage.log').write_bytes(original + b'changed\n')
        self.check_resume(stage, 'checkpoint-stage-log-mismatch')

    def test_path_replacement_cannot_substitute_the_executed_artifact(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / 'test-binary'
            binary.write_text('#!/bin/sh\necho original-artifact\n')
            binary.chmod(0o700)
            with binary.open('rb') as stream:
                artifact = driver.artifact_identity(stream)
                replacement = binary.with_suffix('.replacement')
                replacement.write_text('#!/bin/sh\necho substituted-artifact\nexit 99\n')
                replacement.chmod(0o700)
                replacement.replace(binary)
                result = driver.run_probe(stream, artifact, {'PATH': '/usr/bin:/bin'})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), 'original-artifact')

    def test_in_place_artifact_mutation_refuses_dispatch(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / 'test-binary'
            binary.write_text('#!/bin/sh\necho original-artifact\n')
            binary.chmod(0o700)
            with binary.open('rb') as stream:
                artifact = driver.artifact_identity(stream)
                binary.write_text('#!/bin/sh\necho changed-artifact\n')
                with self.assertRaisesRegex(ValueError, 'artifact-changed-before-dispatch'):
                    driver.run_probe(stream, artifact, {'PATH': '/usr/bin:/bin'})


if __name__ == '__main__':
    unittest.main()
