#!/usr/bin/env python3
"""Exact-route HTTP/SSE/WebSocket gateway for a native provider in a no-NIC VM.

The host validates every request independently. Guest CONNECT terminates locally;
there is no host CONNECT tunnel. Frames contain credentials and MUST NOT be logged.
"""
import base64
import hashlib
import http.client
import ipaddress
import json
import os
import queue
import re
import socket
import socketserver
import ssl
import stat
import struct
import subprocess
import threading
import urllib.parse
from pathlib import Path

CHUNK = 64 * 1024
HEADER_LIMIT = 64 * 1024
HOP = {'connection', 'proxy-connection', 'proxy-authorization', 'keep-alive',
       'transfer-encoding', 'te', 'trailer', 'upgrade', 'host'}
PHASES = {'request_line', 'request_headers', 'connect_authority', 'connect_tls', 'target', 'request_body', 'await_upstream', 'response_body'}
FAILURES = {'authority', 'path', 'headers', 'request', 'http_version', 'host', 'framing', 'request_limit', 'upstream', 'websocket_upgrade', 'transport_unavailable', 'SSLError', 'SSLCertVerificationError', 'TimeoutError', 'OSError', 'EOFError', 'protocol_failure'}
TOKEN = re.compile(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]+$")


class Denied(Exception):
    pass


def authority(value):
    if not isinstance(value, str) or not value.isascii() or any(c in value for c in '/@\\?#\r\n '):
        raise Denied('authority')
    parsed = urllib.parse.urlsplit('https://' + value)
    try:
        host, port = parsed.hostname, parsed.port or 443
    except ValueError:
        raise Denied('authority') from None
    if not host or not 1 <= port <= 65535 or '*' in host or '%' in host:
        raise Denied('authority')
    try:
        ipaddress.ip_address(host)
    except ValueError:
        if not re.fullmatch(r'[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?', host):
            raise Denied('authority')
    return (f'[{host.lower()}]:{port}' if ':' in host else f'{host.lower()}:{port}'), host.lower(), port


def path_value(path):
    if not isinstance(path, str) or not path.startswith('/') or path.startswith('//') or len(path) > 16384:
        raise Denied('path')
    if any(ord(c) < 32 or ord(c) == 127 for c in path) or '\\' in path or '#' in path:
        raise Denied('path')
    parsed = urllib.parse.urlsplit(path)
    try:
        decoded = urllib.parse.unquote(parsed.path, errors='strict')
    except (UnicodeError, ValueError):
        raise Denied('path') from None
    # One interpretation for both the gateway and upstream route handler.
    if '%' in decoded or '\\' in decoded or any(c in decoded for c in '\r\n\0'):
        raise Denied('path')
    if any(part in ('.', '..') for part in decoded.split('/')) or decoded.startswith('//'):
        raise Denied('path')
    return decoded


def headers_value(headers):
    if not isinstance(headers, list) or len(headers) > 128:
        raise Denied('headers')
    out, size = [], 0
    for pair in headers:
        if not isinstance(pair, (list, tuple)) or len(pair) != 2:
            raise Denied('headers')
        name, value = pair
        if not isinstance(name, str) or not TOKEN.fullmatch(name) or not isinstance(value, str):
            raise Denied('headers')
        if any(ord(c) < 32 and c != '\t' or ord(c) == 127 for c in value):
            raise Denied('headers')
        try:
            size += len(name.encode('ascii')) + len(value.encode('latin1')) + 4
        except UnicodeError:
            raise Denied('headers') from None
        out.append((name, value))
    if size > HEADER_LIMIT:
        raise Denied('headers')
    return out


