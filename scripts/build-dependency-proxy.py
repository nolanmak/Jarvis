#!/usr/bin/env python3
"""Read-only public package retrieval for a networkless build guest.

Only the guest's root-owned HTTP adapter can touch the private mailbox. Still
validate it as untrusted input. Each network read runs in a bounded child with
no inherited environment, credentials, proxy settings, or package execution.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ORIGINS = {
    'npm': 'https://registry.npmjs.org',
    'cargo-index': 'https://index.crates.io',
    'cargo-download': 'https://static.crates.io',
    'pypi': 'https://pypi.org',
    'pypi-files': 'https://files.pythonhosted.org',
}
MAX_BODY = 32 * 1024 * 1024
MAX_TOTAL = 512 * 1024 * 1024
MAX_REQUESTS = 2048


def upstream(route):
    if not isinstance(route, str) or len(route) > 4096 or not route.startswith('/'):
        raise ValueError('invalid package route')
    decoded = urllib.parse.unquote(route)
    if not re.fullmatch(r'/[A-Za-z0-9@._+/-]+', decoded):
        raise ValueError('invalid package route characters')
    if decoded.startswith('/pypi/'):
        if not re.fullmatch(r'/pypi/simple/[a-z0-9]+(?:-[a-z0-9]+)*/', decoded):
            raise ValueError('invalid Python index route')
        return ORIGINS['pypi'] + decoded[len('/pypi'):]
    if decoded.startswith('/pypi-files/') and not re.fullmatch(
            r'/pypi-files/packages/(?:[A-Za-z0-9_-]+/)*[A-Za-z0-9_.+-]+\.whl', decoded):
        raise ValueError('invalid Python wheel route')
    parts = decoded.split('/')[1:]
    if len(parts) < 2 or parts[0] not in ORIGINS or any(part in ('', '.', '..') for part in parts):
        raise ValueError('invalid package route scope')
    return ORIGINS[parts[0]] + '/' + '/'.join(parts[1:])


def python_index(body):
    """Expose wheel-only PEP 691 metadata and bind subsequent fetches to it.

    Metadata sidecars are disabled: pip reads metadata from the hash-verified
    wheel instead of introducing a second unauthenticated download path.
    """
    value = json.loads(body)
    if not isinstance(value, dict) or not isinstance(value.get('files'), list):
        raise ValueError('invalid Python index metadata')
    if len(value['files']) > 100000:
        raise ValueError('Python index entry limit')
    files, wheels = [], {}
    for entry in value['files']:
        if not isinstance(entry, dict) or not isinstance(entry.get('filename'), str):
            raise ValueError('invalid Python index entry')
        name = entry['filename']
        if not name.endswith('.whl'):
            continue
        if not re.fullmatch(r'[A-Za-z0-9_.+-]+\.whl', name):
            raise ValueError('invalid Python wheel name')
        url = entry.get('url')
        hashes = entry.get('hashes')
        if not isinstance(url, str) or not isinstance(hashes, dict):
            raise ValueError('Python wheel lacks integrity metadata')
        digest = hashes.get('sha256')
        if not isinstance(digest, str) or not re.fullmatch('[0-9a-f]{64}', digest):
            raise ValueError('Python wheel lacks SHA256')
        parsed = urllib.parse.urlsplit(url)
        if (parsed.scheme != 'https' or parsed.netloc != 'files.pythonhosted.org'
                or parsed.query or parsed.fragment or parsed.path.rsplit('/', 1)[-1] != name):
            raise ValueError('Python wheel URL outside registry policy')
        route = '/pypi-files' + parsed.path
        if upstream(route) != url or (route in wheels and wheels[route] != digest):
            raise ValueError('inconsistent Python wheel metadata')
        wheels[route] = digest
        files.append({**entry, 'core-metadata': False, 'dist-info-metadata': False})
    return json.dumps({**value, 'files': files}, separators=(',', ':')).encode(), wheels


class RegistryRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        before, after = urllib.parse.urlsplit(request.full_url), urllib.parse.urlsplit(newurl)
        if (after.scheme != 'https' or after.netloc != before.netloc or after.query or after.fragment
                or after.username or after.password):
            raise ValueError('registry redirect left its approved origin')
        prefix = next(key for key, origin in ORIGINS.items() if origin == 'https://' + after.netloc)
        approved = upstream('/' + prefix + after.path)
        return super().redirect_request(request, fp, code, msg, headers, approved)


def fetch(route):
    url = upstream(route)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), RegistryRedirect())
    request = urllib.request.Request(url, headers={
        'Accept': ('application/vnd.npm.install-v1+json' if route.startswith('/npm/') else
                   'application/vnd.pypi.simple.v1+json' if route.startswith('/pypi/') else '*/*'),
        'User-Agent': 'Jarvis-dependency-fetch/1',
    }, method='GET')
    try:
        with opener.open(request, timeout=10) as response:
            body = response.read(MAX_BODY + 1)
            if len(body) > MAX_BODY:
                raise ValueError('package response exceeds limit')
            content_type = response.headers.get('Content-Type', '').split(';')[0]
            if 'json' not in content_type:
                content_type = 'application/octet-stream'
            else:
                content_type = ('application/vnd.pypi.simple.v1+json'
                                if route.startswith('/pypi/') else 'application/json')
            return response.status, content_type, body
    except urllib.error.HTTPError as error:
        return error.code, 'application/json', b'{"error":"public package unavailable"}'


def atomic(path, data):
    descriptor, temporary = tempfile.mkstemp(prefix='.dependency-', dir=path.parent)
    try:
        with os.fdopen(descriptor, 'wb') as stream:
            stream.write(data)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def read_request(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 8192:
            raise ValueError('untrusted package request')
        value = json.load(stream)
    if not isinstance(value, dict) or set(value) != {'route'}:
        raise ValueError('invalid package request')
    upstream(value['route'])
    return value['route']


class Broker:
    def __init__(self, control, *, python=False):
        self.python = python
        self.wheels = {}
        self.control = Path(control)
        self.requests = 0
        self.bytes = 0

    def poll(self, remaining):
        deadline = time.monotonic() + max(0, remaining)
        paths = list(self.control.glob('*.request'))
        if len(paths) > 64:
            raise ValueError('dependency request queue exceeds limit')
        for path in paths:
            identifier = path.stem
            if not re.fullmatch('[0-9a-f]{32}', identifier):
                raise ValueError('invalid dependency request identifier')
            response = self.control / (identifier + '.response')
            body = self.control / (identifier + '.body')
            try:
                self.requests += 1
                if self.requests > MAX_REQUESTS or self.bytes >= MAX_TOTAL:
                    raise ValueError('dependency retrieval budget exceeded')
                route = read_request(path)
                if route.startswith(('/pypi/', '/pypi-files/')) != self.python:
                    raise ValueError('package registry outside execution profile')
                if route.startswith('/pypi-files/') and route not in self.wheels:
                    raise ValueError('wheel absent from resolved index metadata')
                budget = min(20, deadline - time.monotonic())
                if budget <= 0:
                    raise ValueError('dependency request deadline expired')
                result = subprocess.run([sys.executable, '-I', __file__, '--fetch', str(path), str(body)],
                    env={'PATH': os.defpath}, stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=budget, check=True)
                metadata = json.loads(result.stdout)
                received_size = body.stat().st_size
                self.bytes += received_size
                if self.python and metadata.get('status') == 200:
                    received = body.read_bytes()
                    if route.startswith('/pypi/'):
                        filtered, wheels = python_index(received)
                        if len(filtered) > MAX_BODY or len(self.wheels) + len(wheels) > 100000:
                            raise ValueError('Python index exceeds metadata budget')
                        for wheel, digest in wheels.items():
                            if wheel in self.wheels and self.wheels[wheel] != digest:
                                raise ValueError('registry changed wheel identity during preparation')
                        self.wheels.update(wheels)
                        atomic(body, filtered)
                        metadata['content_type'] = 'application/vnd.pypi.simple.v1+json'
                    elif hashlib.sha256(received).hexdigest() != self.wheels[route]:
                        raise ValueError('wheel integrity mismatch')
                if self.bytes > MAX_TOTAL:
                    raise ValueError('dependency retrieval budget exceeded')
            except (OSError, ValueError, subprocess.SubprocessError):
                atomic(body, b'{"error":"dependency retrieval refused or unavailable"}')
                metadata = {'status': 502, 'content_type': 'application/json'}
            finally:
                path.unlink(missing_ok=True)
            # Publishing metadata last makes the complete body visible first.
            atomic(response, json.dumps(metadata).encode())


GUEST_PROXY = r'''
import http.server,json,ssl,threading,time,uuid
from pathlib import Path

def make_proxy(control, python=False):
    control=Path(control)
    origins={'registry.npmjs.org':'npm','index.crates.io':'cargo-index','static.crates.io':'cargo-download'}
    if python: origins={'pypi.org':'pypi','files.pythonhosted.org':'pypi-files'}
    context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain('/etc/jarvis-registry-ca.pem','/root/registry-key.pem')
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self,*args): pass
        def do_POST(self): self.send_error(405)
        def do_PUT(self): self.send_error(405)
        def do_DELETE(self): self.send_error(405)
        def do_CONNECT(self): self.send_error(405)
        def do_HEAD(self): self.do_GET()
        def do_GET(self):
            identifier=uuid.uuid4().hex
            request=control/(identifier+'.request')
            response=control/(identifier+'.response')
            body_file=control/(identifier+'.body')
            temporary=control/(identifier+'.pending')
            try:
                host=self.headers.get('Host','').lower().removesuffix(':443')
                if host not in origins:
                    self.send_error(403); return
                if len(self.path)>4096:
                    self.send_error(414); return
                temporary.write_text(json.dumps({'route':'/'+origins[host]+self.path}))
                temporary.chmod(0o600)
                temporary.replace(request)
                deadline=time.monotonic()+30
                while not response.exists():
                    if time.monotonic()>deadline: raise TimeoutError()
                    time.sleep(0.02)
                metadata=json.loads(response.read_text())
                body=body_file.read_bytes()
                self.send_response(metadata['status'])
                self.send_header('Content-Type',metadata['content_type'])
                self.send_header('Content-Length',str(len(body)))
                self.end_headers()
                if self.command!='HEAD': self.wfile.write(body)
            except (OSError,ValueError,TimeoutError):
                self.send_error(502)
            finally:
                for path in (request,response,body_file,temporary): path.unlink(missing_ok=True)
    class Server(http.server.HTTPServer):
        request_queue_size=64
        def get_request(self):
            connection,address=super().get_request()
            connection.settimeout(10)
            try: return context.wrap_socket(connection,server_side=True),address
            except Exception:
                connection.close(); raise
    return Server(('127.0.0.1',443),Handler)
'''


if __name__ == '__main__':
    if len(sys.argv) != 4 or sys.argv[1] != '--fetch':
        raise SystemExit(2)
    try:
        status, content_type, body = fetch(read_request(Path(sys.argv[2])))
        atomic(Path(sys.argv[3]), body)
        print(json.dumps({'status': status, 'content_type': content_type}))
    except Exception:
        raise SystemExit(2)
