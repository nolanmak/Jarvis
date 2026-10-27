"""Actual flock/subprocess coverage for command-scoped storage, no model calls."""
import fcntl
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
from unittest.mock import patch

SCRIPTS = Path(__file__).parents[1]
spec = importlib.util.spec_from_file_location('storage_bridge', SCRIPTS / 'codex-tool-bridge.py')
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class CommandStorageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.config = {'lock': str(self.root / 'disk-build.lock'), 'floor_bytes': 0, 'admission_bytes': 0}

    def available(self):
        fd = os.open(self.config['lock'], os.O_RDWR | os.O_CREAT, 0o600)
        try:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except BlockingIOError:
                return False
        finally:
            os.close(fd)

    def test_model_reading_holds_no_lease_and_actual_build_holds_it(self):
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'allowed_tools': ['Read', 'Bash(cargo test*)'], 'build_runner': 'vm',
            'build_vm_config': str(self.root.parent / 'synthetic-vm.json'),
            'build_scratch_dir': str(self.root), 'command_storage': self.config})
        self.addCleanup(policy.close)
        (self.root / 'source.txt').write_text('fixture')
        policy.read('source.txt')
        self.assertTrue(self.available(), 'model reading cannot exclude independent managed builds')
        with patch.object(policy, 'command_argv', return_value=['cargo', 'test']), \
                patch.object(policy, 'run_vm_build', side_effect=lambda *_: {'exit_code': 0, 'held': not self.available()}):
            result = policy.run_command('cargo test')
        self.assertTrue(result['held'])
        self.assertTrue(self.available())

    def test_managed_wrapper_is_canonical_vm_cargo_not_host_script(self):
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'allowed_tools': ['Bash(./scripts/cargo-managed.sh *)', 'Bash(scripts/cargo-managed.sh *)']})
        self.addCleanup(policy.close)
        for prefix in ('scripts/cargo-managed.sh', './scripts/cargo-managed.sh'):
            self.assertEqual(policy.command_argv(prefix + ' test --workspace'), ['cargo', 'test', '--workspace'])
        with self.assertRaises(bridge.Denied):
            policy.command_argv('/tmp/cargo-managed.sh test')

    def test_floor_refusal_releases_lease_before_any_build(self):
        bad = dict(self.config, admission_bytes=2**63)
        with self.assertRaises(bridge.Readiness):
            with bridge.CommandStorage(bad, self.root):
                self.fail('build admitted below floor')
        self.assertTrue(self.available())

    def test_independent_actual_commands_serialize(self):
        code = """import importlib.util,json,sys
s=importlib.util.spec_from_file_location('bridge',sys.argv[1]);b=importlib.util.module_from_spec(s);s.loader.exec_module(b)
with b.CommandStorage(json.loads(sys.argv[2]),sys.argv[3]):
 print('admitted',flush=True)
"""
        with bridge.CommandStorage(self.config, self.root):
            child = subprocess.Popen([sys.executable, '-c', code, str(SCRIPTS / 'codex-tool-bridge.py'), json.dumps(self.config), str(self.root)], stdout=subprocess.PIPE, text=True)
            time.sleep(0.15)
            self.assertIsNone(child.poll())
            self.assertFalse(self.available())
        output, _ = child.communicate(timeout=5)
        self.assertEqual(output.strip(), 'admitted')
        self.assertEqual(child.returncode, 0)

    def test_cancellation_keeps_lock_until_detached_descendants_reaped(self):
        receipt = self.root / 'cleanup'
        pids = self.root / 'pids'
        worker = self.root / 'worker.py'
        worker.write_text('import os,time,sys\npid=os.fork()\nif pid==0:\n os.setsid()\n time.sleep(60)\nelse:\n open(sys.argv[1],"w").write(str(os.getpid())+" "+str(pid))\n time.sleep(60)\n')
        storage = bridge.CommandStorage(self.config, self.root).__enter__()
        env = {'PATH': os.defpath, 'JARVIS_STORAGE_LEASE_FD': str(storage.fd)}
        supervisor = subprocess.Popen([sys.executable, '-I', str(SCRIPTS / 'provider-supervisor.py'), str(receipt), sys.executable, str(worker), str(pids)], env=env, pass_fds=(storage.fd,))
        try:
            deadline = time.monotonic() + 5
            while not pids.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(pids.exists())
            descendants = [int(pid) for pid in pids.read_text().split()]
            storage.__exit__()
            self.assertFalse(self.available(), 'closing bridge FD must not unlock the live subtree')
            supervisor.terminate()
            supervisor.wait(timeout=8)
            self.assertEqual(receipt.read_text(), 'all-descendants-reaped\n')
            self.assertTrue(all(not Path(f'/proc/{pid}').exists() for pid in descendants))
            self.assertTrue(self.available())
        finally:
            storage.__exit__()
            if supervisor.poll() is None:
                supervisor.terminate()
                supervisor.wait(timeout=8)


if __name__ == '__main__':
    unittest.main()
