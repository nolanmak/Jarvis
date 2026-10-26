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
        try:
            spec = importlib.util.spec_from_file_location('run_resources', Path(__file__).with_name('run-resource-scope.py'))
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            scope = module.declared_scope()
            resource_path.write_text(json.dumps({'phase':'admitted', **scope.observation()}))
            resource_path.chmod(0o600)
        except BaseException:
            resource_path.write_text(json.dumps({'phase':'admission_failed','exhausted':True}))
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
        while not proceed.exists():
            if stopping or time.monotonic() >= deadline:
                if scope:
                    resource_path.write_text(json.dumps({'phase':'cancelled_before_dispatch', **scope.observation()}))
                    scope.close()
                Path(receipt).write_text('all-descendants-reaped\n')
                Path(receipt).chmod(0o600)
                return 125
            time.sleep(0.005)
        if proceed.read_text() != 'admitted\n':
            raise RuntimeError('invalid controller launch admission')
    if vm_preflight:
        os.environ['JARVIS_NATIVE_VM_READY'] = str(Path(receipt).with_name('vm-ready'))
        os.environ['JARVIS_NATIVE_VM_CONTINUE'] = str(Path(receipt).with_name('launch-continue'))
    else:
        os.environ.pop('JARVIS_NATIVE_VM_READY', None)
        os.environ.pop('JARVIS_NATIVE_VM_CONTINUE', None)
    try:
        child = subprocess.Popen(argv, start_new_session=True,
                                 preexec_fn=scope.attach_child if scope else None)
    except BaseException:
        if scope:
            resource_path.write_text(json.dumps({'phase':'launch_failed', **scope.observation()}))
            scope.close()
        Path(receipt).write_text('all-descendants-reaped\n')
        Path(receipt).chmod(0o600)
        raise
    status = None
    try:
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
                    if scope:
                        observation = scope.observation()
                        scope.close()
                        resource_path.write_text(json.dumps({'phase':'completed', **observation}))
                    descriptor = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                    with os.fdopen(descriptor, 'w') as stream:
                        stream.write('all-descendants-reaped\n')
                        stream.flush()
                        os.fsync(stream.fileno())
                    return 128 + signal.SIGTERM if stopping else (status if status is not None and status >= 0 else 1)
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
