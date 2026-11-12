#!/usr/bin/env python3
"""Actual subprocess fixtures for durable raw-verifier output, without models."""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

SUPERVISOR = Path(__file__).resolve().parents[1] / 'provider-supervisor.py'


class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.session = self.root / 'supervisor'
        self.capture = self.root / 'capture'
        for path in (self.session, self.capture):
            path.mkdir(mode=0o700)
        self.command = hashlib.sha256(b'original raw command fixture').hexdigest()
        request = self.capture / 'request.json'
        request.write_text(json.dumps({'version': 1, 'command': self.command, 'output_limit': 1024}))
        request.chmod(0o600)
        proceed = self.session / 'launch-continue'
        proceed.write_bytes(b'admitted\n')
        proceed.chmod(0o600)
        self.environment = {'PATH': '/usr/bin:/bin', 'JARVIS_SUPERVISOR_ADMISSION': '1',
                            'JARVIS_RAW_VERIFIER_CAPTURE': str(self.capture)}

    def launch(self, code):
        return subprocess.run([sys.executable, '-I', str(SUPERVISOR),
                               str(self.session / 'cleanup-complete'),
                               sys.executable, '-I', '-c', code],
                              env=self.environment, capture_output=True, timeout=10)

    def manifest(self):
        path = self.capture / 'result.json'
        self.assertTrue(path.exists(), 'the supervisor lost the actual command result')
        return json.loads(path.read_text())

    def test_completed_command_is_recorded_before_controller_consumption(self):
        result = self.launch("import os,sys; assert 'JARVIS_RAW_VERIFIER_CAPTURE' not in os.environ; print('false'); print('diagnostic',file=sys.stderr)")
        record = self.manifest()
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(record['command'], self.command)
        self.assertEqual(record['exit_code'], 0)
        self.assertFalse(record['interrupted'])
        self.assertTrue(record['output_complete'])
        for name, expected in [('stdout', b'false\n'), ('stderr', b'diagnostic\n')]:
            self.assertEqual((self.capture / name).read_bytes(), expected)
            self.assertEqual(record[name + '_sha256'], hashlib.sha256(expected).hexdigest())
            self.assertEqual(record[name + '_bytes'], len(expected))
        self.assertEqual(result.stdout, b'', 'recording must not depend on a live controller pipe')

    def test_output_limit_is_a_recorded_interruption(self):
        self.launch("import sys; sys.stdout.write('x'*8192); sys.stdout.flush()")
        record = self.manifest()
        self.assertTrue(record['interrupted'])
        self.assertFalse(record['output_complete'])
        self.assertLessEqual((self.capture / 'stdout').stat().st_size, 1024)

    def test_untrusted_capture_directory_refuses_before_child_release(self):
        self.capture.chmod(0o755)
        touched = self.root / 'child-ran'
        result = self.launch(f"from pathlib import Path; Path({str(touched)!r}).touch()")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(touched.exists())

    def test_cancellation_before_release_is_retained_without_running_child(self):
        (self.session / 'launch-continue').unlink()
        touched = self.root / 'child-ran'
        process = subprocess.Popen([sys.executable, '-I', str(SUPERVISOR),
                                    str(self.session / 'cleanup-complete'), sys.executable, '-I', '-c',
                                    f"from pathlib import Path; Path({str(touched)!r}).touch()"],
                                   env=self.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 5
            while not (self.session / 'launch-ready').exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue((self.session / 'launch-ready').exists())
            process.terminate()
            process.communicate(timeout=5)
            record = self.manifest()
            self.assertTrue(record['interrupted'])
            self.assertEqual(record['terminal_cause'], 'cancelled_before_dispatch')
            self.assertFalse(touched.exists())
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=5)

    def test_owner_death_retains_an_interrupted_result_and_reaps_child(self):
        started = self.root / 'started'
        child_code = f"import os,time; from pathlib import Path; print('before owner exit',flush=True); Path({str(started)!r}).write_text(str(os.getpid())); time.sleep(60)"
        owner_code = "import subprocess,sys; p=subprocess.Popen(sys.argv[1:],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); print(p.pid,flush=True); sys.stdin.readline()"
        owner = subprocess.Popen([sys.executable, '-I', '-c', owner_code,
                                  sys.executable, '-I', str(SUPERVISOR),
                                  str(self.session / 'cleanup-complete'),
                                  sys.executable, '-I', '-c', child_code],
                                 env=self.environment, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        supervisor = int(owner.stdout.readline())
        try:
            deadline = time.monotonic() + 5
            while not started.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(started.exists(), 'the live child never reached its handshake')
            child = int(started.read_text())
            owner.kill()
            owner.communicate(timeout=5)
            deadline = time.monotonic() + 5
            while not (self.capture / 'result.json').exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            record = self.manifest()
            self.assertTrue(record['interrupted'])
            self.assertEqual(record['command'], self.command)
            self.assertIn(b'before owner exit', (self.capture / 'stdout').read_bytes())
            self.assertFalse(Path(f'/proc/{child}').exists(), 'result published before the child was reaped')
            self.assertEqual((self.session / 'cleanup-complete').read_bytes(), b'all-descendants-reaped\n')
        finally:
            if owner.poll() is None:
                owner.kill()
                owner.communicate(timeout=5)
            try:
                command = Path(f'/proc/{supervisor}/cmdline').read_bytes()
                if str(self.session).encode() in command:
                    os.kill(supervisor, signal.SIGTERM)
            except (FileNotFoundError, ProcessLookupError):
                pass


if __name__ == '__main__':
    unittest.main()
