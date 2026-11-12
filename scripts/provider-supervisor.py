#!/usr/bin/env python3
"""Own and reap one provider tree, including descendants that call setsid."""
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import selectors
import signal
import stat
import subprocess
import sys
import time


class RawCapture:
    """Controller-owned output, outside the sandbox's writable roots."""
    def __init__(self, directory):
        self.directory = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        info = os.fstat(self.directory)
        if info.st_uid != os.geteuid() or info.st_mode & 0o077:
            raise RuntimeError('raw capture directory is not private')
        descriptor = os.open('request.json', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=self.directory)
        with os.fdopen(descriptor, 'rb') as request:
            info = os.fstat(request.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_nlink != 1 or info.st_mode & 0o077 or info.st_size > 4096:
                raise RuntimeError('invalid raw capture request')
            value = json.loads(request.read(4097))
        if set(value) != {'version', 'command', 'output_limit'} or value['version'] != 1 or not isinstance(value['command'], str) or len(value['command']) != 64 or any(c not in '0123456789abcdef' for c in value['command']):
            raise RuntimeError('invalid raw capture identity')
        if type(value['output_limit']) is not int or not 1 <= value['output_limit'] <= 8 * 1024 * 1024:
            raise RuntimeError('invalid raw capture bound')
        self.command = value['command']
        self.limit = value['output_limit']
        self.selector = selectors.DefaultSelector()
        self.outputs = {}
        self.sizes = {}
        self.hashes = {}
        self.limited = False
        for name in ('stdout', 'stderr'):
            descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=self.directory)
            self.outputs[name] = os.fdopen(descriptor, 'wb')
            self.sizes[name] = 0
            self.hashes[name] = hashlib.sha256()

    def attach(self, child):
        for name in ('stdout', 'stderr'):
            stream = getattr(child, name)
            os.set_blocking(stream.fileno(), False)
            self.selector.register(stream, selectors.EVENT_READ, name)

    def drain(self, timeout=0):
        for key, _ in self.selector.select(timeout):
            try:
                data = os.read(key.fd, 65536)
            except BlockingIOError:
                continue
            if not data:
                self.selector.unregister(key.fileobj)
                key.fileobj.close()
                continue
            name = key.data
            remaining = self.limit - self.sizes[name]
            if len(data) > remaining:
                self.limited = True
            data = data[:remaining]
            self.outputs[name].write(data)
            self.sizes[name] += len(data)
            self.hashes[name].update(data)

    def finish(self, status, stopping, forced, observation, cause=None):
        # Called only after waitpid proves every descendant was reaped. Drain
        # the actual pipes to EOF; loss/overflow never becomes behavioral red.
        deadline = time.monotonic() + 1
        while self.selector.get_map() and time.monotonic() < deadline:
            self.drain(0.01)
        complete = not self.selector.get_map() and not self.limited
        interrupted = stopping or forced or status is None or status < 0 or not complete or observation.get('exhausted', False)
        value = {'version': 1, 'command': self.command, 'identity': observation.get('identity'),
                 'exit_code': None if interrupted else status, 'interrupted': interrupted,
                 'output_complete': complete,
                 'terminal_cause': cause or ('output_limit' if self.limited else 'resource_exhausted' if observation.get('exhausted', False) else 'controller_stopped' if stopping else 'descendants_killed' if forced else 'signalled' if status is None or status < 0 else 'completed')}
        for name, stream in self.outputs.items():
            stream.flush()
            os.fsync(stream.fileno())
            stream.close()
            value[name + '_bytes'] = self.sizes[name]
            value[name + '_sha256'] = self.hashes[name].hexdigest()
        descriptor = os.open('result.pending', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=self.directory)
        with os.fdopen(descriptor, 'wb') as result:
            result.write(json.dumps(value, sort_keys=True).encode())
            result.flush()
            os.fsync(result.fileno())
        os.link('result.pending', 'result.json', src_dir_fd=self.directory, dst_dir_fd=self.directory, follow_symlinks=False)
        os.unlink('result.pending', dir_fd=self.directory)
        os.fsync(self.directory)
        self.selector.close()
        os.close(self.directory)