class Policy:
    def __init__(self, raw):
        if raw.get('version') != 1 or not raw.get('authorities'):
            raise Denied('policy')
        self.authorities = {}
        for entry in raw['authorities']:
            key, host, port = authority(entry['authority'])
            if key in self.authorities or not entry.get('routes'):
                raise Denied('policy')
            tls = entry.get('tls', True)
            local = entry.get('allow_loopback', False)
            if not isinstance(tls, bool) or not isinstance(local, bool):
                raise Denied('policy')
            if local:
                try:
                    if not ipaddress.ip_address(host).is_loopback:
                        raise Denied('policy')
                except ValueError:
                    raise Denied('policy') from None
            if not tls and not local:
                raise Denied('policy')
            for route in entry['routes']:
                prefix = route.get('path_prefix')
                if path_value(prefix) != prefix or '?' in prefix or prefix == '/':
                    raise Denied('policy')
                if not route.get('methods') or any(m not in ('GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS') for m in route['methods']):
                    raise Denied('policy')
            self.authorities[key] = dict(entry, host=host, port=port, tls=tls, allow_loopback=local)
        limits = raw.get('limits', {})
        self.connections = self.limit(limits, 'max_connections', 8, 1, 32)
        self.request_bytes = self.limit(limits, 'max_request_bytes', 8 * 1024**2, 1, 32 * 1024**2)
        self.response_bytes = self.limit(limits, 'max_response_bytes', 64 * 1024**2, 1, 256 * 1024**2)
        self.frame_bytes = self.limit(limits, 'max_frame_bytes', CHUNK, 1024, CHUNK)
        self.timeout = self.limit(limits, 'idle_timeout_seconds', 300, 1, 1800)

    @staticmethod
    def limit(values, key, default, low, high):
        value = values.get(key, default)
        if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
            raise Denied('policy')
        return value

    def admit(self, auth, method, path, websocket=False):
        key, _, _ = authority(auth)
        entry = self.authorities.get(key)
        decoded = path_value(path)
        if entry is None:
            raise Denied('authority')
        for route in entry['routes']:
            prefix = route['path_prefix']
            matches = decoded.startswith(prefix) if prefix.endswith('/') else decoded == prefix or decoded.startswith(prefix + '/')
            if matches and method in route['methods'] and (not websocket or route.get('websocket', False)):
                return key, entry
        raise Denied('route')


def encode(data):
    return base64.b64encode(data).decode('ascii')


