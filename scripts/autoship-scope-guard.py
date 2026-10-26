#!/usr/bin/env python3
"""Inherited filesystem-write confinement for native autoship model processes.

The provider process requires its own network transport. This guard therefore
makes NO hostname-egress guarantee in provider_transport mode. Tool commands
requiring that guarantee must use the separate no-network command bridge.
Missing kernel enforcement is fatal; there is no unsandboxed fallback.
"""
import ctypes
import errno
import json
import os
from pathlib import Path
import platform
import stat
import sys


class Ruleset(ctypes.Structure):
    _fields_ = [('filesystem', ctypes.c_uint64), ('network', ctypes.c_uint64),
                ('scoped', ctypes.c_uint64)]


class PathRule(ctypes.Structure):
    _pack_ = 1
    _fields_ = [('access', ctypes.c_uint64), ('parent_fd', ctypes.c_int32)]


def private_json(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(fd) as source:
        info = os.fstat(source.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_nlink != 1 or info.st_mode & 0o077 or info.st_size > 2 * 1024 * 1024):
            raise RuntimeError('scope policy is not an owner-private regular file')
        return json.load(source)


def canonical_directory(raw):
    path = Path(raw)
    if not path.is_absolute() or path == Path('/'):
        raise RuntimeError('scope root must be a specific absolute directory')
    if path != path.resolve(strict=True) or not path.is_dir():
        raise RuntimeError('scope root must be canonical and not symlinked')
    return path


def validated_policy(policy):
    expected = {'version', 'write_roots', 'protected_roots', 'network'}
    if policy.get('version') in (2, 3):
        expected.add('write_files')
    if policy.get('version') == 3:
        expected.update(('read_roots', 'read_files'))
    if set(policy) != expected or policy['version'] not in (1, 2, 3):
        raise RuntimeError('unsupported scope policy')
    if policy['network'] not in ('deny', 'provider_transport'):
        raise RuntimeError('native guard cannot enforce hostname egress; use the command bridge')
    roots = [canonical_directory(p) for p in policy['write_roots']]
    protected = [canonical_directory(p) for p in policy['protected_roots']]
    files = []
    for raw in policy.get('write_files', []):
        path = Path(raw)
        info = path.lstat()
        if (not path.is_absolute() or path != path.resolve(strict=True)
                or not stat.S_ISREG(info.st_mode) or info.st_nlink != 1):
            raise RuntimeError('file write grant must be a canonical singly linked file')
        files.append(path)
    if not roots or not protected or len(roots) + len(files) > 10000 or len(protected) > 16:
        raise RuntimeError('scope roots missing or oversized')
    for root in roots + files:
        for denied in protected:
            if root == denied or root in denied.parents or denied in root.parents:
                raise RuntimeError('writable scope intersects protected control state')
    return roots, files


def read_grants(policy):
    """Version 3 is default-deny for content reads and executable loading.

    Grants come only from a controller-owned manifest; write permission never
    implies read permission. Existing v1/v2 callers retain their honest write-
    only guarantee until their provider runtime/bridge has migrated.
    """
    roots = [canonical_directory(raw) for raw in policy['read_roots']]
    files = []
    for raw in policy['read_files']:
        path = Path(raw)
        info = path.lstat()
        if (not path.is_absolute() or path != path.resolve(strict=True)
                or not stat.S_ISREG(info.st_mode) or info.st_nlink != 1):
            raise RuntimeError('read file must be canonical and singly linked')
        files.append(path)
    if not roots or len(roots) + len(files) > 10000:
        raise RuntimeError('read grants missing or oversized')
    for granted in roots + files:
        for raw in policy['protected_roots']:
            denied = canonical_directory(raw)
            if granted == denied or granted in denied.parents or denied in granted.parents:
                raise RuntimeError('read grant intersects protected control state')
    return roots, files


def restrict(policy):
    roots, files = validated_policy(policy)
    reads = read_grants(policy) if policy['version'] == 3 else None
    if platform.system() != 'Linux' or platform.machine() not in ('x86_64', 'aarch64'):
        raise RuntimeError('native scope requires a supported Linux architecture')
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    if libc.syscall(444, 0, 0, 1) < 6:
        raise RuntimeError('native scope requires Landlock ABI 6 or newer')
    seccomp = ctypes.CDLL('libseccomp.so.2', use_errno=True)
    seccomp.seccomp_init.argtypes = [ctypes.c_uint32]
    seccomp.seccomp_init.restype = ctypes.c_void_p
    seccomp.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    seccomp.seccomp_syscall_resolve_name.restype = ctypes.c_int
    seccomp.seccomp_rule_add.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int, ctypes.c_uint]
    seccomp.seccomp_load.argtypes = [ctypes.c_void_p]
    seccomp.seccomp_release.argtypes = [ctypes.c_void_p]
    # Handle every filesystem mutation, including truncate/refer/ioctl. Reads
    # remain available to the native provider; tool-level read scope is separate.
    write = (1 << 1) | sum(1 << n for n in range(4, 16))
    read = (1 << 0) | (1 << 2) | (1 << 3)
    attrs = Ruleset(write | (read if reads else 0), 3 if policy['network'] == 'deny' else 0, 3)
    ruleset = libc.syscall(444, ctypes.byref(attrs), ctypes.sizeof(attrs), 0)
    if ruleset < 0:
        raise OSError(ctypes.get_errno(), 'scope ruleset creation failed')
    try:
        for path in roots + files:
            descriptor = os.open(path, os.O_PATH | os.O_CLOEXEC | os.O_NOFOLLOW)
            try:
                access = write if path in roots else (1 << 1) | (1 << 14) | (1 << 15)
                rule = PathRule(access, descriptor)
                if libc.syscall(445, ruleset, 1, ctypes.byref(rule), 0):
                    raise OSError(ctypes.get_errno(), 'scope rule installation failed')
            finally:
                os.close(descriptor)
        if reads:
            for path in reads[0] + reads[1]:
                descriptor = os.open(path, os.O_PATH | os.O_CLOEXEC | os.O_NOFOLLOW)
                try:
                    access = read if path in reads[0] else (1 << 0) | (1 << 2)
                    rule = PathRule(access, descriptor)
                    if libc.syscall(445, ruleset, 1, ctypes.byref(rule), 0):
                        raise OSError(ctypes.get_errno(), 'read rule installation failed')
                finally:
                    os.close(descriptor)
        # stdout/stderr are inherited; commands may also need /dev/null.
        descriptor = os.open('/dev/null', os.O_PATH | os.O_CLOEXEC)
        try:
            rule = PathRule((1 << 1) | (1 << 14) | (1 << 15), descriptor)
            if libc.syscall(445, ruleset, 1, ctypes.byref(rule), 0):
                raise OSError(ctypes.get_errno(), 'null device scope failed')
        finally:
            os.close(descriptor)
        if libc.prctl(38, 1, 0, 0, 0) or libc.syscall(446, ruleset, 0):
            raise OSError(ctypes.get_errno(), 'scope enforcement failed')
    finally:
        os.close(ruleset)
    context = seccomp.seccomp_init(0x7fff0000)
    if not context:
        raise RuntimeError('syscall filter unavailable')
    try:
        denied = ['ptrace', 'process_vm_readv', 'process_vm_writev', 'pidfd_getfd',
                  'mount', 'umount2', 'pivot_root', 'setns', 'unshare', 'bpf',
                  'open_by_handle_at', 'name_to_handle_at', 'io_uring_setup']
        if policy['network'] == 'deny':
            denied += ['socket', 'connect', 'bind', 'listen', 'accept', 'accept4']
        for name in denied:
            number = seccomp.seccomp_syscall_resolve_name(name.encode())
            if number >= 0 and seccomp.seccomp_rule_add(context, 0x50000 | errno.EPERM, number, 0):
                raise RuntimeError('syscall filter rule failed')
        if reads:
            # ABI 8 cannot restrict pathname UNIX sockets. Do not mistake
            # filesystem read denial for blocking a host credential broker.
            # Anonymous socketpair remains available for internal child IPC.
            class ArgCompare(ctypes.Structure):
                _fields_ = [('arg', ctypes.c_uint), ('op', ctypes.c_int),
                            ('a', ctypes.c_uint64), ('b', ctypes.c_uint64)]
            seccomp.seccomp_rule_add_array.argtypes = [ctypes.c_void_p, ctypes.c_uint32,
                ctypes.c_int, ctypes.c_uint, ctypes.POINTER(ArgCompare)]
            comparison = ArgCompare(0, 4, 1, 0)  # SCMP_CMP_EQ, AF_UNIX
            number = seccomp.seccomp_syscall_resolve_name(b'socket')
            if number < 0 or seccomp.seccomp_rule_add_array(context, 0x50000 | errno.EPERM,
                    number, 1, ctypes.byref(comparison)):
                raise RuntimeError('host UNIX socket isolation unavailable')
        if seccomp.seccomp_load(context):
            raise RuntimeError('syscall filter enforcement failed')
    finally:
        seccomp.seccomp_release(context)


def main():
    if len(sys.argv) < 4 or sys.argv[2] != '--':
        raise RuntimeError('expected policy -- executable arguments')
    policy = private_json(sys.argv[1])
    restrict(policy)
    os.execvpe(sys.argv[3], sys.argv[3:], os.environ)


if __name__ == '__main__':
    try:
        main()
    except Exception:
        # Never echo policy paths, credential locations, or provider arguments.
        print('Autoship scope enforcement unavailable; launch refused.', file=sys.stderr)
        sys.exit(126)
