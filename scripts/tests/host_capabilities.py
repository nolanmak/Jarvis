"""Host capability probes for suites that exercise real kernel confinement.

A host that cannot enforce the command sandbox SKIPS those tests with the
reason by default: a developer kernel may lack Landlock ABI 6 (Linux 6.12+),
libseccomp or Poppler. The probes inspect the host directly rather than running
`codex-command-sandbox.py`, so a sandbox regression still fails on every host
that supports enforcement.

CI sets REQUIRE_ENFORCEABLE_SANDBOX=1. Then a missing capability FAILS those
tests, and the report below exits non-zero, so a runner image that loses
enforcement cannot pass with zero sandbox coverage.

Not a test module (the discovery pattern is `*_test.py`). Suites load it by
path, and `python3 scripts/tests/host_capabilities.py` prints the report CI
logs before running the suites.
"""
import ctypes
import functools
import os
from pathlib import Path
import platform
import sys
import unittest

# Mirrors the requirement enforced by codex-command-sandbox.py.
REQUIRED_LANDLOCK_ABI = 6
LANDLOCK_CREATE_RULESET = 444  # Same number on x86_64 and aarch64.
LANDLOCK_CREATE_RULESET_VERSION = 1
POPPLER = ('/usr/bin/pdfinfo', '/usr/bin/pdftoppm')
# Set to 1 where enforcement must be present (CI): missing becomes a failure.
REQUIRE_ENV = 'REQUIRE_ENFORCEABLE_SANDBOX'


def enforcement_required():
    return os.environ.get(REQUIRE_ENV) == '1'


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


def requirement(reason):
    """Decorate a test method or TestCase class needing a host capability.

    `reason` is None when the capability is present: the test runs. Otherwise
    the test skips with `reason`, or fails when REQUIRE_ENFORCEABLE_SANDBOX=1.
    The switch is read when the test runs, not at import.
    """
    def guard(test):
        @functools.wraps(test)
        def guarded(self, *args, **kwargs):
            if enforcement_required():
                self.fail(f'{REQUIRE_ENV}=1 but {reason}')
            self.skipTest(reason)
        return guarded

    def decorate(target):
        if reason is None:
            return target
        if isinstance(target, type):
            for name, value in list(vars(target).items()):
                if name.startswith('test') and callable(value):
                    setattr(target, name, guard(value))
            return target
        return guard(target)
    return decorate


def main():
    sandbox = sandbox_unavailable_reason()
    poppler = poppler_unavailable_reason()
    print('Landlock ABI:', landlock_abi(), '(kernel', platform.release() + ')')
    print('command sandbox:', sandbox or 'enforceable')
    print('PDF renderer:', poppler or 'installed')
    missing = [reason for reason in (sandbox, poppler) if reason]
    if missing and enforcement_required():
        for reason in missing:
            print(f'{REQUIRE_ENV}=1 but {reason}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