def decode(data, limit):
    if not isinstance(data, str) or len(data) > 4 * ((limit + 2) // 3):
        raise Denied('frame')
    try:
        raw = base64.b64decode(data, validate=True)
    except (ValueError, TypeError):
        raise Denied('frame') from None
    if len(raw) > limit:
        raise Denied('frame')
    return raw


def safe_socket(entry, timeout):
    addresses = socket.getaddrinfo(entry['host'], entry['port'], type=socket.SOCK_STREAM)
    admitted = []
    for family, socktype, proto, _, address in addresses:
        ip = ipaddress.ip_address(address[0])
        if entry['allow_loopback']:
            if not ip.is_loopback:
                raise Denied('address')
        elif not ip.is_global:
            raise Denied('address')
        admitted.append((family, socktype, proto, address))
    if not admitted:
        raise Denied('address')
    last = None
    for family, socktype, proto, address in admitted:
        connection = socket.socket(family, socktype, proto)
        connection.settimeout(timeout)
        try:
            # Connect the inspected numeric address, never resolve a second time.
            connection.connect(address)
            if entry['tls']:
                context = ssl.create_default_context(cafile=entry.get('ca_file'))
                connection = context.wrap_socket(connection, server_hostname=entry['host'])
            return connection
        except (OSError, ssl.SSLError) as error:
            connection.close()
            last = error
    raise Denied('upstream_unavailable') from last


def read_exact(stream, amount):
    chunks = []
    while amount:
        chunk = stream.read(amount)
        if not chunk:
            raise EOFError()
        chunks.append(chunk)
        amount -= len(chunk)
    return b''.join(chunks)


def ws_read(stream, masked, limit):
    first, second = read_exact(stream, 2)
    fin, opcode = bool(first & 128), first & 15
    if first & 112 or opcode not in (0, 1, 2, 8, 9, 10) or bool(second & 128) != masked:
        raise Denied('websocket_frame')
    size = second & 127
    if size == 126:
        size = struct.unpack('!H', read_exact(stream, 2))[0]
    elif size == 127:
        size = struct.unpack('!Q', read_exact(stream, 8))[0]
    if size > limit or (opcode >= 8 and (not fin or size > 125)):
        raise Denied('websocket_frame')
    mask = read_exact(stream, 4) if masked else None
    data = read_exact(stream, size)
    if mask:
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
    return opcode, fin, data


def ws_write(sock, opcode, fin, data, masked=False):
    if opcode not in (0, 1, 2, 8, 9, 10) or (opcode >= 8 and (not fin or len(data) > 125)):
        raise Denied('websocket_frame')
    head = bytes([(128 if fin else 0) | opcode])
    marker = 128 if masked else 0
    if len(data) < 126:
        head += bytes([marker | len(data)])
    elif len(data) <= 65535:
        head += bytes([marker | 126]) + struct.pack('!H', len(data))
    else:
        head += bytes([marker | 127]) + struct.pack('!Q', len(data))
    if masked:
        mask = os.urandom(4)
        head += mask
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
    sock.sendall(head + data)


def ws_chunks(opcode, fin, data, limit):
    if not data:
        yield opcode, fin, data
        return
    for offset in range(0, len(data), limit):
        end = min(offset + limit, len(data))
        yield (opcode if offset == 0 else 0), (fin and end == len(data)), data[offset:end]


class State:
    def __init__(self):
        self.queue = queue.Queue(64)
        self.socket = None
        self.cancelled = threading.Event()
        self.received = 0

    def close(self):
        self.cancelled.set()
        if self.socket:
            try:
                self.socket.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.socket.close()
        try:
            self.queue.put_nowait({'type': 'gateway_cancel'})
        except queue.Full:
            pass


class HostGateway:
    def __init__(self, policy, send_frame):
        self.policy = Policy(policy)
        self.send_frame = send_frame
        self.states = {}
        self.lock = threading.Lock()
        self.closed = False
        self.guest_failures = []
        self.host_failures = []
        self.exchanges = {}
        self.exchange_overflow = False
        self.exchange_sequence = 0

    def transport_receipt(self):
        """Host observations only: no request headers, URLs, queries or payloads."""
        with self.lock:
            rows = [dict(value) for _, value in sorted(self.exchanges.items())]
            return {'version': 1, 'exchanges': rows, 'overflow': self.exchange_overflow,
                    'host_failures': list(self.host_failures)}

    def record_exchange(self, request, headers, status, request_bytes, response_bytes, completed):
        key, entry = self.policy.admit(request['authority'], request['method'], request['path'], request.get('websocket', False))
        path = path_value(request['path'])
        route = next(route for route in entry['routes'] if request['method'] in route['methods']
            and (path.startswith(route['path_prefix']) if route['path_prefix'].endswith('/') else
                 path == route['path_prefix'] or path.startswith(route['path_prefix'] + '/'))
            and (not request.get('websocket', False) or route.get('websocket', False)))
        route_id = hashlib.sha256(json.dumps([key, route], sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        inference = {'/v1/responses': 'responses', '/backend-api/codex/responses': 'responses',
                     '/responses': 'responses', '/v1/chat/completions': 'chat_completions',
                     '/v1/messages': 'messages'}.get(path)
        websocket = request.get('websocket', False)
        inference = inference if request['method'] == 'POST' or websocket and request['method'] == 'GET' else None
        authenticated = any(name.lower() in ('authorization', 'cookie') and bool(value.strip()) for name, value in headers)
        value = {'route_sha256': route_id, 'method': request['method'], 'status': status,
                 'transport': 'websocket' if websocket else 'http', 'inference': inference,
                 'authenticated_request': authenticated, 'remote_tls': entry['tls'] and not entry['allow_loopback'],
                 'request_bytes': request_bytes, 'response_bytes': response_bytes,
                 'completed': completed, 'successful': bool(inference and authenticated and request_bytes > 0
                    and response_bytes > 0 and (status == 101 if websocket else 200 <= status < 300 and completed))}
        # Per-request identity is hashed and never stored as guest-provided text.
        ident = str(request['_receipt_sequence']) if '_receipt_sequence' in request else hashlib.sha256(request['id'].encode()).hexdigest()
        with self.lock:
            if ident not in self.exchanges and len(self.exchanges) >= 256:
                self.exchange_overflow = True
            else:
                self.exchanges[ident] = value
            if not websocket and not 200 <= status < 300:
                self.host_failures = (self.host_failures + [{'phase': 'upstream_response', 'category': 'HttpStatus',
                    'http_status': status, 'method': request['method'], 'route_sha256': route_id}])[-16:]

    def record_failure(self, error, phase):
        category = type(error).__name__
        if category not in {'Denied', 'TimeoutError', 'ConnectionError', 'ConnectionRefusedError',
                            'ConnectionResetError', 'BrokenPipeError', 'OSError', 'gaierror', 'SSLError',
                            'SSLCertVerificationError', 'RemoteDisconnected', 'BadStatusLine', 'IncompleteRead', 'Empty',
                            'AttributeError', 'TypeError', 'ValueError', 'UnicodeEncodeError', 'UnicodeDecodeError'}:
            category = 'Exception'
        value = {'phase': phase, 'category': category}
        if isinstance(error, Denied) and str(error) in {'upstream_unavailable', 'websocket_upgrade', 'websocket_key', 'websocket_frame', 'headers', 'redirect', 'response_limit', 'cancelled', 'address', 'route', 'authority', 'path', 'frame', 'websocket_request'}:
            value['reason'] = str(error)
        cause = error.__cause__ or error.__context__
        if cause is not None:
            cause_name = type(cause).__name__
            if cause_name in {'TimeoutError', 'ConnectionRefusedError', 'ConnectionResetError', 'BrokenPipeError', 'OSError', 'gaierror', 'SSLError', 'SSLCertVerificationError'}:
                value['cause_category'] = cause_name
                if type(getattr(cause, 'errno', None)) is int:
                    value['cause_errno'] = cause.errno
        status = getattr(error, 'http_status', None)
        if type(status) is int and 100 <= status <= 599:
            value['http_status'] = status
        number = getattr(error, 'errno', None)
        if type(number) is int:
            value['errno'] = number
        with self.lock:
            self.host_failures = (self.host_failures + [value])[-16:]

    def emit(self, kind, ident, **fields):
        try:
            self.send_frame(dict(type=kind, id=ident, **fields))
        except Exception:
            with self.lock:
                state = self.states.get(ident)
            if state:
                state.close()

    def receive(self, frame):
        ident = frame.get('id')
        if not isinstance(ident, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', ident):
            return
        try:
            kind = frame.get('type')
            if kind == 'gateway_guest_failure':
                if frame.get('phase') not in PHASES or frame.get('category') not in FAILURES:
                    raise Denied('frame')
                with self.lock:
                    self.guest_failures = (self.guest_failures + [{'phase': frame['phase'], 'category': frame['category']}])[-16:]
                return
            if kind == 'gateway_open':
                self.policy.admit(frame.get('authority'), frame.get('method'), frame.get('path'), frame.get('websocket', False))
                headers_value(frame.get('headers', []))
                with self.lock:
                    if self.closed or ident in self.states or len(self.states) >= self.policy.connections:
                        raise Denied('capacity')
                    state = State()
                    self.states[ident] = state
                    self.exchange_sequence += 1
                    request = dict(frame, _receipt_sequence=self.exchange_sequence)
                threading.Thread(target=self.serve, args=(ident, state, request), daemon=True).start()
                return
            with self.lock:
                state = self.states.get(ident)
            if state is None:
                raise Denied('unknown_request')
            if kind == 'gateway_cancel':
                state.close()
                return
            if kind not in ('gateway_data', 'gateway_end', 'gateway_ws_frame'):
                raise Denied('frame')
            if kind in ('gateway_data', 'gateway_ws_frame'):
                data = decode(frame.get('data'), self.policy.frame_bytes)
                state.received += len(data)
                if state.received > self.policy.request_bytes:
                    raise Denied('request_limit')
                frame = dict(frame, decoded=data)
            state.queue.put_nowait(frame)
        except (Denied, queue.Full, ValueError, TypeError, KeyError):
            with self.lock:
                state = self.states.get(ident)
            if state:
                state.close()
            self.emit('gateway_error', ident, code='request_denied')

    def next_frame(self, state):
        frame = state.queue.get(timeout=self.policy.timeout)
        if state.cancelled.is_set() or frame['type'] == 'gateway_cancel':
            raise Denied('cancelled')
        return frame

    def serve(self, ident, state, request):
        phase = 'request_framing'
        try:
            body = bytearray()
            while True:
                frame = self.next_frame(state)
                if frame['type'] == 'gateway_end':
                    break
                if frame['type'] != 'gateway_data':
                    raise Denied('frame')
                body.extend(frame['decoded'])
            key, entry = self.policy.admit(request['authority'], request['method'], request['path'], request.get('websocket', False))
            supplied = headers_value(request.get('headers', []))
            phase = 'upstream_connect'
            connection = safe_socket(entry, self.policy.timeout)
            state.socket = connection
            if state.cancelled.is_set():
                raise Denied('cancelled')
            if request.get('websocket', False):
                if body or request['method'] != 'GET':
                    raise Denied('websocket_request')
                phase = 'websocket_exchange'
                self.websocket(ident, state, request, supplied, key)
                return
            # Reconstruct framing; an untrusted Content-Length cannot smuggle a
            # second request. Upstream connection is private and never pooled.
            headers = [(n, v) for n, v in supplied if n.lower() not in HOP | {'content-length'}]
            wire = f"{request['method']} {request['path']} HTTP/1.1\r\nHost: {key}\r\nConnection: close\r\nContent-Length: {len(body)}\r\n"
            phase = 'request_send'
            connection.sendall((wire + ''.join(f'{n}: {v}\r\n' for n, v in headers) + '\r\n').encode('latin1') + body)
            phase = 'response_headers'
            response = http.client.HTTPResponse(connection)
            response.begin()
            received = headers_value(response.getheaders())
            for name, value in received:
                if name.lower() == 'location' and response.status in (301, 302, 303, 307, 308):
                    location = urllib.parse.urlsplit(urllib.parse.urljoin(('https' if entry['tls'] else 'http') + '://' + key + request['path'], value))
                    if location.scheme != ('https' if entry['tls'] else 'http') or location.username or location.password:
                        raise Denied('redirect')
                    redirect_method = 'GET' if response.status == 303 else request['method']
                    self.policy.admit(location.netloc, redirect_method, urllib.parse.urlunsplit(('', '', location.path or '/', location.query, '')))
            self.emit('gateway_headers', ident, status=response.status, headers=[(n, v) for n, v in received if n.lower() not in HOP | {'content-length'}])
            total = 0
            phase = 'response_body'
            while not state.cancelled.is_set():
                chunk = response.read1(self.policy.frame_bytes)
                if not chunk:
                    break
                total += len(chunk)
                if total > self.policy.response_bytes:
                    raise Denied('response_limit')
                self.emit('gateway_data', ident, data=encode(chunk))
            if not state.cancelled.is_set():
                self.record_exchange(request, supplied, response.status, len(body), total, True)
                self.emit('gateway_end', ident)
        except Exception as error:
            self.record_failure(error, phase)
            # Exceptions may contain request headers, URLs, or response bodies.
            # Only a fixed code crosses the diagnostic boundary.
            self.emit('gateway_error', ident, code='upstream_failed')
        finally:
            state.close()
            with self.lock:
                if self.states.get(ident) is state:
                    del self.states[ident]

    def websocket(self, ident, state, request, headers, key):
        keys = [v for n, v in headers if n.lower() == 'sec-websocket-key']
        if len(keys) != 1 or len(decode(keys[0], 16)) != 16:
            raise Denied('websocket_key')
        clean = [(n, v) for n, v in headers if n.lower() not in HOP | {'content-length', 'sec-websocket-extensions'}]
        wire = f"GET {request['path']} HTTP/1.1\r\nHost: {key}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n"
        state.socket.sendall((wire + ''.join(f'{n}: {v}\r\n' for n, v in clean) + '\r\n').encode('latin1'))
        stream = state.socket.makefile('rb')
        status = stream.readline(8192)
        if not re.fullmatch(rb'HTTP/1\.[01] 101 [^\r\n]*\r\n', status):
            error = Denied('websocket_upgrade')
            observed = re.match(rb'HTTP/1\.[01] ([1-5][0-9]{2})(?: |\r)', status)
            if observed:
                error.http_status = int(observed[1])
            raise error
        response = read_headers(stream)
        values = {n.lower(): v for n, v in response}
        expected = base64.b64encode(hashlib.sha1((keys[0] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode('ascii')).digest()).decode('ascii')
        if values.get('sec-websocket-accept') != expected or values.get('upgrade', '').lower() != 'websocket' or 'sec-websocket-extensions' in values:
            raise Denied('websocket_upgrade')
        self.emit('gateway_headers', ident, status=101, headers=response)

        counters = {'request': 0, 'response': 0}
        counter_lock = threading.Lock()
        def observe(completed=False):
            with counter_lock:
                self.record_exchange(request, headers, 101, counters['request'], counters['response'], completed)
        def writer():
            try:
                while not state.cancelled.is_set():
                    frame = self.next_frame(state)
                    if frame['type'] != 'gateway_ws_frame':
                        raise Denied('websocket_frame')
                    ws_write(state.socket, frame['opcode'], frame['fin'], frame['decoded'], masked=True)
                    if frame['opcode'] in (0, 1, 2):
                        with counter_lock:
                            counters['request'] += len(frame['decoded'])
                        observe()
                    if frame['opcode'] == 8:
                        return
            except Exception:
                state.close()
        threading.Thread(target=writer, daemon=True).start()
        total = 0
        while not state.cancelled.is_set():
            opcode, fin, data = ws_read(stream, False, min(self.policy.response_bytes, 8 * 1024**2))
            total += len(data)
            if total > self.policy.response_bytes:
                raise Denied('response_limit')
            if opcode in (0, 1, 2):
                with counter_lock:
                    counters['response'] += len(data)
                observe()
            for part_opcode, part_fin, part in ws_chunks(opcode, fin, data, self.policy.frame_bytes):
                self.emit('gateway_ws_frame', ident, opcode=part_opcode, fin=part_fin, data=encode(part))
            if opcode == 8:
                observe(completed=True)
                break
        self.emit('gateway_end', ident)

    def close(self):
        with self.lock:
            self.closed = True
            states = list(self.states.values())
        for state in states:
            state.close()


def read_headers(stream):
    headers, used = [], 0
    while True:
        line = stream.readline(HEADER_LIMIT + 1)
        used += len(line)
        if used > HEADER_LIMIT or not line.endswith(b'\r\n'):
            raise Denied('headers')
        if line == b'\r\n':
            return headers_value(headers)
        if line[:1] in (b' ', b'\t') or b':' not in line:
            raise Denied('headers')
        name, value = line[:-2].split(b':', 1)
        headers.append((name.decode('ascii'), value.decode('latin1').strip()))


def provision(policy, directory):
    admitted = Policy(policy)
    root = Path(directory)
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    if root.is_symlink() or root.stat().st_mode & 0o077:
        raise Denied('private_directory')
    ca, ca_key = root / 'ca.pem', root / 'ca.key'

    def openssl(*args):
        subprocess.run(['/usr/bin/openssl', *map(str, args)], check=True, stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={'PATH': '/usr/bin:/bin'})
    openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', ca_key,
            '-out', ca, '-days', '2', '-subj', '/CN=Native provider isolated run CA',
            '-addext', 'basicConstraints=critical,CA:TRUE,pathlen:0',
            '-addext', 'keyUsage=critical,keyCertSign,cRLSign',
            '-addext', 'subjectKeyIdentifier=hash')
    os.chmod(ca_key, 0o600)
    guest_root = 'run/native-provider-gateway'
    entries = [(f'{guest_root}/ca.pem', stat.S_IFREG | 0o644, ca.read_bytes(), 0, 0)]
    config = {'version': 1, 'ca_path': f'/{guest_root}/ca.pem', 'ca_file': f'/{guest_root}/ca.pem', 'authorities': {},
              'frame_bytes': admitted.frame_bytes, 'request_bytes': admitted.request_bytes,
              'timeout': admitted.timeout, 'connections': admitted.connections}
    for index, (key, entry) in enumerate(admitted.authorities.items()):
        cert, private, csr, extension = (root / f'host-{index}.{suffix}' for suffix in ('crt', 'key', 'csr', 'ext'))
        try:
            ipaddress.ip_address(entry['host'])
            san = 'IP:' + entry['host']
        except ValueError:
            san = 'DNS:' + entry['host']
        extension.write_text('subjectAltName=' + san + '\n'
                             'basicConstraints=critical,CA:FALSE\n'
                             'keyUsage=critical,digitalSignature,keyEncipherment\n'
                             'extendedKeyUsage=serverAuth\n'
                             'subjectKeyIdentifier=hash\n'
                             'authorityKeyIdentifier=keyid,issuer\n')
        openssl('req', '-new', '-newkey', 'rsa:2048', '-nodes', '-keyout', private,
                '-out', csr, '-subj', '/CN=' + entry['host'])
        openssl('x509', '-req', '-in', csr, '-CA', ca, '-CAkey', ca_key, '-set_serial', str(index + 1),
                '-out', cert, '-days', '2', '-extfile', extension)
        os.chmod(private, 0o600)
        cert_path, key_path = f'{guest_root}/{cert.name}', f'{guest_root}/{private.name}'
        entries += [(cert_path, stat.S_IFREG | 0o644, cert.read_bytes(), 0, 0),
                    (key_path, stat.S_IFREG | 0o600, private.read_bytes(), 0, 0)]
        config['authorities'][key] = {'cert_path': '/' + cert_path, 'key_path': '/' + key_path}
    return config, entries


class GuestGateway:
    def __init__(self, config, send_frame):
        self.config, self.send_frame = config, send_frame
        self.states, self.lock = {}, threading.Lock()
        self.server = None
        self.slots = threading.BoundedSemaphore(config['connections'])
        self.contexts = {}
        for key, paths in config['authorities'].items():
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(paths['cert_path'], paths['key_path'])
            self.contexts[key] = context

    def emit(self, frame):
        try:
            self.send_frame(frame)
        except Exception:
            raise Denied('transport_unavailable') from None

    def receive(self, frame):
        with self.lock:
            state = self.states.get(frame.get('id'))
        if state:
            try:
                state.queue.put(frame, timeout=1)
            except queue.Full:
                state.close()

    def start(self):
        gateway = self
        class Server(socketserver.ThreadingTCPServer):
            allow_reuse_address = False
            daemon_threads = True
            # Admission happens before creating a handler thread.
            def process_request(self, request, client_address):
                if not gateway.slots.acquire(blocking=False):
                    request.close()
                    return
                try:
                    super().process_request(request, client_address)
                except Exception:
                    gateway.slots.release()
                    raise
        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                try:
                    gateway.handle(self.request)
                finally:
                    gateway.slots.release()
        self.server = Server(('127.0.0.1', 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return 'http://127.0.0.1:' + str(self.server.server_address[1])

    def handle(self, client):
        client.settimeout(self.config['timeout'])
        ident = os.urandom(16).hex()
        state = State()
        state.socket = client
        opened = False
        phase = 'request_line'
        try:
            stream = client.makefile('rb')
            line = stream.readline(16385).decode('ascii')
            method, target, version = line.rstrip('\r\n').split(' ')
            if version != 'HTTP/1.1':
                raise Denied('http_version')
            phase = 'request_headers'
            headers = read_headers(stream)
            fixed_authority = None
            if method == 'CONNECT':
                phase = 'connect_authority'
                fixed_authority, _, _ = authority(target)
                context = self.contexts.get(fixed_authority)
                if context is None:
                    raise Denied('authority')
                client.sendall(b'HTTP/1.1 200 Connection Established\r\n\r\n')
                stream.close()
                phase = 'connect_tls'
                client = context.wrap_socket(client, server_side=True)
                state.socket = client
                stream = client.makefile('rb')
                line = stream.readline(16385).decode('ascii')
                method, target, version = line.rstrip('\r\n').split(' ')
                if version != 'HTTP/1.1' or method == 'CONNECT':
                    raise Denied('request')
                headers = read_headers(stream)
            phase = 'target'
            parsed = urllib.parse.urlsplit(target)
            hosts = [v for n, v in headers if n.lower() == 'host']
            if len(hosts) != 1:
                raise Denied('host')
            key, _, _ = authority(hosts[0])
            if fixed_authority and key != fixed_authority:
                raise Denied('host')
            if parsed.netloc:
                if parsed.username or parsed.password or authority(parsed.netloc)[0] != key or parsed.scheme not in ('http', 'https'):
                    raise Denied('host')
            if key not in self.contexts:
                raise Denied('authority')
            path = urllib.parse.urlunsplit(('', '', parsed.path or '/', parsed.query, ''))
            path_value(path)
            websocket = any(n.lower() == 'upgrade' and v.lower() == 'websocket' for n, v in headers)
            lengths = [v for n, v in headers if n.lower() == 'content-length']
            transfers = [v for n, v in headers if n.lower() == 'transfer-encoding']
            if len(lengths) > 1 or len(transfers) > 1 or (lengths and transfers) or (transfers and transfers != ['chunked']):
                raise Denied('framing')
            with self.lock:
                self.states[ident] = state
            opened = True
            self.emit({'type': 'gateway_open', 'id': ident, 'method': method, 'authority': key,
                             'path': path, 'headers': headers, 'websocket': websocket})
            phase = 'request_body'
            total = 0
            if transfers:
                while True:
                    size_line = stream.readline(128)
                    if not re.fullmatch(rb'[0-9A-Fa-f]+\r\n', size_line):
                        raise Denied('framing')
                    size = int(size_line, 16)
                    if size == 0:
                        if read_headers(stream):
                            raise Denied('trailers')
                        break
                    total += size
                    if total > self.config['request_bytes']:
                        raise Denied('request_limit')
                    remaining = size
                    while remaining:
                        chunk = read_exact(stream, min(remaining, self.config['frame_bytes']))
                        remaining -= len(chunk)
                        self.emit({'type': 'gateway_data', 'id': ident, 'data': encode(chunk)})
                    if read_exact(stream, 2) != b'\r\n':
                        raise Denied('framing')
            else:
                remaining = int(lengths[0]) if lengths else 0
                if remaining < 0 or remaining > self.config['request_bytes']:
                    raise Denied('request_limit')
                while remaining:
                    chunk = read_exact(stream, min(remaining, self.config['frame_bytes']))
                    remaining -= len(chunk)
                    self.emit({'type': 'gateway_data', 'id': ident, 'data': encode(chunk)})
            self.emit({'type': 'gateway_end', 'id': ident})
            phase = 'await_upstream'
            response = state.queue.get(timeout=self.config['timeout'])
            if response['type'] != 'gateway_headers':
                raise Denied('upstream')
            phase = 'response_body'
            status = response['status']
            received = headers_value(response['headers'])
            if websocket:
                if status != 101:
                    raise Denied('websocket_upgrade')
                client.sendall(('HTTP/1.1 101 Switching Protocols\r\n' + ''.join(f'{n}: {v}\r\n' for n, v in received) + '\r\n').encode('latin1'))
                def reader():
                    try:
                        while not state.cancelled.is_set():
                            opcode, fin, data = ws_read(stream, True, self.config['request_bytes'])
                            for part_opcode, part_fin, part in ws_chunks(opcode, fin, data, self.config['frame_bytes']):
                                self.emit({'type': 'gateway_ws_frame', 'id': ident, 'opcode': part_opcode, 'fin': part_fin, 'data': encode(part)})
                            if opcode == 8:
                                return
                    except Exception:
                        try:
                            self.emit({'type': 'gateway_cancel', 'id': ident})
                        except Denied:
                            pass
                        state.close()
                threading.Thread(target=reader, daemon=True).start()
                while not state.cancelled.is_set():
                    frame = state.queue.get(timeout=self.config['timeout'])
                    if frame['type'] == 'gateway_end':
                        break
                    if frame['type'] != 'gateway_ws_frame':
                        raise Denied('upstream')
                    ws_write(client, frame['opcode'], frame['fin'], decode(frame['data'], self.config['frame_bytes']))
                    if frame['opcode'] == 8:
                        break
            else:
                outgoing = [(n, v) for n, v in received if n.lower() not in HOP | {'content-length'}]
                client.sendall((f'HTTP/1.1 {status} Provider Response\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n' + ''.join(f'{n}: {v}\r\n' for n, v in outgoing) + '\r\n').encode('latin1'))
                while True:
                    frame = state.queue.get(timeout=self.config['timeout'])
                    if frame['type'] == 'gateway_end':
                        client.sendall(b'0\r\n\r\n')
                        break
                    if frame['type'] != 'gateway_data':
                        raise Denied('upstream')
                    data = decode(frame['data'], self.config['frame_bytes'])
                    client.sendall(f'{len(data):x}\r\n'.encode() + data + b'\r\n')
        except Exception as error:
            category = str(error) if isinstance(error, Denied) else type(error).__name__
            if category not in FAILURES:
                category = 'protocol_failure'
            try:
                self.emit({'type': 'gateway_guest_failure', 'id': ident, 'phase': phase, 'category': category})
            except Denied:
                pass
            try:
                client.sendall(b'HTTP/1.1 502 Gateway Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            except OSError:
                pass
        finally:
            if opened:
                try:
                    self.emit({'type': 'gateway_cancel', 'id': ident})
                except Denied:
                    pass
            with self.lock:
                self.states.pop(ident, None)
            state.close()

    def close(self):
        if self.server:
            self.server.shutdown()
            self.server.server_close()
        with self.lock:
            states = list(self.states.values())
        for state in states:
            state.close()
