#!/usr/bin/env python3
"""Local admission tests: no VM, credentials, provider calls or cgroup changes."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import threading
import unittest
from unittest import mock
import hashlib
import base64

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('native_vm', HERE / 'native-provider-vm.py')
vm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vm)


class ReadonlyTransportAdmissionTests(unittest.TestCase):
    def test_readonly_tool_channel_is_refused_before_runtime_or_module_loading(self):
        original = {'version': 1, 'readonly_review': True, 'resume': False, 'mcp_argv': None}
        changes = [{'mcp_argv': ['/host/tool-bridge']}, {'mcp_argv': []}, {'resume': True},
                   {'readonly_review': 'true'}, {'readonly_review': 1}, {'readonly_review': None}]
        for change in changes:
            with self.subTest(change=change), mock.patch.object(vm, 'read_launch_spec', return_value=({**original, **change}, '0' * 64)), \
                    mock.patch.object(vm, 'module') as modules, mock.patch.object(vm, 'validate_runtime') as runtime:
                with self.assertRaisesRegex(ValueError, 'readonly review'):
                    vm.main('/fixture/launch.json')
                modules.assert_not_called()
                runtime.assert_not_called()
        for missing in ('mcp_argv', 'resume'):
            spec = {key: value for key, value in original.items() if key != missing}
            with self.subTest(missing=missing), mock.patch.object(vm, 'read_launch_spec', return_value=(spec, '0' * 64)), mock.patch.object(vm, 'module') as modules:
                with self.assertRaisesRegex(ValueError, 'readonly review'):
                    vm.main('/fixture/launch.json')
                modules.assert_not_called()

    def test_readonly_without_channel_and_legacy_builder_continue_to_runtime_admission(self):
        for spec in ({'version': 1, 'readonly_review': True, 'resume': False, 'mcp_argv': None},
                     {'version': 1, 'mcp_argv': ['/declared/builder-bridge']}):
            with self.subTest(spec=spec), mock.patch.object(vm, 'read_launch_spec', return_value=(spec, '0' * 64)), \
                    mock.patch.object(vm, 'module', side_effect=RuntimeError('runtime admission reached')):
                with self.assertRaisesRegex(RuntimeError, 'runtime admission reached'):
                    vm.main('/fixture/launch.json')


class LaunchInputSnapshotTests(unittest.TestCase):
    def test_completed_and_failed_receipts_bind_consumed_spec_not_later_path_contents(self):
        for outcome in ('complete', 'disconnect', 'complete-removed', 'disconnect-removed', 'asset-invalid'):
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                state = root / 'session'
                state.mkdir(mode=0o700)
                executable = root / 'provider'
                executable.write_bytes(b'synthetic provider never executed')
                executable.chmod(0o700)
                asset = root / 'asset-snapshot'
                asset.write_bytes(b'original bounded instructions')
                asset.chmod(0o400)
                asset_manifest = {'version': 1, 'source': str(asset), 'name': 'asset-0',
                    'bytes': asset.stat().st_size, 'sha256': hashlib.sha256(asset.read_bytes()).hexdigest()}
                if outcome == 'asset-invalid':
                    asset_manifest['sha256'] = 'f' * 64
                launch = {'version': 1, 'readonly_review': True, 'resume': False, 'mcp_argv': None,
                          'guest_runtime': '/fixture/runtime', 'runtime_config': '/fixture/config',
                          'session_directory': str(state), 'memory_mb': 512, 'refresh_auth': False,
                          'executable': str(executable), 'gateway': {}, 'auth_relative': '.codex/auth.json',
                          'argv': ['consumed-original-argument'], 'environment': {}, 'assets': [asset_manifest]}
                path = root / 'launch.json'
                original = json.dumps(launch).encode()
                path.write_bytes(original)
                path.chmod(0o600)
                proceed = root / 'continue'
                proceed.write_bytes(b'admitted\n')
                proceed.chmod(0o600)
                backend = mock.Mock()
                backend.Runtime.load.return_value.config = {'library_dir': '/fixture/lib', 'module_dir': '/fixture/modules'}
                backend.initrd.return_value = b'synthetic initrd'
                def qemu_command(config, image, shares, unused):
                    self.assertEqual((shares[1][1] / 'asset-0').read_bytes(), b'original bounded instructions')
                    return ['synthetic-qemu', '-serial', 'stdio']
                backend.qemu_command.side_effect = qemu_command
                gateway_module = mock.Mock()
                gateway_module.provision.return_value = ({}, [])
                gateway = mock.Mock(guest_failures=0, host_failures=0)
                gateway.transport_receipt.return_value = {}
                def gateway_started(*args, **kwargs):
                    changed = {**launch, 'argv': ['later-substituted-argument'], 'readonly_review': False,
                               'mcp_argv': ['/later/host/tool-channel']}
                    if outcome.endswith('-removed'):
                        path.unlink()
                    else:
                        path.write_bytes(json.dumps(changed).encode())
                    return gateway
                gateway_module.HostGateway.side_effect = gateway_started
                channel = mock.Mock()
                def observe_start(frame):
                    if frame.get('type') == 'start':
                        observation = json.loads((root / 'vm-startup-inputs.json').read_bytes())
                        self.assertEqual(observation, {'version': 1, 'phase': 'inputs_staged',
                            'launch_sha256': hashlib.sha256(original).hexdigest()})
                        self.assertEqual((root / 'ready').read_bytes(), b'ready\n')
                channel.send.side_effect = observe_start
                channel.receive.return_value = [{'type': 'ready'},
                    {'type': 'worker_quiescent', 'scans': 2, 'signalled': 0, 'reaped': 0,
                     'remaining_workers': 0, 'quiet_scans': 2}, {'type': 'exit', 'code': 0}]
                if outcome.startswith('disconnect'):
                    channel.receive.side_effect = RuntimeError('synthetic native disconnect')
                process = mock.Mock(returncode=0)
                process.poll.return_value = None
                with mock.patch.object(vm, 'module', side_effect=[backend, gateway_module]), \
                        mock.patch.object(vm, 'validate_runtime'), \
                        mock.patch.object(vm, 'credential_use', return_value=None), \
                        mock.patch.object(vm, 'runtime_seed', return_value=([], None)), \
                        mock.patch.object(vm, 'stage_native_companions'), \
                        mock.patch.object(vm, 'boot_entries', return_value=[]) as boot, \
                        mock.patch.object(vm, 'publish_runtime_generation', return_value='generation-synthetic'), \
                        mock.patch.object(vm.subprocess, 'Popen', return_value=process), \
                        mock.patch.object(vm.socket, 'socket'), \
                        mock.patch.object(vm, 'Channel', return_value=channel), \
                        mock.patch.object(vm.threading, 'Thread'), \
                        mock.patch.dict(os.environ, {'JARVIS_NATIVE_VM_READY': str(root / 'ready'),
                                                    'JARVIS_NATIVE_VM_CONTINUE': str(proceed)}):
                    if outcome == 'asset-invalid':
                        with self.assertRaisesRegex(ValueError, 'asset.*digest'):
                            vm.main(path)
                        self.assertFalse((root / 'ready').exists())
                        self.assertFalse((root / 'vm-startup-inputs.json').exists())
                        backend.qemu_command.assert_not_called()
                        channel.send.assert_not_called()
                        continue
                    if outcome.startswith('complete'):
                        self.assertEqual(vm.main(path), 0)
                        receipt = json.loads((state / 'complete.json').read_bytes())
                    else:
                        with self.assertRaisesRegex(RuntimeError, 'synthetic native disconnect'):
                            vm.main(path)
                        self.assertFalse((state / 'complete.json').exists())
                        receipt = json.loads((root / 'failed-gateway-observation.json').read_bytes())
                        self.assertTrue(receipt['diagnostic_only'])
                self.assertEqual(boot.call_args.args[1]['argv'], ['consumed-original-argument'])
                self.assertFalse(boot.call_args.args[1]['mcp'])
                self.assertEqual(receipt['launch_sha256'], hashlib.sha256(original).hexdigest(),
                                 'receipt hashed replacement path bytes instead of the consumed launch input')


class AssetInputBindingTests(unittest.TestCase):
    def manifest(self, source, name='asset-0', data=b'original instructions'):
        return {'version': 1, 'source': str(source), 'name': name, 'bytes': len(data),
                'sha256': hashlib.sha256(data).hexdigest()}

    def test_changed_asset_digest_is_refused_before_guest_materialization(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'snapshot'
            source.write_bytes(b'changed! instructions')
            source.chmod(0o400)
            binary = root / 'provider'
            binary.mkdir(mode=0o700)
            with self.assertRaisesRegex(ValueError, 'asset.*(digest|bytes|size)'):
                vm.stage_input_assets([self.manifest(source)], binary)
            self.assertEqual(list(binary.iterdir()), [])

    def test_entire_batch_is_checked_before_writing_guest_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'snapshot'
            source.write_bytes(b'original instructions')
            source.chmod(0o400)
            binary = root / 'provider'
            binary.mkdir(mode=0o700)
            first = self.manifest(source)
            second = {**self.manifest(source, 'asset-1'), 'sha256': 'f' * 64}
            with self.assertRaisesRegex(ValueError, 'asset.*digest'):
                vm.stage_input_assets([first, second], binary)
            self.assertEqual(list(binary.iterdir()), [])
            vm.stage_input_assets([first, self.manifest(source, 'asset-1')], binary)
            for name in ('asset-0', 'asset-1'):
                self.assertEqual((binary / name).read_bytes(), b'original instructions')
                self.assertEqual((binary / name).stat().st_mode & 0o777, 0o444)

    def test_manifest_and_snapshot_identity_refuse_unknown_or_mutable_inputs(self):
        changes = ({'version': True}, {'version': 2}, {'bytes': True}, {'bytes': -1},
                   {'bytes': 16 * 1024 * 1024 + 1}, {'sha256': 'A' * 64}, {'extra': 1},
                   {'name': 'asset-other'}, {'name': '../asset-0'}, {'source': 'relative'})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'snapshot'
            source.write_bytes(b'original instructions')
            source.chmod(0o400)
            binary = root / 'provider'
            binary.mkdir(mode=0o700)
            for change in changes:
                with self.subTest(change=change), self.assertRaises(ValueError):
                    vm.stage_input_assets([{**self.manifest(source), **change}], binary)
                self.assertEqual(list(binary.iterdir()), [])
            for field in ('version', 'bytes', 'sha256'):
                unknown = {k: v for k, v in self.manifest(source).items() if k != field}
                with self.subTest(missing=field), self.assertRaises(ValueError):
                    vm.stage_input_assets([unknown], binary)
            source.chmod(0o600)
            with self.assertRaises(ValueError):
                vm.stage_input_assets([self.manifest(source)], binary)
            source.chmod(0o400)
            link = root / 'hardlink'
            os.link(source, link)
            with self.assertRaises(ValueError):
                vm.stage_input_assets([self.manifest(source)], binary)
            link.unlink()
            symlink = root / 'symlink'
            symlink.symlink_to(source)
            with self.assertRaises(OSError):
                vm.stage_input_assets([self.manifest(symlink)], binary)
            fifo = root / 'fifo'
            os.mkfifo(fifo, 0o400)
            with self.assertRaises(ValueError):
                vm.stage_input_assets([self.manifest(fifo)], binary)
            self.assertEqual(list(binary.iterdir()), [])



class NativeVmTests(unittest.TestCase):
    def test_export_target_rejects_existing_public_or_symlink_parents(self):
        for case in ('public-root', 'public-parent', 'symlink-parent'):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                staging = root / 'generation'
                staging.mkdir(mode=0o700)
                if case == 'public-root':
                    staging.chmod(0o775)
                elif case == 'public-parent':
                    (staging / 'nested').mkdir(mode=0o700)
                    (staging / 'nested').chmod(0o775)
                else:
                    outside = root / 'outside'
                    outside.mkdir(mode=0o700)
                    (staging / 'nested').symlink_to(outside, target_is_directory=True)
                before = {path: path.lstat().st_mode for path in root.rglob('*')}
                with self.assertRaises(ValueError):
                    vm.export_target(staging, 'nested/file')
                self.assertEqual(before, {path: path.lstat().st_mode for path in root.rglob('*')})

    def wait_file(self, path, process):
        deadline = time.monotonic() + 5
        while not path.exists():
            self.assertIsNone(process.poll(), 'supervisor exited before handshake')
            self.assertLess(time.monotonic(), deadline, 'handshake did not complete')
            time.sleep(0.01)

    def launch(self, root, segmented_ready=False, ready_value='ready\n'):
        worker = root / 'worker.py'
        worker.write_text('''import os,time
from pathlib import Path
ready=Path(os.environ['JARVIS_NATIVE_VM_READY'])
if os.environ.get('FIXTURE_SEGMENTED_READY') == '1':
    ready.write_text('')
    ready.with_name('empty-ready-published').touch()
    while not ready.with_name('publish-prefix').exists(): time.sleep(.005)
    ready.write_text('rea')
    ready.with_name('prefix-ready-published').touch()
    while not ready.with_name('publish-ready').exists(): time.sleep(.005)
ready.write_text(os.environ['FIXTURE_READY_VALUE'])
ready.chmod(0o600)
proceed=Path(os.environ['JARVIS_NATIVE_VM_CONTINUE'])
while not proceed.exists(): time.sleep(.005)
if proceed.read_text() != 'admitted\\n': raise SystemExit(126)
ready.with_name('actual-model-dispatch').write_text('called')
''')
        env = {'PATH': os.defpath, 'JARVIS_SUPERVISOR_VM_PREFLIGHT': '1',
               'JARVIS_SUPERVISOR_ADMISSION': '1', 'FIXTURE_READY_VALUE': ready_value}
        if segmented_ready:
            env['FIXTURE_SEGMENTED_READY'] = '1'
        return subprocess.Popen([sys.executable, '-I', str(HERE / 'provider-supervisor.py'),
                                 str(root / 'cleanup-complete'), sys.executable, str(worker)], env=env)

    def test_preflight_waits_for_complete_readiness_publication(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            process = self.launch(root, segmented_ready=True)
            try:
                for published, advance in [('empty-ready-published', 'publish-prefix'),
                                           ('prefix-ready-published', 'publish-ready')]:
                    self.wait_file(root / published, process)
                    # Keep the writer paused while the supervisor observes a
                    # visible empty/prefix file; neither grants admission.
                    time.sleep(.1)
                    self.assertIsNone(process.poll(), 'partial readiness must wait for publication')
                    self.assertFalse((root / 'launch-ready').exists())
                    self.assertFalse((root / 'actual-model-dispatch').exists())
                    (root / advance).touch()
                self.wait_file(root / 'launch-ready', process)
                self.assertFalse((root / 'actual-model-dispatch').exists())
                vm.atomic(root / 'launch-continue', b'admitted\n')
                self.assertEqual(process.wait(timeout=5), 0)
                self.assertTrue((root / 'actual-model-dispatch').exists())
                self.assertEqual((root / 'cleanup-complete').read_text(), 'all-descendants-reaped\n')
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)

    def test_preflight_rejects_malformed_readiness_without_admission(self):
        for value in ('ready\nextra', 'wrong\n'):
            with self.subTest(value=value), tempfile.TemporaryDirectory() as name:
                root = Path(name)
                process = self.launch(root, ready_value=value)
                try:
                    self.assertNotEqual(process.wait(timeout=5), 0)
                    self.assertFalse((root / 'launch-ready').exists())
                    self.assertFalse((root / 'actual-model-dispatch').exists())
                    self.assertEqual((root / 'cleanup-complete').read_text(), 'all-descendants-reaped\n')
                finally:
                    if process.poll() is None:
                        process.terminate()
                        process.wait(timeout=5)

    def test_cancel_partial_readiness_reaps_worker_without_admission(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            process = self.launch(root, segmented_ready=True)
            try:
                self.wait_file(root / 'empty-ready-published', process)
                process.terminate()
                self.assertNotEqual(process.wait(timeout=5), 0)
                self.assertFalse((root / 'launch-ready').exists())
                self.assertFalse((root / 'actual-model-dispatch').exists())
                self.assertEqual((root / 'cleanup-complete').read_text(), 'all-descendants-reaped\n')
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)

    def test_long_state_root_uses_private_short_socket_without_moving_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary) / ('registered-' + 'x' * 90) / ('session-' + 'y' * 90)
            state.mkdir(parents=True)
            artifact = state / 'durable-evidence.json'
            artifact.write_text('{"retained":true}')
            self.assertGreater(len(os.fsencode(state / 'native-vm-launch-12345678/channel.sock')), 107)
            with vm.short_channel_endpoint() as endpoint:
                self.assertLessEqual(len(os.fsencode(endpoint)), 107)
                self.assertEqual(endpoint.parent.stat().st_mode & 0o777, 0o700)
                with socket.socket(socket.AF_UNIX) as server, socket.socket(socket.AF_UNIX) as client:
                    server.bind(str(endpoint));server.listen(1)
                    client.connect(str(endpoint))
                    accepted, _ = server.accept()
                    with accepted:
                        client.sendall(b'private-control')
                        self.assertEqual(accepted.recv(64), b'private-control')
                parent = endpoint.parent
            self.assertFalse(parent.exists())
            self.assertEqual(artifact.read_text(), '{"retained":true}')

    def test_preflight_ready_does_not_dispatch_without_controller_admission(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            process = self.launch(root)
            try:
                self.wait_file(root / 'launch-ready', process)
                self.assertFalse((root / 'actual-model-dispatch').exists())
                vm.atomic(root / 'launch-continue', b'admitted\n')
                self.assertEqual(process.wait(timeout=5), 0)
                self.assertTrue((root / 'actual-model-dispatch').exists())
                self.assertEqual((root / 'cleanup-complete').read_text(), 'all-descendants-reaped\n')
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()

    def test_cancel_waiting_vm_cleans_without_model_dispatch(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            process = self.launch(root)
            try:
                self.wait_file(root / 'launch-ready', process)
                process.send_signal(signal.SIGTERM)
                process.wait(timeout=5)
                self.assertFalse((root / 'actual-model-dispatch').exists())
                self.assertEqual((root / 'cleanup-complete').read_text(), 'all-descendants-reaped\n')
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()

    def test_undeclared_runtime_sibling_refuses_launch(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            vm.atomic(root / 'python', b'declared runtime')
            vm.atomic(root / 'manifest.json', json.dumps({'version': 1, 'files': [
                {'path': 'python', 'sha256': vm.hashlib.sha256(b'declared runtime').hexdigest()}
            ]}).encode())
            vm.validate_runtime(root)
            vm.atomic(root / 'unrelated-credential', b'not granted')
            with self.assertRaisesRegex(ValueError, 'undeclared runtime file'):
                vm.validate_runtime(root)

    def test_refresh_tokens_preserves_identity_and_concurrent_owner_update(self):
        token = 'header.' + vm.base64.urlsafe_b64encode(json.dumps({'sub':'same-user','iss':'same-issuer','aud':'same-client'}).encode()).decode().rstrip('=') + '.signature'
        original = {'auth_mode': 'chatgpt', 'OPENAI_API_KEY': None, 'last_refresh': 'old',
                    'tokens': {'access_token': 'old-access', 'refresh_token': 'old-refresh',
                               'id_token': token, 'account_id': 'same-account'}}
        updated = json.loads(json.dumps(original))
        updated['tokens']['access_token'] = 'new-access'
        updated['tokens']['refresh_token'] = 'new-refresh'
        updated['last_refresh'] = 'new'
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            vm.atomic(root / 'codex.json', json.dumps(original).encode())
            generation = root / 'generation'
            generation.mkdir(mode=0o700)
            vm.atomic(generation / 'auth.json', json.dumps(updated).encode())
            spec = {'refresh_auth': True, 'provider': 'codex', 'auth_file': str(root / 'codex.json'), 'credential_directory': str(root),
                    'auth_relative': 'auth.json'}
            digest = vm.hashlib.sha256((root / 'codex.json').read_bytes()).hexdigest()
            vm.refresh_auth(spec, generation, digest)
            self.assertEqual(json.loads((root / 'codex.json').read_text()), updated)
            owner = {**updated, 'last_refresh': 'owner-newer'}
            vm.atomic(root / 'codex.json', json.dumps(owner).encode())
            vm.refresh_auth(spec, generation, digest)
            self.assertEqual(json.loads((root / 'codex.json').read_text()), owner)
        forged = json.loads(json.dumps(updated))
        forged['tokens']['account_id'] = 'other-account'
        with self.assertRaisesRegex(ValueError, 'account identity'):
            vm.refreshed_credential(original, forged, 'codex')
        forged = json.loads(json.dumps(updated))
        forged['OPENAI_API_KEY'] = 'different-static-key'
        with self.assertRaisesRegex(ValueError, 'identity or configuration'):
            vm.refreshed_credential(original, forged, 'codex')
        forged = json.loads(json.dumps(updated))
        forged['tokens']['id_token'] = 'header.' + vm.base64.urlsafe_b64encode(json.dumps({'sub':'other-user','iss':'same-issuer','aud':'same-client'}).encode()).decode().rstrip('=') + '.signature'
        with self.assertRaisesRegex(ValueError, 'identity token account changed'):
            vm.refreshed_credential(original, forged, 'codex')

    def test_credential_use_serializes_seed_through_refresh_not_just_writeback(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            spec = {'refresh_auth': True, 'provider': 'codex', 'auth_file': str(root / 'codex.json'),
                    'credential_directory': str(root)}
            first = vm.credential_use(spec)
            try:
                with self.assertRaises(BlockingIOError):
                    vm.credential_use(spec)
            finally:
                os.close(first)
            second = vm.credential_use(spec)
            os.close(second)

    def test_claude_refresh_cannot_expand_scope_or_replace_static_fields(self):
        old = {'claudeAiOauth': {'accessToken': 'a', 'refreshToken': 'r', 'expiresAt': 1,
                                'scopes': ['inference'], 'subscriptionType': 'max'}}
        new = json.loads(json.dumps(old))
        new['claudeAiOauth'].update(accessToken='b', refreshToken='s', expiresAt=2)
        self.assertEqual(vm.refreshed_credential(old, new, 'claude'), new)
        new['claudeAiOauth']['scopes'].append('admin')
        with self.assertRaisesRegex(ValueError, 'scope changed'):
            vm.refreshed_credential(old, new, 'claude')

    def test_runtime_resume_uses_only_committed_generation(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            for generation in ['generation-complete', 'generation-uncommitted']:
                (root / generation).mkdir(mode=0o700)
                vm.atomic(root / generation / 'session.json', generation.encode())
            vm.atomic(root / 'complete.json', json.dumps({'generation': 'generation-complete'}).encode())
            seed, auth = vm.runtime_seed(root, None, '.codex/auth.json')
            self.assertEqual(seed, [('session.json', b'generation-complete')])
            self.assertIsNone(auth)

    def test_runtime_symlink_never_exports_controller_file(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / 'generation-a').mkdir(mode=0o700)
            (root / 'generation-a' / 'secret').symlink_to('/etc/passwd')
            vm.atomic(root / 'complete.json', b'{"generation":"generation-a"}')
            with self.assertRaises(OSError):
                vm.runtime_seed(root, None, '.codex/auth.json')


class RefreshCheckpointTests(unittest.TestCase):
    def fixture(self, root):
        spec = {'refresh_auth': True, 'provider': 'claude', 'auth_file': str(root / 'claude.json'),
                'credential_directory': str(root), 'auth_relative': '.claude/.credentials.json'}
        vm.atomic(root / 'claude.json', json.dumps({'claudeAiOauth': {
            'accessToken': 'old-access', 'refreshToken': 'old-refresh', 'expiresAt': 1,
            'scopes': ['inference'], 'subscriptionType': 'max'}}).encode())
        return spec

    request = {'authority': 'platform.claude.com:443', 'method': 'POST', 'path': '/v1/oauth/token'}
    entry = {'tls': True, 'allow_loopback': False}
    body = b'{"grant_type":"refresh_token","refresh_token":"old-refresh","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e"}'
    response = b'{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"scope":"inference"}'

    def test_rotation_then_killed_process_seeds_fresh_session_without_export(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec = self.fixture(root)
            script = """import importlib.util,json,os,signal,sys
