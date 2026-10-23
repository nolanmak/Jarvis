"""Real renderer socket and short synthetic MP4 contract; needs setup.sh first."""

import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
RENDERER = ROOT / 'sidecars/renderer'


def request(path, payload, timeout=120):
    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(timeout)
        peer.connect(str(path))
        peer.sendall((json.dumps(payload) + '\n').encode())
        response = bytearray()
        while b'\n' not in response:
            data = peer.recv(65536)
            if not data:
                raise AssertionError('renderer closed before responding')
            response.extend(data)
    return json.loads(response.split(b'\n', 1)[0])


class RendererSmokeTests(unittest.TestCase):
    def test_synthetic_render_and_singleton_restart(self):
        with tempfile.TemporaryDirectory(prefix='jarvis-render-cafe-', dir='/tmp') as scratch:
            runtime = Path(scratch)
            runtime.chmod(0o700)
            sock = runtime / 'renderer.sock'
            output = runtime / 'synthetic.mp4'
            command = [sys.executable, str(ROOT / 'scripts/start-sidecar.py'),
                       'renderer', str(sock), 'node', str(RENDERER / 'server.mjs')]
            env = {**os.environ, 'AUGMENTAGENT_RENDERER_SOCK': str(sock)}
            owner = subprocess.Popen(command, cwd=RENDERER, env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    try:
                        ping = request(sock, {'request_id': 'ping', 'op': 'ping'}, 1)
                        break
                    except (OSError, AssertionError):
                        self.assertIsNone(owner.poll(), owner.stderr.read().decode() if owner.poll() is not None else '')
                        time.sleep(0.1)
                else:
                    self.fail('renderer did not open a healthy socket')
                self.assertEqual(ping['result']['pong'], True)
                duplicate = subprocess.run(command, cwd=RENDERER, env=env,
                                           capture_output=True, timeout=5)
                self.assertNotEqual(duplicate.returncode, 0)
                self.assertTrue(request(sock, {'request_id': 'again', 'op': 'ping'})['ok'])
                rendered = request(sock, {
                    'request_id': 'render', 'op': 'render', 'timeout_ms': 120000,
                    'params': {'props': {'title': 'Synthetic', 'body': 'No account data',
                                         'durationSec': 0.2}, 'out_path': str(output)},
                })
                self.assertTrue(rendered['ok'], rendered)
                self.assertEqual(rendered['result']['path'], str(output))
                self.assertGreater(rendered['result']['bytes'], 1000)
                self.assertIn(b'ftyp', output.read_bytes()[:32])
            finally:
                if owner.poll() is None:
                    owner.terminate()
                try:
                    owner.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    owner.kill()
                    owner.wait(timeout=5)
                owner.stderr.close()


if __name__ == '__main__':
    unittest.main()
