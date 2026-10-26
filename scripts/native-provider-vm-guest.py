#!/usr/bin/env python3
"""Trusted guest supervisor; worker never receives the host control channel."""
import base64
import ctypes
import json
import os
import queue
from pathlib import Path
import resource
import shutil
import socket
import stat
import subprocess
import sys
import time
import threading

MAX_FRAME = 2 * 1024 * 1024
MAX_HOME = 64 * 1024 * 1024



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



def send_frame(output, lock, frame):
    data = json.dumps(frame, separators=(',', ':')).encode() + b'\n'
    if len(data) > MAX_FRAME:
        raise ValueError('oversized guest frame')
    with lock:
        remaining = memoryview(data)
        while remaining:
            written = output.write(remaining)
            if not isinstance(written, int) or written <= 0:
                raise RuntimeError('guest control channel closed during frame')
            remaining = remaining[written:]


def worker(job):
    uid, gid = job['uid'], job['gid']
    if uid <= 0 or gid <= 0:
        raise ValueError('worker must be unprivileged')
    if ctypes.CDLL(None).prctl(38, 1, 0, 0, 0):
        raise RuntimeError('no-new-privileges unavailable')
    os.setgroups([])
    os.setgid(gid)
    os.setuid(uid)
    os.umask(0o077)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_NPROC, (128, 128))
    resource.setrlimit(resource.RLIMIT_NOFILE, (256, 256))
    resource.setrlimit(resource.RLIMIT_FSIZE, (MAX_HOME, MAX_HOME))
    os.chdir('/workspace')
    os.execve('/native/cli', ['/native/cli', *job['argv']], job['environment'])


