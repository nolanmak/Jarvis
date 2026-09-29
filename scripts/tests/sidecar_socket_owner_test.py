"""A sidecar may replace only its own stale socket, never a live peer's."""

import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest


WRAPPER = Path(__file__).resolve().parents[1] / 'start-sidecar.py'
SERVER = '''import os,socket,time
s=socket.socket(socket.AF_UNIX)
s.bind(os.environ['FETCH_SOCKET'])
os.chmod(os.environ['FETCH_SOCKET'], 0o600)
s.listen()
time.sleep(60)
'''


class SocketOwnerTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix='jarvis-socket-owner-', dir='/tmp')
        self.addCleanup(self.scratch.cleanup)
        self.directory = Path(self.scratch.name) / 'runtime café'
        self.directory.mkdir(mode=0o700)
        self.sock = self.directory / 'fetch.sock'
        self.env = {**os.environ, 'FETCH_SOCKET': str(self.sock)}
        self.command = [sys.executable, str(WRAPPER), 'fetch', str(self.sock),
                        sys.executable, '-c', SERVER]
        self.children = []
        self.addCleanup(self.stop_children)

    def stop_children(self):
        for child in self.children:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=3)
            child.stderr.close()

    def start(self):
        child = subprocess.Popen(self.command, env=self.env, stdout=subprocess.DEVNULL,
                                 stderr=subprocess.PIPE, text=True)
        self.children.append(child)
        deadline = time.monotonic() + 3
        while not self.connects() and child.poll() is None and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertIsNone(child.poll(), child.stderr.read() if child.poll() is not None else '')
        self.assertTrue(self.connects())
        return child

    def connects(self):
        peer = socket.socket(socket.AF_UNIX)
        try:
            peer.connect(str(self.sock))
            return True
        except (ConnectionRefusedError, FileNotFoundError):
            return False
        finally:
            peer.close()

    def test_second_owner_cannot_remove_live_socket(self):
        first = self.start()
        original = self.sock.stat().st_ino
        second = subprocess.run(self.command, env=self.env, capture_output=True,
                                text=True, timeout=3)
        self.assertNotEqual(second.returncode, 0)
        self.assertEqual(self.sock.stat().st_ino, original)
        self.assertIsNone(first.poll())

    def test_stale_socket_is_recovered_after_owner_exits(self):
        first = self.start()
        first.terminate()
        first.wait(timeout=3)
        self.start()
        self.assertTrue(self.connects())

    def test_symlink_socket_is_rejected_without_touching_target(self):
        target = self.directory / 'target'
        target.write_text('synthetic secret')
        self.sock.symlink_to(target)
        result = subprocess.run(self.command, env=self.env, capture_output=True,
                                text=True, timeout=3)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.read_text(), 'synthetic secret')

    def test_fetch_credentials_require_private_file_and_reach_only_child_environment(self):
        credential = self.directory / 'fetch.env'
        credential.write_text('FIRECRAWL_API_KEY=synthetic-test-key\n')
        env = {**self.env, 'AUGMENTAGENT_FETCH_CREDENTIALS': str(credential)}
        command = [sys.executable, str(WRAPPER), 'fetch', str(self.sock),
                   sys.executable, '-c', 'import os; print(os.getenv("FIRECRAWL_API_KEY"))']
        credential.chmod(0o644)
        refused = subprocess.run(command, env=env, capture_output=True, text=True, timeout=3)
        self.assertNotEqual(refused.returncode, 0)
        self.assertNotIn('synthetic-test-key', refused.stderr)
        credential.chmod(0o600)
        allowed = subprocess.run(command, env=env, capture_output=True, text=True, timeout=3)
        self.assertEqual(allowed.returncode, 0, allowed.stderr)
        self.assertEqual(allowed.stdout.strip(), 'synthetic-test-key')


if __name__ == '__main__':
    unittest.main()
