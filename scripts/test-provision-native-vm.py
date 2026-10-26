#!/usr/bin/env python3
"""Private fixture-only profile union tests; no runtime, network or model calls."""
import importlib.util
import json
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('native_provision', Path(__file__).with_name('provision-native-vm-probe.py'))
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


def fake_stage(destination, native, bases=()):
    destination.mkdir(mode=0o700)
    records = {}
    for base in bases:
        p.copy_base_runtime(base, destination, records)
    source = destination.parent / ('source-' + native.name)
    source.write_bytes(native.name.encode())
    try:
        p.copy_runtime_file(destination, 'lib/' + native.name + '.so', source, records)
    finally:
        source.unlink()
    (destination / 'manifest.json').write_text(json.dumps({'version': 1, 'packages': [],
        'files': [{'path': name, 'sha256': details[0]} for name, details in records.items()]}))
    (destination / 'manifest.json').chmod(0o400)


class ProvisionTests(unittest.TestCase):
    def args(self, root, provider, extend=None):
        gateway = root / (provider + '-gateway.json')
        gateway.write_text(json.dumps({'version': 1, 'authorities': [{'authority': provider + '.example.com:443',
            'tls': True, 'allow_loopback': False,
            'routes': [{'path_prefix': '/v1/', 'methods': ['POST'], 'websocket': False}]}]}))
        return types.SimpleNamespace(directory=root / 'deployment' if extend is None else None,
            extend_policy=extend, runtime_config=root / 'runtime.json' if extend is None else None,
            provider=provider, native_executable=Path(provider), auth_file=None, allow_auth_refresh=False,
            gateway_policy=gateway, environment_key=[], memory_mb=None, max_state_bytes=None, max_sessions=None)

    def test_extension_preserves_existing_grants_state_and_runtime_dependencies(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            path = p.provision(self.args(root, 'codex'))
            original = json.loads(path.read_text())
            state = Path(original['state_root'])
            marker = state / 'durable-native-session'
            marker.write_bytes(b'keep session identity')
            previous_runtime = Path(original['guest_runtime'])
            p.provision(self.args(root, 'claude', path))
            result = json.loads(path.read_text())
            self.assertEqual(result['providers']['codex'], original['providers']['codex'])
            self.assertEqual(result['state_root'], original['state_root'])
            self.assertEqual(marker.read_bytes(), b'keep session identity')
            self.assertTrue(previous_runtime.is_dir(), 'active runs may still read the previous generation')
            runtime = Path(result['guest_runtime'])
            self.assertEqual((runtime / 'lib/codex.so').read_bytes(), b'codex')
            self.assertEqual((runtime / 'lib/claude.so').read_bytes(), b'claude')
            p.helper('native-provider-vm.py').validate_runtime(runtime)
            for provider in ['qwen', 'glm']:
                p.provision(self.args(root, provider, path))
            final = json.loads(path.read_text())
            self.assertEqual(set(final['providers']), {'codex', 'claude', 'qwen', 'glm'})
            self.assertEqual(final['providers']['claude'], result['providers']['claude'])

    def test_extension_refuses_global_policy_changes(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            path = p.provision(self.args(root, 'codex'))
            original = path.read_bytes()
            args = self.args(root, 'claude', path)
            args.memory_mb = 4096
            with self.assertRaisesRegex(ValueError, 'global runtime/resource policy'):
                p.provision(args)
            self.assertEqual(path.read_bytes(), original)

    def test_owner_policy_edit_during_staging_wins(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            path = p.provision(self.args(root, 'codex'))
            changed = json.loads(path.read_text())
            changed['max_sessions'] = 3
            changed_bytes = json.dumps(changed).encode()
            def concurrent(destination, native, bases):
                fake_stage(destination, native, bases)
                path.write_bytes(changed_bytes)
            with patch.object(p, 'stage_runtime', concurrent):
                with self.assertRaisesRegex(ValueError, 'changed during extension'):
                    p.provision(self.args(root, 'claude', path))
            self.assertEqual(path.read_bytes(), changed_bytes)
            self.assertEqual(len(list(path.parent.glob('runtime-*'))), 1)

    def test_duplicate_profile_never_replaces_existing_routes(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            path = p.provision(self.args(root, 'codex'))
            original = path.read_bytes()
            with self.assertRaisesRegex(ValueError, 'already declared'):
                p.provision(self.args(root, 'codex', path))
            self.assertEqual(path.read_bytes(), original)

    def test_conflicting_runtime_cannot_publish_or_mutate_old_generation(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            path = p.provision(self.args(root, 'codex'))
            original = path.read_bytes()
            previous = Path(json.loads(original)['guest_runtime'])
            def conflict(destination, native, bases):
                destination.mkdir(mode=0o700)
                records = {}
                p.copy_base_runtime(bases[0], destination, records)
                source = root / 'conflicting-library'
                source.write_bytes(b'different ABI')
                p.copy_runtime_file(destination, 'lib/codex.so', source, records)
            with patch.object(p, 'stage_runtime', conflict):
                with self.assertRaisesRegex(ValueError, 'conflicting file content'):
                    p.provision(self.args(root, 'claude', path))
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual((previous / 'lib/codex.so').read_bytes(), b'codex')
            self.assertEqual(len(list(path.parent.glob('runtime-*'))), 1, 'unpublished generation cleaned')

    def test_tampered_base_manifest_refuses_union(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            path = p.provision(self.args(root, 'codex'))
            original = path.read_bytes()
            previous = Path(json.loads(original)['guest_runtime'])
            (previous / 'lib/codex.so').chmod(0o600)
            (previous / 'lib/codex.so').write_bytes(b'tampered')
            with self.assertRaisesRegex(ValueError, 'digest changed'):
                p.provision(self.args(root, 'claude', path))
            self.assertEqual(path.read_bytes(), original)


if __name__ == '__main__':
    unittest.main()
