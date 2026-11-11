#!/usr/bin/env python3
"""Controller-owned native CLI launcher: no guest NIC or host home export.

The enclosing provider supervisor owns resource admission and descendant cleanup.
This launcher streams framed guest output, delegates tool calls to the original
host policy bridge, and services only the declared provider gateway protocol.
"""
import base64
import contextlib
import hashlib
import fcntl
import uuid
import importlib.util
import json
import math
import os
import queue
from pathlib import Path
import selectors
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import datetime

MAX_FRAME = 2 * 1024 * 1024
MAX_HOME = 64 * 1024 * 1024


MAX_EXECUTABLE = 512 * 1024 * 1024
MAX_COMPANION = 128 * 1024 * 1024


MAX_CREDENTIAL_BYTES = 1024 * 1024



MAX_BOOT_ARCHIVE_BYTES = 128 * 1024 * 1024
MAX_BOOT_IMAGE_BYTES = 129 * 1024 * 1024


def boot_source_bytes(path, maximum):
    """Snapshot one declared regular source within remaining archive capacity."""
    if type(maximum) is not int or maximum < 0:
        raise ValueError('invalid native initrd byte budget')
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
    except OSError:
        raise ValueError('native initrd source unavailable') from None
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode):
            raise ValueError('native initrd source is not regular')
        if before.st_size > maximum:
            raise ValueError('native initrd source exceeds byte budget')
        data = bytearray()
        while True:
            block = os.read(fd, min(65536, maximum - len(data) + 1))
            if not block:
                break
            data.extend(block)
            if len(data) > maximum:
                raise ValueError('native initrd source exceeds byte budget')
        after = os.fstat(fd)
        identity = lambda info: (info.st_dev, info.st_ino, info.st_mode, info.st_uid,
                                 info.st_gid, info.st_nlink, info.st_size,
                                 info.st_mtime_ns, info.st_ctime_ns)
        if len(data) != before.st_size or identity(before) != identity(after):
            raise ValueError('native initrd source changed during read')
        return bytes(data)
    except OSError:
        raise ValueError('native initrd source unavailable') from None
    finally:
        os.close(fd)


def boot_entry_bytes(name, data):
    if not isinstance(name, str) or '\0' in name or not isinstance(data, bytes):
        raise ValueError('invalid native initrd entry')
    header = 110 + len(name.encode()) + 1
    return (header + 3) // 4 * 4 + (len(data) + 3) // 4 * 4


class BootEntries(list):
    """Charge actual newc metadata/alignment while retaining the trailer room."""
    def __init__(self):
        super().__init__()
        self.encoded_bytes = boot_entry_bytes('TRAILER!!!', b'')
        if self.encoded_bytes > MAX_BOOT_ARCHIVE_BYTES:
            raise ValueError('native initrd exceeds byte budget')

    def append(self, entry):
        size = boot_entry_bytes(entry[0], entry[2])
        if self.encoded_bytes + size > MAX_BOOT_ARCHIVE_BYTES:
            raise ValueError('native initrd exceeds byte budget')
        super().append(entry)
        self.encoded_bytes += size

    def extend(self, entries):
        for entry in entries:
            self.append(entry)

    def append_input(self, name, mode, source):
        if isinstance(source, os.PathLike):
            remaining = MAX_BOOT_ARCHIVE_BYTES - self.encoded_bytes - boot_entry_bytes(name, b'')
            if remaining < 0:
                raise ValueError('native initrd exceeds byte budget')
            data = boot_source_bytes(source, remaining // 4 * 4)
        else:
            data = source
        self.append((name, mode, data, 0, 0))


def write_boot_image(path, entries, encoder):
    # The shared codec checks actual encoded and compressed bytes, independently
    # of pre-reading accounting. No output path is opened until both bounds pass.
    image = encoder(entries, max_bytes=MAX_BOOT_ARCHIVE_BYTES, max_image_bytes=MAX_BOOT_IMAGE_BYTES)
    if not isinstance(image, bytes) or len(image) > MAX_BOOT_IMAGE_BYTES:
        raise ValueError('native compressed initrd exceeds byte budget')
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    with os.fdopen(descriptor, 'wb') as stream:
        os.fchmod(stream.fileno(), 0o600)
        stream.write(image)
        stream.flush()
        os.fsync(stream.fileno())


class BoundedWriter:
    """Never block the shared control/gateway dispatcher on an application pipe."""
    def __init__(self, write, finish, failed):
        self.write, self.finish, self.failed = write, finish, failed
        self.pending = queue.Queue(16)
        self.closed = threading.Event()
        self.ended = False
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def submit(self, data):
        if self.closed.is_set() or self.ended or (data is not None and len(data) > 65536):
            raise RuntimeError('invalid or closed native stream')
        if data is None:
            self.ended = True
        try:
            self.pending.put_nowait(data)
        except queue.Full:
            raise RuntimeError('native stream backpressure limit') from None

    def run(self):
        try:
            while True:
                data = self.pending.get()
                if data is None:
                    self.finish()
                    return
                self.write(data)
        except BaseException:
            # Pipe/transport exceptions can embed private protocol payloads.
            self.failed()
        finally:
            self.closed.set()



def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def private_json(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd) as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or info.st_size > MAX_FRAME):
            raise ValueError('native VM launch input is not private')
        return json.load(stream)


def private_bytes(path, maximum):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or info.st_size > maximum):
            raise ValueError('native VM credential/runtime input is not private')
        return stream.read(maximum + 1)




class AtomicPublicationUncertain(OSError):
    def __init__(self):
        super().__init__('native atomic publication uncertain')


