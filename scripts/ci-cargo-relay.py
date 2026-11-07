#!/usr/bin/env python3
"""Unprivileged spawn-interface experiment; never a CI or merge authority.

There is intentionally no credential-transition mode. The controller, client
and delegated tool all use the same non-root UID in this prototype.
"""
import array
import errno
import fcntl
import hmac
import importlib.util
import json
import os
from pathlib import Path
import select
import signal
import socket
import stat
import struct
import subprocess
import sys
import threading
import time
import uuid

SEALS = fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL
LIMIT = 8 * 1024 * 1024
STRING_LIMIT = 1024 * 1024
REQUEST = struct.Struct('<8s32sI64i')
RESPONSE = struct.Struct('<8sII')
CONFIG = struct.Struct('<8sIII32s108s')
STAGES = {'started': 1, 'finished': 2, 'error': 3, 'signaled': 4}
FORWARDED_SIGNALS = {int(value) for value in signal.valid_signals()} - {
    int(signal.SIGKILL), int(signal.SIGSTOP), 32, 33}
BASE = Path(__file__).resolve().parent


def _strings(value):
    if not isinstance(value, bytes) or b'\0' in value or len(value) > STRING_LIMIT:
        raise ValueError('unsupported relay string')
    return struct.pack('<I', len(value)) + value


def encode_request(file, argv, environment, cwd, *, search_path=None):
    if not 1 <= len(argv) <= 4096 or len(environment) > 4096:
        raise ValueError('unsupported relay argument count')
    ignored = sum(1 << (number-1) for number in FORWARDED_SIGNALS if signal.getsignal(number) == signal.SIG_IGN)
    blocked = sum(1 << (int(number)-1) for number in signal.pthread_sigmask(signal.SIG_BLOCK, []))
    payload = struct.pack('<8sIIIQQ', b'JARVARG2', int(search_path is not None), len(argv), len(environment), ignored, blocked)
    payload += b''.join(_strings(value) for value in (file, cwd, search_path or b'', *argv, *environment))
    if len(payload) > LIMIT:
        raise ValueError('relay request too large')
    return payload


def request_header(token, numbers):
    if len(token) != 32 or not 3 <= len(numbers) <= 64:
        raise ValueError('invalid relay descriptor header')
    return REQUEST.pack(b'JARVREQ2', token, len(numbers), *(list(numbers) + [0] * (64-len(numbers))))


def decode_response(data):
    magic, stage, value = RESPONSE.unpack(data)
    if magic != b'JARVRSP2' or stage not in STAGES.values():
        raise ValueError('invalid relay response')
    return next(name for name, number in STAGES.items() if number == stage), value


def _response(connection, stage, value):
    connection.sendall(RESPONSE.pack(b'JARVRSP2', STAGES[stage], value))


def _payload(fd,*,expected_magic=b'JARVARG2'):
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or not 36 <= info.st_size <= LIMIT:
        raise ValueError('invalid relay payload')
    if fcntl.fcntl(fd, fcntl.F_GET_SEALS) & SEALS != SEALS:
        raise ValueError('unsealed relay payload')
    body = os.pread(fd, info.st_size + 1, 0)
    if len(body) != info.st_size:
        raise ValueError('incomplete relay payload')
    magic, lookup, argc, envc, ignored, blocked = struct.unpack_from('<8sIIIQQ', body)
    unsupported = sum(1 << (number-1) for number in (9, 19, 32, 33))
    if (expected_magic not in (b'JARVARG2',b'JARVARG3') or magic != expected_magic
            or lookup not in (0, 1) or not 1 <= argc <= 4096 or envc > 4096
            or (ignored | blocked) & unsupported):
        raise ValueError('invalid relay payload header')
    offset = 36
    def field():
        nonlocal offset
        if offset + 4 > len(body): raise ValueError('incomplete relay field')
        length, = struct.unpack_from('<I', body, offset)
        offset += 4
        if length > STRING_LIMIT or offset + length > len(body): raise ValueError('invalid relay field size')
        value = body[offset:offset+length]
        offset += length
        if b'\0' in value: raise ValueError('invalid relay field bytes')
        return value
    file, cwd, search = field(), field(), field()
    argv = [field() for _ in range(argc)]
    environment = {}
    for _ in range(envc):
        key, equal, value = field().partition(b'=')
        if not equal or not key or key in environment: raise ValueError('invalid relay environment')
        environment[key] = value
    if offset != len(body) or not file or not cwd.startswith(b'/') or len(cwd) > 4096:
        raise ValueError('invalid relay request shape')
    return file, argv, environment, cwd, search if lookup else None, ignored, blocked


