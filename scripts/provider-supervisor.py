#!/usr/bin/env python3
"""Own and reap one provider tree, including descendants that call setsid."""
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def supervise(receipt, argv, *, forward_fds=(), close_after_spawn=(),
              return_raw_status=False, on_spawn=None, on_exec=None):
    """Own descendants; optionally observe a trusted bootstrap's actual exec.

    on_exec requires the single-threaded bootstrap to use PTRACE_TRACEME and
    SIGSTOP immediately before its requested exec. Only the original child's
    kernel PTRACE_EVENT_EXEC invokes the callback, while the tool is stopped.
    Missing callbacks never prove execution, regardless of process exit status.
    This observer detaches after the callback; it is not an input collector.
    """
    libc = ctypes.CDLL(None, use_errno=True)
    def ptrace(request,pid,data=None):
        libc.ptrace.restype=ctypes.c_long
        libc.ptrace.argtypes=(ctypes.c_uint,ctypes.c_int,ctypes.c_void_p,ctypes.c_void_p)
        if libc.ptrace(request,pid,None,data)!=0:
            raise OSError(ctypes.get_errno(),'original exec observation unavailable')
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
                if scope:
                    resource_path.write_text(json.dumps({'phase':'cancelled_before_dispatch', **scope.observation()}))
                    scope.close()
                Path(receipt).write_text('all-descendants-reaped\n')
                Path(receipt).chmod(0o600)
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
        if (type(return_raw_status) is not bool or (on_spawn is not None and not callable(on_spawn))
                or (on_exec is not None and (not callable(on_exec) or vm_preflight))):
            raise RuntimeError('invalid internal supervisor observer')
        if on_exec is not None and {entry.name for entry in Path('/proc/self/task').iterdir()}!={str(os.getpid())}:
            raise RuntimeError('exec observation requires a single-threaded supervisor')
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
        if (not isinstance(forward_fds, tuple)
                or not isinstance(close_after_spawn, tuple)
                or len(forward_fds) > 64 or len(close_after_spawn) > 64
                or any(type(fd) is not int or fd < 3 for fd in forward_fds)
                or any(type(fd) is not int or fd < 3 for fd in close_after_spawn)
                or len(set(forward_fds)) != len(forward_fds)
                or len(set(close_after_spawn)) != len(close_after_spawn)
                or any(fd not in forward_fds or fd in storage_fds for fd in close_after_spawn)):
            raise RuntimeError('invalid explicit child descriptor set')
        inherited = tuple(sorted(set(storage_fds) | set(forward_fds)))
        if len(inherited) > 64:
            raise RuntimeError('too many inherited child descriptors')
        for descriptor in forward_fds:
            os.fstat(descriptor)
        attach=scope.attach_child if scope else None
        if on_exec is not None:
            origin=os.getpid()
            def attach():
                # EXITKILL cannot cover the interval before TRACEME/options.
                # The private worker is single-threaded before this fork.
                if libc.prctl(1,signal.SIGKILL,0,0,0)!=0 or os.getppid()!=origin:
                    raise RuntimeError('exec bootstrap lost its original supervisor')
                if scope:scope.attach_child()
                if os.getppid()!=origin:
                    raise RuntimeError('exec bootstrap lost its original supervisor')
        child = subprocess.Popen(argv, start_new_session=True,
                                 pass_fds=inherited,
                                 preexec_fn=attach)
    except BaseException:
        if scope:
            resource_path.write_text(json.dumps({'phase':'launch_failed', **scope.observation()}))
            scope.close()
        Path(receipt).write_text('all-descendants-reaped\n')
        Path(receipt).chmod(0o600)
        raise
    status = None
    try:
        # Close exec-start pipe copies immediately after dispatch, inside the
        # descendant cleanup guard. A failed close must never take the launch-
        # failed path, which may acknowledge cleanup without a child to reap.
        # Storage ownership remains held through the original cleanup path.
        for descriptor in close_after_spawn:
            os.close(descriptor)
        if on_spawn is not None:
            on_spawn(child)
        if vm_preflight:
            vm_ready = Path(receipt).with_name('vm-ready')
            deadline = time.monotonic() + 60
            while not vm_ready.exists():
                if stopping or child.poll() is not None or time.monotonic() >= deadline:
                    return 125
                time.sleep(0.005)
            if vm_ready.read_text() != 'ready\n':
                raise RuntimeError('invalid VM readiness')
            ready = Path(receipt).with_name('launch-ready')
            ready.write_text('ready\n')
            ready.chmod(0o600)
        exec_armed=False
        exec_observed=False
        while not stopping:
            if on_exec is not None and not exec_observed:
                # Popen.wait/poll treats a traced stop as process completion.
                # This original parent must consume and distinguish stops until
                # the requested exec is positively observed and detached.
                pid,waited_status=os.waitpid(child.pid,os.WNOHANG|os.WUNTRACED)
                if pid==0:
                    time.sleep(.005)
                    continue
                if os.WIFEXITED(waited_status) or os.WIFSIGNALED(waited_status):
                    status=os.waitstatus_to_exitcode(waited_status)
                    child.returncode=status
                    break
                if not os.WIFSTOPPED(waited_status):
                    raise RuntimeError('original exec wait state unavailable')
                event=waited_status>>16
                stopped_signal=os.WSTOPSIG(waited_status)
                if not exec_armed:
                    if (event or stopped_signal!=signal.SIGSTOP
                            or {entry.name for entry in Path(f'/proc/{child.pid}/task').iterdir()}!={str(child.pid)}):
                        raise RuntimeError('original exec bootstrap is not a single stopped thread')
                    # TRACEEXEC distinguishes the actual kernel event from a
                    # user SIGTRAP. EXITKILL owns the pre-exec death interval.
                    ptrace(0x4200,child.pid,ctypes.c_void_p(0x10|0x100000))
                    exec_armed=True
                    ptrace(7,child.pid)
                elif event==4 and stopped_signal==signal.SIGTRAP:
                    former=ctypes.c_ulong()
                    ptrace(0x4201,child.pid,ctypes.byref(former))
                    if former.value!=child.pid:
                        raise RuntimeError('exec changed the original supervised thread')
                    on_exec(child)
                    ptrace(17,child.pid)  # Tool cannot run before observer succeeds.
                    exec_observed=True
                else:
                    # Signals during the armed interval cancel staging. They
                    # cannot become a claimed tool start or be silently lost.
                    raise RuntimeError('original exec staging was interrupted')
                continue
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
                    pid, waited_status = os.waitpid(-1, os.WNOHANG)
                except ChildProcessError:
                    if scope:
                        observation = scope.observation()
                        scope.close()
                        resource_path.write_text(json.dumps({'phase':'completed', **observation}))
                    descriptor = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                    with os.fdopen(descriptor, 'w') as stream:
                        stream.write('all-descendants-reaped\n')
                        stream.flush()
                        os.fsync(stream.fileno())
                    # The relay needs the actual wait status; ordinary provider
                    # callers retain their existing shell-style signal codes.
                    if return_raw_status:
                        return status if status is not None else 1
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
                if os.WIFSTOPPED(waited_status):
                    # A callback/setup failure may leave an original ptrace
                    # stop pending. Reap only terminal states before receipt.
                    try:os.kill(pid,signal.SIGKILL)
                    except ProcessLookupError:pass
                    continue
                if return_raw_status and pid == child.pid and status is None:
                    status = os.waitstatus_to_exitcode(waited_status)
                    child.returncode=status
            if time.monotonic() >= deadline:
                raise RuntimeError('provider descendant cleanup did not complete')
            time.sleep(0.005)


if __name__ == '__main__':
    try:
        sys.exit(supervise(sys.argv[1], sys.argv[2:]))
    except Exception:
        print('Provider supervision failed; cleanup is unverified.', file=sys.stderr)
        sys.exit(125)