def supervise(receipt, argv):
    libc = ctypes.CDLL(None, use_errno=True)
    stopping = False

    def stop(signum, frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    parent = os.getppid()
    expected_parent = os.environ.get('JARVIS_SUPERVISOR_PARENT_PID')
    if expected_parent is not None and parent != int(expected_parent):
        raise RuntimeError('provider supervision lost its originating parent')
    capture_path = os.environ.pop('JARVIS_RAW_VERIFIER_CAPTURE', None)
    if capture_path and (os.environ.get('JARVIS_SUPERVISOR_ADMISSION') != '1' or os.environ.get('JARVIS_SUPERVISOR_VM_PREFLIGHT') == '1'):
        raise RuntimeError('raw capture requires a non-VM admitted command')
    capture = RawCapture(capture_path) if capture_path else None
    # Orphaned grandchildren are reparented here, not to init, so detached
    # sessions remain enumerable and reapable by this invocation alone.
    if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
        raise RuntimeError('provider supervision requires Linux subreaper support')
    if libc.prctl(1, signal.SIGTERM, 0, 0, 0) != 0 or os.getppid() != parent:
        raise RuntimeError('provider supervision lost its parent')
    children_path = Path(f'/proc/self/task/{os.getpid()}/children')
    children_path.read_text()  # readiness before any provider can run
    resource_path = Path(receipt).with_name('resource-accounting.json')
    scope = None
    if 'JARVIS_RUN_RESOURCE_POLICY' in os.environ:
        module = None
        try:
            spec = importlib.util.spec_from_file_location('run_resources', Path(__file__).with_name('run-resource-scope.py'))
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            scope = module.declared_scope()
            resource_path.write_text(json.dumps({'phase':'admitted', **scope.observation()}))
            resource_path.chmod(0o600)
        except BaseException as error:
            category=type(error).__name__
            if category not in {'RuntimeError','ValueError','TypeError','JSONDecodeError','OSError','FileNotFoundError','PermissionError','BlockingIOError','NotADirectoryError','TimeoutError'}:
                category='BoundaryFailure'
            refusal = {'phase':'admission_failed','exhausted':True,'refusal_code':'admission_setup_failed','failure_class':category}
            if isinstance(error,OSError) and type(error.errno) is int and 0<=error.errno<=4095:
                refusal['errno']=error.errno
            if module is not None and hasattr(module,'refusal_receipt'):
                refusal = module.refusal_receipt(error)
            resource_path.write_text(json.dumps(refusal))
            resource_path.chmod(0o600)
            Path(receipt).write_text('all-descendants-reaped\n')
            Path(receipt).chmod(0o600)
            raise
    vm_preflight = os.environ.pop('JARVIS_SUPERVISOR_VM_PREFLIGHT', None) == '1'
    admission = os.environ.pop('JARVIS_SUPERVISOR_ADMISSION', None) == '1'
    if vm_preflight and not admission:
        raise RuntimeError('VM preflight requires controller admission')
    if admission and not vm_preflight:
        ready = Path(receipt).with_name('launch-ready')
        proceed = Path(receipt).with_name('launch-continue')
        ready.write_text('ready\n')
        ready.chmod(0o600)
        deadline = time.monotonic() + 10
        expected = b'admitted\n'
        while True:
            if stopping or time.monotonic() >= deadline:
                observation = scope.observation() if scope else {}
                if scope:
                    resource_path.write_text(json.dumps({'phase':'cancelled_before_dispatch', **observation}))
                    scope.close()
                Path(receipt).write_text('all-descendants-reaped\n')
                Path(receipt).chmod(0o600)
                if capture:
                    capture.finish(None, True, False, observation, 'cancelled_before_dispatch')
                return 125
            try:
                with proceed.open('rb') as stream:
                    published = stream.read(len(expected)+1)
            except FileNotFoundError:
                published = b''
            if published == expected:
                break
            # Older controllers create the inode before writing its bytes.
            # A visible empty/prefix file is still publication in progress,
            # never authority to dispatch. Malformed content still refuses.
            if not expected.startswith(published):
                raise RuntimeError('invalid controller launch admission')
            time.sleep(0.005)
    if vm_preflight:
        os.environ['JARVIS_NATIVE_VM_READY'] = str(Path(receipt).with_name('vm-ready'))
        os.environ['JARVIS_NATIVE_VM_CONTINUE'] = str(Path(receipt).with_name('launch-continue'))
    else:
        os.environ.pop('JARVIS_NATIVE_VM_READY', None)
        os.environ.pop('JARVIS_NATIVE_VM_CONTINUE', None)
    try:
        # Trusted build broker carries the same flock open description through
        # supervisor and QEMU. A killed broker must not release build storage
        # before this subtree has actually exited.
        storage_fd = os.environ.get('JARVIS_STORAGE_LEASE_FD')
        listed = os.environ.get('JARVIS_STORAGE_LEASE_FDS')
        storage_fds = tuple(int(value) for value in listed.split(',')) if listed else (() if storage_fd is None else (int(storage_fd),))
        if len(storage_fds) > 64 or (listed and (not storage_fds or str(storage_fds[0]) != storage_fd)) or any(fd < 3 for fd in storage_fds):
            raise RuntimeError('invalid inherited storage descriptor set')
        for descriptor in storage_fds:
            os.fstat(descriptor)  # fail before launch on a missing inherited FD
        child = subprocess.Popen(argv, start_new_session=True,
                                 pass_fds=storage_fds,
                                 preexec_fn=scope.attach_child if scope else None,
                                 **({'stdout': subprocess.PIPE, 'stderr': subprocess.PIPE} if capture else {}))
        if capture:
            capture.attach(child)
    except BaseException:
        failed_observation = scope.observation() if scope else {}
        if scope:
            resource_path.write_text(json.dumps({'phase':'launch_failed', **failed_observation}))
            scope.close()
        Path(receipt).write_text('all-descendants-reaped\n')
        Path(receipt).chmod(0o600)
        if capture:
            capture.finish(None, True, False, failed_observation, 'launch_failed')
        raise
    status = None
    try:
        if vm_preflight:
            vm_ready = Path(receipt).with_name('vm-ready')
            deadline = time.monotonic() + 60
            expected = b'ready\n'
            while True:
                if stopping or child.poll() is not None or time.monotonic() >= deadline:
                    return 125
                try:
                    with vm_ready.open('rb') as stream:
                        published = stream.read(len(expected) + 1)
                except FileNotFoundError:
                    published = b''
                if published == expected:
                    break
                # The VM launcher may create the inode before publishing its
                # bytes. A visible prefix is still pending, never readiness.
                if not expected.startswith(published):
                    raise RuntimeError('invalid VM readiness')
                time.sleep(0.005)
            ready = Path(receipt).with_name('launch-ready')
            ready.write_text('ready\n')
            ready.chmod(0o600)
        while not stopping:
            if capture:
                capture.drain(0.05)
                if capture.limited:
                    stopping = True
                    break
                status = child.poll()
                if status is not None:
                    break
                continue
            try:
                status = child.wait(timeout=0.05)
                break
            except subprocess.TimeoutExpired:
                pass
    finally:
        forced = False
        try:
            os.killpg(child.pid, signal.SIGKILL)
            forced = True
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 2
        while True:
            # Each killed parent's remaining descendants are adopted here.
            # Iterate until waitpid proves there are no children left.
            for raw in children_path.read_text().split():
                try:
                    os.kill(int(raw), signal.SIGKILL)
                    forced = True
                except ProcessLookupError:
                    pass
            while True:
                try:
                    pid, _ = os.waitpid(-1, os.WNOHANG)
                except ChildProcessError:
                    observation = {}
                    if scope:
                        observation = scope.observation()
                        scope.close()
                        resource_path.write_text(json.dumps({'phase':'completed', **observation}))
                    descriptor = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                    with os.fdopen(descriptor, 'w') as stream:
                        stream.write('all-descendants-reaped\n')
                        stream.flush()
                        os.fsync(stream.fileno())
                    if capture:
                        capture.finish(status, stopping, forced, observation)
                    if stopping:
                        return 128 + signal.SIGTERM
                    if status is None:
                        return 1
                    # #1779: a provider killed by a signal while we live (OOM
                    # SIGKILL, `pkill claude`) is forwarded the shell's way,
                    # 128+n, so the daemon reads it as an interruption rather
                    # than a provider failure.
                    return status if status >= 0 else 128 - status
                if pid == 0:
                    break
            if time.monotonic() >= deadline:
                raise RuntimeError('provider descendant cleanup did not complete')
            time.sleep(0.005)


if __name__ == '__main__':
    try:
        sys.exit(supervise(sys.argv[1], sys.argv[2:]))
    except Exception:
        print('Provider supervision failed; cleanup is unverified.', file=sys.stderr)
        sys.exit(125)
