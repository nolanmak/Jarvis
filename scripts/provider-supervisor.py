#!/usr/bin/env python3
"""Own and reap one provider tree, including descendants that call setsid."""
import ctypes
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def write_receipt(receipt):
    descriptor = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        stream.write('all-descendants-reaped\n')
        stream.flush()
        os.fsync(stream.fileno())


def supervise_linux(receipt, argv):
    libc = ctypes.CDLL(None, use_errno=True)
    stopping = False

    def stop(signum, frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    parent = os.getppid()
    # Orphaned grandchildren are reparented here, not to init, so detached
    # sessions remain enumerable and reapable by this invocation alone.
    if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
        raise RuntimeError('provider supervision requires Linux subreaper support')
    if libc.prctl(1, signal.SIGTERM, 0, 0, 0) != 0 or os.getppid() != parent:
        raise RuntimeError('provider supervision lost its parent')
    children_path = Path(f'/proc/self/task/{os.getpid()}/children')
    children_path.read_text()  # readiness before any provider can run
    child = subprocess.Popen(argv, start_new_session=True)
    status = None
    try:
        while not stopping:
            try:
                status = child.wait(timeout=0.05)
                break
            except subprocess.TimeoutExpired:
                pass
    finally:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 2
        while True:
            # Each killed parent's remaining descendants are adopted here.
            # Iterate until waitpid proves there are no children left.
            for raw in children_path.read_text().split():
                try:
                    os.kill(int(raw), signal.SIGKILL)
                except ProcessLookupError:
                    pass
            while True:
                try:
                    pid, _ = os.waitpid(-1, os.WNOHANG)
                except ChildProcessError:
                    write_receipt(receipt)
                    return 128 + signal.SIGTERM if stopping else (status if status is not None and status >= 0 else 1)
                if pid == 0:
                    break
            if time.monotonic() >= deadline:
                raise RuntimeError('provider descendant cleanup did not complete')
            time.sleep(0.005)


class DarwinProcesses:
    """Kernel coalition membership survives fork, setsid and reparenting.

    launchd assigns a private resource coalition to each submitted job. Unlike
    parent/group polling, its membership includes a double-forked process even
    when both intermediate parents exited before the first observation.
    """
    def __init__(self):
        self.lib = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
        self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int,
                                         ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_listpids.argtypes = [ctypes.c_uint32, ctypes.c_uint32,
                                          ctypes.c_void_p, ctypes.c_int]

    def info(self, pid, flavor, size):
        import errno
        buffer = ctypes.create_string_buffer(size)
        ctypes.set_errno(0)
        count = self.lib.proc_pidinfo(pid, flavor, 0, buffer, size)
        if count == size:
            return buffer.raw
        if count == 0 and ctypes.get_errno() == errno.ESRCH:
            return None
        raise RuntimeError(f'macOS process inspection failed (flavor={flavor}, count={count}, errno={ctypes.get_errno()})')

    def coalition(self, pid):
        import struct
        raw = self.info(pid, 20, 40)  # PROC_PIDCOALITIONINFO, five uint64 fields
        return struct.unpack_from('=Q', raw)[0] if raw is not None else None

    def identity(self, pid):
        import struct
        raw = self.info(pid, 17, 56)  # PROC_PIDUNIQIDENTIFIERINFO
        return struct.unpack_from('=Q', raw, 16)[0] if raw is not None else None

    def members(self, coalition):
        # PROC_UID_ONLY: foreign users cannot be members of this unprivileged
        # job, and need not grant this process permission to inspect them.
        needed = self.lib.proc_listpids(4, os.getuid(), None, 0)
        if needed <= 0:
            raise RuntimeError('macOS process enumeration failed')
        while True:
            capacity = needed + 4096
            buffer = ctypes.create_string_buffer(capacity)
            count = self.lib.proc_listpids(4, os.getuid(), buffer, capacity)
            if count <= 0:
                raise RuntimeError('macOS process enumeration failed')
            if count < capacity:
                break
            needed = capacity * 2
        import struct
        found = {}
        for (pid,) in struct.iter_unpack('=i', buffer.raw[:count]):
            if pid <= 0 or pid == os.getpid():
                continue
            identity = self.identity(pid)
            if identity is not None and self.coalition(pid) == coalition:
                # Recheck after membership lookup to reject a reused PID.
                if self.identity(pid) == identity:
                    found[pid] = identity
        return found

    def cleanup(self, coalition, child):
        deadline = time.monotonic() + 2
        while True:
            members = self.members(coalition)
            for pid, identity in members.items():
                if self.identity(pid) == identity and self.coalition(pid) == coalition:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            child.poll()  # reap our direct child; launchd reaps orphans
            if not self.members(coalition):
                return
            if time.monotonic() >= deadline:
                raise RuntimeError('macOS provider coalition cleanup did not complete')
            time.sleep(.005)


def darwin_worker(socket_path, label):
    import array
    import json
    import socket
    control = socket.socket(socket.AF_UNIX)
    control.connect(socket_path)
    processes = DarwinProcesses()
    coalition = processes.coalition(os.getpid())
    control.sendall((json.dumps({'pid': os.getpid(), 'coalition': coalition}) + '\n').encode())
    data, ancillary, flags, address = control.recvmsg(1, socket.CMSG_SPACE(3 * array.array('i').itemsize))
    descriptors = array.array('i')
    for level, kind, value in ancillary:
        if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
            descriptors.frombytes(value)
    if data != b'F' or len(descriptors) != 3 or flags & socket.MSG_CTRUNC:
        raise RuntimeError('macOS provider stdio transfer failed')
    with control.makefile('rb') as reader:
        config = json.loads(reader.readline(4 * 1024 * 1024))
    os.chdir(config['cwd'])
    child = subprocess.Popen(config['argv'], env=config['env'],
                             stdin=descriptors[0], stdout=descriptors[1], stderr=descriptors[2],
                             start_new_session=True)
    for descriptor in descriptors:
        os.close(descriptor)
    stopping = False
    def stop(signum, frame):
        nonlocal stopping
        stopping = True
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    control.settimeout(.05)
    disconnected = False
    try:
        while child.poll() is None and not stopping:
            try:
                message = control.recv(1)
                disconnected = not message
                stopping = True
            except socket.timeout:
                pass
    finally:
        processes.cleanup(coalition, child)
    status = 143 if stopping else (child.returncode if child.returncode >= 0 else 1)
    try:
        control.sendall((str(status) + '\n').encode())
    except (BrokenPipeError, ConnectionResetError):
        disconnected = True
    finally:
        control.close()
    if disconnected:
        # The controller was killed. Remove this transient job after cleanup;
        # its missing receipt deliberately keeps the request fail-closed.
        subprocess.run(['/bin/launchctl', 'bootout', f'gui/{os.getuid()}/{label}'],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
    return status


def supervise_darwin(receipt, argv):
    import array
    import json
    import plistlib
    import socket
    import tempfile
    import uuid
    parent = os.getppid()
    stopping = False
    def stop(signum, frame):
        nonlocal stopping
        stopping = True
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    # Short private socket paths also work when macOS TMPDIR is very long.
    with tempfile.TemporaryDirectory(prefix='aa-provider-', dir='/tmp') as root:
        path = str(Path(root) / 'control.sock')
        label = 'com.nolanmak.augmentagent.provider.' + uuid.uuid4().hex
        target = f'gui/{os.getuid()}/{label}'
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(path)
        listener.listen(1)
        listener.settimeout(10)
        plist = Path(root) / 'job.plist'
        plist.write_bytes(plistlib.dumps({
            'Label': label, 'ProgramArguments': [sys.executable, '-I', str(Path(__file__).resolve()),
                                                '--darwin-worker', path, label],
            'RunAtLoad': True, 'ProcessType': 'Background',
            'StandardErrorPath': str(Path(root) / 'worker-error'),
        }))
        control = None
        try:
            result = subprocess.run(['/bin/launchctl', 'bootstrap', f'gui/{os.getuid()}', str(plist)],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            if result.returncode:
                raise RuntimeError('macOS provider supervision requires a logged-in launchd GUI session')
            control, _ = listener.accept()
            control.settimeout(10)
            with control.makefile('rb') as reader:
                hello = json.loads(reader.readline(4096))
            processes = DarwinProcesses()
            coalition = processes.coalition(hello['pid'])
            if not coalition or coalition != hello['coalition'] or coalition == processes.coalition(os.getpid()):
                raise RuntimeError('launchd did not isolate the provider coalition')
            control.sendmsg([b'F'], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array('i', [0, 1, 2]))])
            control.sendall((json.dumps({'argv': argv, 'cwd': os.getcwd(), 'env': dict(os.environ)}) + '\n').encode())
            control.settimeout(.05)
            reply = b''
            sent_stop = False
            while b'\n' not in reply:
                if (stopping or os.getppid() != parent) and not sent_stop:
                    control.sendall(b'S')
                    sent_stop = True
                try:
                    chunk = control.recv(64)
                    if not chunk:
                        raise RuntimeError('macOS provider cleanup is unverified')
                    reply += chunk
                except socket.timeout:
                    pass
            status = int(reply.strip())
            write_receipt(receipt)
            return status
        finally:
            if not Path(receipt).exists():
                diagnostic = Path(root) / 'worker-error'
                if diagnostic.is_file():
                    # Worker emits fixed diagnostics only, never argv or env.
                    sys.stderr.write(diagnostic.read_text()[:1024])
            if control is not None:
                control.close()
            listener.close()
            subprocess.run(['/bin/launchctl', 'bootout', target], stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=5)


if __name__ == '__main__':
    try:
        if sys.argv[1] == '--darwin-worker':
            sys.exit(darwin_worker(sys.argv[2], sys.argv[3]))
        supervise = supervise_darwin if sys.platform == 'darwin' else supervise_linux
        sys.exit(supervise(sys.argv[1], sys.argv[2:]))
    except Exception as error:
        detail = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        print(f'Provider supervision failed; cleanup is unverified: {detail}.', file=sys.stderr)
        sys.exit(125)