from pathlib import Path
module=importlib.util.spec_from_file_location('vm',sys.argv[1]); vm=importlib.util.module_from_spec(module); module.loader.exec_module(vm)
spec=json.loads(sys.argv[2]); lock=vm.credential_use(spec); checkpoint=vm.RefreshCheckpoint(spec,lock)
checkpoint.begin(json.loads(sys.argv[3]),{'tls':True,'allow_loopback':False},sys.argv[4].encode())
checkpoint.sent(); checkpoint.commit(sys.argv[5].encode())
# Simulate death immediately after issuer response checkpoint, no guest export.
os.kill(os.getpid(),signal.SIGKILL)
"""
            process = subprocess.run([sys.executable, '-c', script, str(HERE / 'native-provider-vm.py'),
                json.dumps(spec), json.dumps(self.request), self.body.decode(), self.response.decode()],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
            self.assertEqual(process.returncode, -signal.SIGKILL)
            lock = vm.credential_use(spec)
            try:
                vm.RefreshCheckpoint(spec, lock)
                entries, _ = vm.runtime_seed(root / 'fresh-session', spec['auth_file'], spec['auth_relative'])
                self.assertEqual(json.loads(dict(entries)[spec['auth_relative']])['claudeAiOauth']['refreshToken'], 'new-refresh')
                self.assertFalse((root / 'complete.json').exists())
            finally:
                os.close(lock)

    def test_received_journal_recovers_and_pending_outcome_refuses(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); spec = self.fixture(root); lock = vm.credential_use(spec)
            try:
                checkpoint = vm.RefreshCheckpoint(spec, lock)
                checkpoint.begin(self.request, self.entry, self.body); checkpoint.sent()
                with self.assertRaisesRegex(ValueError, 'outcome uncertain'):
                    vm.RefreshCheckpoint(spec, lock)
                checkpoint.rejected(503); checkpoint.end()
                with self.assertRaisesRegex(ValueError, 'outcome uncertain'):
                    checkpoint.begin(self.request, self.entry, self.body)
                checkpoint.lock.acquire()
                original = vm.private_bytes(Path(spec['auth_file']), 1024 * 1024)
                vm.atomic(checkpoint.journal, json.dumps({'state': 'received',
                    'original_digest': hashlib.sha256(original).hexdigest(), 'received_at_ms': 1000,
                    'response': base64.b64encode(self.response).decode()}).encode())
                checkpoint.end()
                vm.RefreshCheckpoint(spec, lock)
                self.assertEqual(json.loads(Path(spec['auth_file']).read_bytes())['claudeAiOauth']['expiresAt'], 3601000)
                self.assertEqual(json.loads(Path(spec['auth_file']).read_bytes())['claudeAiOauth']['refreshToken'], 'new-refresh')
            finally:
                os.close(lock)

    def test_parallel_stale_and_wrong_scope_refreshes_do_not_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); spec = self.fixture(root); lock = vm.credential_use(spec)
            try:
                checkpoint = vm.RefreshCheckpoint(spec, lock)
                with self.assertRaisesRegex(ValueError, 'duplicate'):
                    checkpoint.begin(self.request, self.entry, self.body[:-1] + b',"refresh_token":"other"}')
                self.assertTrue(checkpoint.begin(self.request, self.entry, self.body))
                import concurrent.futures
                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                    with self.assertRaisesRegex(ValueError, 'already in progress'):
                        pool.submit(checkpoint.begin, self.request, self.entry, self.body).result()
                with self.assertRaisesRegex(ValueError, 'scope changed'):
                    checkpoint.commit(self.response.replace(b'inference', b'admin'))
                self.assertEqual(json.loads(Path(spec['auth_file']).read_bytes())['claudeAiOauth']['refreshToken'], 'old-refresh')
                checkpoint.commit(self.response); checkpoint.end()
                with self.assertRaisesRegex(ValueError, 'does not match'):
                    checkpoint.begin(self.request, self.entry, self.body)
                with self.assertRaisesRegex(ValueError, 'query'):
                    checkpoint.begin(dict(self.request, path='/v1/oauth/token?other=1'), self.entry, self.body)
            finally:
                os.close(lock)

    def test_scope_subset_is_refused_before_dispatch_even_if_response_omits_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); spec = self.fixture(root); lock = vm.credential_use(spec)
            try:
                checkpoint = vm.RefreshCheckpoint(spec, lock)
                data = json.loads(self.body); data['scope'] = ''
                with self.assertRaisesRegex(ValueError, 'changes credential scope'):
                    checkpoint.begin(self.request, self.entry, json.dumps(data).encode())
                self.assertFalse(checkpoint.journal.exists())
                data['scope'] = 'inference'
                checkpoint.begin(self.request, self.entry, json.dumps(data).encode())
                response = json.loads(self.response); response.pop('scope')
                checkpoint.commit(json.dumps(response).encode()); checkpoint.end()
                self.assertEqual(json.loads(Path(spec['auth_file']).read_bytes())['claudeAiOauth']['scopes'], ['inference'])
            finally:
                os.close(lock)

    def test_publication_drains_checkpoint_before_stale_guest_export(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); spec = self.fixture(root); lock = vm.credential_use(spec)
            try:
                original = vm.private_bytes(Path(spec['auth_file']), 1024 * 1024)
                checkpoint = vm.RefreshCheckpoint(spec, lock)
                checkpoint.begin(self.request, self.entry, self.body); checkpoint.sent()
                staging = root / 'staging'; (staging / '.claude').mkdir(parents=True)
                stale_export = json.loads(original)
                stale_export['claudeAiOauth'].update(accessToken='guest-access', refreshToken='guest-refresh')
                vm.atomic(staging / spec['auth_relative'], json.dumps(stale_export).encode())
                def drain():
                    checkpoint.commit(self.response); checkpoint.end()
                gateway = mock.Mock(); gateway.close.side_effect = drain
                generation = vm.publish_runtime_generation(gateway, spec, staging, root,
                    hashlib.sha256(original).hexdigest(), lock)
                self.assertTrue((root / generation).is_dir())
                self.assertEqual(json.loads(Path(spec['auth_file']).read_bytes())['claudeAiOauth']['refreshToken'], 'new-refresh')
                self.assertEqual(json.loads(checkpoint.journal.read_bytes())['state'], 'committed')
            finally:
                os.close(lock)

    def test_close_waits_for_worker_inside_begin_before_refreshing_flag(self):
        gateway_spec = importlib.util.spec_from_file_location('refresh_race_gateway', HERE / 'native-provider-gateway.py')
        gateway = importlib.util.module_from_spec(gateway_spec); gateway_spec.loader.exec_module(gateway)
        policy = {'version': 1, 'authorities': [{'authority': 'platform.claude.com:443', 'tls': True,
            'allow_loopback': False, 'routes': [{'path_prefix': '/v1/oauth/token', 'exact': True,
            'methods': ['POST'], 'websocket': False}]}]}
        entered = threading.Event(); release = threading.Event(); closed = threading.Event()
        def begin(*_):
            entered.set()
            if not release.wait(3):
                raise RuntimeError('fixture did not release worker')
            return True
        checkpoint = mock.Mock(); checkpoint.begin.side_effect = begin
        host = gateway.HostGateway(policy, lambda _: None, refresh_checkpoint=checkpoint)
        with mock.patch.object(gateway, 'safe_socket', return_value=mock.Mock()):
            host.receive(dict(self.request, id='race', type='gateway_open', headers=[]))
            host.receive({'id': 'race', 'type': 'gateway_data', 'data': gateway.encode(self.body)})
            host.receive({'id': 'race', 'type': 'gateway_end'})
            self.assertTrue(entered.wait(2))
            closer = threading.Thread(target=lambda: (host.close(), closed.set()))
            closer.start()
            try:
                self.assertFalse(closed.wait(0.1), 'close released lease while begin/recover remained active')
            finally:
                release.set(); closer.join(3)
            self.assertTrue(closed.is_set())
            checkpoint.end.assert_called_once()

    def test_codex_optional_tokens_and_distinct_journal_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            token = 'header.' + base64.urlsafe_b64encode(json.dumps({'sub': 'same', 'iss': 'issuer', 'aud': 'client'}).encode()).decode().rstrip('=') + '.signature'
            original = {'tokens': {'access_token': 'old', 'refresh_token': 'old-refresh', 'id_token': token}, 'last_refresh': 'old'}
            spec = {'refresh_auth': True, 'provider': 'codex', 'auth_file': str(root / 'codex.json'), 'credential_directory': str(root)}
            vm.atomic(Path(spec['auth_file']), json.dumps(original).encode())
            lock = vm.credential_use(spec)
            try:
                checkpoint = vm.RefreshCheckpoint(spec, lock)
                alternate = vm.RefreshCheckpoint(dict(spec, auth_file=str(root / 'codex.token')), lock)
                self.assertNotEqual(checkpoint.journal, alternate.journal)
                request = {'authority': 'auth.openai.com:443', 'method': 'POST', 'path': '/oauth/token'}
                body = json.dumps({'grant_type': 'refresh_token', 'refresh_token': 'old-refresh', 'client_id': 'app_EMoamEEZ73f0CkXaXp7hrann'}).encode()
                checkpoint.begin(request, self.entry, body); checkpoint.sent()
                checkpoint.commit(b'{"access_token":"new","id_token":null}', 1000); checkpoint.end()
                updated = json.loads(Path(spec['auth_file']).read_bytes())
                self.assertEqual(updated['tokens'], dict(original['tokens'], access_token='new'))
                self.assertEqual(updated['last_refresh'], '1970-01-01T00:00:01Z')
            finally:
                os.close(lock)

    def test_gateway_checkpoints_before_headers_and_refuses_persistence_failure(self):
        gateway_spec = importlib.util.spec_from_file_location('refresh_gateway', HERE / 'native-provider-gateway.py')
        gateway = importlib.util.module_from_spec(gateway_spec); gateway_spec.loader.exec_module(gateway)
        policy = {'version': 1, 'authorities': [{'authority': 'platform.claude.com:443', 'tls': True,
            'allow_loopback': False, 'routes': [{'path_prefix': '/v1/oauth/token', 'exact': True,
            'methods': ['POST'], 'websocket': False}]}]}
        for scenario in ('success', 'disk_failure', 'encoded_route'):
            fail = scenario == 'disk_failure'
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory); spec = self.fixture(root); lock = vm.credential_use(spec)
                checkpoint = vm.RefreshCheckpoint(spec, lock)
                frames = []
                def emit(frame):
                    if frame['type'] == 'gateway_headers':
                        self.assertEqual(json.loads(Path(spec['auth_file']).read_bytes())['claudeAiOauth']['refreshToken'], 'new-refresh')
                    frames.append(frame)
                host = gateway.HostGateway(policy, emit, refresh_checkpoint=checkpoint)
                state = gateway.State()
                state.queue.put({'type': 'gateway_data', 'decoded': self.body})
                state.queue.put({'type': 'gateway_end'})
                response = mock.Mock(status=200)
                response.getheaders.return_value = [('Content-Type', 'application/json')]
                def cancelled_response(_):
                    state.close()  # Guest cancelled after issuer accepted rotation.
                    return self.response
                response.read.side_effect = cancelled_response
                try:
                    with mock.patch.object(gateway, 'safe_socket', return_value=mock.Mock()), mock.patch.object(gateway.http.client, 'HTTPResponse', return_value=response):
                        if fail:
                            with mock.patch.object(checkpoint, 'commit', side_effect=OSError('synthetic disk failure')):
                                host.serve('test', state, dict(self.request, id='test'))
                        else:
                            request = dict(self.request, id='test')
                            if scenario == 'encoded_route':
                                request['path'] = '/v1/oauth/%74oken'
                            host.serve('test', state, request)
                    self.assertEqual(any(frame['type'] == 'gateway_headers' for frame in frames), scenario == 'success')
                    self.assertEqual(any(frame['type'] == 'gateway_error' for frame in frames), scenario != 'success')
                finally:
                    host.close(); os.close(lock)


if __name__ == '__main__':
    unittest.main()
