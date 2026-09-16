#!/usr/bin/env python3
"""Own and reap one provider tree, including descendants that call setsid."""
import ctypes
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