class _AtomicPublisher:
    """One original helper's bounded publication and conservative failure fence."""
    def __init__(self):
        self._lock = threading.Lock()
        self._uncertain = False

    @staticmethod
    def _parent_current(parent, descriptor, original):
        try:
            held = os.fstat(descriptor)
            current = parent.lstat()
            identity = lambda info: (info.st_dev, info.st_ino, info.st_uid, info.st_mode)
            return (stat.S_ISDIR(held.st_mode) and stat.S_ISDIR(current.st_mode)
                    and held.st_uid == os.getuid() and not held.st_mode & 0o077
                    and identity(held) == original and identity(current) == original)
        except OSError:
            return False

    @staticmethod
    def _original_temporary(parent_fd, name, descriptor, original):
        held = os.fstat(descriptor)
        current = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        identity = lambda info: (info.st_dev, info.st_ino, info.st_uid, info.st_gid)
        return (identity(held) == original and identity(current) == original
                and stat.S_ISREG(held.st_mode) and stat.S_ISREG(current.st_mode)
                and held.st_uid == os.getuid() and held.st_nlink == current.st_nlink == 1
                and not held.st_mode & 0o077 and not current.st_mode & 0o077)

    def publish(self, path, data):
        with self._lock:
            if self._uncertain:
                raise AtomicPublicationUncertain()
            if not isinstance(data, bytes) or len(data) > MAX_FRAME:
                raise ValueError('native atomic payload exceeds byte budget')
            path = Path(path)
            parent_fd = temporary_fd = None
            name = None
            replaced = False
            primary = None
            try:
                parent_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
                info = os.fstat(parent_fd)
                parent_identity = (info.st_dev, info.st_ino, info.st_uid, info.st_mode)
                if not self._parent_current(path.parent, parent_fd, parent_identity):
                    self._uncertain = True
                    raise AtomicPublicationUncertain()
                for _ in range(16):
                    candidate = '.native-atomic-' + uuid.uuid4().hex
                    try:
                        temporary_fd = os.open(candidate, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                                               0o600, dir_fd=parent_fd)
                        name = candidate
                        break
                    except FileExistsError:
                        continue
                if temporary_fd is None:
                    raise OSError('native atomic temporary creation unavailable')
                info = os.fstat(temporary_fd)
                temporary_identity = (info.st_dev, info.st_ino, info.st_uid, info.st_gid)
                os.fchmod(temporary_fd, 0o600)
                offset = 0
                while offset < len(data):
                    chunk = memoryview(data)[offset:offset + 65536]
                    written = os.write(temporary_fd, chunk)
                    if type(written) is not int or not 0 < written <= len(chunk):
                        raise OSError('native atomic write made invalid progress')
                    offset += written
                # Writes above are unbuffered; fsync settles the complete payload.
                os.fsync(temporary_fd)
                if not self._parent_current(path.parent, parent_fd, parent_identity):
                    self._uncertain = True
                    raise AtomicPublicationUncertain()
                if (not self._original_temporary(parent_fd, name, temporary_fd, temporary_identity)
                        or os.fstat(temporary_fd).st_size != len(data)):
                    self._uncertain = True
                    raise AtomicPublicationUncertain()
                os.replace(name, path.name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
                replaced = True
                os.fsync(parent_fd)
                if not self._parent_current(path.parent, parent_fd, parent_identity):
                    self._uncertain = True
                    raise AtomicPublicationUncertain()
            except BaseException as error:
                primary = error
                if replaced:
                    self._uncertain = True
                elif temporary_fd is not None:
                    try:
                        if (not self._parent_current(path.parent, parent_fd, parent_identity)
                                or not self._original_temporary(parent_fd, name, temporary_fd, temporary_identity)):
                            self._uncertain = True
                        else:
                            os.unlink(name, dir_fd=parent_fd)
                            os.fsync(parent_fd)
                            if not self._parent_current(path.parent, parent_fd, parent_identity):
                                self._uncertain = True
                    except BaseException:
                        self._uncertain = True
                raise
            finally:
                close_error = None
                for descriptor in (temporary_fd, parent_fd):
                    if descriptor is not None:
                        try:
                            os.close(descriptor)
                        except OSError as error:
                            self._uncertain = True
                            close_error = close_error or error
                if close_error is not None and primary is None:
                    raise close_error


_atomic_publisher = _AtomicPublisher()


def credential_bytes(path):
    data = private_bytes(path, MAX_CREDENTIAL_BYTES)
    # The file may grow after the pre-read stat.
    if len(data) > MAX_CREDENTIAL_BYTES:
        raise ValueError('native credential exceeds byte budget')
    return data


def credential_json(data):
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate credential JSON field')
            result[key] = value
        return result
    def invalid_constant(_):
        raise ValueError('nonfinite credential JSON number')
    value = json.loads(data.decode('utf-8'), object_pairs_hook=unique_object,
                       parse_constant=invalid_constant)
    def valid_scalars(item):
        if isinstance(item, str):
            item.encode('utf-8', errors='strict')
        elif isinstance(item, dict):
            for key, child in item.items():
                valid_scalars(key)
                valid_scalars(child)
        elif isinstance(item, list):
            for child in item:
                valid_scalars(child)
        elif isinstance(item, float) and not math.isfinite(item):
            raise ValueError('nonfinite credential JSON number')
    valid_scalars(value)
    return value




def atomic(path, data):
    return _atomic_publisher.publish(path, data)



def safe_relative(raw):
    path = Path(raw)
    if (not isinstance(raw, str) or not raw or path.is_absolute()
            or any(part in ('..', '.') for part in path.parts)
            or '\0' in raw or len(raw) > 1024 or len(path.parts) > 16):
        raise ValueError('invalid native runtime export path')
    return path


@contextlib.contextmanager
def short_channel_endpoint():
    """Socket only: AF_UNIX names cannot inherit arbitrarily long state roots.

    All durable artifacts remain in the registered session. This private,
    ephemeral directory contains only the live transport socket and is removed
    after QEMU cleanup, including startup failure.
    """
    with tempfile.TemporaryDirectory(prefix='jv-', dir='/tmp') as directory:
        root = Path(directory)
        metadata = root.stat()
        if root.is_symlink() or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
            raise ValueError('native channel directory is not private')
        endpoint = root / 'channel'
        if len(os.fsencode(endpoint)) > 107:
            raise ValueError('native channel path exceeds AF_UNIX bound')
        yield endpoint


class Channel:
    def __init__(self, connection):
        self.connection = connection
        self.lock = threading.Lock()

    def send(self, frame):
        data = json.dumps(frame, separators=(',', ':')).encode() + b'\n'
        if len(data) > MAX_FRAME:
            raise ValueError('native VM frame exceeds limit')
        with self.lock:
            self.connection.sendall(data)

    def receive(self):
        with self.connection.makefile('rb') as stream:
            while True:
                raw = stream.readline(MAX_FRAME + 1)
                if not raw:
                    return
                if len(raw) > MAX_FRAME or not raw.endswith(b'\n'):
                    raise ValueError('invalid native VM frame')
                frame = json.loads(raw)
                if not isinstance(frame, dict) or not isinstance(frame.get('type'), str):
                    raise ValueError('invalid native VM frame type')
                yield frame


def jwt_identity(token):
    parts = token.split('.')
    if len(parts) != 3 or any(not part for part in parts):
        raise ValueError('credential identity token is not a JWT')
    raw = parts[1].encode()
    payload = json.loads(base64.b64decode(raw + b'=' * (-len(raw) % 4), altchars=b'-_', validate=True))
    if not isinstance(payload, dict) or not all(key in payload for key in ('sub', 'iss', 'aud')):
        raise ValueError('credential identity claims unavailable')
    identity = {key: payload[key] for key in ('sub', 'iss', 'aud')}
    account = payload.get('https://api.openai.com/auth', {})
    if not isinstance(account, dict):
        raise ValueError('credential account claims malformed')
    identity['account'] = {key: account[key] for key in
        ('chatgpt_account_id', 'user_id', 'organization_id') if key in account}
    return identity


def refreshed_credential(original, refreshed, provider):
    """Only provider-declared rotating fields may change; identity stays fixed."""
    if not isinstance(original, dict) or not isinstance(refreshed, dict):
        raise ValueError('credential schema is not an object')
    if provider == 'claude':
        container = 'claudeAiOauth'
        rotating = {'accessToken': str, 'refreshToken': str, 'expiresAt': int}
        timestamps = set()
    elif provider == 'codex':
        container = 'tokens'
        rotating = {'access_token': str, 'refresh_token': str, 'id_token': str}
        timestamps = {'last_refresh'}
    else:
        raise ValueError('provider does not declare OAuth refresh')
    before, after = original.get(container), refreshed.get(container)
    if not isinstance(before, dict) or not isinstance(after, dict) or set(before) != set(after):
        raise ValueError('credential token schema changed')
    if set(original) != set(refreshed):
        raise ValueError('credential outer schema changed')
    for key in original:
        if key == container:
            continue
        if key in timestamps:
            if not isinstance(refreshed[key], str) or len(refreshed[key]) > 128:
                raise ValueError('invalid credential refresh timestamp')
        elif original[key] != refreshed[key]:
            raise ValueError('credential identity or configuration changed')
    for key in before:
        if key not in rotating:
            if before[key] != after[key]:
                raise ValueError('credential account identity or scope changed')
        elif type(after[key]) is not rotating[key]:
            raise ValueError('invalid refreshed token shape')
        elif isinstance(after[key], str) and not 1 <= len(after[key]) <= 128 * 1024:
            raise ValueError('invalid refreshed token length')
        elif isinstance(after[key], int) and after[key] <= 0:
            raise ValueError('invalid credential expiry')
    if not set(rotating) <= set(before):
        raise ValueError('required rotating credential fields unavailable')
    if provider == 'codex' and jwt_identity(before['id_token']) != jwt_identity(after['id_token']):
        raise ValueError('credential identity token account changed')
    return refreshed


def credential_use(spec):
    if not spec.get('refresh_auth'):
        return None
    auth = Path(spec['auth_file'])
    controlled = Path(spec['credential_directory'])
    if auth != controlled / (spec['provider'] + '.json'):
        raise ValueError('refresh credential is outside controlled writer domain')
    for parent in auth.parents:
        info = parent.stat()
        if info.st_uid not in (0, os.getuid()) or (info.st_mode & 0o022 and not (info.st_uid == 0 and info.st_mode & stat.S_ISVTX)):
            raise ValueError('credential ancestor is writable by other principals')
    if auth.resolve() != auth or auth.parent.stat().st_uid != os.getuid() or auth.parent.stat().st_mode & 0o077:
        raise ValueError('credential refresh parent is not private')
    lock_fd = os.open(auth.with_name(auth.name + '.native-vm-refresh.lock'),
                      os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        info = os.fstat(lock_fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            raise ValueError('credential refresh lock is not private')
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return lock_fd
    except BaseException:
        os.close(lock_fd)
        raise


def refresh_auth(spec, generation, original_digest, held_lock=None):
    if not spec.get('refresh_auth'):
        return
    lock_fd = held_lock if held_lock is not None else credential_use(spec)
    auth = Path(spec['auth_file'])
    try:
        current = credential_bytes(auth)
        if hashlib.sha256(current).hexdigest() != original_digest:
            return  # Independently refreshed controller credentials always win.
        candidate = credential_bytes(generation / safe_relative(spec['auth_relative']))
        original = credential_json(current)
        updated = refreshed_credential(original, credential_json(candidate), spec['provider'])
        if updated != original:
            atomic(auth, candidate)
    finally:
        if held_lock is None:
            os.close(lock_fd)


class RefreshCheckpoint:
    """Persist authenticated issuer rotation before delivering it to the guest.

    This does not grant network access: the gateway independently admits the
    exact route. The process-wide credential_use lock must already be held.
    """
    def __init__(self, spec, held_lock):
        if not spec.get('refresh_auth') or held_lock is None:
            raise ValueError('refresh checkpoint requires controlled credential lease')
        self.spec = spec
        self.lock = threading.Lock()
        self.current = None
        self.journal = Path(spec['auth_file']).with_name(Path(spec['auth_file']).name + '.refresh-checkpoint.json')
        self.recover()

    def recover(self):
        if not self.journal.exists():
            return
        record = json.loads(private_bytes(self.journal, 2 * 1024 * 1024))
        current = credential_bytes(Path(self.spec['auth_file']))
        credential_json(current)
        if record['state'] == 'committed':
            return
        digest = hashlib.sha256(current).hexdigest()
        if record.get('result_digest') == digest:
            atomic(self.journal, json.dumps({'state': 'committed', 'result_digest': digest}).encode())
            return
        if record.get('original_digest') != digest:
            raise ValueError('refresh checkpoint credential lineage changed')
        if record['state'] != 'received':
            raise ValueError('credential refresh outcome uncertain; issuer reconciliation required')
        self.current = current
        try:
            self.commit(base64.b64decode(record['response'], validate=True), record['received_at_ms'])
        finally:
            self.current = None

    def sent(self):
        atomic(self.journal, json.dumps({'state': 'pending',
            'original_digest': hashlib.sha256(self.current).hexdigest()}).encode())

    def rejected(self, status):
        # Non-success is not proof that the issuer did not rotate before an
        # internal/proxy error. Keep this outcome fenced, including same-launch retries.
        atomic(self.journal, json.dumps({'state': 'pending', 'http_status': status,
            'original_digest': hashlib.sha256(self.current).hexdigest()}).encode())

    def begin(self, request, entry, body):
        endpoint = {'claude': ('platform.claude.com:443', '/v1/oauth/token'),
                    'codex': ('auth.openai.com:443', '/oauth/token')}[self.spec['provider']]
        authority = request['authority'].lower()
        if ':' not in authority:
            authority += ':443'
        if (authority, urllib.parse.unquote(urllib.parse.urlsplit(request['path']).path, errors='strict')) != endpoint:
            return False
        if request['path'] != endpoint[1]:
            raise ValueError('refresh query is not supported')
        if request['method'] != 'POST' or request.get('websocket') or not entry['tls'] or entry['allow_loopback']:
            raise ValueError('invalid refresh transport')
        if len(body) > 1024 * 1024:
            raise ValueError('refresh request too large')
        def unique_object(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError('duplicate refresh JSON field')
                result[key] = value
            return result
        data = json.loads(body, object_pairs_hook=unique_object)
        if not isinstance(data, dict) or data.get('grant_type') != 'refresh_token':
            raise ValueError('only refresh grant is permitted for credential checkpoint')
        expected_client = {'claude': '9d1c250a-e61b-44d9-88ed-5944d1962f5e',
                           'codex': 'app_EMoamEEZ73f0CkXaXp7hrann'}[self.spec['provider']]
        allowed = {'grant_type', 'refresh_token', 'client_id'} | ({'scope', 'expires_in'} if self.spec['provider'] == 'claude' else {'refresh_reason'})
        if set(data) - allowed:
            raise ValueError('undeclared refresh request fields')
        if data.get('client_id') != expected_client:
            raise ValueError('refresh client is not the declared native client')
        if not self.lock.acquire(blocking=False):
            raise ValueError('credential refresh already in progress')
        try:
            self.recover()
            self.current = credential_bytes(Path(self.spec['auth_file']))
            original = credential_json(self.current)
            container, token = ('claudeAiOauth', 'refreshToken') if self.spec['provider'] == 'claude' else ('tokens', 'refresh_token')
            if 'scope' in data and (not isinstance(data['scope'], str) or set(data['scope'].split()) != set(original[container]['scopes'])):
                raise ValueError('refresh request changes credential scope')
            if data.get('refresh_token') != original[container][token]:
                raise ValueError('refresh request does not match controlled credential')
            return True
        except BaseException:
            self.end()
            raise

    def commit(self, response, received_at_ms=None):
        if self.current is None or len(response) > 1024 * 1024:
            raise ValueError('invalid refresh checkpoint response')
        if received_at_ms is None:
            received_at_ms = int(time.time() * 1000)
        if type(received_at_ms) is not int or received_at_ms <= 0:
            raise ValueError('invalid refresh receipt time')
        record = {'state': 'received', 'received_at_ms': received_at_ms, 'original_digest': hashlib.sha256(self.current).hexdigest(),
                  'response': base64.b64encode(response).decode()}
        atomic(self.journal, json.dumps(record).encode())
        data = json.loads(response)
        if not isinstance(data, dict):
            raise ValueError('invalid refresh response schema')
        original = credential_json(self.current)
        updated = credential_json(self.current)
        if self.spec['provider'] == 'claude':
            tokens = updated['claudeAiOauth']
            tokens['accessToken'] = data['access_token']
            if data.get('refresh_token') is not None:
                tokens['refreshToken'] = data['refresh_token']
            lifetime = data['expires_in']
            if type(lifetime) is not int or not 0 < lifetime <= 366 * 86400:
                raise ValueError('invalid refresh lifetime')
            tokens['expiresAt'] = received_at_ms + lifetime * 1000
            if 'scope' in data and (not isinstance(data['scope'], str) or set(data['scope'].split()) != set(tokens['scopes'])):
                raise ValueError('refresh scope changed')
        else:
            for name in ('access_token', 'refresh_token', 'id_token'):
                if data.get(name) is not None:
                    updated['tokens'][name] = data[name]
            if 'last_refresh' in updated:
                updated['last_refresh'] = datetime.datetime.fromtimestamp(received_at_ms / 1000, datetime.timezone.utc).isoformat().replace('+00:00', 'Z')
        refreshed_credential(original, updated, self.spec['provider'])
        target = Path(self.spec['auth_file'])
        if credential_bytes(target) != self.current:
            raise ValueError('credential changed during refresh checkpoint')
        encoded = json.dumps(updated, ensure_ascii=False, allow_nan=False, separators=(',', ':')).encode('utf-8')
        if len(encoded) > MAX_CREDENTIAL_BYTES:
            raise ValueError('native credential exceeds byte budget')
        record['result_digest'] = hashlib.sha256(encoded).hexdigest()
        atomic(self.journal, json.dumps(record).encode())
        atomic(target, encoded)
        atomic(self.journal, json.dumps({'state': 'committed', 'result_digest': record['result_digest']}).encode())

    def end(self):
        self.current = None
        self.lock.release()


def publish_runtime_generation(gateway, spec, staging, state, original_auth, credential_lock):
    # Gateway recovery/checkpoint workers share this launch's credential lease.
    # Settle them before export writeback, not merely during final cleanup.
    gateway.close()
    generation = 'generation-' + uuid.uuid4().hex
    os.replace(staging, state / generation)
    refresh_auth(spec, state / generation, original_auth, credential_lock)
    return generation


def runtime_seed(runtime, auth_file, auth_relative):
    entries = []
    total = 0
    generation = private_json(runtime / 'complete.json')['generation'] if (runtime / 'complete.json').exists() else None
    if generation and (not generation.startswith('generation-') or len(safe_relative(generation).parts) != 1):
        raise ValueError('invalid native runtime generation')
    home = runtime / safe_relative(generation) if generation else runtime / 'empty'
    if home.is_symlink():
        raise ValueError('native runtime generation is a symlink')
    if home.exists():
        for base, directories, files in os.walk(home, followlinks=False):
            for name in directories:
                item = Path(base) / name
                if item.is_symlink():
                    raise ValueError('native runtime directory is a symlink')
            for name in files:
                item = Path(base) / name
                data = private_bytes(item, MAX_HOME - total)
                total += len(data)
                if total > MAX_HOME:
                    raise ValueError('native runtime exceeds budget')
                entries.append((str(item.relative_to(home)), data))
    original = None
    if auth_file:
        data = credential_bytes(Path(auth_file))
        credential_json(data)
        original = hashlib.sha256(data).hexdigest()
        previous = private_json(runtime / 'complete.json') if (runtime / 'complete.json').exists() else {}
        # Preserve an OAuth refresh produced inside this isolated conversation.
        # A separately refreshed controller credential supersedes the seed.
        if previous.get('auth_generation') != original or not any(name == auth_relative for name, _ in entries):
            entries = [(name, contents) for name, contents in entries if name != auth_relative]
            entries.append((auth_relative, data))
    if sum(len(contents) for _, contents in entries) > MAX_HOME:
        raise ValueError('native runtime seed exceeds budget')
    return entries, original


def boot_entries(config, job, seed, guest_source, gateway_source, cert_entries):
    entries = BootEntries()
    directories = ('bootstrap', 'dev', 'proc', 'sys', 'tmp', 'modules', 'usr', 'etc', 'native',
                   'home', 'home/worker', 'workspace', 'seed', 'root', 'root/control',
                   'run', 'run/native-provider-gateway')
    for name in directories:
        mode = 0o700 if name.startswith('root') else 0o1777 if name == 'tmp' else 0o755
        entries.append((name, stat.S_IFDIR | mode, b'', 0, 0))
    entries.append(('dev/console', stat.S_IFCHR | 0o600, b'', 5, 1))
    entries.append_input('bootstrap/busybox', stat.S_IFREG | 0o755, Path(config['busybox']))
    for name in ('sh', 'mount', 'ip', 'insmod', 'poweroff', 'mkdir'):
        entries.append(('bootstrap/' + name, stat.S_IFLNK | 0o777, b'busybox', 0, 0))
    for name in ('bin', 'sbin', 'lib', 'lib64'):
        entries.append((name, stat.S_IFLNK | 0o777, ('usr/' + name).encode(), 0, 0))
    commands = []
    for index, raw in enumerate(config['modules']):
        entries.append_input(f'modules/{index}.ko', stat.S_IFREG | 0o400, Path(raw))
        commands.append(f'insmod /modules/{index}.ko || poweroff -f')
    # Only this process-specific seed enters the guest; no host directory is
    # exported for writable home state. Runtime exports are bounded frames.
    seen = set(directories)
    for relative, data in seed:
        path = Path('seed') / safe_relative(relative)
        for parent in reversed(path.parents):
            if str(parent) not in ('.', '') and str(parent) not in seen:
                entries.append((str(parent), stat.S_IFDIR | 0o700, b'', 0, 0))
                seen.add(str(parent))
        entries.append((str(path), stat.S_IFREG | 0o600, data, 0, 0))
    entries.extend(cert_entries)
    entries.append(('root/job.json', stat.S_IFREG | 0o400, json.dumps(job).encode(), 0, 0))
    entries.append_input('root/guest.py', stat.S_IFREG | 0o400, guest_source)
    entries.append_input('root/native_provider_gateway.py', stat.S_IFREG | 0o400, gateway_source)
    boot = '#!/bootstrap/sh\nexport PATH=/bootstrap:/usr/bin:/bin\nmount -t devtmpfs devtmpfs /dev\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\n' + '\n'.join(commands) + '''
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev provider /native || poweroff -f
ip link set lo up
/usr/bin/python3 -I -S /root/guest.py
poweroff -f
'''
    entries.append(('init', stat.S_IFREG | 0o700, boot.encode(), 0, 0))
    return entries


def validate_runtime(path):
    guest_runtime = Path(path)
    if guest_runtime.is_symlink() or not guest_runtime.is_absolute() or guest_runtime.stat().st_mode & 0o077:
        raise ValueError('native guest runtime is not private')
    manifest = json.loads((guest_runtime / 'manifest.json').read_text())
    if manifest.get('version') != 1 or not manifest.get('files'):
        raise ValueError('native guest runtime manifest unavailable')
    declared = set()
    for entry in manifest['files']:
        relative = safe_relative(entry['path'])
        path = guest_runtime / relative
        if path.is_symlink() or not path.resolve().is_relative_to(guest_runtime):
            raise ValueError('runtime manifest contains a link')
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != entry['sha256']:
            raise ValueError('runtime artifact digest changed')
        declared.add(str(relative))
    inventory = list(guest_runtime.rglob('*'))
    if any(path.is_symlink() for path in inventory):
        raise ValueError('runtime symlink is undeclared')
    actual = {str(path.relative_to(guest_runtime)) for path in inventory if path.is_file()}
    if actual != declared | {'manifest.json'}:
        raise ValueError('undeclared runtime file')


def _staging_identity(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_gid,
            info.st_size, info.st_mtime_ns, info.st_ctime_ns)


@contextlib.contextmanager
def _native_staging_source(source, limit):
    source = Path(source)
    if not source.is_absolute():
        raise ValueError('native staging source must be absolute')
    # NONBLOCK prevents a substituted FIFO from blocking before fstat rejects it.
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        initial = os.fstat(fd)
        if (not stat.S_ISREG(initial.st_mode) or initial.st_uid not in (0, os.getuid())
                or initial.st_mode & 0o022):
            raise ValueError('native staging source is not trusted')
        if initial.st_size > limit:
            raise ValueError('native staging source exceeds byte budget')
        yield fd
        if (_staging_identity(os.fstat(fd)) != _staging_identity(initial)
                or _staging_identity(source.stat(follow_symlinks=False)) != _staging_identity(initial)):
            raise ValueError('native staging source changed')
    finally:
        os.close(fd)


def _native_staging_chunks(fd, limit):
    remaining = limit
    while True:
        chunk = os.read(fd, min(64 * 1024, remaining + 1))
        if not chunk:
            return
        if len(chunk) > remaining:
            raise ValueError('native staging source exceeds byte budget')
        remaining -= len(chunk)
        yield chunk


def _native_staging_digest(source, limit):
    digest = hashlib.sha256()
    with _native_staging_source(source, limit) as fd:
        for chunk in _native_staging_chunks(fd, limit):
            digest.update(chunk)
    return digest.hexdigest()


def _stage_native_file(source, destination, limit, expected_digest=None):
    destination = Path(destination)
    target_fd = None
    created = None
    try:
        with _native_staging_source(source, limit) as source_fd:
            target_fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL
                                | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
            created = os.fstat(target_fd)
            digest = hashlib.sha256()
            for chunk in _native_staging_chunks(source_fd, limit):
                digest.update(chunk)
                pending = memoryview(chunk)
                while pending:
                    count = os.write(target_fd, pending)
                    if count <= 0:
                        raise OSError('native staging write made no progress')
                    pending = pending[count:]
            if expected_digest is not None and digest.hexdigest() != expected_digest:
                raise ValueError('Codex runtime companion digest changed')
        os.fchmod(target_fd, 0o555)
    except BaseException:
        if created is not None:
            # The private launch directory has no guest writers during staging.
            # Never remove a preexisting or visibly replaced destination.
            try:
                current = destination.lstat()
                if (current.st_dev, current.st_ino) == (created.st_dev, created.st_ino):
                    destination.unlink()
            except FileNotFoundError:
                pass
        raise
    finally:
        if target_fd is not None:
            os.close(target_fd)


def stage_native_executable(executable, destination):
    _stage_native_file(executable, destination, MAX_EXECUTABLE)


def stage_native_companions(executable, guest_runtime, destination):
    executable = Path(executable)
    if executable.resolve().name != 'codex':
        return
    runtime = Path(guest_runtime)
    manifest = json.loads((runtime / 'manifest.json').read_text())
    digest = _native_staging_digest(Path(destination) / 'cli', MAX_EXECUTABLE)
    entries = manifest.get('native_companions', {}).get(digest)
    if not isinstance(entries, list) or len(entries) != 1:
        raise ValueError('Codex runtime companion missing for executable digest; reprovision runtime')
    entry = entries[0]
    if entry.get('name') != 'codex-code-mode-host':
        raise ValueError('Codex runtime companion identity invalid')
    relative = safe_relative(entry['path'])
    declared = {item['path']: item['sha256'] for item in manifest['files']}
    source = runtime / relative
    if source.is_symlink() or not source.resolve().is_relative_to(runtime) or declared.get(str(relative)) != entry.get('sha256'):
        raise ValueError('Codex runtime companion is not manifested')
    target = Path(destination) / entry['name']
    _stage_native_file(source, target, MAX_COMPANION, entry['sha256'])


def main(spec_path):
    vm = module('native_vm_runtime', 'codex-build-vm.py')
    gateway_module = module('native_provider_gateway', 'native-provider-gateway.py')
    spec = private_json(spec_path)
    if spec.get('version') != 1:
        raise ValueError('unsupported native VM launch version')
    validate_runtime(spec['guest_runtime'])
    runtime = vm.Runtime.load(Path(spec['runtime_config']))
    config = dict(runtime.config)
    config['memory_mb'] = spec['memory_mb']
    if not 512 <= config['memory_mb'] <= 4096:
        raise ValueError('native VM memory policy invalid')
    state = Path(spec['session_directory'])
    if state.is_symlink() or state.resolve() != state or state.stat().st_mode & 0o077:
        raise ValueError('native session directory is not private')
    if spec.get('resume') and not (state / 'complete.json').is_file():
        raise ValueError('native conversation recovery requires completed runtime export')
    lock_fd = os.open(state / 'lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    try:
        credential_lock = credential_use(spec)
    except BlockingIOError:
        ready = os.environ.get('JARVIS_NATIVE_VM_READY')
        if ready:
            atomic(Path(ready).with_name('vm-startup-failure.json'), b'{"phase":"native_credential_in_use"}')
        raise RuntimeError('native credential is in use; retry after capacity is available') from None
    refresh_checkpoint = RefreshCheckpoint(spec, credential_lock) if spec.get('refresh_auth') else None
    seed, original_auth = runtime_seed(state, spec.get('auth_file'), spec['auth_relative'])
    with tempfile.TemporaryDirectory(prefix='native-vm-launch-', dir=state) as temporary, short_channel_endpoint() as endpoint:
        private = Path(temporary)
        binary = private / 'provider'
        binary.mkdir()
        executable = Path(spec['executable'])
        stage_native_executable(executable, binary / 'cli')
        stage_native_companions(executable, spec['guest_runtime'], binary)
        assets = spec.get('assets', [])
        if len(assets) > 32:
            raise ValueError('native asset count exceeds budget')
        asset_bytes = 0
        for asset in assets:
            name = safe_relative(asset['name'])
            if len(name.parts) != 1 or not str(name).startswith('asset-'):
                raise ValueError('invalid native input asset')
            source = Path(asset['source'])
            fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            with os.fdopen(fd, 'rb') as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_size > 16 * 1024 * 1024:
                    raise ValueError('native input asset exceeds policy')
                data = stream.read(16 * 1024 * 1024 + 1)
                asset_bytes += len(data)
                if asset_bytes > 16 * 1024 * 1024:
                    raise ValueError('native total asset budget exceeded')
                (binary / name).write_bytes(data)
                (binary / name).chmod(0o444)
        # The guest receives only a relay executable, never the host bridge
        # policy, settings, hooks, source checkout or daemon credential store.
        shutil.copyfile(Path(__file__).with_name('native-provider-mcp-relay.py'), binary / 'mcp-relay.py')
        guest_config, certificates = gateway_module.provision(spec['gateway'], private)
        job = {'argv': spec['argv'], 'environment': spec['environment'], 'uid': os.getuid(), 'gid': os.getgid(),
               'gateway': guest_config, 'mcp': bool(spec.get('mcp_argv')), 'auth_relative': spec['auth_relative']}
        entries = boot_entries(config, job, seed, Path(__file__).with_name('native-provider-vm-guest.py'),
                               Path(__file__).with_name('native-provider-gateway.py'), certificates)
        image = private / 'initrd.gz'
        write_boot_image(image, entries, vm.initrd)
        command = vm.qemu_command(config, image, [('runtime', Path(spec['guest_runtime']), True), ('provider', binary, True)], None)
        command[command.index('-serial') + 1] = 'null'
        command.extend(['-device', 'virtio-serial-pci', '-chardev',
                        f'socket,id=native,path={endpoint},server=on,wait=off', '-device',
                        'virtserialport,chardev=native,name=org.jarvis.native.control'])
        environment = {'PATH': os.defpath, 'LD_LIBRARY_PATH': config['library_dir'], 'QEMU_MODULE_DIR': config['module_dir']}
        process = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        bridge = None
        gateway = None
        channel_socket = socket.socket(socket.AF_UNIX)
        phase = 'channel_startup'
        frame_counts = {}
        admitted = False
        exported = 0
        received = set()
        active_export = None
        quiescence = None
        exit_code = None
        try:
            deadline = time.monotonic() + 30
            while True:
                if process.poll() is not None:
                    raise RuntimeError('native VM could not boot')
                try:
                    channel_socket.connect(str(endpoint))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    if time.monotonic() > deadline:
                        raise TimeoutError('native VM channel admission timed out')
                    time.sleep(0.02)
            channel = Channel(channel_socket)
            diagnostic_path = Path(spec_path).parent / 'gateway-route-denials.jsonl' if spec.get('probe_route_diagnostics') is True else None
            gateway = gateway_module.HostGateway(spec['gateway'], channel.send, diagnostic_path=diagnostic_path,
                refresh_checkpoint=refresh_checkpoint)
            if spec.get('mcp_argv'):
                bridge = subprocess.Popen(spec['mcp_argv'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                          stderr=subprocess.DEVNULL, env={'PATH': os.defpath, 'HOME': str(Path.home())})
                def bridge_output():
                    for raw in iter(lambda: bridge.stdout.read1(65536), b''):
                        channel.send({'type': 'mcp_output', 'data': base64.b64encode(raw).decode()})
                    channel.send({'type': 'mcp_end'})
                threading.Thread(target=bridge_output, daemon=True).start()
                def bridge_failed():
                    try:
                        channel_socket.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                def bridge_write(data):
                    bridge.stdin.write(data)
                    bridge.stdin.flush()
                bridge_writer = BoundedWriter(bridge_write, bridge.stdin.close, bridge_failed)
            else:
                bridge_writer = None
            def stdin_input():
                for raw in iter(lambda: sys.stdin.buffer.read1(65536), b''):
                    channel.send({'type': 'stdin', 'data': base64.b64encode(raw).decode()})
                channel.send({'type': 'stdin_end'})
            admitted = False
            exported = 0
            received = set()
            export_directories = set()
            staging = private / 'export'
            staging.mkdir(mode=0o700)
            exit_code = None
            active_export = None
            quiescence = None
            phase = 'receive_frames'
            for frame in channel.receive():
                kind = frame['type']
                safe_kind = kind if kind in ('ready', 'stdout', 'stderr', 'mcp_input', 'worker_quiescent', 'runtime_start', 'runtime_data', 'runtime_end', 'completion_failure', 'guest_fatal', 'exit') else 'gateway' if kind.startswith('gateway_') else 'unknown'
                frame_counts[safe_kind] = frame_counts.get(safe_kind, 0) + 1
                if kind == 'ready' and not admitted:
                    ready = os.environ.get('JARVIS_NATIVE_VM_READY')
                    proceed = os.environ.get('JARVIS_NATIVE_VM_CONTINUE')
                    if not ready or not proceed:
                        raise RuntimeError('native VM controller admission missing')
                    atomic(Path(ready), b'ready\n')
                    deadline = time.monotonic() + 15
                    while not Path(proceed).exists():
                        if time.monotonic() >= deadline:
                            raise TimeoutError('native VM admission timed out')
                        time.sleep(0.005)
                    if private_bytes(Path(proceed), 32) != b'admitted\n':
                        raise ValueError('invalid native VM admission')
                    # Durable controller dispatch intent precedes releasing the
                    # guest. An interrupted handshake must not imply no call.
                    atomic(Path(ready).with_name('vm-dispatch-intent'),
                           json.dumps({'version': 1, 'phase': 'dispatch_uncertain'}).encode())
                    channel.send({'type': 'start'})
                    admitted = True
                    threading.Thread(target=stdin_input, daemon=True).start()
                elif kind in ('stdout', 'stderr') and admitted:
                    data = base64.b64decode(frame['data'], validate=True)
                    stream = sys.stdout.buffer if kind == 'stdout' else sys.stderr.buffer
                    stream.write(data)
                    stream.flush()
                elif kind == 'mcp_input':
                    if bridge is None:
                        raise ValueError('undeclared tool channel')
                    bridge_writer.submit(base64.b64decode(frame['data'], validate=True))
                elif kind.startswith('gateway_'):
                    gateway.receive(frame)
                elif kind == 'worker_quiescent' and admitted and quiescence is None:
                    keys = ('scans', 'signalled', 'reaped', 'remaining_workers', 'quiet_scans')
                    if any(type(frame.get(key)) is not int or not 0 <= frame[key] <= 100000 for key in keys):
                        raise ValueError('invalid native quiescence receipt')
                    if frame['remaining_workers'] != 0 or frame['quiet_scans'] < 2 or frame['scans'] < 2:
                        raise ValueError('native workers remain before runtime export')
                    quiescence = {key: frame[key] for key in keys}
                elif kind == 'runtime_start' and admitted and quiescence is not None:
                    relative = safe_relative(frame['path'])
                    size = frame['size']
                    if active_export is not None or type(size) is not int or size < 0:
                        raise ValueError('invalid runtime export start')
                    exported += size
                    if exported > MAX_HOME or str(relative) in received or len(received) >= 4096:
                        raise ValueError('native runtime export exceeds budget or duplicates file')
                    received.add(str(relative))
                    target = staging / relative
                    export_directories.update(str(parent) for parent in relative.parents if str(parent) != '.')
                    if len(export_directories) > 2048:
                        raise ValueError('native runtime directory budget exceeded')
                    target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                    stream = open(target, 'xb')
                    os.fchmod(stream.fileno(), 0o600)
                    active_export = [stream, size, 0]
                elif kind == 'runtime_data' and active_export is not None:
                    data = base64.b64decode(frame['data'], validate=True)
                    stream, size, offset = active_export
                    if frame['offset'] != offset or offset + len(data) > size:
                        raise ValueError('invalid runtime export chunk')
                    stream.write(data)
                    active_export[2] += len(data)
                elif kind == 'runtime_end' and active_export is not None:
                    stream, size, offset = active_export
                    if size != offset:
                        raise ValueError('truncated runtime export')
                    stream.flush()
                    os.fsync(stream.fileno())
                    stream.close()
                    active_export = None
                    channel.send({'type': 'export_progress'})
                elif kind == 'guest_fatal':
                    reason = frame.get('reason')
                    if reason not in ('stdin_write_failed', 'mcp_write_failed', 'oversized_or_truncated_frame', 'stdin_submit_failed', 'mcp_submit_failed', 'unknown_frame', 'controller_eof', 'receive_exception'):
                        raise ValueError('invalid guest fatal diagnostic')
                    print(json.dumps({'native_guest_fatal': reason}), file=sys.stderr, flush=True)
                    raise RuntimeError('guest failed before completion')
                elif kind == 'completion_failure':
                    if frame.get('phase') not in ('quiescence', 'output_drain', 'runtime_export', 'exit_receipt', 'export_ack') or frame.get('category') not in ('ValueError', 'RuntimeError', 'OSError', 'PermissionError', 'FileNotFoundError', 'BoundaryFailure'):
                        raise ValueError('invalid completion diagnostic')
                    diagnostic = {'phase': frame['phase'], 'category': frame['category']}
                    if type(frame.get('errno')) is int:
                        diagnostic['errno'] = frame['errno']
                    if frame.get('export_kind') in ('symlink', 'directory_symlink', 'open_failure') and frame.get('export_area') in ('codex_arg0', 'other'):
                        diagnostic.update(export_kind=frame['export_kind'], export_area=frame['export_area'])
                    print(json.dumps({'native_guest_completion_failure': diagnostic}), file=sys.stderr, flush=True)
                    raise RuntimeError('guest completion failed')
                elif kind == 'exit':
                    exit_code = frame['code']
                    if type(exit_code) is not int or not 0 <= exit_code <= 255:
                        raise ValueError('invalid native exit status')
                    if active_export is not None or not admitted or quiescence is None:
                        raise ValueError('incomplete export cannot be acknowledged')
                    # This acknowledges receipt only, not success/approval. Keep
                    # the guest alive until every exported byte has been fsynced.
                    channel.send({'type': 'export_received'})
                    break
                else:
                    raise ValueError('unknown native guest frame')
            if exit_code is None or active_export is not None or not admitted or quiescence is None:
                raise RuntimeError('native VM disconnected without completion')
            phase = 'qemu_shutdown'
            process.wait(timeout=10)
            if process.returncode:
                raise RuntimeError('native VM shutdown failed')
            phase = 'publish_generation'
            generation = publish_runtime_generation(gateway, spec, staging, state, original_auth, credential_lock)
            atomic(state / 'complete.json', json.dumps({'version': 1, 'exit_code': exit_code,
                   'generation': generation, 'export_bytes': exported,
                   'auth_generation': original_auth, 'worker_quiescence': quiescence,
                   'launch_sha256': hashlib.sha256(private_bytes(Path(spec_path), 2 * 1024 * 1024)).hexdigest(),
                   'gateway_transport': gateway.transport_receipt()}).encode())
            # Only controller-generated complete exports are retained. Keep the
            # previous generation through pointer publication for crash safety.
            previous = sorted(state.glob('generation-*'), key=lambda path: path.stat().st_mtime)
            for obsolete in previous[:-2]:
                if obsolete.name != generation and not obsolete.is_symlink():
                    shutil.rmtree(obsolete)
            if exit_code and (gateway.guest_failures or gateway.host_failures):
                print(json.dumps({'native_gateway_failures': gateway.guest_failures, 'native_gateway_host_failures': gateway.host_failures}), file=sys.stderr, flush=True)
            return exit_code
        except Exception:
            try:
                print(json.dumps({'native_host_failure_state': {'phase': phase, 'frames': frame_counts,
                    'admitted': admitted, 'quiescent': quiescence is not None, 'export_bytes': exported,
                    'export_files': len(received), 'partial_export': active_export is not None,
                    'guest_exit_code': exit_code, 'qemu_exit_code': process.poll()}}), file=sys.stderr, flush=True)
                if gateway is not None:
                    # Retained observation is diagnostic only: no generation publication,
                    # complete.json, or successful launch authority follows this path.
                    try:
                        atomic(Path(spec_path).parent / 'failed-gateway-observation.json', json.dumps({
                            'version': 1, 'diagnostic_only': True,
                            'launch_sha256': hashlib.sha256(private_bytes(Path(spec_path), 2 * 1024 * 1024)).hexdigest(),
                            'gateway_transport': gateway.transport_receipt()}).encode())
                    except Exception:
                        # Diagnostic loss cannot replace the original launch failure.
                        print(json.dumps({'native_diagnostic_publication_failed': True}), file=sys.stderr, flush=True)

                if gateway is not None and (gateway.guest_failures or gateway.host_failures):
                    print(json.dumps({'native_gateway_failures': gateway.guest_failures, 'native_gateway_host_failures': gateway.host_failures}), file=sys.stderr, flush=True)
            except Exception:
                pass  # Diagnostics never replace the original launch error.
            raise
        finally:
            if gateway is not None:
                gateway.close()
            channel_socket.close()
            os.close(lock_fd)
            if bridge is not None:
                bridge.terminate()
            if process.poll() is None:
                process.terminate()
            for child in (bridge, process):
                if child is None:
                    continue
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            if credential_lock is not None:
                os.close(credential_lock)


def entry(spec_path):
    try:
        return main(spec_path)
    except Exception as error:
        # Only a fixed exception category is safe; messages may contain secrets.
        category = type(error).__name__
        if category not in {'JSONDecodeError','ValueError','RuntimeError','TimeoutError','OSError','BrokenPipeError','FileNotFoundError','PermissionError','ChildProcessError','CalledProcessError'}:
            category = 'BoundaryFailure'
        diagnostic = {'native_vm_failure': category}
        if isinstance(error, OSError) and isinstance(error.errno, int):
            diagnostic['errno'] = error.errno
        try:
            ready = os.environ.get('JARVIS_NATIVE_VM_READY')
            if ready and not Path(ready).exists():
                failure = Path(ready).with_name('vm-startup-failure.json')
                if not failure.exists():
                    # Fixed category/errno only: no exception messages, paths,
                    # credentials or provider payloads cross this receipt boundary.
                    try:
                        atomic(failure, json.dumps(dict(diagnostic, phase='native_setup_failed')).encode())
                    except Exception:
                        pass
            print(json.dumps(diagnostic), file=sys.stderr)
            print('Native VM boundary unavailable; launch terminated without host fallback.', file=sys.stderr)
        except Exception:
            pass  # Missing diagnostic delivery is not successful native work.
        return 126


if __name__ == '__main__':
    raise SystemExit(entry(sys.argv[1] if len(sys.argv) > 1 else None))
