"""Read-only public dependency gateway contracts; synthetic requests only."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
import urllib.error

SPEC = importlib.util.spec_from_file_location('dependency_proxy', Path(__file__).parents[1] / 'build-dependency-proxy.py')
proxy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(proxy)


class DependencyProxyTests(unittest.TestCase):
    def test_routes_pin_origin_and_reject_authority_traversal_and_query_injection(self):
        self.assertEqual(proxy.upstream('/npm/@example%2ffixture'), 'https://registry.npmjs.org/@example/fixture')
        self.assertEqual(proxy.upstream('/cargo-index/config.json'), 'https://index.crates.io/config.json')
        for path in ['https://example.invalid/package', '//example.invalid/package', '/npm//example.invalid',
                     '/npm/../private', '/npm/%2e%2e/private', '/npm/%252e%252e/private',
                     '/npm/a?token=synthetic', '/npm/a#fragment', '/npm/a\\b', '/unknown/a', '/npm/a\n']:
            with self.subTest(path=path), self.assertRaises(ValueError):
                proxy.upstream(path)


    def test_python_routes_are_limited_to_canonical_simple_pages_and_wheels(self):
        self.assertEqual(proxy.upstream('/pypi/simple/openpyxl/'), 'https://pypi.org/simple/openpyxl/')
        self.assertEqual(proxy.upstream('/pypi-files/packages/ab/cd/example-1-py3-none-any.whl'),
                         'https://files.pythonhosted.org/packages/ab/cd/example-1-py3-none-any.whl')
        for route in ('/pypi/simple/', '/pypi/simple/OpenPyXL/', '/pypi/simple/foo_bar/',
                      '/pypi/pypi/foo/json', '/pypi/simple/foo/?x=y', '/pypi/simple/foo',
                      '/pypi-files/packages/foo.tar.gz', '/pypi-files/elsewhere/foo.whl',
                      '/pypi-files/packages/../foo.whl'):
            with self.subTest(route=route), self.assertRaises(ValueError):
                proxy.upstream(route)

    def test_existing_build_broker_cannot_fetch_python_packages(self):
        import json
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            req = root / ('c' * 32 + '.request')
            req.write_text(json.dumps({'route': '/pypi/simple/openpyxl/'})); req.chmod(0o600)
            with patch('subprocess.run') as fetch:
                proxy.Broker(root).poll(1)
                fetch.assert_not_called()
            self.assertEqual(json.loads((root / ('c' * 32 + '.response')).read_text())['status'], 502)


    def test_python_index_binds_exact_wheel_urls_and_hashes(self):
        import json
        digest = 'a' * 64
        url = 'https://files.pythonhosted.org/packages/ab/example-1-py3-none-any.whl'
        body = json.dumps({'meta': {'api-version': '1.0'}, 'name': 'example', 'files': [
            {'filename': 'example-1-py3-none-any.whl', 'url': url, 'hashes': {'sha256': digest},
             'core-metadata': True, 'dist-info-metadata': True},
            {'filename': 'example-1.tar.gz', 'url': 'https://files.pythonhosted.org/packages/example-1.tar.gz', 'hashes': {'sha256': digest}},
        ]}).encode()
        filtered, wheels = proxy.python_index(body)
        self.assertEqual(wheels, {'/pypi-files/packages/ab/example-1-py3-none-any.whl': digest})
        files = json.loads(filtered)['files']
        self.assertEqual(len(files), 1)
        self.assertFalse(files[0].get('core-metadata'))
        self.assertFalse(files[0].get('dist-info-metadata'))

    def test_python_index_rejects_unbound_wheels_and_credentialed_urls(self):
        import json
        base = {'filename': 'example-1-py3-none-any.whl',
                'url': 'https://files.pythonhosted.org/packages/example-1-py3-none-any.whl',
                'hashes': {'sha256': 'a' * 64}}
        changes = [ {'hashes': {}}, {'hashes': {'sha256': 'not-a-digest'}},
            {'url': 'https://attacker.invalid/packages/example-1-py3-none-any.whl'},
            {'url': 'http://files.pythonhosted.org/packages/example-1-py3-none-any.whl'},
            {'url': 'https://' + 'fixture-user' + '@' + 'files.pythonhosted.org/packages/example-1-py3-none-any.whl'},
            {'url': base['url'] + '?token=synthetic'}, {'url': base['url'] + '#secret'},
            {'url': 'https://files.pythonhosted.org/packages/../example-1-py3-none-any.whl'},
            {'filename': '../example-1-py3-none-any.whl'},
        ]
        for change in changes:
            with self.subTest(change=change), self.assertRaises(ValueError):
                proxy.python_index(json.dumps({'files': [{**base, **change}]}).encode())


    def test_compute_broker_denies_build_registries_and_unlisted_wheels_before_fetch(self):
        import json
        from unittest.mock import patch
        for route in ('/npm/example', '/cargo-index/config.json', '/pypi-files/packages/example-1-py3-none-any.whl'):
            with self.subTest(route=route), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                req = root / ('d' * 32 + '.request')
                req.write_text(json.dumps({'route': route})); req.chmod(0o600)
                with patch('subprocess.run') as fetch:
                    proxy.Broker(root, python=True).poll(1)
                    fetch.assert_not_called()
                self.assertEqual(json.loads((root / ('d' * 32 + '.response')).read_text())['status'], 502)

    def test_compute_broker_checks_download_digest_before_publishing(self):
        import hashlib
        import json
        import subprocess
        from unittest.mock import patch
        route = '/pypi-files/packages/example-1-py3-none-any.whl'
        for correct in (True, False):
            with self.subTest(correct=correct), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                req = root / ('e' * 32 + '.request')
                req.write_text(json.dumps({'route': route})); req.chmod(0o600)
                broker = proxy.Broker(root, python=True)
                broker.wheels[route] = hashlib.sha256(b'expected').hexdigest()
                def fetched(argv, **kwargs):
                    Path(argv[-1]).write_bytes(b'expected' if correct else b'CORRUPT')
                    return subprocess.CompletedProcess(argv, 0, json.dumps({'status': 200, 'content_type': 'application/octet-stream'}))
                with patch('subprocess.run', fetched):
                    broker.poll(1)
                result = json.loads((root / ('e' * 32 + '.response')).read_text())
                self.assertEqual(result['status'], 200 if correct else 502)
                if not correct:
                    self.assertNotIn(b'CORRUPT', (root / ('e' * 32 + '.body')).read_bytes())

    def test_redirects_cannot_leave_the_public_registry_origin(self):
        handler = proxy.RegistryRedirect()
        import urllib.request
        request = urllib.request.Request('https://registry.npmjs.org/fixture')
        with self.assertRaises(ValueError):
            handler.redirect_request(request, None, 302, '', {}, 'http://127.0.0.1/private')

    def test_broker_rejects_symlink_requests_without_reading_the_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); control = root / 'control'; control.mkdir(mode=0o700)
            private = root / 'private.json'; private.write_text('SYNTHETIC_PRIVATE')
            request = control / ('a' * 32 + '.request'); request.symlink_to(private)
            broker = proxy.Broker(control)
            broker.poll(1)
            self.assertEqual(private.read_text(), 'SYNTHETIC_PRIVATE')
            self.assertNotIn('SYNTHETIC_PRIVATE', (control / ('a' * 32 + '.body')).read_text())

    def test_request_budget_and_timeout_fail_closed_with_fixed_errors(self):
        import json
        import subprocess
        from unittest.mock import patch
        for exhausted in (False, True):
            with self.subTest(exhausted=exhausted), tempfile.TemporaryDirectory() as tmp:
                control = Path(tmp)
                request = control / ('b' * 32 + '.request')
                request.write_text(json.dumps({'route': '/npm/synthetic-fixture'})); request.chmod(0o600)
                broker = proxy.Broker(control)
                if exhausted: broker.requests = proxy.MAX_REQUESTS
                with patch('subprocess.run', side_effect=subprocess.TimeoutExpired('synthetic', 1)) as run:
                    broker.poll(1)
                self.assertEqual(run.call_count, 0 if exhausted else 1)
                response = json.loads((control / ('b' * 32 + '.response')).read_text())
                self.assertEqual(response['status'], 502)
                self.assertNotIn('synthetic-fixture', (control / ('b' * 32 + '.body')).read_text())
                self.assertFalse(request.exists())

    def test_fetch_child_discards_proxy_environment_and_sends_no_credentials(self):
        from unittest.mock import patch
        seen = []
        class Response:
            status = 200
            headers = {'Content-Type': 'application/json'}
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def read(self, limit): return b'{"name":"synthetic-fixture"}'
        class Opener:
            def open(self, request, timeout):
                seen.append(request)
                return Response()
        def build(*handlers):
            self.assertTrue(any(isinstance(h, __import__('urllib.request', fromlist=['ProxyHandler']).ProxyHandler)
                                and h.proxies == {} for h in handlers))
            return Opener()
        with patch('urllib.request.build_opener', build):
            status, content_type, body = proxy.fetch('/npm/synthetic-fixture')
        self.assertEqual(status, 200)
        self.assertEqual(content_type, 'application/json')
        self.assertIn(b'synthetic-fixture', body)
        self.assertFalse(any(key.lower() in ('authorization', 'cookie', 'proxy-authorization')
                             for key in seen[0].headers))


if __name__ == '__main__':
    unittest.main()