def _close(descriptors):
    for fd in descriptors:
        try: os.close(fd)
        except OSError as error:
            if error.errno != errno.EBADF: raise


class Relay:
    """Private same-UID endpoint with bounded supervised dispatches."""
    def __init__(self, root, *, timeout=30, library=None):
        self.root = Path(root)
        info = self.root.lstat()
        if (os.geteuid() == 0 or self.root.resolve() != self.root or not stat.S_ISDIR(info.st_mode)
                or info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700
                or not isinstance(timeout, (int, float)) or not 1 <= timeout <= 60):
            raise ValueError('unprivileged private relay required')
        self.timeout = timeout
        self.library = str(Path(library).resolve(strict=True)) if library is not None else ''
        self.socket_path = self.root / 'relay.sock'
        if len(os.fsencode(self.socket_path)) >= 108: raise ValueError('relay socket path too long')
        self.token = os.urandom(32)
        self.completed = []
        self._stop = threading.Event()
        self._slots = threading.BoundedSemaphore(4)
        self._lock = threading.Lock()
        self._threads = []
        self._children = set()
        self._requests = 0
        self.errors = []

    def __enter__(self):
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET | socket.SOCK_CLOEXEC)
        self.listener.bind(str(self.socket_path))
        self.socket_path.chmod(0o600)
        self.listener.listen(8)
        self.listener.settimeout(.05)
        self.thread = threading.Thread(target=self._accept, daemon=True)
        self.thread.start()
        return self

    def __exit__(self, *_):
        self._stop.set()
        self.thread.join(timeout=2)
        self.listener.close()
        # Each request owns one cancellation dispatch. Repeated TERM can race
        # with interpreter teardown after the original status was committed.
        for thread in self._threads: thread.join(timeout=5)
        if self.thread.is_alive() or any(thread.is_alive() for thread in self._threads) or self._children:
            raise RuntimeError('relay cleanup remains unverified')
        self.socket_path.unlink()

    def config_fd(self):
        body = CONFIG.pack(b'JARVCFG2', os.geteuid(), os.getpid(), int(self.timeout*1000),
                           self.token, os.fsencode(self.socket_path))
        fd = os.memfd_create('jarvis-relay-config', os.MFD_ALLOW_SEALING | os.MFD_CLOEXEC)
        try:
            os.write(fd, body)
            fcntl.fcntl(fd, fcntl.F_ADD_SEALS, SEALS)
            return fd
        except BaseException:
            os.close(fd)
            raise

    def _accept(self):
        while not self._stop.is_set():
            try: connection, _ = self.listener.accept()
            except socket.timeout: continue
            self._requests += 1
            if self._requests > 4096 or not self._slots.acquire(blocking=False):
                try: _response(connection, 'error', errno.EBUSY)
                finally: connection.close()
                continue
            thread = threading.Thread(target=self._handle, args=(connection,), daemon=True)
            self._threads.append(thread)
            thread.start()

    def _receive(self, connection, descriptors):
        pid, uid, _ = struct.unpack('3i', connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if pid <= 0 or uid != os.geteuid(): raise ValueError('relay peer differs')
        data, ancillary, flags, _ = connection.recvmsg(REQUEST.size, socket.CMSG_SPACE(65*4), socket.MSG_CMSG_CLOEXEC)
        for level, kind, value in ancillary:
            if level != socket.SOL_SOCKET or kind != socket.SCM_RIGHTS: raise ValueError('invalid relay ancillary data')
            received = array.array('i')
            received.frombytes(value[:len(value) - len(value)%received.itemsize])
            descriptors.extend(received)
        if flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC) or len(data) != REQUEST.size:
            raise ValueError('incomplete relay descriptor frame')
        magic, token, count, *numbers = REQUEST.unpack(data)
        if (magic != b'JARVREQ2' or not hmac.compare_digest(token, self.token) or not 3 <= count <= 64
                or len(descriptors) != count+1 or numbers[:3] != [0, 1, 2]
                or any(numbers[count:]) or len(set(numbers[:count])) != count
                or any(number < 3 or number > 65535 for number in numbers[3:count])):
            raise ValueError('invalid relay descriptor binding')
        _payload(descriptors[0])
        if any(not stat.S_ISFIFO(os.fstat(fd).st_mode) for fd in descriptors[4:]):
            raise ValueError('unsupported inheritable relay descriptor')
        return numbers[:count]

    def _handle(self, connection):
        descriptors = []
        try:
            connection.settimeout(self.timeout)
            numbers = self._receive(connection, descriptors)
            self._run(connection, descriptors, numbers)
        except (OSError, ValueError, struct.error, RuntimeError, subprocess.TimeoutExpired):
            self.errors.append('request_or_cleanup_unverified')
            try: _response(connection, 'error', errno.EINVAL)
            except OSError: pass
        finally:
            _close(descriptors)
            connection.close()
            self._slots.release()

    def _run(self, connection, descriptors, numbers):
        directory = self.root / uuid.uuid4().hex
        directory.mkdir(mode=0o700)
        receipt = directory / 'cleanup'
        read, write = os.pipe2(os.O_CLOEXEC)
        control, child_control = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET | socket.SOCK_CLOEXEC)
        tool_pidfd = None
        exec_ready = False
        child = None
        started = False
        exec_error = None
        cancelled = False
        acknowledgement = bytearray()
        deadline = time.monotonic() + self.timeout
        cancellation_deadline = None
        signals_received = 0
        try:
            mapping = [f'{target}:{source}' for target, source in zip(numbers[3:], descriptors[4:])]
            argv = [sys.executable, '-I', str(Path(__file__).resolve()), '--worker',
                    str(descriptors[0]), str(write), str(child_control.fileno()), str(receipt), self.library, *mapping]
            child = subprocess.Popen(argv, stdin=descriptors[1], stdout=descriptors[2], stderr=descriptors[3],
                pass_fds=tuple(descriptors+[write, child_control.fileno()]), env={'PATH': os.defpath,
                    'JARVIS_SUPERVISOR_PARENT_PID': str(os.getpid())})
            with self._lock: self._children.add(child)
            os.close(write); write = None
            child_control.close()
            _close(descriptors); descriptors.clear()
            while True:
                if self._stop.is_set() or time.monotonic() >= deadline: cancelled = True
                readers = [connection] + ([] if exec_ready or exec_error is not None else [read])
                if tool_pidfd is None: readers.append(control)
                ready, _, _ = select.select(readers, [], [], .02)
                if connection in ready:
                    frame, ancillary, flags, _ = connection.recvmsg(16, socket.CMSG_SPACE(4), socket.MSG_CMSG_CLOEXEC)
                    unexpected = []
                    for level, kind, value in ancillary:
                        if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                            values = array.array('i'); values.frombytes(value[:len(value)-len(value)%4]); unexpected.extend(values)
                    _close(unexpected)
                    if ancillary or flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC):
                        raise ValueError('invalid relay signal descriptors')
                    if not frame:
                        cancelled = True
                    else:
                        if len(frame) != 16 or not started or tool_pidfd is None:
                            raise ValueError('invalid relay signal frame')
                        magic, number, reserved = struct.unpack('<8sII', frame)
                        if magic != b'JARVSIG2' or number not in FORWARDED_SIGNALS or reserved:
                            raise ValueError('unsupported relay signal')
                        signals_received += 1
                        if signals_received > 4096: raise ValueError('relay signal budget exhausted')
                        try: signal.pidfd_send_signal(tool_pidfd, number)
                        except ProcessLookupError: pass  # terminal status is still reconciled below
                if control in ready:
                    frame, ancillary, flags, _ = control.recvmsg(8, socket.CMSG_SPACE(4), socket.MSG_CMSG_CLOEXEC)
                    received = []
                    for level, kind, value in ancillary:
                        if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                            values = array.array('i'); values.frombytes(value[:len(value)-len(value)%4]); received.extend(values)
                    if frame != b'JARVPID2' or flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC) or len(received) != 1:
                        _close(received)
                        raise ValueError('original tool pidfd unavailable')
                    tool_pidfd = received[0]
                    try: signal.pidfd_send_signal(tool_pidfd, 0)
                    except ProcessLookupError: pass
                if read in ready:
                    block = os.read(read, 5-len(acknowledgement))
                    acknowledgement.extend(block)
                    if len(acknowledgement) > 4:
                        exec_error = errno.EIO
                        cancelled = True
                    elif not block:
                        if acknowledgement:
                            exec_error = struct.unpack('<I', acknowledgement)[0] if len(acknowledgement) == 4 else errno.EIO
                        else:
                            exec_ready = True
                if exec_ready and tool_pidfd is not None and not started:
                    if not cancelled: _response(connection, 'started', 0)
                    started = True
                if cancelled and child.poll() is None:
                    if cancellation_deadline is None:
                        cancellation_deadline = time.monotonic()+4
                        child.terminate()
                    if time.monotonic() >= cancellation_deadline:
                        raise RuntimeError('relay descendant cleanup remains unverified')
                status = child.poll()
                # Drain the exec-start pipe even when a short-lived failed exec
                # has already exited, so ENOENT cannot race into generic EIO.
                if status is not None and (started or exec_error is not None or cancelled): break
            info = receipt.lstat()
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_nlink != 1
                    or stat.S_IMODE(info.st_mode) != 0o600 or receipt.read_bytes() != b'all-descendants-reaped\n'):
                raise ValueError('relay descendant cleanup unverified')
            status_path = directory / 'exit-status'
            info = status_path.lstat()
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_nlink != 1
                    or stat.S_IMODE(info.st_mode) != 0o600 or info.st_size > 128):
                raise ValueError('original tool exit status unavailable')
            recorded = json.loads(status_path.read_bytes())
            if (not isinstance(recorded, dict) or set(recorded) != {'version', 'returncode'}
                    or type(recorded['version']) is not int or recorded['version'] != 1
                    or type(recorded['returncode']) is not int or not -64 <= recorded['returncode'] <= 255):
                raise ValueError('invalid original tool exit status')
            raw_status = recorded['returncode']
            if status != (raw_status if raw_status >= 0 else 128-raw_status):
                raise ValueError('original supervisor status mismatch')
            self.completed.append({'cleanup': 'all-descendants-reaped', 'exit_code': raw_status,
                'cargo_metadata_isolation_established': False, 'shipping_authority': False})
            if not cancelled:
                if exec_error is not None: _response(connection, 'error', exec_error)
                elif started:
                    _response(connection, 'signaled' if raw_status < 0 else 'finished', abs(raw_status))
                else: _response(connection, 'error', errno.EIO)
        finally:
            if child is not None:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=4)
                with self._lock: self._children.discard(child)
            _close([read] + ([] if write is None else [write]))
            control.close(); child_control.close()
            if tool_pidfd is not None: os.close(tool_pidfd)


