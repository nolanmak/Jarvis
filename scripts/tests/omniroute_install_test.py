"""Installer acceptance against a disposable host model; no real service is started."""
import base64
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def load():
    spec = importlib.util.spec_from_file_location('omni_install', ROOT / 'scripts/lib/omniroute_install.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.mod = load()
        sleeper = patch.object(self.mod.time, 'sleep', return_value=None)
        sleeper.start()
        self.addCleanup(sleeper.stop)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.payload = b'pinned package fixture'
        self.pin = {'version': '3.8.52', 'integrity': 'sha512-' + base64.b64encode(hashlib.sha512(self.payload).digest()).decode()}
        self.calls = []
        self.requests = []
        self.secrets = {}
        self.version = None
        self.active = False
        self.desired = self.pin['version']
        self.bad_health = False
        self.settings = {'cloudEnabled': False, 'requireLogin': True}
        self.compression = {}
        self.keys = 0
        self.tokens = []
        self.inst = self.mod.Installer(root / 'config/augmentagent', root / 'data/augmentagent/omniroute', self.pin)
        self.inst.command = self.command
        self.inst.request = self.request
        self.inst.download = lambda path: path.write_bytes(self.payload)
        self.inst.binary = '/fixture/bin/omniroute'

    def command(self, argv, **kwargs):
        self.calls.append(argv)
        out = ''
        rc = 0
        if argv[:2] == ['node', '--version']:
            out = 'v22.22.2\n'
        elif argv[0] == 'secret-tool':
            account = argv[-1]
            if argv[1] == 'lookup':
                out = self.secrets.get(account, '')
                rc = 0 if out else 1
            elif argv[1] == 'store':
                self.secrets[account] = kwargs['input'].strip()
        elif argv[-1] == '--version':
            out = self.version or ''
            rc = 0 if self.version else 1
        elif argv[:3] == ['npm', 'install', '-g']:
            self.version = '3.8.52' if Path(argv[3]).name in ('installed.tgz', 'previous.tgz') else self.desired
        elif 'backup' in argv:
            backup = self.inst.data / 'backups/omniroute-backup-fixture'
            backup.mkdir(parents=True)
            (backup / 'backup-info.json').write_text('{}')
        elif argv[:2] == ['systemctl', '--user']:
            if 'restart' in argv:
                self.active = True
            if 'stop' in argv:
                self.active = False
            if 'is-active' in argv:
                rc = 0 if self.active else 3
        elif argv[0] == 'ss':
            out = 'LISTEN 0 511 127.0.0.1:20128 0.0.0.0:*\n'
        return subprocess.CompletedProcess(argv, rc, out, '')

    def request(self, method, path, body=None, cookie=None, key=None):
        self.requests.append((method, path, body, cookie, key))
        if self.bad_health and self.version == self.desired:
            raise OSError('unhealthy release')
        if path == '/api/auth/login':
            return {}, 'auth_token=fixture'
        if path == '/api/settings':
            if method == 'PATCH':
                self.settings.update({k: v for k, v in body.items() if k != 'currentPassword'})
            return dict(self.settings), ''
        if path == '/api/settings/compression':
            if method == 'PUT':
                self.compression.update(body)
            return dict(self.compression), ''
        if path.startswith('/api/tunnels/'):
            return {'running': False, 'enabled': False}, ''
        if path == '/api/keys':
            self.keys += 1
            return {'key': 'sk-fixture', 'id': 'inference'}, ''
        if path == '/api/cli/tokens':
            self.tokens.append(body['scope'])
            return {'token': 'oma_live_fixture_' + body['scope'], 'id': body['scope']}, ''
        return {}, ''

    def test_fresh_host_loopback_health_keys_direct_and_rerun_noop(self):
        self.inst.install()
        unit = self.inst.unit.read_text()
        for expected in ['serve --headless --port 20128', 'OMNIROUTE_SERVER_HOST=127.0.0.1',
                         'OMNIROUTE_HEADLESS=1', 'REQUIRE_API_KEY=true', 'Restart=on-failure']:
            self.assertIn(expected, unit)
        self.assertTrue(self.active)
        config = json.loads(self.inst.router.read_text())
        self.assertEqual((config['gateway'], config['mode'], config['base_url']),
                         ('omniroute', 'direct', 'http://127.0.0.1:20128/v1'))
        self.assertEqual(self.keys, 1)
        self.assertEqual(self.tokens, ['read', 'write'])
        self.assertNotIn('INITIAL_PASSWORD=', self.inst.env_file.read_text())
        for p in [self.inst.env_file, self.inst.router, self.inst.receipt]:
            self.assertEqual(p.stat().st_mode & 0o777, 0o600)
        self.assertFalse(self.compression['enabled'])
        self.assertEqual(self.compression['defaultMode'], 'off')
        self.assertFalse(self.compression['cavemanConfig']['enabled'])
        self.assertFalse(self.compression['rtkConfig']['enabled'])
        paths = [p for _, p, *_ in self.requests]
        for path in ['/api/health', '/api/health/ping', '/api/monitoring/health', '/v1/models']:
            self.assertIn(path, paths)
        self.assertTrue(any(c[0] == 'ss' for c in self.calls))
        changes = len([c for c in self.calls if c[0] == 'npm' or 'restart' in c])
        contents = self.inst.router.read_bytes()
        self.inst.install()
        self.assertEqual(changes, len([c for c in self.calls if c[0] == 'npm' or 'restart' in c]))
        self.assertEqual(self.keys, 1)
        self.assertEqual(self.tokens, ['read', 'write'])
        self.assertEqual(contents, self.inst.router.read_bytes())

    def test_upgrade_requires_flag_then_backups_and_restores_failed_health(self):
        self.inst.install()
        original = self.inst.receipt.read_bytes()
        self.inst.pin = dict(self.pin, version='3.8.53')
        self.desired = '3.8.53'
        with self.assertRaisesRegex(RuntimeError, '--upgrade'):
            self.inst.install()
        self.bad_health = True
        with self.assertRaisesRegex(RuntimeError, 'rollback'):
            self.inst.install(upgrade=True)
        backups = [i for i, c in enumerate(self.calls) if 'backup' in c]
        installs = [i for i, c in enumerate(self.calls) if c[:3] == ['npm', 'install', '-g']]
        self.assertLess(backups[-1], installs[-2])
        self.assertTrue(any(c[-3:] == ['restore', 'fixture', '--yes'] for c in self.calls))
        self.assertEqual(self.version, '3.8.52')
        self.assertTrue(self.active)
        self.assertEqual(self.inst.receipt.read_bytes(), original)

    def test_public_listener_is_rejected_and_fresh_service_stopped(self):
        command = self.inst.command
        def public(argv, **kwargs):
            result = command(argv, **kwargs)
            if argv[0] == 'ss':
                result.stdout += 'LISTEN 0 511 0.0.0.0:20128 0.0.0.0:*\n'
            return result
        self.inst.command = public
        with self.assertRaisesRegex(RuntimeError, 'loopback'):
            self.inst.install()
        self.assertFalse(self.active)
        self.assertFalse(self.inst.router.exists())
        self.assertFalse(any(path == '/api/auth/login' for _, path, *_ in self.requests))

    def test_failed_first_start_can_be_retried_without_adopting_unmanaged_install(self):
        command = self.inst.command
        def public(argv, **kwargs):
            result = command(argv, **kwargs)
            if argv[0] == 'ss':
                result.stdout = 'LISTEN 0 511 0.0.0.0:20128 0.0.0.0:*\n'
            return result
        self.inst.command = public
        with self.assertRaisesRegex(RuntimeError, 'loopback'):
            self.inst.install()
        self.inst.command = command
        self.inst.install()
        self.assertTrue(self.active)
        self.assertTrue(self.inst.receipt.exists())
        self.assertEqual(self.keys, 1)

    def test_too_old_node_and_conflicting_router_refuse_before_install(self):
        command = self.inst.command
        def old(argv, **kwargs):
            result = command(argv, **kwargs)
            if argv == ['node', '--version']:
                result.stdout = 'v22.22.1\n'
            return result
        self.inst.command = old
        with self.assertRaisesRegex(RuntimeError, '22.22.2'):
            self.inst.install()
        self.assertFalse(any(c[0] == 'npm' for c in self.calls))
        self.inst.command = command
        self.inst.router.parent.mkdir(parents=True, exist_ok=True)
        self.inst.router.write_text('{"gateway":"9router","mode":"auto"}')
        self.inst.router.chmod(0o600)
        with self.assertRaisesRegex(RuntimeError, 'existing'):
            self.inst.install()
        self.assertFalse(any(c[0] == 'npm' for c in self.calls))

    def test_checksum_rejects_tampered_package(self):
        self.inst.download = self.mod.Installer.download.__get__(self.inst)
        with patch.object(self.mod.urllib.request, 'urlopen', return_value=io.BytesIO(b'tampered')):
            with self.assertRaisesRegex(RuntimeError, 'integrity'):
                self.inst.download(Path(self.tmp.name) / 'package.tgz')

    def test_secret_file_symlink_is_not_followed(self):
        sentinel = Path(self.tmp.name) / 'sentinel'
        sentinel.write_text('keep')
        self.inst.env_file.parent.mkdir(parents=True)
        self.inst.env_file.symlink_to(sentinel)
        with self.assertRaises((RuntimeError, OSError)):
            self.inst.install()
        self.assertEqual(sentinel.read_text(), 'keep')

    def test_uninstall_keeps_data_unless_purge_is_explicit(self):
        self.inst.install()
        sentinel = self.inst.data / 'storage.sqlite'
        sentinel.write_bytes(b'owned fixture')
        self.inst.uninstall()
        self.assertEqual(sentinel.read_bytes(), b'owned fixture')
        self.assertFalse(self.inst.unit.exists())
        self.assertFalse(self.inst.receipt.exists())
        self.assertTrue(any(c == ['npm', 'rm', '-g', 'omniroute'] for c in self.calls))

    def test_purge_removes_only_owned_omniroute_state(self):
        self.inst.install()
        sibling = self.inst.data.parent / 'unrelated'
        sibling.write_text('keep')
        self.inst.uninstall(purge=True)
        self.assertFalse(self.inst.data.exists())
        self.assertFalse(self.inst.env_file.exists())
        self.assertFalse(self.inst.router.exists())
        self.assertEqual(sibling.read_text(), 'keep')
        self.assertTrue(any(c[:2] == ['secret-tool', 'clear'] for c in self.calls))

    def test_real_http_transport_sends_auth_and_refuses_redirect(self):
        seen = []
        class Handler(BaseHTTPRequestHandler):
            def do_GET(handler):
                seen.append((handler.path, handler.headers.get('Authorization'), handler.headers.get('Cookie')))
                if handler.path == '/redirect':
                    handler.send_response(302)
                    handler.send_header('Location', '/leaked')
                    handler.end_headers()
                else:
                    handler.send_response(200)
                    handler.send_header('Set-Cookie', 'auth_token=test; HttpOnly')
                    handler.end_headers()
                    handler.wfile.write(b'{"ok":true}')
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with patch.object(self.mod, 'BASE', 'http://127.0.0.1:' + str(server.server_port)):
                call = self.mod.Installer.request.__get__(self.inst)
                value, cookie = call('GET', '/v1/models', cookie='auth_token=fixture', key='sk-fixture')
                self.assertEqual(value, {'ok': True})
                self.assertEqual(cookie, 'auth_token=test')
                with self.assertRaises(self.mod.urllib.error.HTTPError) as caught:
                    call('GET', '/redirect', key='sk-fixture')
                caught.exception.close()
            self.assertEqual(seen[0], ('/v1/models', 'Bearer sk-fixture', 'auth_token=fixture'))
            self.assertFalse(any(path == '/leaked' for path, *_ in seen))
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_backup_failure_restarts_previous_service_without_updating(self):
        self.inst.install()
        self.inst.pin = dict(self.pin, version='3.8.53')
        self.desired = '3.8.53'
        command = self.inst.command
        def broken_backup(argv, **kwargs):
            if 'backup' in argv:
                return subprocess.CompletedProcess(argv, 1, '', 'backup failed')
            return command(argv, **kwargs)
        self.inst.command = broken_backup
        before = len([c for c in self.calls if c[0] == 'npm'])
        with self.assertRaises(RuntimeError):
            self.inst.install(upgrade=True)
        self.assertEqual(before, len([c for c in self.calls if c[0] == 'npm']))
        self.assertTrue(self.active)
        self.assertEqual(self.version, '3.8.52')

    def test_node_floor_versions(self):
        for version, valid in [('22.22.1', False), ('22.22.2', True), ('23.0.0', False),
                               ('24.0.0', True), ('26.9.0', True), ('27.0.0', False)]:
            self.assertEqual(self.mod.supported_node('v' + version), valid)


if __name__ == '__main__':
    unittest.main()
