"""Offline CDP page and sidecar restart contract; needs browser/setup.sh."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from urllib.request import urlopen

from playwright.sync_api import sync_playwright


ROOT = Path(__file__).resolve().parents[2]
SIDECAR = ROOT / 'sidecars/browser'
PYTHON = SIDECAR / '.venv/bin/python'


class Page(BaseHTTPRequestHandler):
    def do_GET(self):
        content = b'<html><body><h1>Synthetic browser page</h1></body></html>'
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(content)))
        self.end_headers()
        self.wfile.write(content)

    def log_message(self, *_args):
        pass


def request(path, operation, params=None):
    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(10)
        peer.connect(str(path))
        peer.sendall((json.dumps({'request_id': operation, 'op': operation,
                                  'params': params or {}, 'timeout_ms': 10000}) + '\n').encode())
        response = bytearray()
        while b'\n' not in response:
            data = peer.recv(65536)
            if not data:
                raise AssertionError('browser sidecar closed before responding')
            response.extend(data)
    return json.loads(response.split(b'\n', 1)[0])


class BrowserSmokeTests(unittest.TestCase):
    def test_synthetic_page_and_sidecar_restart(self):
        with tempfile.TemporaryDirectory(prefix='jarvis-browser-', dir='/tmp') as scratch:
            runtime = Path(scratch)
            runtime.chmod(0o700)
            sock = runtime / 'browser.sock'
            profile = runtime / 'profile café'
            page_server = ThreadingHTTPServer(('127.0.0.1', 0), Page)
            thread = threading.Thread(target=page_server.serve_forever, daemon=True)
            thread.start()
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                cdp_port = probe.getsockname()[1]
            with sync_playwright() as playwright:
                binary = playwright.chromium.executable_path
            chrome = subprocess.Popen([
                binary, '--headless=new', '--no-sandbox', '--disable-gpu',
                '--no-first-run', f'--remote-debugging-port={cdp_port}',
                '--remote-debugging-address=127.0.0.1', f'--user-data-dir={profile}',
            ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            owner = None
            env = {**os.environ, 'AUGMENTAGENT_BROWSER_SOCK': str(sock),
                   'AUGMENTAGENT_BROWSER_CDP': f'http://127.0.0.1:{cdp_port}'}
            command = [str(PYTHON), str(ROOT / 'scripts/start-sidecar.py'),
                       'browser', str(sock), str(PYTHON), str(SIDECAR / 'sidecar.py')]
            try:
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    try:
                        with urlopen(f'http://127.0.0.1:{cdp_port}/json/version', timeout=1):
                            break
                    except OSError:
                        self.assertIsNone(chrome.poll(), chrome.stderr.read().decode() if chrome.poll() is not None else '')
                        time.sleep(0.1)
                else:
                    self.fail('synthetic Chrome CDP did not start')

                for cycle in range(2):
                    owner = subprocess.Popen(command, env=env, cwd=SIDECAR,
                                             stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                    deadline = time.monotonic() + 20
                    while time.monotonic() < deadline:
                        try:
                            pong = request(sock, 'ping')
                            break
                        except (OSError, AssertionError):
                            self.assertIsNone(owner.poll(), owner.stderr.read().decode() if owner.poll() is not None else '')
                            time.sleep(0.1)
                    else:
                        self.fail('browser sidecar did not become ready')
                    self.assertTrue(pong['ok'], pong)
                    duplicate = subprocess.run(command, env=env, cwd=SIDECAR,
                                               capture_output=True, timeout=5)
                    self.assertNotEqual(duplicate.returncode, 0)
                    self.assertTrue(request(sock, 'ping')['ok'])
                    navigated = request(sock, 'navigate', {
                        'url': f'http://127.0.0.1:{page_server.server_port}/fixture',
                    })
                    self.assertTrue(navigated['ok'], navigated)
                    text = request(sock, 'get_text')
                    self.assertTrue(text['ok'], text)
                    self.assertIn('Synthetic browser page', text['result']['text'])
                    owner.terminate()
                    owner.wait(timeout=5)
                    owner.stderr.close()
                    owner = None
            finally:
                if owner is not None:
                    if owner.poll() is None:
                        owner.terminate()
                    owner.wait(timeout=5)
                    owner.stderr.close()
                if chrome.poll() is None:
                    chrome.terminate()
                chrome.wait(timeout=5)
                chrome.stderr.close()
                page_server.shutdown()
                page_server.server_close()
                thread.join(timeout=5)


if __name__ == '__main__':
    unittest.main()
