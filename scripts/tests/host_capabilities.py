"""Host capability probes for suites that exercise real kernel confinement.

A host that cannot enforce the command sandbox must SKIP those tests with the
reason, never fail them: hosted CI runners may lack Landlock ABI 6 (Linux
6.12+), libseccomp or Poppler. The probes inspect the host directly rather than
running `codex-command-sandbox.py`, so a sandbox regression still fails on
every host that supports enforcement.

Not a test module (the discovery pattern is `*_test.py`). Suites load it by
path, and `python3 scripts/tests/host_capabilities.py` prints the report CI
logs before running the suites.
"""
import ctypes
import os
from pathlib import Path
import platform

# Mirrors the requirement enforced by codex-command-sandbox.py.
REQUIRED_LANDLOCK_ABI = 6
LANDLOCK_CREATE_RULESET = 444  # Same number on x86_64 and aarch64.
LANDLOCK_CREATE_RULESET_VERSION = 1
POPPLER = ('/usr/bin/pdfinfo', '/usr/bin/pdftoppm')


def landlock_abi():
    """Kernel Landlock ABI version, or a negative errno when unavailable."""
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    abi = libc.syscall(LANDLOCK_CREATE_RULESET, None, 0, LANDLOCK_CREATE_RULESET_VERSION)
    return abi if abi >= 0 else -ctypes.get_errno()


def sandbox_unavailable_reason():
    """Why this host cannot enforce the command sandbox, or None if it can."""
    system, machine = platform.system(), platform.machine()
    if system != 'Linux' or machine not in ('x86_64', 'aarch64'):
        return f'command sandbox needs Linux on x86_64/aarch64; host is {system} {machine}'
    abi = landlock_abi()
    if abi < 0:
        return (f'Landlock is unavailable on this kernel ({os.strerror(-abi)}); '
                f'the command sandbox needs Landlock ABI {REQUIRED_LANDLOCK_ABI}+ (Linux 6.12+)')
    if abi < REQUIRED_LANDLOCK_ABI:
        return (f'kernel Landlock ABI is {abi}; the command sandbox needs '
                f'ABI {REQUIRED_LANDLOCK_ABI}+ (Linux 6.12+)')
    try:
        ctypes.CDLL('libseccomp.so.2')
    except OSError:
        return 'libseccomp.so.2 is not installed (apt install libseccomp2)'
    return None


def poppler_unavailable_reason():
    """Why the trusted PDF renderer is missing, or None if it is installed."""
    missing = [path for path in POPPLER if not Path(path).is_file()]
    if missing:
        return 'Poppler is not installed (apt install poppler-utils): missing ' + ', '.join(missing)
    return None


if __name__ == '__main__':
    print('Landlock ABI:', landlock_abi(), '(kernel', platform.release() + ')')
    print('command sandbox:', sandbox_unavailable_reason() or 'enforceable')
    print('PDF renderer:', poppler_unavailable_reason() or 'installed')
