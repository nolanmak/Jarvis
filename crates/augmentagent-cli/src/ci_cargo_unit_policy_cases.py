"""Actual kernel cases for the inherited namespace/async-I/O unit policy."""
import ctypes
import errno
import os
import resource
import signal
import sys
import threading

resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
mode = sys.argv[2]
before = libc.prctl(39, 0, 0, 0, 0)
library = ctypes.CDLL(sys.argv[1], use_errno=True)
assert libc.prctl(39, 0, 0, 0, 0) == before, 'library load activated policy'


def denied(number, *args, expected=errno.EPERM):
    ctypes.set_errno(0)
    result = libc.syscall(number, *args)
    assert result == -1 and ctypes.get_errno() == expected, (number, result, ctypes.get_errno())


ready = threading.Event()
release = threading.Event()
errors = []


def existing_thread():
    ready.set()
    release.wait()
    try:
        denied(208, 0, 0, 0, 0, 0)  # io_getevents, also checks TSYNC.
        denied(435, 0, 0, expected=errno.ENOSYS)
    except BaseException as error:
        errors.append(error)


if mode == 'threads':
    thread = threading.Thread(target=existing_thread, daemon=True)
    thread.start()
    assert ready.wait(2)
if library.jarvis_ci_restrict_unit() != 0:
    raise OSError(ctypes.get_errno(), 'unit policy unavailable')
assert libc.prctl(39, 0, 0, 0, 0) == 1

if mode == 'namespace':
    # Explicit arguments distinguish policy denial from invalid kernel inputs.
    for number in (155, 161, 165, 166, 272, 308, 428, 429, 430, 431, 432, 433, 442):
        denied(number, 0, 0, 0, 0, 0, 0)
elif mode == 'async':
    for number in (206, 207, 208, 209, 210, 333, 425, 426, 427):
        denied(number, 0, 0, 0, 0, 0, 0)
elif mode == 'clone':
    for flag in (0x80, 0x20000, 0x2000000, 0x4000000, 0x8000000,
                 0x10000000, 0x20000000, 0x40000000, 1 << 32):
        denied(56, ctypes.c_ulong(flag | signal.SIGCHLD), 0, 0, 0, 0)
    denied(435, 0, 0, expected=errno.ENOSYS)
elif mode == 'fork':
    child = os.fork()
    if child == 0:
        denied(208, 0, 0, 0, 0, 0)
        os._exit(0)
    assert os.waitpid(child, 0)[1] == 0
elif mode == 'exec':
    # The first ordinary exec is allowed; the kernel policy survives exec.
    program = ('import ctypes,errno; l=ctypes.CDLL(None,use_errno=True); '
               'assert l.syscall(208,0,0,0,0,0)==-1; '
               'assert ctypes.get_errno()==errno.EPERM; print("GUARDED")')
    os.execve(sys.executable, [sys.executable, '-I', '-c', program], {})
elif mode == 'threads':
    release.set()
    thread.join(2)
    assert not thread.is_alive() and not errors, errors
    # Newly created threads still work after clone3 is refused with ENOSYS.
    after = threading.Thread(target=existing_thread)
    after.start()
    after.join(2)
    assert not after.is_alive() and not errors, errors
elif mode == 'x32':
    libc.syscall(0x40000000 | 39)
    raise SystemExit('foreign ABI was not rejected')
else:
    raise SystemExit('unknown unit policy case')
print('GUARDED')