def _worker(arguments):
    if os.geteuid() == 0: raise ValueError('unprivileged relay worker required')
    payload, acknowledgement, control, receipt, library, *mapping = arguments
    payload, acknowledgement, control = int(payload), int(acknowledgement), int(control)
    _payload(payload)  # Validate before any tool can run or an FD is remapped.
    mapping = [tuple(map(int, item.split(':'))) for item in mapping]
    floor = max([payload, acknowledgement, control, *(number for pair in mapping for number in pair)]) + 1
    saved = [(target, fcntl.fcntl(source, fcntl.F_DUPFD_CLOEXEC, floor)) for target, source in mapping]
    data = fcntl.fcntl(payload, fcntl.F_DUPFD_CLOEXEC, floor)
    ack = fcntl.fcntl(acknowledgement, fcntl.F_DUPFD_CLOEXEC, floor)
    channel = fcntl.fcntl(control, fcntl.F_DUPFD_CLOEXEC, floor)
    _close(set([payload, acknowledgement, control, *(source for _, source in mapping)]))
    for target, source in saved:
        os.dup2(source, target, inheritable=True)
        os.close(source)
    spec = importlib.util.spec_from_file_location('relay_supervisor', BASE / 'provider-supervisor.py')
    supervisor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(supervisor)
    def spawned(child):
        descriptor = os.pidfd_open(child.pid)
        try:
            with socket.socket(fileno=channel) as endpoint:
                endpoint.sendmsg([b'JARVPID2'], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array('i', [descriptor]))])
        finally: os.close(descriptor)
    status = supervisor.supervise(receipt,
        [sys.executable, '-I', str(Path(__file__).resolve()), '--exec', str(data), str(ack), library],
        forward_fds=tuple([target for target, _ in mapping]+[data, ack]), close_after_spawn=(data, ack),
        return_raw_status=True, on_spawn=spawned)
    descriptor = os.open(Path(receipt).with_name('exit-status'), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as output:
        output.write(json.dumps({'version': 1, 'returncode': status}))
        output.flush(); os.fsync(output.fileno())
    return status if status >= 0 else 128-status


def _execute(arguments):
    if os.geteuid() == 0: raise ValueError('unprivileged relay exec required')
    payload, ack, library = arguments
    payload, ack = int(payload), int(ack)
    os.set_inheritable(ack, False)
    try:
        file, argv, environment, cwd, search, ignored, blocked = _payload(payload)
        os.close(payload)
        # The payload is data, never the environment of a controller interpreter.
        environment.pop(b'JARVIS_CARGO_RELAY_FD', None)
        if library and b'LD_PRELOAD' in environment:
            values = environment[b'LD_PRELOAD'].replace(b':', b' ').split()
            values = [value for value in values if value != os.fsencode(library)]
            if values: environment[b'LD_PRELOAD'] = b' '.join(values)
            else: environment.pop(b'LD_PRELOAD')
        os.chdir(cwd)
        for number in FORWARDED_SIGNALS:
            signal.signal(number, signal.SIG_IGN if ignored & (1 << (number-1)) else signal.SIG_DFL)
        signal.pthread_sigmask(signal.SIG_SETMASK, [number for number in FORWARDED_SIGNALS if blocked & (1 << (number-1))])
        if search is None or b'/' in file:
            os.execve(file, argv, environment)
        denied = False
        for entry in search.split(b':'):
            try: os.execve(os.path.join(entry or b'.', file), argv, environment)
            except OSError as error:
                if error.errno == errno.EACCES: denied = True
                elif error.errno not in (errno.ENOENT, errno.ENOTDIR): raise
        raise OSError(errno.EACCES if denied else errno.ENOENT, 'tool unavailable')
    except BaseException as error:
        number = error.errno if isinstance(error, OSError) and type(error.errno) is int and 1 <= error.errno <= 4095 else errno.EINVAL
        os.write(ack, struct.pack('<I', number))
        return 127


if __name__ == '__main__':
    try:
        if sys.argv[1:2] == ['--worker']: code = _worker(sys.argv[2:])
        elif sys.argv[1:2] == ['--exec']: code = _execute(sys.argv[2:])
        else: raise ValueError('no public relay launcher')
    except BaseException:
        code = 125
    raise SystemExit(code)
