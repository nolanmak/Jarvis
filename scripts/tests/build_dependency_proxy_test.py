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
