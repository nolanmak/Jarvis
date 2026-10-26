#!/usr/bin/env python3
"""Local admission tests: no VM, credentials, provider calls or cgroup changes."""
import importlib.util
import json
import os
from pathlib import Path
import signal
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
