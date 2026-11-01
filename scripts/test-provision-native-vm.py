#!/usr/bin/env python3
"""Private fixture-only profile union tests; no runtime, network or model calls."""
import importlib.util
import json
import hashlib
import os
import subprocess
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
    def test_codex_companion_is_manifest_bound_and_launchable_without_host_fallback(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            installed = root / 'installed'; installed.mkdir()
            native = installed / 'codex'
            native.write_text('#!/bin/sh\nexec "$(dirname "$0")/codex-code-mode-host"\n')
            native.chmod(0o755)
            companion = installed / 'codex-code-mode-host'
            companion.write_text('#!/bin/sh\nprintf "companion-started\\n"\n')
            companion.chmod(0o755)
            self.assertEqual(p.native_companions(native), [companion])
            runtime = root / 'runtime'; runtime.mkdir(mode=0o700)
            digest = hashlib.sha256(native.read_bytes()).hexdigest()
            relative = 'libexec/codex/' + digest + '/codex-code-mode-host'
            records = {}; p.copy_runtime_file(runtime, relative, companion, records)
            manifest = {'version':1,'files':[{'path':relative,'sha256':records[relative][0]}],
                'native_companions':{digest:[{'name':'codex-code-mode-host','path':relative,'sha256':records[relative][0]}]}}
            (runtime/'manifest.json').write_text(json.dumps(manifest))
            vm = p.helper('native-provider-vm.py'); vm.validate_runtime(runtime)
            destination = root/'native'; destination.mkdir()
            cli = destination/'cli'; cli.write_bytes(native.read_bytes()); cli.chmod(0o555)
            vm.stage_native_companions(native, runtime, destination)
            self.assertEqual(subprocess.check_output([str(cli)], env={'PATH':'/usr/bin:/bin'}), b'companion-started\n')
            (destination/'codex-code-mode-host').unlink()
            cli.chmod(0o755); cli.write_text('#!/bin/sh\nexit 0\n')
            with self.assertRaisesRegex(ValueError,'digest'):
                vm.stage_native_companions(native, runtime, destination)
            cli.write_bytes(native.read_bytes())
            (runtime/relative).chmod(0o600)
            (runtime/relative).write_text('changed')
            with self.assertRaisesRegex(ValueError,'digest'):
                vm.stage_native_companions(native, runtime, destination)
            companion.unlink(); companion.symlink_to(native)
            with self.assertRaisesRegex(ValueError,'trusted'):
                p.native_companions(native)

    def test_refresh_retains_generation_when_publication_succeeds_then_reports_error(self):
        for restored in (False,True):
            with tempfile.TemporaryDirectory() as name, patch.object(p,'stage_runtime',fake_stage):
                root=Path(name);policy=p.provision(self.args(root,'codex'))
                before=json.loads(policy.read_text());helper=p.helper('native-provider-vm.py');atomic=helper.atomic
                published=[]
                def write_then_fail(path,contents):
                    atomic(path,contents)
                    published.append(json.loads(contents)['guest_runtime'])
                    # Another actor can restore the pointer after a workflow
                    # observed the new generation. Pointer absence isn't proof
                    # that the generation was never published.
                    if restored:atomic(path,json.dumps(before).encode())
                    raise OSError('simulated post-publication durability failure')
                with patch.object(helper,'atomic',write_then_fail), self.assertRaises(OSError):
                    p.refresh_runtime_policy(policy,Path('/synthetic/new-runtime'))
                self.assertNotEqual(before['guest_runtime'],published[0])
                self.assertTrue(Path(published[0]).exists(),'possibly published runtime must survive uncertain completion')
                self.assertTrue(Path(before['guest_runtime']).exists())

    def test_runtime_only_refresh_preserves_grants_state_and_prior_generation(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            args = self.args(root, 'codex')
            policy = p.provision(args)
            before = json.loads(policy.read_text())
            old = Path(before['guest_runtime'])
            p.refresh_runtime_policy(policy, Path('/synthetic/companion-update'))
            after = json.loads(policy.read_text())
            self.assertNotEqual(before['guest_runtime'],after['guest_runtime'])
            self.assertTrue(old.exists())
            self.assertEqual({k:v for k,v in before.items() if k!='guest_runtime'}, {k:v for k,v in after.items() if k!='guest_runtime'})
            stable = policy.read_bytes()
            def race(destination,native,bases=()):
                fake_stage(destination,native,bases)
                policy.write_bytes(stable+b' ')
            with patch.object(p,'stage_runtime',race), self.assertRaisesRegex(ValueError,'changed during'):
                p.refresh_runtime_policy(policy,Path('/synthetic/next'))
            self.assertEqual(policy.read_bytes(),stable+b' ')

    def args(self, root, provider, extend=None):
        gateway = root / (provider + '-gateway.json')
        gateway.write_text(json.dumps({'version': 1, 'authorities': [{'authority': provider + '.example.com:443',
            'tls': True, 'allow_loopback': False,
            'routes': [{'path_prefix': '/v1/', 'methods': ['POST'], 'websocket': False}]}]}))
        return types.SimpleNamespace(directory=root / 'deployment' if extend is None else None,
            extend_policy=extend, runtime_config=root / 'runtime.json' if extend is None else None,
            provider=provider, native_executable=Path(provider), auth_file=None, allow_auth_refresh=False,
            gateway_policy=gateway, environment_key=[], memory_mb=None, max_state_bytes=None, max_sessions=None)

    def test_new_provisioning_records_fresh_native_store_origin_once(self):
        with tempfile.TemporaryDirectory() as name, patch.object(p, 'stage_runtime', fake_stage):
            root = Path(name)
            path = p.provision(self.args(root, 'codex'))
            state = Path(json.loads(path.read_text())['state_root'])
            bootstrap = state / 'authority-bootstrap.json'
            before = bootstrap.read_bytes()
            value = json.loads(before)
            self.assertEqual(set(value), {'version', 'epoch', 'root_identity_sha256'})
            self.assertEqual(value['version'], 1)
            self.assertEqual(str(p.uuid.UUID(value['epoch'])), value['epoch'])
            info = state.stat()
            expected = p.hashlib.sha256(json.dumps({'device': info.st_dev, 'inode': info.st_ino}, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
            self.assertEqual(value['root_identity_sha256'], expected)
            self.assertEqual(bootstrap.stat().st_mode & 0o777, 0o600)
            p.provision(self.args(root, 'claude', path))
            self.assertEqual(bootstrap.read_bytes(), before, 'provider extension replaced fresh-root provenance')

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
