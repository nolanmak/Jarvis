import http.server
import json
import pathlib
import tempfile
import threading
import unittest

import verify_deployment


def private_env(path, values):
    path.write_text(''.join(f'{key}={value}\n' for key, value in values.items()))
    path.chmod(0o600)


def catalog_server(models, key=None, health=False):
    calls = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            calls.append((self.path, self.headers.get('Authorization')))
            if key and self.headers.get('Authorization') != f'Bearer {key}':
                status, body = 401, {'error': 'unauthorized'}
            elif self.path == '/health' and health:
                status, body = 200, {'status': 'ok'}
            elif self.path in ('/v1/models',):
                status, body = 200, {'data': [{'id': model} for model in models]}
            else:
                status, body = 404, {'error': 'unknown path'}
            raw = json.dumps(body).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, calls


class DeploymentVerificationTests(unittest.TestCase):
    def test_public_secret_file_is_rejected_before_network(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            adapter = root / 'adapter.env'
            router = root / 'router.env'
            private_env(adapter, {'RUNPOD_API_KEY': 'fixture-runpod-key',
                                  'ADAPTER_API_KEY': 'fixture-adapter-key'})
            adapter.chmod(0o644)
            private_env(router, {'OPENAI_BASE_URL': 'http://127.0.0.1:1/v1',
                                 'OPENAI_API_KEY': 'fixture-router-key'})
            with self.assertRaisesRegex(verify_deployment.DeploymentError, 'owner-private'):
                verify_deployment.verify(adapter, router, 'http://127.0.0.1:1')

    def test_missing_client_secret_fails_before_any_gateway_request(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            adapter = root / 'adapter.env'
            router = root / 'router.env'
            private_env(adapter, {'RUNPOD_API_KEY': 'fixture-runpod-key'})
            private_env(router, {'OPENAI_BASE_URL': 'http://127.0.0.1:1/v1',
                                 'OPENAI_API_KEY': 'fixture-router-key'})
            with self.assertRaisesRegex(verify_deployment.DeploymentError, 'ADAPTER_API_KEY'):
                verify_deployment.verify(adapter, router, 'http://127.0.0.1:1')

    def test_unreachable_router_fails_after_read_only_adapter_checks(self):
        server, thread, calls = catalog_server(['qwen38-27b', 'glm-5.3-flash'],
                                               key='fixture-adapter-key', health=True)
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                adapter = root / 'adapter.env'
                router = root / 'router.env'
                private_env(adapter, {'ADAPTER_API_KEY': 'fixture-adapter-key',
                                      'RUNPOD_API_KEY': 'fixture-runpod-key'})
                private_env(router, {'OPENAI_BASE_URL': 'http://127.0.0.1:1/v1',
                                     'OPENAI_API_KEY': 'fixture-router-key'})
                with self.assertRaisesRegex(verify_deployment.DeploymentError, '9Router'):
                    verify_deployment.verify(adapter, router, f'http://127.0.0.1:{server.server_port}')
            self.assertEqual([path for path, _ in calls], ['/health', '/v1/models'])
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_preflight_reads_both_catalogs_without_inference(self):
        adapter_server, adapter_thread, adapter_calls = catalog_server(
            ['qwen38-27b', 'glm-5.3-flash'], key='fixture-adapter-key', health=True)
        router_server, router_thread, router_calls = catalog_server(
            ['runpod/qwen38-27b', 'runpod/glm-5.3-flash'], key='fixture-router-key')
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                adapter = root / 'adapter.env'
                router = root / 'router.env'
                private_env(adapter, {'ADAPTER_API_KEY': 'fixture-adapter-key',
                                      'RUNPOD_API_KEY': 'fixture-runpod-key'})
                private_env(router, {'OPENAI_BASE_URL': f'http://127.0.0.1:{router_server.server_port}/v1',
                                     'OPENAI_API_KEY': 'fixture-router-key'})
                report = verify_deployment.verify(
                    adapter, router, f'http://127.0.0.1:{adapter_server.server_port}')
            self.assertEqual(set(report['adapter_models']), {'qwen38-27b', 'glm-5.3-flash'})
            self.assertEqual(set(report['router_models']), {'runpod/qwen38-27b', 'runpod/glm-5.3-flash'})
            self.assertEqual([path for path, _ in adapter_calls], ['/health', '/v1/models'])
            self.assertEqual([path for path, _ in router_calls], ['/v1/models'])
        finally:
            for server, thread in ((adapter_server, adapter_thread),
                                   (router_server, router_thread)):
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)

    def test_redirect_target_never_receives_a_client_key(self):
        redirected = []

        class Destination(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                redirected.append(self.headers.get('Authorization'))
                self.send_response(200)
                self.end_headers()

            def log_message(self, *args):
                pass

        destination = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Destination)

        class Source(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(302)
                self.send_header('Location', f'http://localhost:{destination.server_port}/stolen')
                self.end_headers()

            def log_message(self, *args):
                pass

        source = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Source)
        threads = [threading.Thread(target=server.serve_forever, daemon=True)
                   for server in (source, destination)]
        for thread in threads:
            thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                adapter = root / 'adapter.env'
                router = root / 'router.env'
                private_env(adapter, {'RUNPOD_API_KEY': 'fixture-runpod-key',
                                      'ADAPTER_API_KEY': 'fixture-adapter-key'})
                private_env(router, {'OPENAI_BASE_URL': 'http://127.0.0.1:1/v1',
                                     'OPENAI_API_KEY': 'fixture-router-key'})
                with self.assertRaisesRegex(verify_deployment.DeploymentError, 'adapter'):
                    verify_deployment.verify(adapter, router, f'http://127.0.0.1:{source.server_port}')
            self.assertEqual(redirected, [])
        finally:
            for server in (source, destination):
                server.shutdown()
                server.server_close()
            for thread in threads:
                thread.join(timeout=2)


if __name__ == '__main__':
    unittest.main()
