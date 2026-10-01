"""Real process lifecycle tests; all payloads and launchd jobs are disposable."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

HELPER = Path(__file__).resolve().parents[1] / 'provider-supervisor.py'


class SupervisorTests(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.TemporaryDirectory(prefix='aa-supervisor-', dir='/tmp')
        self.addCleanup(self.root.cleanup)
        self.receipt = Path(self.root.name) / 'receipt'
        self.effect = Path(self.root.name) / 'escaped'
        self.processes = []
        self.addCleanup(self.stop_fixtures)

    def stop_fixtures(self):
        for process in self.processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream:
                    stream.close()

    def start(self, payload, *, parent=False):
        argv = [sys.executable, '-I', str(HELPER), str(self.receipt),
                sys.executable, '-I', '-c', payload, str(self.effect)]
        if parent:
            argv = [sys.executable, '-I', '-c',
                    'import subprocess,sys,time; subprocess.Popen(sys.argv[1:]); time.sleep(60)', *argv]
        process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.processes.append(process)
        return process

    def wait_ready(self, process):
        import selectors
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(15), 'provider did not start')
            line = process.stdout.readline()
            self.assertEqual(line, b'ready\n', process.stderr.read().decode() if not line else '')

    def assert_clean(self):
        deadline = time.monotonic() + 8
        while not self.receipt.exists() and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertEqual(self.receipt.read_text(), 'all-descendants-reaped\n')
        time.sleep(.8)
        self.assertFalse(self.effect.exists(), 'detached descendant escaped cleanup')

    @staticmethod
    def tree(parent_stays):
        # The intermediate parent exits immediately, before a process-tree poll
        # can observe it. setsid removes the final child from the original group.
        return (
            'import os,sys,time\n'
            'if os.fork() == 0:\n'
            ' os.setsid()\n'
            ' if os.fork() != 0: os._exit(0)\n'
            ' print("ready",flush=True)\n'
            ' time.sleep(.7)\n'
            ' open(sys.argv[1],"w").write("escaped")\n'
            ' os._exit(0)\n'
            + ('time.sleep(60)\n' if parent_stays else 'time.sleep(.15)\n')
        )

    def test_normal_exit_cleans_double_fork_and_detached_session(self):
        process = self.start(self.tree(False))
        self.wait_ready(process)
        self.assertEqual(process.wait(timeout=15), 0, process.stderr.read().decode())
        self.assert_clean()

    def test_cancel_cleans_double_fork_and_detached_session(self):
        process = self.start(self.tree(True))
        self.wait_ready(process)
        process.terminate()
        self.assertEqual(process.wait(timeout=15), 128 + signal.SIGTERM)
        self.assert_clean()

    def test_parent_death_cleans_double_fork_and_detached_session(self):
        process = self.start(self.tree(True), parent=True)
        self.wait_ready(process)
        process.kill()
        process.wait(timeout=5)
        self.assert_clean()

    def test_environment_cwd_and_stdio_reach_provider(self):
        process = self.start('import os,sys; print(os.getcwd()); print(sys.stdin.readline().strip())')
        # stdin is inherited by the supervisor in production; the process tree
        # scenarios above also exercise stdout descriptor forwarding.
        out, err = process.communicate(b'stdin fixture\n', timeout=15)
        self.assertEqual(process.returncode, 0, err.decode())
        self.assertIn(os.getcwd().encode(), out)
        self.assertIn(b'stdin fixture', out)
        self.assert_clean()

    @unittest.skipUnless(sys.platform == 'darwin', 'launchd worker recovery')
    def test_killed_controller_cleans_tools_without_issuing_a_receipt(self):
        process = self.start(self.tree(True))
        self.wait_ready(process)
        process.kill()
        process.wait(timeout=5)
        time.sleep(1)
        self.assertFalse(self.effect.exists())
        self.assertFalse(self.receipt.exists())

    def test_cancel_does_not_signal_another_session(self):
        other_receipt = Path(self.root.name) / 'other-receipt'
        other = subprocess.Popen([sys.executable, '-I', str(HELPER), str(other_receipt),
                                  sys.executable, '-I', '-c',
                                  'import time; print("ready",flush=True); time.sleep(60)'],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.processes.append(other)
        self.wait_ready(other)
        process = self.start(self.tree(True))
        self.wait_ready(process)
        process.terminate()
        process.wait(timeout=15)
        self.assertIsNone(other.poll(), 'another session was signalled')
        self.assert_clean()

    @unittest.skipUnless(sys.platform == 'darwin', 'native macOS identity')
    def test_bridge_identity_distinguishes_running_and_exited_processes(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location('bridge', HELPER.with_name('codex-tool-bridge.py'))
        bridge = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(bridge)
        current = bridge.process_start_time(os.getpid())
        process = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
        self.processes.append(process)
        identity = bridge.process_start_time(process.pid)
        self.assertIsNotNone(identity)
        self.assertNotEqual(current, identity)
        process.terminate()
        process.wait()
        self.assertIsNone(bridge.process_start_time(process.pid))


if __name__ == '__main__':
    unittest.main()