def channel():
    for port in Path('/sys/class/virtio-ports').iterdir():
        if (port / 'name').read_text().strip() == 'org.jarvis.native.control':
            fd = os.open('/dev/' + port.name, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
            info = os.fstat(fd)
            if not stat.S_ISCHR(info.st_mode) or info.st_uid != 0:
                os.close(fd)
                raise RuntimeError('invalid controller channel')
            os.fchmod(fd, 0o600)
            return fd
    raise RuntimeError('controller channel unavailable')


def worker_pids(uid, proc=Path('/proc')):
    result = []
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            # /proc/PID inode ownership changes when a worker disables dumpable;
            # the kernel's Uid status fields retain its actual credentials.
            for line in (entry / 'status').read_text().splitlines():
                if line.startswith('Uid:'):
                    if uid in [int(value) for value in line.split()[1:]]:
                        result.append(int(entry.name))
                    break
        except (FileNotFoundError, ProcessLookupError):
            continue
    return result


def quiesce_worker(uid, timeout=5, enumerate_pids=None, kill=None, reap=None):
    if uid <= 0:
        raise ValueError('invalid worker identity')
    enumerate_pids = enumerate_pids or (lambda: worker_pids(uid))
    kill = kill or (lambda pid: os.kill(pid, 9))
    def reap_children():
        count = 0
        while True:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                return count
            if pid == 0:
                return count
            count += 1
    reap = reap or reap_children
    deadline = time.monotonic() + timeout
    quiet = 0
    scans, reaped, signalled = 0, 0, set()
    while time.monotonic() < deadline:
        scans += 1
        pids = enumerate_pids()
        for pid in pids:
            try:
                kill(pid)
                signalled.add(pid)
            except ProcessLookupError:
                pass
        reaped += reap() or 0
        if not pids:
            quiet += 1
            if quiet >= 2:
                return {'scans': scans, 'signalled': len(signalled), 'reaped': reaped,
                        'remaining_workers': 0, 'quiet_scans': quiet}
        else:
            quiet = 0
        time.sleep(0.01)
    raise RuntimeError('native descendants did not quiesce; runtime export refused')


def export_home(send):
    total = 0
    for root, directories, files in os.walk('/home/worker', followlinks=False):
        for name in directories:
            if (Path(root) / name).is_symlink():
                error = ValueError('runtime directory symlink')
                error.export_kind = 'directory_symlink'
                error.export_area = 'codex_arg0' if str(Path(root) / name).startswith('/home/worker/.codex/tmp/arg0/') else 'other'
                raise error
        for name in files:
            path = Path(root) / name
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            except OSError as error:
                error.export_kind = 'symlink' if path.is_symlink() else 'open_failure'
                error.export_area = 'codex_arg0' if str(path).startswith('/home/worker/.codex/tmp/arg0/') else 'other'
                raise
            with os.fdopen(fd, 'rb') as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    raise ValueError('runtime file is not regular')
                total += info.st_size
                if total > MAX_HOME:
                    raise ValueError('runtime export exceeds budget')
                relative = str(path.relative_to('/home/worker'))
                send({'type': 'runtime_start', 'path': relative, 'size': info.st_size})
                offset = 0
                while data := stream.read(65536):
                    send({'type': 'runtime_data', 'offset': offset,
                          'data': base64.b64encode(data).decode()})
                    offset += len(data)
                if offset != info.st_size:
                    raise ValueError('runtime changed during export')
                send({'type': 'runtime_end'})


def main():
    sys.path.insert(0, '/root')
    from native_provider_gateway import GuestGateway
    job = json.loads(Path('/root/job.json').read_text())
    if ctypes.CDLL(None).prctl(36, 1, 0, 0, 0):
        raise RuntimeError('guest child subreaper unavailable')
    fd = channel()
    output = os.fdopen(os.dup(fd), 'wb', buffering=0)
    lock = threading.Lock()

    def send(frame):
        send_frame(output, lock, frame)

    uid, gid = job['uid'], job['gid']
    if uid <= 0 or gid <= 0:
        raise ValueError('invalid worker identity')
    shutil.copytree('/seed', '/home/worker', dirs_exist_ok=True)
    for root, directories, files in os.walk('/home/worker'):
        os.chown(root, uid, gid)
        os.chmod(root, 0o700)
        for name in files:
            os.chown(Path(root) / name, uid, gid)
    os.chown('/workspace', uid, gid)
    Path('/etc/passwd').write_text(f'root:x:0:0:root:/root:/bin/sh\nworker:x:{uid}:{gid}:worker:/home/worker:/bin/sh\n')
    Path('/etc/group').write_text(f'root:x:0:\nworker:x:{gid}:\n')
    gateway = GuestGateway(job['gateway'], send)
    proxy = gateway.start()
    job['environment'].update({'HOME': '/home/worker', 'PATH': '/usr/bin:/bin',
        'TMPDIR': '/tmp', 'CODEX_HOME': '/home/worker/.codex',
        'CLAUDE_CONFIG_DIR': '/home/worker/.claude', 'HTTP_PROXY': proxy,
        'HTTPS_PROXY': proxy, 'http_proxy': proxy, 'https_proxy': proxy,
        'NO_PROXY': '', 'no_proxy': '', 'ALL_PROXY': '', 'all_proxy': '', 'SSL_CERT_FILE': job['gateway']['ca_file'],
        'NODE_EXTRA_CA_CERTS': job['gateway']['ca_file']})
    # One relay connection is an untrusted byte stream to the original host
    # policy bridge, not access to the gateway/control frame parser.
    relay = socket.socket(socket.AF_UNIX)
    relay.bind('/tmp/native-mcp.sock')
    os.chown('/tmp/native-mcp.sock', uid, gid)
    os.chmod('/tmp/native-mcp.sock', 0o600)
    relay.listen(1)
    relay_connection = []
    relay_ready = threading.Event()
    def relay_input():
        connection, _ = relay.accept()
        relay_connection.append(connection)
        relay_ready.set()
        while data := connection.recv(65536):
            send({'type': 'mcp_input', 'data': base64.b64encode(data).decode()})
    if job['mcp']:
        threading.Thread(target=relay_input, daemon=True).start()
    started = threading.Event()
    child_ready = threading.Event()
    child = []
    def failed_stream():
        os._exit(126)
    def stdin_write(data):
        if not child_ready.wait(60):
            raise RuntimeError('native child unavailable')
        child[0].stdin.write(data)
        child[0].stdin.flush()
    def stdin_finish():
        if not child_ready.wait(60):
            raise RuntimeError('native child unavailable')
        child[0].stdin.close()
    stdin_writer = BoundedWriter(stdin_write, stdin_finish, failed_stream)
    def mcp_write(data):
        if not relay_ready.wait(30):
            raise RuntimeError('native tool relay unavailable')
        relay_connection[0].sendall(data)
    def mcp_finish():
        if not relay_ready.wait(30):
            raise RuntimeError('native tool relay unavailable')
        relay_connection[0].shutdown(socket.SHUT_WR)
    mcp_writer = BoundedWriter(mcp_write, mcp_finish, failed_stream) if job['mcp'] else None
    def receive():
        with os.fdopen(fd, 'rb') as stream:
            for raw in iter(lambda: stream.readline(MAX_FRAME + 1), b''):
                if len(raw) > MAX_FRAME or not raw.endswith(b'\n'):
                    os._exit(126)
                frame = json.loads(raw)
                kind = frame['type']
                if kind == 'start' and not started.is_set():
                    started.set()
                elif kind.startswith('gateway_'):
                    gateway.receive(frame)
                elif kind in ('stdin', 'stdin_end'):
                    try:
                        stdin_writer.submit(base64.b64decode(frame['data'], validate=True) if kind == 'stdin' else None)
                    except Exception:
                        os._exit(126)
                elif kind in ('mcp_output', 'mcp_end') and job['mcp']:
                    try:
                        mcp_writer.submit(base64.b64decode(frame['data'], validate=True) if kind == 'mcp_output' else None)
                    except Exception:
                        os._exit(126)
                else:
                    os._exit(126)
        os._exit(126)
    threading.Thread(target=receive, daemon=True).start()
    send({'type': 'ready'})
    if not started.wait(60):
        raise RuntimeError('controller admission not received')
    # Separate root helper drops privileges before exec; no preexec_fn in this
    # threaded supervisor and all control descriptors remain close-on-exec.
    Path('/root/worker.json').write_text(json.dumps(job))
    process = subprocess.Popen(['/usr/bin/python3', '-I', '-S', '/root/guest.py', '--worker'],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    child.append(process)
    child_ready.set()
    def copy(stream, kind):
        while data := stream.read1(65536):
            send({'type': kind, 'data': base64.b64encode(data).decode()})
    threads = [threading.Thread(target=copy, args=(process.stdout, 'stdout')),
               threading.Thread(target=copy, args=(process.stderr, 'stderr'))]
    for thread in threads:
        thread.start()
    code = process.wait()
    complete_worker(send, threads, uid, code)


def complete_worker(send, threads, uid, code):
    phase = 'quiescence'
    try:
        quiescence = quiesce_worker(uid)
        send(dict(type='worker_quiescent', **quiescence))
        phase = 'output_drain'
        for thread in threads:
            thread.join(timeout=5)
            if thread.is_alive():
                raise RuntimeError('native output did not close')
        phase = 'runtime_export'
        export_home(send)
        phase = 'exit_receipt'
        send({'type': 'exit', 'code': code if code >= 0 else 128 - code})
    except Exception as error:
        category = type(error).__name__
        if category not in ('ValueError', 'RuntimeError', 'OSError', 'PermissionError', 'FileNotFoundError'):
            category = 'BoundaryFailure'
        diagnostic = {'type': 'completion_failure', 'phase': phase, 'category': category}
        if isinstance(error, OSError) and isinstance(error.errno, int):
            diagnostic['errno'] = error.errno
        if getattr(error, 'export_kind', None) in ('symlink', 'directory_symlink', 'open_failure'):
            diagnostic['export_kind'] = error.export_kind
            diagnostic['export_area'] = error.export_area
        try:
            send(diagnostic)
        except Exception:
            pass
        raise


if __name__ == '__main__':
    if sys.argv[1:] == ['--worker']:
        worker(json.loads(Path('/root/worker.json').read_text()))
    else:
        try:
            main()
        except BaseException:
            os._exit(126)
