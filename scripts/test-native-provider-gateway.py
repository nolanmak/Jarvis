#!/usr/bin/env python3
"""Local transport fixtures only: no native CLI, account, or external network."""
import base64
import hashlib
import http.client
import http.server
import importlib.util
import os
from pathlib import Path
import queue
import socket
import socketserver
import ssl
import tempfile
import threading
import unittest

spec = importlib.util.spec_from_file_location('gateway', Path(__file__).with_name('native-provider-gateway.py'))
g = importlib.util.module_from_spec(spec)
spec.loader.exec_module(g)


def policy(port, **limits):
    return {'version': 1, 'authorities': [{'authority': f'127.0.0.1:{port}', 'tls': False,
        'allow_loopback': True, 'routes': [{'path_prefix': '/v1/', 'methods': ['GET', 'POST'], 'websocket': True}]}],
        'limits': dict(idle_timeout_seconds=3, **limits)}


class Fixture(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    requests = []
    release = threading.Event()

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        self.requests.append((self.path, body, self.headers.get('Authorization')))
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.requests.append((self.path, None, self.headers.get('Authorization')))
        if self.path == '/v1/redirect':
            self.send_response(302)
            self.send_header('Location', 'http://169.254.169.254/latest/meta-data')
            self.send_header('Content-Length', '0')
            self.end_headers()
        elif self.path == '/v1/events':
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(b'data: first\n\n')
            self.wfile.flush()
            self.release.wait(3)
            self.wfile.write(b'data: last\n\n')
            self.wfile.flush()
            self.close_connection = True
        elif self.path == '/v1/ws':
            key = self.headers['Sec-WebSocket-Key']
            self.send_response(101)
            self.send_header('Upgrade', 'websocket')
            self.send_header('Connection', 'Upgrade')
            self.send_header('Sec-WebSocket-Accept', base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode())
            self.end_headers()
            opcode, fin, data = g.ws_read(self.rfile, True, g.CHUNK)
            while not fin:
                continuation, fin, part = g.ws_read(self.rfile, True, g.CHUNK)
                if continuation != 0:
                    raise AssertionError("invalid continuation")
                data += part
            g.ws_write(self.connection, opcode, fin, data)
            g.ws_write(self.connection, 8, True, b'')
            self.close_connection = True
        else:
            self.send_response(204)
            self.send_header('Content-Length', '0')
            self.end_headers()


class GatewayTests(unittest.TestCase):
    def setUp(self):
        Fixture.requests = []
        Fixture.release.clear()
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.port = self.server.server_address[1]
        self.responses = queue.Queue()
        self.host = g.HostGateway(policy(self.port), self.responses.put)

    def tearDown(self):
        Fixture.release.set()
        self.host.close()
        self.server.shutdown()
        self.server.server_close()

    def open(self, path, method='GET', **values):
        self.host.receive(dict(type='gateway_open', id='request', authority=f'127.0.0.1:{self.port}',
                              method=method, path=path, headers=[], **values))

    def result(self):
        frames = []
        while True:
            frame = self.responses.get(timeout=4)
            frames.append(frame)
            if frame['type'] in ('gateway_end', 'gateway_error'):
                return frames

    def test_request_exact_bytes_credentials_forwarded_not_diagnostic(self):
        self.host.receive(dict(type='gateway_open', id='request', authority=f'127.0.0.1:{self.port}', method='POST',
            path='/v1/messages', headers=[['Authorization', 'Bearer fixture-private'], ['Content-Length', '999']]))
        data = b'{"literal":"private-response", "unicode":"\xce\xbb"}'
        self.host.receive(dict(type='gateway_data', id='request', data=g.encode(data)))
        self.host.receive(dict(type='gateway_end', id='request'))
        frames = self.result()
        self.assertEqual(frames[-1]['type'], 'gateway_end')
        self.assertEqual(b''.join(g.decode(f['data'], g.CHUNK) for f in frames if f['type'] == 'gateway_data'), data)
        self.assertEqual(Fixture.requests, [('/v1/messages', data, 'Bearer fixture-private')])
        self.assertNotIn('fixture-private', str(frames))

    def test_sse_arrives_before_provider_finishes(self):
        self.open('/v1/events')
        self.host.receive(dict(type='gateway_end', id='request'))
        self.assertEqual(self.responses.get(timeout=2)['type'], 'gateway_headers')
        first = self.responses.get(timeout=2)
        self.assertEqual(g.decode(first['data'], g.CHUNK), b'data: first\n\n')
        self.assertFalse(Fixture.release.is_set())
        Fixture.release.set()
        frames = self.result()
        self.assertIn(b'data: last\n\n', [g.decode(f['data'], g.CHUNK) for f in frames if f['type'] == 'gateway_data'])

    def test_redirect_cannot_escape_policy(self):
        self.open('/v1/redirect')
        self.host.receive(dict(type='gateway_end', id='request'))
        self.assertEqual(self.result()[0], dict(type='gateway_error', id='request', code='upstream_failed'))
        self.assertEqual(len(Fixture.requests), 1)

    def test_disallowed_route_authority_method_and_encoded_traversal_never_connect(self):
        for path in ['/outside', '/v1/../outside', '/v1/%2e%2e/outside', '/v1/%252e%252e/outside', '//outside', '/v1/x\r\nInjected: yes']:
            self.open(path)
            self.assertEqual(self.responses.get(timeout=1)['type'], 'gateway_error')
        self.open('/v1/messages', method='CONNECT')
        self.assertEqual(self.responses.get(timeout=1)['type'], 'gateway_error')
        self.host.receive(dict(type='gateway_open', id='request', authority='169.254.169.254:80', method='GET', path='/v1/messages', headers=[]))
        self.assertEqual(self.responses.get(timeout=1)['type'], 'gateway_error')
        self.assertEqual(Fixture.requests, [])

    def test_untrusted_host_header_does_not_change_admitted_destination(self):
        self.host.receive(dict(type='gateway_open', id='request', authority=f'127.0.0.1:{self.port}', method='GET',
                              path='/v1/ok', headers=[['Host', 'evil.invalid']]))
        self.host.receive(dict(type='gateway_end', id='request'))
        self.assertEqual(self.result()[-1]['type'], 'gateway_end')
        self.assertEqual(len(Fixture.requests), 1)

    def test_websocket_duplex_masked_frames(self):
        key = base64.b64encode(os.urandom(16)).decode()
        self.host.receive(dict(type='gateway_open', id='request', authority=f'127.0.0.1:{self.port}', method='GET',
             path='/v1/ws', websocket=True, headers=[['Sec-WebSocket-Key', key], ['Sec-WebSocket-Version', '13']]))
        self.host.receive(dict(type='gateway_end', id='request'))
        self.assertEqual(self.responses.get(timeout=2)['status'], 101)
        self.host.receive(dict(type='gateway_ws_frame', id='request', opcode=1, fin=True, data=g.encode(b'actual duplex')))
        frame = self.responses.get(timeout=2)
        self.assertEqual(frame['type'], 'gateway_ws_frame')
        self.assertEqual(g.decode(frame['data'], g.CHUNK), b'actual duplex')
        self.assertEqual(self.result()[-1]['type'], 'gateway_end')

    def test_large_websocket_message_is_losslessly_fragmented(self):
        key = base64.b64encode(os.urandom(16)).decode()
        self.host.receive(dict(type='gateway_open', id='request', authority=f'127.0.0.1:{self.port}', method='GET',
             path='/v1/ws', websocket=True, headers=[['Sec-WebSocket-Key', key], ['Sec-WebSocket-Version', '13']]))
        self.host.receive(dict(type='gateway_end', id='request'))
        self.assertEqual(self.responses.get(timeout=2)['status'], 101)
        payload = b'x' * 200000
        for opcode, fin, data in g.ws_chunks(1, True, payload, g.CHUNK):
            self.host.receive(dict(type='gateway_ws_frame', id='request', opcode=opcode, fin=fin, data=g.encode(data)))
        frames = self.result()
        data = [frame for frame in frames if frame['type'] == 'gateway_ws_frame' and frame['opcode'] != 8]
        self.assertEqual(b''.join(g.decode(frame['data'], g.CHUNK) for frame in data), payload)
        self.assertEqual([frame['opcode'] for frame in data], [1, 0, 0, 0])
        self.assertEqual([frame['fin'] for frame in data], [False, False, False, True])

    def test_limits_and_cancel_do_not_open_another_connection(self):
        small = g.HostGateway(policy(self.port, max_request_bytes=4, max_connections=1), self.responses.put)
        try:
            small.receive(dict(type='gateway_open', id='one', authority=f'127.0.0.1:{self.port}', method='POST', path='/v1/messages', headers=[]))
            small.receive(dict(type='gateway_open', id='two', authority=f'127.0.0.1:{self.port}', method='POST', path='/v1/messages', headers=[]))
            self.assertEqual(self.responses.get(timeout=1)['code'], 'request_denied')
            small.receive(dict(type='gateway_data', id='one', data=g.encode(b'12345')))
            self.assertEqual(self.responses.get(timeout=1)['type'], 'gateway_error')
            self.assertEqual(Fixture.requests, [])
        finally:
            small.close()

    def test_guest_https_connect_is_terminated_then_exact_request_relayed(self):
        with tempfile.TemporaryDirectory() as temp:
            os.chmod(temp, 0o700)
            config, entries = g.provision(policy(self.port), temp)
            # Materialize the initrd entries under a disposable fixture root.
            root = Path(temp) / 'guest'
            for path, mode, body, uid, gid in entries:
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(body)
                target.chmod(stat_mode(mode))
            config['ca_path'] = str(root / config['ca_path'].lstrip('/'))
            for item in config['authorities'].values():
                item['cert_path'] = str(root / item['cert_path'].lstrip('/'))
                item['key_path'] = str(root / item['key_path'].lstrip('/'))
            guest = g.GuestGateway(config, self.host.receive)
            self.host.send_frame = guest.receive
            proxy = guest.start()
            port = int(proxy.rsplit(':', 1)[1])
            try:
                context = ssl.create_default_context(cafile=config['ca_path'])
                connection = http.client.HTTPSConnection('127.0.0.1', port, context=context, timeout=4)
                connection.set_tunnel('127.0.0.1', self.port)
                connection.request('POST', '/v1/messages', body=b'preserved across TLS', headers={'Authorization': 'Bearer fixture'})
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(response.read(), b'preserved across TLS')
                connection.close()
                self.assertEqual(Fixture.requests[-1][2], 'Bearer fixture')
                raw = socket.create_connection(('127.0.0.1', port), timeout=2)
                raw.sendall(b'CONNECT attacker.invalid:443 HTTP/1.1\r\nHost: attacker.invalid\r\n\r\n')
                self.assertIn(b'502', raw.recv(1024))
                raw.close()
            finally:
                guest.close()


def stat_mode(value):
    return value & 0o777


if __name__ == '__main__':
    unittest.main()
