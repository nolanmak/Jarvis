#!/usr/bin/env python3
"""Linux command confinement for the Jarvis bridge, independent of user namespaces.

Requires Landlock ABI 6+ and libseccomp; missing enforcement is a hard failure.
Only explicit roots plus system runtime libraries are readable. Writable roots
must be disposable build workspaces, never the credential-bearing daemon home.
The parent must set a wall timeout and reap the entire process group.
"""
import ctypes
import errno
import json
import os
from pathlib import Path
import platform
import resource
import signal
import stat
import sys


class Ruleset(ctypes.Structure):
    _fields_ = [('filesystem', ctypes.c_uint64), ('network', ctypes.c_uint64),
                ('scoped', ctypes.c_uint64)]


class PathRule(ctypes.Structure):
    _pack_ = 1
    _fields_ = [('access', ctypes.c_uint64), ('parent_fd', ctypes.c_int32)]


CONTROL_PARTS = {'.git', '.codex', '.claude', '.ssh', '.gnupg', '.aws', '.azure'}


def source_read_entries(roots):
    # Grant opened inodes, not paths re-resolved after checking: otherwise a
    # concurrent symlink swap could turn a safe source name into a secret.
    count = 0
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    def walk(fd):
        nonlocal count
        yield fd, 1 << 3
        with os.scandir(fd) as entries:
            names = [entry.name for entry in entries]
        for name in names:
            count += 1
            if count > 100000:
                raise RuntimeError('source read scope exceeds entry limit')
            if name in CONTROL_PARTS or name == '.env' or name.startswith('.env.'):
                continue
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
            if stat.S_ISDIR(info.st_mode):
                child = os.open(name, directory_flags, dir_fd=fd)
                try:
                    yield from walk(child)
                finally:
                    os.close(child)
            elif stat.S_ISREG(info.st_mode):
                child = os.open(name, os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
                try:
                    current = os.fstat(child)
                    if stat.S_ISREG(current.st_mode) and current.st_nlink == 1:
                        yield child, (1 << 0) | (1 << 2)
                finally:
                    os.close(child)
    for raw in roots:
        root = Path(raw)
        if not root.is_absolute() or root == Path('/'):
            raise RuntimeError('source scope must be a specific absolute directory')
        fd = os.open(root, directory_flags)
        try:
            yield from walk(fd)
        finally:
            os.close(fd)


def restrict(read_roots, write_roots, runtime_reads=()):
    if platform.system() != 'Linux' or platform.machine() not in ('x86_64', 'aarch64'):
        raise RuntimeError('command sandbox requires supported Linux architecture')
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    abi = libc.syscall(444, 0, 0, 1)
    if abi < 6:
        raise RuntimeError('command sandbox requires Landlock ABI 6 or newer')
    # Load the filter library before restricting dynamic library reads.
    seccomp = ctypes.CDLL('libseccomp.so.2', use_errno=True)
    seccomp.seccomp_init.argtypes = [ctypes.c_uint32]
    seccomp.seccomp_init.restype = ctypes.c_void_p
    seccomp.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    seccomp.seccomp_syscall_resolve_name.restype = ctypes.c_int
    seccomp.seccomp_rule_add.argtypes = [ctypes.c_void_p, ctypes.c_uint32,
                                        ctypes.c_int, ctypes.c_uint]
    seccomp.seccomp_load.argtypes = [ctypes.c_void_p]
    seccomp.seccomp_release.argtypes = [ctypes.c_void_p]

    fs_access = (1 << 16) - 1  # ABI 5 includes IOCTL_DEV; ABI 6 adds scope.
    read_access = (1 << 0) | (1 << 2) | (1 << 3)
    attrs = Ruleset(fs_access, 3, 3)  # deny TCP; scope signals and abstract UNIX.
    ruleset = libc.syscall(444, ctypes.byref(attrs), ctypes.sizeof(attrs), 0)
    if ruleset < 0:
        raise OSError(ctypes.get_errno(), 'cannot create command ruleset')
    try:
        for descriptor, access in source_read_entries(read_roots):
            rule = PathRule(access, descriptor)
            if libc.syscall(445, ruleset, 1, ctypes.byref(rule), 0) != 0:
                raise OSError(ctypes.get_errno(), 'cannot add source scope')
        entries = [(p, fs_access) for p in write_roots]
        entries += [(p, read_access) for p in runtime_reads]
        entries += [(p, read_access) for p in ('/usr', '/bin', '/lib', '/lib64') if Path(p).exists()]
        # Node loads the system OpenSSL configuration before running scripts.
        # Grant the file itself, never /etc/ssl (which may contain private keys).
        entries += [(p, read_access) for p in (
            '/etc/ld.so.cache', '/etc/ssl/openssl.cnf', '/etc/passwd', '/etc/nsswitch.conf'
        ) if Path(p).exists()]
        entries += [('/dev/null', (1 << 1) | (1 << 2))]
        for raw, access in entries:
            path = Path(raw).resolve(strict=True)
            if path == Path('/'):
                raise RuntimeError('unrestricted filesystem roots are forbidden')
            fd = os.open(path, os.O_PATH | os.O_CLOEXEC)
            try:
                if not stat.S_ISDIR(os.fstat(fd).st_mode):
                    access &= (1 << 0) | (1 << 1) | (1 << 2) | (1 << 14) | (1 << 15)
                rule = PathRule(access, fd)
                if libc.syscall(445, ruleset, 1, ctypes.byref(rule), 0) != 0:
                    raise OSError(ctypes.get_errno(), 'cannot add command scope')
            finally:
                os.close(fd)
        if libc.prctl(38, 1, 0, 0, 0) != 0:  # PR_SET_NO_NEW_PRIVS
            raise OSError(ctypes.get_errno(), 'cannot forbid privilege escalation')
        if libc.syscall(446, ruleset, 0) != 0:
            raise OSError(ctypes.get_errno(), 'cannot enforce command scope')
    finally:
        os.close(ruleset)

    context = seccomp.seccomp_init(0x7fff0000)  # SCMP_ACT_ALLOW
    if not context:
        raise RuntimeError('cannot create syscall filter')
    try:
        # Prevent network access (including UDP), process introspection and
        # escaping the parent's cleanup group. socketpair is local IPC only.
        denied = ('socket', 'connect', 'bind', 'listen', 'accept', 'accept4',
                  'ptrace', 'process_vm_readv', 'process_vm_writev', 'pidfd_getfd',
                  'mount', 'umount2', 'pivot_root', 'chroot', 'setns', 'unshare',
                  'setsid', 'setpgid', 'bpf', 'keyctl', 'add_key', 'request_key',
                  'io_uring_setup', 'open_by_handle_at', 'init_module', 'finit_module')
        for name in denied:
            number = seccomp.seccomp_syscall_resolve_name(name.encode())
            if number >= 0 and seccomp.seccomp_rule_add(context, 0x00050000 | errno.EPERM, number, 0) != 0:
                raise RuntimeError('cannot install syscall restriction')
        if seccomp.seccomp_load(context) != 0:
            raise RuntimeError('cannot enforce syscall restrictions')
    finally:
        seccomp.seccomp_release(context)


def main():
    if len(sys.argv) < 3:
        raise RuntimeError('expected private policy and command argv')
    descriptor = os.open(sys.argv[1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor) as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
            raise RuntimeError('command policy must be an owner-private regular file')
        policy = json.load(stream)
    libc = ctypes.CDLL(None, use_errno=True)
    parent = os.getppid()
    if libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent:
        raise RuntimeError('cannot bind command lifetime to its parent')
    os.chdir(policy['cwd'])
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    file_limit = policy.get('file_limit_bytes', 256 * 1024 * 1024)
    if type(file_limit) is not int or not 1 <= file_limit <= 256 * 1024 * 1024:
        raise RuntimeError('invalid file resource limit')
    resource.setrlimit(resource.RLIMIT_FSIZE, (file_limit, file_limit))
    memory_limit = policy.get('memory_limit_bytes')
    if memory_limit is not None:
        if type(memory_limit) is not int or not 64 * 1024 * 1024 <= memory_limit <= 2 * 1024 * 1024 * 1024:
            raise RuntimeError('invalid memory resource limit')
        resource.setrlimit(resource.RLIMIT_AS, (memory_limit, memory_limit))
    environment = {key: value for key, value in os.environ.items() if key in (
        'HOME', 'PATH', 'LANG', 'LC_ALL', 'TERM', 'CARGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN',
        'CARGO_TARGET_DIR', 'TMPDIR', 'NPM_CONFIG_CACHE', 'CARGO_NET_OFFLINE',
        'NPM_CONFIG_USERCONFIG', 'NPM_CONFIG_GLOBALCONFIG',
        'GIT_CONFIG_NOSYSTEM', 'GIT_CONFIG_GLOBAL', 'GIT_OPTIONAL_LOCKS', 'GIT_DIR', 'GIT_WORK_TREE')}
    restrict(policy['read_roots'], policy['write_roots'], policy.get('runtime_reads', []))
    os.execvpe(sys.argv[2], sys.argv[2:], environment)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Command sandbox refused execution: {type(error).__name__}: {error}', file=sys.stderr)
        sys.exit(126)
