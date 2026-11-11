#!/usr/bin/env python3
"""Local admission tests: no VM, credentials, provider calls or cgroup changes."""
import importlib.util
import contextlib
import io
from types import SimpleNamespace
import json
import os
from pathlib import Path
import signal
import socket
import stat
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
native = vm


class NativeVmTests(unittest.TestCase):
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


class NativeStagingTests(unittest.TestCase):
    LIMIT = 8192

    def mutate_after_validation(self, source, mutation):
        """Change the real source after the production validator observed it."""
        original_stat, original_fstat = Path.stat, os.fstat
        initial = original_stat(source)
        changed = []

        def observed(info):
            if (info.st_dev, info.st_ino) == (initial.st_dev, initial.st_ino) and not changed:
                changed.append(True)
                mutation()
            return info

        def path_stat(path, *args, **kwargs):
            return observed(original_stat(path, *args, **kwargs))

        def fd_stat(fd):
            return observed(original_fstat(fd))

        return changed, mock.patch.object(Path, 'stat', path_stat), mock.patch.object(vm.os, 'fstat', fd_stat)

    def test_executable_exact_limit_and_oversize(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_EXECUTABLE', self.LIMIT):
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'x' * self.LIMIT); source.chmod(0o700)
            vm.stage_native_executable(source, target)
            self.assertEqual(target.read_bytes(), source.read_bytes())
            self.assertEqual(target.stat().st_mode & 0o777, 0o555)
            target.unlink(); source.write_bytes(b'x' * (self.LIMIT + 1))
            with self.assertRaises(ValueError):
                vm.stage_native_executable(source, target)
            self.assertFalse(target.exists())

    def test_executable_growth_after_validation_stays_bounded(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_EXECUTABLE', self.LIMIT):
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'x' * 4096); source.chmod(0o700)
            changed, path_stat, fd_stat = self.mutate_after_validation(
                source, lambda: source.write_bytes(b'x' * (self.LIMIT * 2)))
            with path_stat, fd_stat, self.assertRaises(ValueError):
                vm.stage_native_executable(source, target)
            self.assertTrue(changed)
            self.assertFalse(target.exists(), 'failed staging must remove its own incomplete copy')

    def test_executable_replacement_after_validation_refuses(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'original'); source.chmod(0o700)
            replacement = root / 'replacement'; replacement.write_bytes(b'replaced'); replacement.chmod(0o700)
            changed, path_stat, fd_stat = self.mutate_after_validation(
                source, lambda: os.replace(replacement, source))
            with path_stat, fd_stat, self.assertRaises(ValueError):
                vm.stage_native_executable(source, target)
            self.assertTrue(changed)
            self.assertFalse(target.exists())

    def test_executable_preserves_preexisting_destination(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'provider'); source.chmod(0o700); target.write_bytes(b'keep')
            with self.assertRaises(FileExistsError):
                vm.stage_native_executable(source, target)
            self.assertEqual(target.read_bytes(), b'keep')

    def test_executable_symlink_is_not_followed(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'provider'); source.chmod(0o700)
            alias = root / 'alias'; alias.symlink_to(source)
            with self.assertRaises((ValueError, OSError)):
                vm.stage_native_executable(alias, target)
            self.assertFalse(target.exists())

    def test_growth_during_copy_never_writes_over_budget(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_EXECUTABLE', self.LIMIT):
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'x' * 4096); source.chmod(0o700)
            original_read, original_write, original_fstat = os.read, os.write, os.fstat
            source_identity = source.stat()
            changed, peak = [], []

            def read(fd, size):
                data = original_read(fd, size)
                info = original_fstat(fd)
                if (info.st_dev, info.st_ino) == (source_identity.st_dev, source_identity.st_ino) and not changed:
                    changed.append(True)
                    with source.open('ab') as stream:
                        stream.write(b'y' * self.LIMIT * 2)
                return data

            def write(fd, data):
                count = original_write(fd, data)
                peak.append(original_fstat(fd).st_size)
                self.assertLessEqual(peak[-1], self.LIMIT, 'do not overrun then delete the evidence')
                return count

            with mock.patch.object(vm.os, 'read', read), mock.patch.object(vm.os, 'write', write):
                with self.assertRaises(ValueError):
                    vm.stage_native_executable(source, target)
            self.assertTrue(changed); self.assertTrue(peak)
            self.assertFalse(target.exists())

    def test_observed_same_size_source_edit_refuses(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'original'); source.chmod(0o700)
            timestamp = source.stat().st_mtime_ns

            def edit():
                source.write_bytes(b'modified')
                os.utime(source, ns=(timestamp + 1000000000, timestamp + 1000000000))

            changed, path_stat, fd_stat = self.mutate_after_validation(source, edit)
            with path_stat, fd_stat, self.assertRaises(ValueError):
                vm.stage_native_executable(source, target)
            self.assertTrue(changed); self.assertFalse(target.exists())

    def test_partial_writes_complete_and_zero_write_cleans_up(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'provider binary'); source.chmod(0o700)
            original_write = os.write
            with mock.patch.object(vm.os, 'write', lambda fd, data: original_write(fd, data[:3])):
                vm.stage_native_executable(source, target)
            self.assertEqual(target.read_bytes(), source.read_bytes())
            target.unlink()
            with mock.patch.object(vm.os, 'write', return_value=0), self.assertRaises(OSError):
                vm.stage_native_executable(source, target)
            self.assertFalse(target.exists())

    def test_failed_copy_preserves_replaced_destination(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'provider binary'); source.chmod(0o700)
            original_write = os.write

            def fail_after_replacement(fd, data):
                original_write(fd, data)
                target.unlink(); target.write_bytes(b'keep replacement')
                raise OSError('injected write failure')

            with mock.patch.object(vm.os, 'write', fail_after_replacement), self.assertRaises(OSError):
                vm.stage_native_executable(source, target)
            self.assertEqual(target.read_bytes(), b'keep replacement')

    def test_fifo_source_refuses_without_blocking(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name); source = root / 'fifo'; target = root / 'cli'; os.mkfifo(source, 0o600)
            code = '''import importlib.util,sys
s=importlib.util.spec_from_file_location('native',sys.argv[1]);m=importlib.util.module_from_spec(s);s.loader.exec_module(m)
try: m.stage_native_executable(sys.argv[2],sys.argv[3])
except (ValueError,OSError): raise SystemExit(0)
raise SystemExit(1)
'''
            result = subprocess.run([sys.executable, '-I', '-c', code,
                                     str(HERE / 'native-provider-vm.py'), str(source), str(target)],
                                    timeout=5, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertFalse(target.exists())

    def test_companion_size_is_checked_without_unbounded_read(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_COMPANION', self.LIMIT):
            root = Path(name); source = root / 'codex'; source.write_bytes(b'provider')
            target = root / 'provider'; target.mkdir(); (target / 'cli').write_bytes(b'provider')
            (target / 'cli').chmod(0o555)
            runtime = root / 'runtime'; runtime.mkdir(); companion = runtime / 'host'
            companion.write_bytes(b'x' * (self.LIMIT + 1)); companion.chmod(0o400)
            digest = hashlib.sha256(companion.read_bytes()).hexdigest()
            (runtime / 'manifest.json').write_text(json.dumps({
                'files': [{'path': 'host', 'sha256': digest}],
                'native_companions': {hashlib.sha256(b'provider').hexdigest(): [
                    {'path': 'host', 'name': 'codex-code-mode-host', 'sha256': digest}]}}))
            original = Path.read_bytes

            def reject_unbounded(path):
                if path == companion:
                    self.fail('companion must be read with an enforced byte bound')
                return original(path)

            with mock.patch.object(Path, 'read_bytes', reject_unbounded), \
                    self.assertRaisesRegex(ValueError, '^native staging source exceeds byte budget$'):
                vm.stage_native_companions(source, runtime, target)
            self.assertFalse((target / 'codex-code-mode-host').exists())

    def test_untrusted_mode_is_distinct_from_size_failure(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_EXECUTABLE', self.LIMIT):
            root = Path(name); source = root / 'provider'; target = root / 'cli'
            source.write_bytes(b'x' * (self.LIMIT + 1)); source.chmod(0o722)
            with self.assertRaisesRegex(ValueError, '^native staging source is not trusted$'):
                vm.stage_native_executable(source, target)
            self.assertFalse(target.exists())


class RuntimeSeedBudgetTests(unittest.TestCase):
    """Final selected home data includes auth, without charging replaced bytes."""

    def seed(self, root, saved, credential=b'{}', preserve=False):
        home = root / 'generation-a'
        home.mkdir(mode=0o700)
        for relative, data in saved.items():
            target = home / relative
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            vm.atomic(target, data)
        auth = root / 'controller.json'
        vm.atomic(auth, credential)
        complete = {'generation': 'generation-a'}
        if preserve:
            complete['auth_generation'] = hashlib.sha256(credential).hexdigest()
        vm.atomic(root / 'complete.json', json.dumps(complete).encode())
        return vm.runtime_seed(root, str(auth), '.codex/auth.json')

    def test_inserting_auth_cannot_exceed_home_budget(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_HOME', 8):
            with self.assertRaisesRegex(ValueError, 'seed exceeds budget'):
                self.seed(Path(name), {'session': b'12345678'})

    def test_default_64_mib_home_has_no_extra_credential_allowance(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            home = root / 'generation-a'
            home.mkdir(mode=0o700)
            with (home / 'session').open('wb') as stream:
                os.fchmod(stream.fileno(), 0o600)
                stream.truncate(64 * 1024 * 1024)
            vm.atomic(root / 'complete.json', b'{"generation":"generation-a"}')
            auth = root / 'controller.json'
            vm.atomic(auth, b'{}')
            with self.assertRaisesRegex(ValueError, 'seed exceeds budget'):
                vm.runtime_seed(root, str(auth), '.codex/auth.json')

    def test_larger_auth_replacement_cannot_exceed_home_budget(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_HOME', 8):
            with self.assertRaisesRegex(ValueError, 'seed exceeds budget'):
                self.seed(Path(name), {'session': b'123456', '.codex/auth.json': b'{}'}, b'{} ')

    def test_auth_insertion_at_exact_home_budget(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_HOME', 8):
            entries, digest = self.seed(Path(name), {'session': b'123456'})
            self.assertEqual(dict(entries), {'session': b'123456', '.codex/auth.json': b'{}'})
            self.assertEqual(sum(len(data) for _, data in entries), 8)
            self.assertEqual(digest, hashlib.sha256(b'{}').hexdigest())

    def test_smaller_replacement_does_not_double_count_old_auth(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_HOME', 8):
            entries, _ = self.seed(Path(name), {'session': b'1234', '.codex/auth.json': b'{}  '})
            self.assertEqual(dict(entries), {'session': b'1234', '.codex/auth.json': b'{}'})
            self.assertEqual(sum(len(data) for _, data in entries), 6)

    def test_preserved_session_auth_does_not_count_unused_controller_auth(self):
        with tempfile.TemporaryDirectory() as name, mock.patch.object(vm, 'MAX_HOME', 8):
            credential = b'{"a":1111}'
            entries, digest = self.seed(Path(name),
                {'session': b'123456', '.codex/auth.json': b'{}'}, credential, preserve=True)
            self.assertEqual(dict(entries), {'session': b'123456', '.codex/auth.json': b'{}'})
            self.assertEqual(sum(len(data) for _, data in entries), 8)
            self.assertEqual(digest, hashlib.sha256(credential).hexdigest())


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
                staging = root / 'staging'; (staging / '.claude').mkdir(mode=0o700, parents=True)
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




class AtomicPublicationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(); self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name); self.root = self.base / 'parent'; self.root.mkdir(mode=0o700)
        self.target = self.root / 'receipt'; self.target.write_bytes(b'original'); self.target.chmod(0o600)
        if hasattr(native, '_AtomicPublisher'):
            patch = mock.patch.object(native, '_atomic_publisher', native._AtomicPublisher())
            patch.start(); self.addCleanup(patch.stop)

    def debris(self):
        return [p for p in self.root.iterdir() if p != self.target]

    def test_repeated_file_sync_failure_cleans_original_temporary_files(self):
        original_sync = os.fsync
        error = OSError('synthetic file sync failure')
        def refusing(fd):
            if stat.S_ISREG(os.fstat(fd).st_mode):
                raise error
            return original_sync(fd)
        for _ in range(4):
            with mock.patch.object(native.os, 'fsync', side_effect=refusing):
                with self.assertRaises(OSError) as raised:
                    native.atomic(self.target, b'x' * 65536)
                self.assertIs(raised.exception, error)
        self.assertEqual(self.debris(), [])
        self.assertEqual(self.target.read_bytes(), b'original')
        native.atomic(self.target, b'next')
        self.assertEqual(self.target.read_bytes(), b'next')

    def test_repeated_rename_failure_cleans_original_temporary_files(self):
        for _ in range(4):
            with mock.patch.object(native.os, 'replace', side_effect=OSError('synthetic replace failure')):
                with self.assertRaises(OSError):
                    native.atomic(self.target, b'x' * 65536)
        self.assertEqual(self.debris(), [])
        self.assertEqual(self.target.read_bytes(), b'original')

    def test_payload_cap_refuses_before_mutating_any_file(self):
        native.atomic(self.target, b'x' * (2 * 1024 * 1024))
        before = self.target.read_bytes()
        with self.assertRaisesRegex(ValueError, 'budget'):
            native.atomic(self.target, before + b'x')
        self.assertEqual(self.target.read_bytes(), before)
        self.assertEqual(self.debris(), [])

    def test_success_creates_private_file_and_complete_bytes(self):
        observed_modes = []
        original_open = os.open
        def opening(path, flags, *args, **kwargs):
            fd = original_open(path, flags, *args, **kwargs)
            if flags & os.O_CREAT:
                observed_modes.append(stat.S_IMODE(os.fstat(fd).st_mode))
            return fd
        previous = os.umask(0)
        try:
            with mock.patch.object(native.os, 'open', side_effect=opening):
                native.atomic(self.target, b'complete replacement')
        finally:
            os.umask(previous)
        self.assertEqual(observed_modes, [0o600])
        self.assertEqual(self.target.read_bytes(), b'complete replacement')
        self.assertEqual(self.debris(), [])

    def test_unlink_failure_latches_before_any_additional_target(self):
        error = OSError('primary sync failure')
        with mock.patch.object(native.os, 'fsync', side_effect=error), \
             mock.patch.object(native.os, 'unlink', side_effect=PermissionError('cleanup refused')):
            with self.assertRaises(OSError) as raised:
                native.atomic(self.target, b'uncertain')
            self.assertIs(raised.exception, error)
        debris = self.debris(); self.assertEqual(len(debris), 1)
        with self.assertRaisesRegex(OSError, '^native atomic publication uncertain$'):
            native.atomic(self.root / 'another-target', b'new')
        self.assertEqual(self.debris(), debris)
        self.assertEqual(self.target.read_bytes(), b'original')

    def test_observed_substitution_missing_and_hardlink_are_preserved(self):
        for effect in ('replacement', 'missing', 'hardlink'):
            with self.subTest(effect=effect):
                publisher = native._AtomicPublisher()
                with mock.patch.object(native, '_atomic_publisher', publisher):
                    error = OSError('primary sync failure')
                    def mutate(fd):
                        if stat.S_ISDIR(os.fstat(fd).st_mode):
                            return
                        temp = next(p for p in self.root.iterdir() if p != self.target)
                        if effect == 'replacement':
                            temp.rename(self.root / 'retained-original'); temp.write_bytes(b'foreign')
                        elif effect == 'missing':
                            temp.unlink()
                        else:
                            os.link(temp, self.root / 'alias')
                        raise error
                    with mock.patch.object(native.os, 'fsync', side_effect=mutate):
                        with self.assertRaises(OSError) as raised:
                            native.atomic(self.target, b'creator data')
                        self.assertIs(raised.exception, error)
                    before = sorted((p.name, p.read_bytes()) for p in self.debris())
                    with self.assertRaisesRegex(OSError, '^native atomic publication uncertain$'):
                        native.atomic(self.target, b'must refuse')
                    self.assertEqual(sorted((p.name, p.read_bytes()) for p in self.debris()), before)
                    if effect == 'replacement':
                        self.assertTrue(any(data == b'foreign' for _, data in before))
                    if effect == 'hardlink':
                        self.assertEqual((self.root / 'alias').stat().st_nlink, 2)
                    for path in self.debris():
                        path.unlink()

    def test_replace_lost_ack_keeps_published_bytes_and_fences_all_writes(self):
        original_replace = os.replace
        error = OSError('lost acknowledgment')
        def replaced(*args, **kwargs):
            original_replace(*args, **kwargs)
            raise error
        with mock.patch.object(native.os, 'replace', side_effect=replaced):
            with self.assertRaises(OSError) as raised:
                native.atomic(self.target, b'published')
            self.assertIs(raised.exception, error)
        self.assertEqual(self.target.read_bytes(), b'published')
        with self.assertRaisesRegex(OSError, '^native atomic publication uncertain$'):
            native.atomic(self.target, b'rollback forbidden')
        self.assertEqual(self.target.read_bytes(), b'published')

    def test_parent_sync_failure_never_rolls_back_the_replacement(self):
        original_sync = os.fsync
        def refusing(fd):
            if stat.S_ISDIR(os.fstat(fd).st_mode):
                raise OSError('parent sync failed')
            return original_sync(fd)
        with mock.patch.object(native.os, 'fsync', side_effect=refusing):
            with self.assertRaises(OSError):
                native.atomic(self.target, b'published')
        self.assertEqual(self.target.read_bytes(), b'published')
        with self.assertRaisesRegex(OSError, '^native atomic publication uncertain$'):
            native.atomic(self.root / 'other', b'new')
        self.assertEqual(self.debris(), [])

    def test_parent_substitution_cannot_publish_into_the_new_parent(self):
        moved = self.base / 'retained-parent'
        def replacing(fd):
            if stat.S_ISREG(os.fstat(fd).st_mode):
                self.root.rename(moved); self.root.mkdir(mode=0o700)
                self.target.write_bytes(b'foreign parent')
        with mock.patch.object(native.os, 'fsync', side_effect=replacing):
            with self.assertRaises(Exception):
                native.atomic(self.target, b'new')
        self.assertEqual(self.target.read_bytes(), b'foreign parent')
        self.assertEqual((moved / 'receipt').read_bytes(), b'original')
        with self.assertRaisesRegex(OSError, '^native atomic publication uncertain$'):
            native.atomic(self.target, b'no')

    def test_partial_writes_complete_and_invalid_progress_preserves_destination(self):
        original_write = os.write
        with mock.patch.object(native.os, 'write', side_effect=lambda fd, data: original_write(fd, data[:7])):
            native.atomic(self.target, b'a' * 101)
        self.assertEqual(self.target.read_bytes(), b'a' * 101)
        for result in (0, None, -1, 1000000):
            with mock.patch.object(native.os, 'write', return_value=result):
                with self.assertRaisesRegex(OSError, 'invalid progress'):
                    native.atomic(self.target, b'must not replace')
            self.assertEqual(self.target.read_bytes(), b'a' * 101)
            self.assertEqual(self.debris(), [])

    def test_pre_replace_substitution_is_not_published(self):
        original_sync = os.fsync
        def substitute(fd):
            if stat.S_ISREG(os.fstat(fd).st_mode):
                temp = self.debris()[0]
                temp.rename(self.root / 'retained-original'); temp.write_bytes(b'foreign')
            else:
                original_sync(fd)
        with mock.patch.object(native.os, 'fsync', side_effect=substitute):
            with self.assertRaisesRegex(OSError, '^native atomic publication uncertain$'):
                native.atomic(self.target, b'creator')
        self.assertEqual(self.target.read_bytes(), b'original')
        self.assertTrue(any(p.read_bytes() == b'foreign' for p in self.debris()))

    def test_outer_entry_remains_sanitized_when_startup_receipt_is_fenced(self):
        with mock.patch.object(native.os, 'fsync', side_effect=OSError('storage refused')), \
             mock.patch.object(native.os, 'unlink', side_effect=OSError('cleanup refused')):
            with self.assertRaises(OSError):
                native.atomic(self.target, b'pending')
        before = self.debris()
        private = 'PRIVATE-CREDENTIAL-PAYLOAD'
        stderr = io.StringIO()
        with mock.patch.object(native, 'main', side_effect=OSError(5, private)), \
             mock.patch.dict(os.environ, {'JARVIS_NATIVE_VM_READY': str(self.root / 'ready')}), \
             contextlib.redirect_stderr(stderr):
            self.assertEqual(native.entry(self.root / 'spec'), 126)
        self.assertNotIn(private, stderr.getvalue())
        self.assertNotIn('Traceback', stderr.getvalue())
        self.assertIn('"native_vm_failure": "OSError"', stderr.getvalue())
        self.assertFalse((self.root / 'vm-startup-failure.json').exists())
        self.assertEqual(self.debris(), before)

    def test_outer_entry_diagnostic_stream_failure_still_returns_126(self):
        broken = mock.Mock(); broken.write.side_effect = OSError('diagnostic stream unavailable')
        with mock.patch.object(native, 'main', side_effect=OSError(5, 'PRIVATE-PAYLOAD')), \
             mock.patch.dict(os.environ, {'JARVIS_NATIVE_VM_READY': str(self.root / 'ready')}), \
             mock.patch.object(native.sys, 'stderr', broken):
            self.assertEqual(native.entry(self.root / 'spec'), 126)
        self.assertTrue(broken.write.called)

    def test_main_diagnostic_refusal_does_not_mask_original_parent_sync_failure(self):
        codec_spec = importlib.util.spec_from_file_location('atomic_codec', HERE / 'codex-build-vm.py')
        codec = importlib.util.module_from_spec(codec_spec); codec_spec.loader.exec_module(codec)
        binary = self.root / 'cli'; binary.write_bytes(b'synthetic'); binary.chmod(0o755)
        state = self.root / 'session'; state.mkdir(mode=0o700)
        config = {'busybox': str(binary), 'modules': [], 'library_dir': '/synthetic/lib', 'module_dir': '/synthetic/modules'}
        request = {'version': 1, 'guest_runtime': str(self.root / 'runtime'), 'runtime_config': 'fixture',
                   'memory_mb': 1024, 'session_directory': str(state), 'auth_relative': 'auth.json',
                   'executable': str(binary), 'gateway': {}, 'argv': ['synthetic'], 'environment': {}}
        source = self.root / 'spec'; source.write_bytes(b'{}'); source.chmod(0o600)
        original_sync = os.fsync
        error = OSError('PRIMARY-PUBLICATION-ERROR')
        def refusing(fd):
            if stat.S_ISDIR(os.fstat(fd).st_mode):
                raise error
            return original_sync(fd)
        process = mock.Mock(); process.poll.return_value = None
        gateway = mock.Mock(); gateway.guest_failures = []; gateway.host_failures = []
        gateway.transport_receipt.return_value = {'version': 1, 'exchanges': [], 'overflow': False, 'host_failures': []}
        gateway_module = SimpleNamespace(provision=lambda *a: ({}, []), HostGateway=lambda *a, **kw: gateway)
        channel = mock.Mock(); channel.receive.return_value = iter([{'type': 'ready'}])
        stderr = io.StringIO()
        with mock.patch.object(native, 'module', side_effect=lambda name, filename: codec if filename == 'codex-build-vm.py' else gateway_module), \
             mock.patch.object(native, 'private_json', return_value=request), \
             mock.patch.object(native, 'validate_runtime'), \
             mock.patch.object(codec.Runtime, 'load', return_value=SimpleNamespace(config=config)), \
             mock.patch.object(codec, 'qemu_command', return_value=['synthetic', '-serial', 'null']), \
             mock.patch.object(native, 'credential_use', return_value=None), \
             mock.patch.object(native, 'runtime_seed', return_value=([], None)), \
             mock.patch.object(native, 'stage_native_companions'), \
             mock.patch.object(native.subprocess, 'Popen', return_value=process), \
             mock.patch.object(native.socket, 'socket'), \
             mock.patch.object(native, 'Channel', return_value=channel), \
             mock.patch.object(native.os, 'fsync', side_effect=refusing), \
             mock.patch.dict(os.environ, {'JARVIS_NATIVE_VM_READY': str(self.root / 'ready'),
                                         'JARVIS_NATIVE_VM_CONTINUE': str(self.root / 'continue')}), \
             contextlib.redirect_stderr(stderr):
            with self.assertRaises(OSError) as raised:
                native.main(source)
            self.assertIs(raised.exception, error)
        self.assertEqual((self.root / 'ready').read_bytes(), b'ready\n')
        self.assertFalse((state / 'complete.json').exists())
        self.assertFalse((self.root / 'failed-gateway-observation.json').exists())
        self.assertIn('native_diagnostic_publication_failed', stderr.getvalue())
        self.assertNotIn('PRIMARY-PUBLICATION-ERROR', stderr.getvalue())
        gateway.close.assert_called_once()

    def test_concurrent_second_writer_cannot_allocate_after_uncertainty(self):
        entered, proceed = threading.Event(), threading.Event()
        original_sync = os.fsync
        created = []
        original_open = os.open
        def opening(path, flags, *args, **kwargs):
            fd = original_open(path, flags, *args, **kwargs)
            if flags & os.O_CREAT:
                created.append(fd)
            return fd
        def refusing(fd):
            if stat.S_ISREG(os.fstat(fd).st_mode):
                entered.set(); self.assertTrue(proceed.wait(2))
            raise OSError('storage sync uncertain')
        outcomes = []
        def publish(path):
            try:
                native.atomic(path, b'data')
            except Exception as error:
                outcomes.append(error)
        with mock.patch.object(native.os, 'open', side_effect=opening), \
             mock.patch.object(native.os, 'fsync', side_effect=refusing):
            first = threading.Thread(target=publish, args=(self.target,)); first.start()
            self.assertTrue(entered.wait(1))
            second = threading.Thread(target=publish, args=(self.root / 'other',)); second.start()
            proceed.set(); first.join(2); second.join(2)
            self.assertFalse(first.is_alive()); self.assertFalse(second.is_alive())
        self.assertEqual(len(created), 1)
        self.assertEqual(len(outcomes), 2)
        self.assertTrue(any(str(error) == 'native atomic publication uncertain' for error in outcomes))
        self.assertTrue(any(type(error) is OSError and str(error) == 'storage sync uncertain' for error in outcomes))




class CredentialRoundtripTests(unittest.TestCase):
    limit = 1024 * 1024

    def fixture(self, root, provider, padding=''):
        token = 'header.' + base64.urlsafe_b64encode(b'{"sub":"same","iss":"issuer","aud":"client"}').decode().rstrip('=') + '.signature'
        if provider == 'claude':
            original = {'padding': padding, 'claudeAiOauth': {
                'accessToken': 'old-access', 'refreshToken': 'old-refresh', 'expiresAt': 1,
                'scopes': ['inference'], 'subscriptionType': 'max'}}
            authority, route = 'platform.claude.com:443', '/v1/oauth/token'
            # Public native OAuth client identifier, never an issuer credential.
            client = '9d1c250a-e61b-44d9-88ed-5944d1962f5e'
        else:
            original = {'padding': padding, 'tokens': {'access_token': 'old-access',
                'refresh_token': 'old-refresh', 'id_token': token}, 'last_refresh': 'old'}
            authority, route = 'auth.openai.com:443', '/oauth/token'
            client = 'app_EMoamEEZ73f0CkXaXp7hrann'
        spec = {'refresh_auth': True, 'provider': provider, 'auth_file': str(root / (provider + '.json')),
                'credential_directory': str(root), 'auth_relative': 'auth.json'}
        auth = Path(spec['auth_file'])
        vm.atomic(auth, json.dumps(original, ensure_ascii=False, separators=(',', ':')).encode('utf-8'))
        request = {'authority': authority, 'method': 'POST', 'path': route}
        body = json.dumps({'grant_type': 'refresh_token', 'refresh_token': 'old-refresh', 'client_id': client}).encode()
        return spec, original, request, body

    def test_oversized_rotation_stays_received_and_blocks_reissue_for_both_providers(self):
        for provider in ('claude', 'codex'):
            with self.subTest(provider=provider), tempfile.TemporaryDirectory() as directory:
                spec, original, request, body = self.fixture(Path(directory), provider, 'p' * 900000)
                auth = Path(spec['auth_file']); before = auth.read_bytes()
                response = json.dumps({'access_token': 'a' * 131072, 'refresh_token': 'r' * 131072,
                                       'expires_in': 3600, 'scope': 'inference'}).encode()
                lock = vm.credential_use(spec)
                try:
                    checkpoint = vm.RefreshCheckpoint(spec, lock)
                    self.assertTrue(checkpoint.begin(request, {'tls': True, 'allow_loopback': False}, body))
                    checkpoint.sent()
                    try:
                        with self.assertRaises(ValueError):
                            checkpoint.commit(response, 1000)
                    finally:
                        checkpoint.end()
                    record = json.loads(checkpoint.journal.read_bytes())
                    self.assertEqual(record['state'], 'received')
                    self.assertNotIn('result_digest', record)
                    self.assertEqual(record['received_at_ms'], 1000)
                    self.assertEqual(base64.b64decode(record['response']), response)
                    self.assertEqual(auth.read_bytes(), before)
                    for _ in range(2):
                        with self.assertRaises(ValueError):
                            checkpoint.begin(request, {'tls': True, 'allow_loopback': False}, body)
                        with self.assertRaises(ValueError):
                            vm.RefreshCheckpoint(spec, lock)
                        self.assertEqual(auth.read_bytes(), before)
                        self.assertEqual(json.loads(checkpoint.journal.read_bytes()), record)
                finally:
                    os.close(lock)

    def test_direct_unicode_export_remains_readable_without_ascii_expansion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec, original, _, _ = self.fixture(root, 'claude', 'é' * 200000)
            auth = Path(spec['auth_file']); before = auth.read_bytes()
            updated = json.loads(before)
            updated['claudeAiOauth']['accessToken'] = 'new-access'
            candidate = json.dumps(updated, ensure_ascii=False, separators=(',', ':')).encode('utf-8')
            self.assertLess(len(candidate), self.limit)
            self.assertGreater(len(json.dumps(updated).encode()), self.limit)
            generation = root / 'generation'; generation.mkdir(mode=0o700)
            vm.atomic(generation / 'auth.json', candidate)
            vm.refresh_auth(spec, generation, hashlib.sha256(before).hexdigest())
            self.assertEqual(vm.private_bytes(auth, self.limit), candidate)
            self.assertEqual(json.loads(auth.read_bytes()), updated)

    def test_checkpoint_exact_encoded_boundary_and_one_byte_excess(self):
        for provider in ('claude', 'codex'):
            for excess in (0, 1):
                with self.subTest(provider=provider, excess=excess), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    spec, original, request, body = self.fixture(root, provider)
                    updated = json.loads(json.dumps(original))
                    if provider == 'claude':
                        updated['claudeAiOauth'].update(accessToken='new-access', refreshToken='new-refresh', expiresAt=3601000)
                    else:
                        updated['tokens'].update(access_token='new-access', refresh_token='new-refresh')
                        updated['last_refresh'] = '1970-01-01T00:00:01Z'
                    padding = 'p' * (self.limit + excess - len(json.dumps(updated, separators=(',', ':')).encode()))
                    original['padding'] = padding; updated['padding'] = padding
                    expected = json.dumps(updated, separators=(',', ':')).encode()
                    self.assertEqual(len(expected), self.limit + excess)
                    auth = Path(spec['auth_file'])
                    before = json.dumps(original, separators=(',', ':')).encode()
                    self.assertLessEqual(len(before), self.limit)
                    vm.atomic(auth, before)
                    lock = vm.credential_use(spec)
                    try:
                        checkpoint = vm.RefreshCheckpoint(spec, lock)
                        checkpoint.begin(request, {'tls': True, 'allow_loopback': False}, body); checkpoint.sent()
                        response = b'{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}'
                        try:
                            if excess:
                                with self.assertRaises(ValueError): checkpoint.commit(response, 1000)
                            else:
                                checkpoint.commit(response, 1000)
                        finally:
                            checkpoint.end()
                        self.assertEqual(auth.read_bytes(), before if excess else expected)
                        self.assertEqual(json.loads(checkpoint.journal.read_bytes())['state'], 'received' if excess else 'committed')
                        if not excess:
                            vm.RefreshCheckpoint(spec, lock)
                            self.assertEqual(vm.runtime_seed(root / 'runtime', str(auth), 'auth.json')[0], [('auth.json', expected)])
                    finally:
                        os.close(lock)

    def test_direct_export_rejects_ambiguous_or_non_utf8_json_without_mutation(self):
        for variant in ('duplicate', 'nan', 'overflow', 'surrogate', 'utf16', 'utf32'):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                spec, original, _, _ = self.fixture(root, 'claude')
                auth = Path(spec['auth_file']); before = auth.read_bytes()
                updated = json.loads(before); updated['claudeAiOauth']['accessToken'] = 'new-access'
                valid = json.dumps(updated)
                if variant == 'duplicate': candidate = valid.replace('"padding": ""', '"padding":"forged","padding":""').encode()
                elif variant == 'nan': candidate = valid.replace('"expiresAt": 1', '"expiresAt": NaN').encode()
                elif variant == 'overflow': candidate = valid.replace('"expiresAt": 1', '"expiresAt": 1e9999').encode()
                elif variant == 'surrogate': candidate = valid.replace('new-access', 'new-\\ud800').encode()
                else: candidate = valid.encode('utf-16' if variant == 'utf16' else 'utf-32')
                generation = root / 'generation'; generation.mkdir(mode=0o700)
                vm.atomic(generation / 'auth.json', candidate)
                with self.assertRaises(ValueError):
                    vm.refresh_auth(spec, generation, hashlib.sha256(before).hexdigest())
                self.assertEqual(auth.read_bytes(), before)

    def test_growing_credential_read_is_rejected_after_actual_read(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'auth.json'
            path.write_bytes(b'{}'); path.chmod(0o600)
            stream = mock.MagicMock()
            descriptor = os.open(path, os.O_RDONLY)
            stream.__enter__.return_value = stream
            stream.fileno.return_value = descriptor
            stream.read.return_value = b'p' * (self.limit + 1)
            try:
                with mock.patch.object(vm.os, 'open', return_value=descriptor), mock.patch.object(vm.os, 'fdopen', return_value=stream):
                    with self.assertRaises(ValueError): vm.credential_bytes(path)
                stream.read.assert_called_once_with(self.limit + 1)
            finally:
                os.close(descriptor)

    def test_gateway_oversize_receipt_blocks_success_and_reissue(self):
        gateway_spec = importlib.util.spec_from_file_location('roundtrip_gateway', HERE / 'native-provider-gateway.py')
        gateway = importlib.util.module_from_spec(gateway_spec); gateway_spec.loader.exec_module(gateway)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec, _, request, body = self.fixture(root, 'claude', 'p' * 900000)
            auth = Path(spec['auth_file']); before = auth.read_bytes()
            lock = vm.credential_use(spec)
            checkpoint = vm.RefreshCheckpoint(spec, lock)
            policy = {'version': 1, 'authorities': [{'authority': request['authority'], 'tls': True,
                'allow_loopback': False, 'routes': [{'path_prefix': request['path'], 'exact': True,
                'methods': ['POST'], 'websocket': False}]}]}
            frames = []
            host = gateway.HostGateway(policy, frames.append, refresh_checkpoint=checkpoint)
            response = mock.Mock(status=200)
            response.getheaders.return_value = [('Content-Type', 'application/json')]
            response.read.return_value = json.dumps({'access_token': 'a' * 131072,
                'refresh_token': 'r' * 131072, 'expires_in': 3600}).encode()
            try:
                with mock.patch.object(gateway, 'safe_socket', return_value=mock.Mock()) as connect, mock.patch.object(gateway.http.client, 'HTTPResponse', return_value=response):
                    state = gateway.State(); state.queue.put({'type': 'gateway_data', 'decoded': body}); state.queue.put({'type': 'gateway_end'})
                    host.serve('test', state, dict(request, id='test'))
                self.assertEqual(connect.return_value.sendall.call_count, 1)
                self.assertTrue(any(frame['type'] == 'gateway_error' for frame in frames))
                self.assertFalse(any(frame['type'] in ('gateway_headers', 'gateway_data', 'gateway_end') for frame in frames))
                self.assertEqual(json.loads(checkpoint.journal.read_bytes())['state'], 'received')
                self.assertEqual(auth.read_bytes(), before)
                with self.assertRaises(ValueError): checkpoint.begin(request, {'tls': True, 'allow_loopback': False}, body)
            finally:
                host.close(); os.close(lock)

    def test_checkpoint_write_failures_preserve_lineage_and_recover_without_rotation(self):
        for provider in ('claude', 'codex'):
            for fail_at in (1, 2, 3, 4):
                with self.subTest(provider=provider, fail_at=fail_at), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    spec, _, request, body = self.fixture(root, provider)
                    auth = Path(spec['auth_file']); before = auth.read_bytes()
                    response = b'{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}'
                    lock = vm.credential_use(spec)
                    try:
                        checkpoint = vm.RefreshCheckpoint(spec, lock)
                        checkpoint.begin(request, {'tls': True, 'allow_loopback': False}, body); checkpoint.sent()
                        real_atomic = vm.atomic
                        calls = []
                        def failing_atomic(path, data):
                            calls.append(path)
                            if len(calls) == fail_at: raise OSError('synthetic publication failure')
                            real_atomic(path, data)
                        try:
                            with mock.patch.object(vm, 'atomic', side_effect=failing_atomic):
                                with self.assertRaises(OSError): checkpoint.commit(response, 1000)
                        finally:
                            checkpoint.end()
                        self.assertEqual(len(calls), fail_at)
                        record = json.loads(checkpoint.journal.read_bytes())
                        self.assertEqual(record['state'], 'pending' if fail_at == 1 else 'received')
                        self.assertEqual(auth.read_bytes() == before, fail_at <= 3)
                        if fail_at == 1:
                            with self.assertRaises(ValueError): vm.RefreshCheckpoint(spec, lock)
                            self.assertEqual(auth.read_bytes(), before)
                        else:
                            self.assertEqual(record['received_at_ms'], 1000)
                            self.assertEqual(base64.b64decode(record['response']), response)
                            vm.RefreshCheckpoint(spec, lock)
                            updated = json.loads(auth.read_bytes())
                            if provider == 'claude':
                                self.assertEqual(updated['claudeAiOauth']['expiresAt'], 3601000)
                                self.assertEqual(updated['claudeAiOauth']['refreshToken'], 'new-refresh')
                            else:
                                self.assertEqual(updated['last_refresh'], '1970-01-01T00:00:01Z')
                                self.assertEqual(updated['tokens']['refresh_token'], 'new-refresh')
                            committed = auth.read_bytes()
                            vm.RefreshCheckpoint(spec, lock)
                            self.assertEqual(auth.read_bytes(), committed)
                            self.assertEqual(json.loads(checkpoint.journal.read_bytes())['state'], 'committed')
                    finally:
                        os.close(lock)

    def test_unchanged_direct_export_does_not_publish(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec, original, _, _ = self.fixture(root, 'claude')
            auth = Path(spec['auth_file']); before = auth.read_bytes()
            generation = root / 'generation'; generation.mkdir(mode=0o700)
            vm.atomic(generation / 'auth.json', json.dumps(original, indent=2).encode())
            with mock.patch.object(vm, 'atomic', side_effect=AssertionError('unexpected publication')):
                vm.refresh_auth(spec, generation, hashlib.sha256(before).hexdigest())
            self.assertEqual(auth.read_bytes(), before)

    def test_committed_marker_cannot_make_oversized_legacy_credential_readable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec, _, _, _ = self.fixture(root, 'claude')
            auth = Path(spec['auth_file'])
            vm.atomic(auth, json.dumps({'padding': 'p' * self.limit}).encode())
            vm.atomic(auth.with_name(auth.name + '.refresh-checkpoint.json'), b'{"state":"committed","result_digest":"old"}')
            lock = vm.credential_use(spec)
            try:
                with self.assertRaises(ValueError):
                    vm.RefreshCheckpoint(spec, lock)
            finally:
                os.close(lock)



if __name__ == '__main__':
    unittest.main()
