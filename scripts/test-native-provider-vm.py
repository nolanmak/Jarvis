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
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('native_vm', HERE / 'native-provider-vm.py')
vm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vm)


class NativeVmTests(unittest.TestCase):
    def wait_file(self, path, process):
        deadline = time.monotonic() + 5
        while not path.exists():
            self.assertIsNone(process.poll(), 'supervisor exited before handshake')
            self.assertLess(time.monotonic(), deadline, 'handshake did not complete')
            time.sleep(0.01)

    def launch(self, root):
        worker = root / 'worker.py'
        worker.write_text('''import os,time
from pathlib import Path
ready=Path(os.environ['JARVIS_NATIVE_VM_READY'])
ready.write_text('ready\\n')
ready.chmod(0o600)
proceed=Path(os.environ['JARVIS_NATIVE_VM_CONTINUE'])
while not proceed.exists(): time.sleep(.005)
if proceed.read_text() != 'admitted\\n': raise SystemExit(126)
ready.with_name('actual-model-dispatch').write_text('called')
''')
        env = {'PATH': os.defpath, 'JARVIS_SUPERVISOR_VM_PREFLIGHT': '1',
               'JARVIS_SUPERVISOR_ADMISSION': '1'}
        return subprocess.Popen([sys.executable, '-I', str(HERE / 'provider-supervisor.py'),
                                 str(root / 'cleanup-complete'), sys.executable, str(worker)], env=env)

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


if __name__ == '__main__':
    unittest.main()
