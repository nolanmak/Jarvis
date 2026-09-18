#!/usr/bin/env python3
"""Constrained tools for provider adapters. Policy is supplied by Jarvis, never the model.

Filesystem operations use directory descriptors and refuse symlinks at every
component. Commands are parsed into argv; model text is never evaluated by a shell.
The execution transport is wired separately from this policy core.
"""
import os
import json
import hashlib
import math
import subprocess
import fnmatch
import re
import resource
import shlex
import stat
import time
import uuid
from contextlib import contextmanager
from pathlib import Path


class Denied(ValueError):
    """An operation is outside the declared profile."""


class Readiness(Denied):
    """Public diagnostics contain fixed categories, never configuration."""
    MESSAGES = {
        'mcp_start': 'Configured MCP server could not start or initialize; check its binary, authentication and transport.',
        'mcp_timeout': 'Configured MCP server timed out; check its availability and timeout setting.',
        'mcp_tools': 'Configured MCP server is missing a required tool; check the server version and tool profile.',
        'build_vm_unavailable': 'Build commands require the private build VM, but its runtime configuration is missing; '
                                'run `augmentagent doctor`, or set AUGMENTAGENT_BUILD_VM=host in the daemon environment to opt out.',
        'build_scratch_unavailable': 'VM build scratch directory {path} is missing, inside a model-writable directory, '
                                     'or not a mode-0700 directory owned by the daemon user; create it with '
                                     '`install -d -m 700` or set AUGMENTAGENT_BUILD_SCRATCH_DIR in the daemon environment.',
        'build_scratch_space': 'VM build scratch {path} has too little space for a new build cache ({detail}); '
                               'free space on that volume or wait for other build sessions to finish.',
        'build_cache_full': 'VM build cache ran out of space at {path} ({detail}); the build did not complete. '
                            'Free space on that volume, or narrow the build, before retrying.',
    }

    def __init__(self, category, path=None, detail=''):
        # Only operator-provisioned paths and sizes are substituted, never secrets.
        message = self.MESSAGES[category].format(path=path if path is not None else '(not configured)', detail=detail)
        super().__init__('JARVIS_READINESS:' + category + ' ' + message)


# #1036: default Bash timeout for cargo/npm/npx when the policy names none. The
# Claude lane's longest Bash call is ten minutes; the tool maximum stays 900 s.
BUILD_TIMEOUT_DEFAULT = 600
COMMAND_TIMEOUT_DEFAULT = 120
COMMAND_TIMEOUT_MAX = 900


def process_start_time(pid):
    """Kernel start time (clock ticks since boot) of pid, or None if it is gone.

    Paired with the pid it identifies one process: pids are reused, start
    times of a reused pid differ.
    """
    try:
        stat_line = Path(f'/proc/{pid}/stat').read_text()
    except OSError:
        return None
    return stat_line.rsplit(')', 1)[1].split()[19]


class BuildScratch:
    """One bridge session's VM build files under the configured scratch root (#1036).

    Nothing here uses the default temp directory. A session directory holds
    the owner record (for the daemon's stale-session sweep), a sparse ext4
    build-cache image that the guest mounts for the Cargo target directory and
    Cargo home (so a session's later builds are incremental), and `tmp/` for
    each command's snapshot and VM control files. Closing the session removes it.

    Disk safety: a new session is admitted only if the volume keeps
    HEADROOM_BYTES free after reserving its whole image and every other
    session's unallocated growth, and if all images' allocated blocks plus the
    new cap stay within BUDGET_BYTES. The scratch volume is shared with the
    daemon's own build caches.
    """
    SESSION_PREFIX = 'jarvis-vm-session-'
    IMAGE_NAME = 'build-cache.img'
    # One checkout's debug target for a couple of workspace crates is ~9-10 GiB
    # (the burn-down target holding channel-core and cli test builds is 9.4 GiB),
    # plus ~0.8 GiB of Cargo home.
    CACHE_BYTES = 12 * 1024**3
    HEADROOM_BYTES = 20 * 1024**3
    BUDGET_BYTES = 24 * 1024**3
    # Below this after a failed build, the volume (not the build) is the cause.
    FULL_BYTES = 1024**3

    def __init__(self, root, refused=False):
        self.root = Path(root) if root else None
        self.refused = refused
        self.cache_bytes = self.CACHE_BYTES
        self.headroom_bytes = self.HEADROOM_BYTES
        self.budget_bytes = self.BUDGET_BYTES
        self.statvfs = os.statvfs
        self.session = None

    def _open_root(self):
        """An O_NOFOLLOW directory fd for the root: owner-only (exactly 0700)."""
        if self.root is None or self.refused or not self.root.is_absolute():
            raise Readiness('build_scratch_unavailable', self.root)
        try:
            descriptor = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        except OSError:
            raise Readiness('build_scratch_unavailable', self.root) from None
        info = os.fstat(descriptor)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            os.close(descriptor)
            raise Readiness('build_scratch_unavailable', self.root)
        return descriptor

    def _require_space(self, root_fd):
        gib = 1024**3
        allocated = outstanding = 0
        for name in os.listdir(root_fd):
            if not name.startswith(self.SESSION_PREFIX):
                continue
            try:
                info = os.stat(f'{name}/{self.IMAGE_NAME}', dir_fd=root_fd, follow_symlinks=False)
            except OSError:
                continue
            if stat.S_ISREG(info.st_mode):
                used = info.st_blocks * 512
                allocated += used
                outstanding += max(info.st_size - used, 0)
        vfs = self.statvfs(root_fd)
        free = vfs.f_bavail * vfs.f_frsize
        if allocated + self.cache_bytes > self.budget_bytes:
            raise Readiness('build_scratch_space', self.root,
                f'build caches hold {allocated / gib:.1f} GiB; a new {self.cache_bytes / gib:.0f} GiB cache '
                f'would exceed the {self.budget_bytes / gib:.0f} GiB budget')
        needed = self.headroom_bytes + outstanding + self.cache_bytes
        if free < needed:
            raise Readiness('build_scratch_space', self.root,
                f'{free / gib:.1f} GiB free; needs {needed / gib:.1f} GiB: {self.headroom_bytes / gib:.0f} GiB '
                f'headroom, {outstanding / gib:.1f} GiB other sessions may still grow, '
                f'{self.cache_bytes / gib:.0f} GiB for this cache')

    def open(self):
        import secrets
        import shutil
        root_fd = self._open_root()
        try:
            if self.session is not None:
                if not self.cache.is_file():
                    raise Readiness('build_scratch_unavailable', self.session)
                return self.session
            import fcntl
            # One admission at a time across bridges: the space check and the
            # new image must not interleave with another session's. Released
            # when root_fd closes.
            fcntl.flock(root_fd, fcntl.LOCK_EX)
            self._require_space(root_fd)
            name = self.SESSION_PREFIX + secrets.token_hex(8)
            os.mkdir(name, 0o700, dir_fd=root_fd)
            session = self.root / name
            try:
                session_fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=root_fd)
                try:
                    owner = os.open('owner.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600,
                                    dir_fd=session_fd)
                    with os.fdopen(owner, 'w') as stream:
                        json.dump({'pid': os.getpid(), 'start_time': process_start_time(os.getpid())}, stream)
                    os.mkdir('tmp', 0o700, dir_fd=session_fd)
                    image = os.open(self.IMAGE_NAME, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600,
                                    dir_fd=session_fd)
                    try:
                        os.ftruncate(image, self.cache_bytes)
                    finally:
                        os.close(image)
                finally:
                    os.close(session_fd)
                mke2fs = shutil.which('mke2fs', path='/usr/sbin:/sbin:/usr/bin:/bin')
                if not mke2fs:
                    raise Denied('build cache filesystem tool (mke2fs) is not installed')
                # Sparse and lazily initialised: disk use grows with the build.
                subprocess.run([mke2fs, '-q', '-F', '-t', 'ext4', '-m', '0',
                                '-E', f'root_owner={os.getuid()}:{os.getgid()},lazy_itable_init=1,nodiscard',
                                str(session / self.IMAGE_NAME)], env={'PATH': '/usr/sbin:/sbin:/usr/bin:/bin'},
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=60, check=True)
            except (OSError, subprocess.SubprocessError, Denied):
                shutil.rmtree(session, ignore_errors=True)
                raise Readiness('build_scratch_unavailable', self.root) from None
            self.session = session
            return session
        finally:
            os.close(root_fd)

    def volume_full(self):
        """Whether the scratch volume is (nearly) out of space."""
        try:
            vfs = self.statvfs(self.root)
        except OSError:
            return False
        return vfs.f_bavail * vfs.f_frsize < self.FULL_BYTES

    @property
    def cache(self):
        return self.session / self.IMAGE_NAME

    @property
    def tmp(self):
        return self.session / 'tmp'

    def close(self):
        import shutil
        if self.session is not None:
            shutil.rmtree(self.session, ignore_errors=True)
            self.session = None


class ReconciliationRequired(Denied):
    """Prior effects are uncertain; reads may gather evidence, writes must wait."""


class SearchLimit(Denied):
    """A search stopped at a fixed resource bound; the message is public."""


class SearchTruncated(Exception):
    """The walk/read phase ran out. Results gathered so far are still returned."""
    def __init__(self, reason):
        super().__init__(reason)
        self.reason = reason


class SearchResults(list):
    """Grep hits, plus a public note when the walk/read phase was cut short."""
    note = None


def _size_text(size):
    return f'{size // (1024 * 1024)} MiB' if size >= 1024 * 1024 else f'{size} bytes'


class SearchBudget:
    """Walk/read budget for one search (#1038): wall clock, bytes and entries.

    Matching has its own separate bound (GREP_MATCH_SECONDS). Slow disks or a
    loaded host therefore shorten the searched set, reported in a note, and
    never become a false pattern timeout.
    """
    def __init__(self):
        self.deadline = SEARCH_CLOCK() + GREP_READ_SECONDS
        self.remaining_bytes = MAX_SEARCH_BYTES

    def check_time(self):
        if SEARCH_CLOCK() >= self.deadline:
            raise SearchTruncated('time')

    def consume(self, size):
        if size > self.remaining_bytes:
            raise SearchTruncated('bytes')
        self.remaining_bytes -= size

    @staticmethod
    def note(reason, files):
        limit = {'time': f'the reading time limit ({GREP_READ_SECONDS:g} s)',
                 'bytes': f'the scan limit ({_size_text(MAX_SEARCH_BYTES)})',
                 'entries': f'the entry limit ({MAX_WALK_ENTRIES:,} entries)'}[reason]
        return (f'Grep results are partial: the search stopped at {limit} after '
                f'{files} {"file" if files == 1 else "files"}; narrow the path to search the rest.')


# Runs as `python3 -I -S -c GREP_MATCHER <bridge pid>` with an empty environment.
# It receives the pattern and already scope-checked file bytes on stdin (an
# in-memory file), never a path, so it cannot open anything itself. It exits
# with the bridge (parent death signal) and on a 3 s CPU limit even if the
# bridge is SIGKILLed. Per-line matching is the same as the earlier in-process
# implementation.
GREP_MATCHER = r'''
import ctypes, json, os, re, resource, sys
resource.setrlimit(resource.RLIMIT_CPU, (3, 3))
resource.setrlimit(resource.RLIMIT_AS, (1 << 30, 1 << 30))
if ctypes.CDLL(None).prctl(1, 9, 0, 0, 0) != 0 or os.getppid() != int(sys.argv[1]):
    sys.exit(3)
source = sys.stdin.buffer
header = json.loads(source.readline())
try:
    expression = re.compile(header['pattern'], re.IGNORECASE if header['ignore_case'] else 0)
except Exception:
    sys.stdout.write('{"invalid": true}')
    sys.exit(0)
matches = []
limit = header['limit']
index = 0
while len(matches) < limit:
    size = source.readline()
    if not size:
        break
    data = source.read(int(size))
    try:
        text = data.decode('utf-8')
    except UnicodeError:
        text = ''
    for number, line in enumerate(text.splitlines(), 1):
        if expression.search(line):
            matches.append([index, number, line[:2000]])
            if len(matches) >= limit:
                break
    index += 1
sys.stdout.write(json.dumps({'matches': matches}))
'''


FILE_TOOLS = {'Read', 'Write', 'Edit', 'Glob', 'Grep', 'LS'}
KNOWN_TOOLS = FILE_TOOLS | {'WebSearch', 'WebFetch', 'NotebookEdit'}
CONTROL_PARTS = {'.git', '.codex', '.claude', '.ssh', '.gnupg', '.aws', '.azure'}
MAX_FILE_BYTES = 8 * 1024 * 1024
# Tool paths: components below the scope root, and bytes of the absolute path
# (Linux PATH_MAX). Enforced for every read, write and search entry (#1042).
MAX_PATH_DEPTH = 32
MAX_PATH_BYTES = 4096
# One JSON-RPC line, at most 24 MiB. That admits a Write of up to
# MAX_FILE_BYTES (8 MiB) of text whose JSON escaping needs at most two bytes per
# byte: '"', '\\', newline and tab escape to two bytes, and serde_json leaves
# non-ASCII as raw UTF-8. That is 16 MiB, plus envelope and path. JSON escapes
# other control characters as six bytes (\u00XX), so text dense with them can
# exceed the cap: a 4 MiB Write of them already does. Such a line is refused with
# -32600 before parsing, so the Write has no side effects. The cap also bounds
# json.loads memory: a line of tiny objects costs about 27x its size to parse.
MAX_REQUEST_BYTES = 24 * 1024 * 1024
# Grep (#1038) runs in two phases with separate bounds.
# 1. Walk and read: GREP_READ_SECONDS of wall clock, MAX_SEARCH_BYTES of file
#    bytes, or MAX_WALK_ENTRIES entries, whichever comes first. Running out
#    returns the hits so far with a note telling the model to narrow the path.
# 2. Match: Python's re has no timeout, so matching runs in a disposable child
#    that is killed after GREP_MATCH_SECONDS of wall clock (1.5 s plus kill and
#    reap keeps a pathological match under 2 s). The wall clock includes child
#    startup and time spent waiting for a CPU, so the error is worded by the
#    child's CPU time. If it used at least GREP_CPU_BOUND_SHARE of the wall time,
#    the pattern is at fault. Otherwise the host was busy: retry or narrow the path.
GREP_READ_SECONDS = 10
GREP_MATCH_SECONDS = 1.5
MAX_SEARCH_BYTES = 64 * 1024 * 1024
MAX_WALK_ENTRIES = 10000
MAX_GREP_RESULTS = 1000
GREP_CPU_BOUND_SHARE = 0.5
# Clock for the walk/read budget; tests substitute it to simulate a slow walk.
SEARCH_CLOCK = time.monotonic
# Wall clock and reaped-children CPU accounting for the match phase; tests
# substitute them to exercise both timeout messages without real CPU load.
MATCH_CLOCK = time.monotonic


def _reaped_children_rusage():
    return resource.getrusage(resource.RUSAGE_CHILDREN)


CHILD_RUSAGE = _reaped_children_rusage


def matching_timeout_message(before, after, wall_seconds):
    """Word a matching timeout by how much CPU the killed matcher actually got."""
    cpu_seconds = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
    if cpu_seconds >= GREP_CPU_BOUND_SHARE * wall_seconds:
        return ('Grep matching stopped at its time limit while the pattern was consuming CPU; '
                'simplify the pattern, for example avoid nested repetition such as (a+)+.')
    return ('Grep matching stopped at its time limit, but the matcher spent most of it waiting '
            'for CPU on a busy host; retry the search, or narrow the path so there is less to match.')


_FILE_VERIFICATION = None


def file_verification(write_roots=()):
    """The command sandbox module, whose file-verification rule the bridge reuses.

    Both scripts run as `python3 -I`, which keeps the script directory off
    sys.path, so the sandbox is loaded by explicit path. It is always packaged
    beside the bridge (codex_tools.rs) and is already trusted to confine
    commands. Sharing its module keeps one hard-link/regular-file rule for
    bridge reads and Landlock grants (#1043). serve() loads and checks it at
    startup against the effective write roots, so a missing or model-writable
    helper fails readiness. The module is loaded once per process, but every
    caller's write roots are checked against its path.
    """
    global _FILE_VERIFICATION
    if _FILE_VERIFICATION is None:
        import importlib.util
        try:
            helper = Path(__file__).with_name('codex-command-sandbox.py').resolve(strict=True)
            _refuse_writable_helper(str(helper), write_roots)
            spec = importlib.util.spec_from_file_location('jarvis_command_sandbox', helper)
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            if not (callable(module.regular_private_file) and callable(module.verify_regular_private_file)):
                raise Denied('file verification helper is incomplete')
        except Denied:
            raise
        except Exception as exc:
            raise Denied('file verification helper is unavailable') from exc
        _FILE_VERIFICATION = (module, str(helper))
    module, helper = _FILE_VERIFICATION
    _refuse_writable_helper(helper, write_roots)
    return module


def _refuse_writable_helper(helper, write_roots):
    # String comparison of resolved paths: this runs for every file read.
    for root in write_roots:
        root = str(root)
        if helper == root or helper.startswith(root.rstrip('/') + '/'):
            raise Denied('file verification helper is in a model-writable directory')


def allowance_file_permitted(info, uid, gid, write_roots=()):
    """Whether a Read-allowance leaf may be read (#1045).

    Allowances name single files in shared directories such as /tmp, where
    another user can plant a file at an allowed name. The file must pass the
    shared regular, single-link rule and belong to `uid`. It must not be
    world-writable. Group write is accepted only for `gid`, the daemon's own
    primary group: the daemon runs with umask 0002, so every attachment it
    writes is 0664, and only the owner or root can give a file another group.
    """
    return (file_verification(write_roots).regular_private_file(info)
            and info.st_uid == uid and not info.st_mode & stat.S_IWOTH
            and (not info.st_mode & stat.S_IWGRP or info.st_gid == gid))


def literal_command_argv(command):
    # Quotes may contain ordinary punctuation as literal argument data.
    # Reject expansion syntax even inside double quotes, and never invoke
    # a shell; a policy match is made on parsed tokens, not string prefixes.
    quote = None
    escaped = False
    for ch in command:
        if ch in '\n\r\x00':
            raise Denied('multiline commands are not supported')
        if escaped:
            escaped = False
            continue
        if ch == '\\' and quote != "'":
            escaped = True
            continue
        if quote:
            if ch == quote:
                quote = None
            elif quote == '"' and ch in '$`':
                raise Denied('shell expansion is not permitted')
        elif ch in "'\"":
            quote = ch
        elif ch in ';|&<>($`)':
            raise Denied('shell operators are not permitted')
    try:
        argv = shlex.split(command)
    except ValueError as exc:
        raise Denied('invalid command quoting') from exc
    return argv


def read_only_operation(name, arguments):
    """Known read contracts, not server-supplied advisory annotations.

    Permission and pre-tool guards still run before any bridge execution.
    Unknown tools/commands remain potentially mutating. The SocialAPI verb set
    mirrors its existing mandatory read-only guard's operation contract.
    """
    # Native discovery loads tool definitions; it does not execute those tools.
    # Their own permission hooks and operation contracts still apply on invocation.
    if name in ('Read', 'Glob', 'Grep', 'LS', 'WebSearch', 'WebFetch', 'ToolSearch'):
        return True
    # These are explicit query contracts in augmentagent-mcp-memory, not
    # arbitrary server annotations or a prefix-based read exemption.
    if name in ('mcp__memory__memory_search', 'mcp__memory__memory_recent',
                'mcp__memory__search_conversation_history', 'mcp__memory__read_conversation_thread',
                'mcp__memory__search_messages', 'mcp__memory__conversation_stats'):
        return True
    if name.startswith('mcp__socialapi__'):
        verb = re.split(r'[_-]', name[len('mcp__socialapi__'):].lower(), maxsplit=1)[0]
        return verb in {'list', 'get', 'fetch', 'read', 'search', 'show', 'view',
                        'find', 'lookup', 'describe', 'count', 'check'}
    if name != 'Bash' or not isinstance(arguments.get('command'), str):
        return False
    try:
        argv = literal_command_argv(arguments['command'])
    except Denied:
        return False
    if not argv:
        return False
    program = Path(argv[0]).name
    if program in ('ls', 'printf'):
        return True
    if len(argv) < 3:
        return False
    if program == 'augmentagent':
        return (argv[1] == 'gmail' and argv[2] in {'search', 'accounts', 'list-attachments', 'get-attachment'}
            or argv[1] == 'repo-docs' and argv[2] in {'sources', 'list', 'get'})
    return program == 'aa-gh' and argv[1] in {'issue', 'pr'} and argv[2] in {'list', 'view', 'diff', 'checks'}


def external_operation(name, arguments):
    """Integration operations dispatched by this broker, shared by both providers."""
    if name.startswith('mcp__'):
        return True
    if name != 'Bash' or not isinstance(arguments.get('command'), str):
        return False
    try:
        argv = literal_command_argv(arguments['command'])
    except Denied:
        # The broker rejects these commands. Primary hooks cannot prove they
        # are local, so do not permit them to replay a completed effect.
        return True
    return bool(argv) and Path(argv[0]).name in ('augmentagent', 'aa-gh')


class CompletedOperation(Denied):
    pass


def same_operation(row, name, arguments):
    if row['tool'] != name:
        return False
    def identity(values):
        if name != 'Bash' or not isinstance(values.get('command'), str):
            return values
        try:
            argv = literal_command_argv(values['command'])
        except Denied:
            return values
        # Quotes, whitespace, the display description and execution timeout
        # do not change the external action. Preserve all other inputs.
        return {**{key: value for key, value in values.items()
                   if key not in ('command', 'timeout', 'description')}, 'command': argv}
    return identity(row['arguments']) == identity(arguments)


class HandoffJournal:
    """Durable operation receipts. Uncertain effects require reconciliation.

    The journal belongs to one logical request and lives outside tool scopes.
    The lock covers execution, so concurrent/restarted brokers cannot both
    perform the same operation. Errors never imply that an external effect did
    not happen. Completed receipts may be returned without executing again.
    """
    def __init__(self, path):
        self.path = Path(path)

    @contextmanager
    def locked(self):
        import fcntl
        descriptor = None
        try:
            info = self.path.parent.stat()
            if info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise Denied('handoff directory must be owner-private')
            descriptor = os.open(str(self.path) + '.lock',
                os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
            info = os.fstat(descriptor)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise Denied('untrusted handoff lock')
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except (OSError, Denied) as exc:
            if descriptor is not None:
                os.close(descriptor)
            raise Denied('handoff state unavailable; reconciliation required') from exc
        try:
            yield
        finally:
            os.close(descriptor)

    def load(self):
        try:
            descriptor = os.open(self.path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        except FileNotFoundError:
            return {'version': 1, 'operations': []}
        except OSError as exc:
            raise Denied('untrusted handoff state') from exc
        try:
            with os.fdopen(descriptor) as stream:
                info = os.fstat(stream.fileno())
                if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                        or info.st_mode & 0o077 or info.st_size > 16 * 1024 * 1024):
                    raise Denied('untrusted handoff state')
                state = json.load(stream)
            if not isinstance(state, dict) or state.get('version') != 1 or not isinstance(state.get('operations'), list):
                raise Denied('invalid handoff state')
            for row in state['operations']:
                if (not isinstance(row, dict) or not isinstance(row.get('tool'), str)
                        or not isinstance(row.get('arguments'), dict)
                        or row.get('status') not in ('started', 'completed', 'not_applied')
                        or (row['status'] == 'not_applied' and (
                            not isinstance(row.get('reconciliation'), dict)
                            or row['reconciliation'].get('outcome') != 'not_applied'
                            or not row['reconciliation'].get('evidence')))
                        or (row['status'] == 'completed' and 'result' not in row)):
                    raise Denied('invalid handoff operation')
            return state
        except (OSError, ValueError) as exc:
            raise Denied('handoff state unreadable; reconciliation required') from exc

    def save(self, state):
        import tempfile
        payload = json.dumps(state, ensure_ascii=True, allow_nan=False).encode()
        if len(payload) > 16 * 1024 * 1024:
            raise Denied('handoff state exceeds limit; reconciliation required')
        descriptor, temporary = tempfile.mkstemp(prefix='.handoff-', dir=self.path.parent)
        try:
            with os.fdopen(descriptor, 'wb') as stream:
                stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
            directory = os.open(self.path.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    @staticmethod
    def fingerprint(row):
        import hashlib
        return hashlib.sha256(json.dumps(row, sort_keys=True, ensure_ascii=True,
            separators=(',', ':'), allow_nan=False).encode()).hexdigest()

    def inspect(self):
        # Deliberately omit arguments, results and evidence from operator status.
        with self.locked():
            return [{'index': index, 'tool': row['tool'], 'status': row['status'],
                'fingerprint': self.fingerprint(row)}
                for index, row in enumerate(self.load()['operations'])]

    def reconcile(self, decision):
        """Owner-only recovery API, never advertised as a model tool.

        Evidence is the operator's attestation after inspecting the authoritative
        service. No absence inference is made from transport failures/timeouts.
        The same lifecycle lock as the provider launcher prevents a new invocation
        starting while a decision is committed. Active or unverified cleanup must
        be recovered through the normal supervisor path first.
        """
        import datetime
        import fcntl
        if (not isinstance(decision, dict)
                or type(decision.get('index')) is not int or decision['index'] < 0
                or not isinstance(decision.get('fingerprint'), str)
                or decision.get('outcome') not in ('completed', 'not_applied')
                or not isinstance(decision.get('evidence'), str)
                or not decision['evidence'].strip() or len(decision['evidence']) > 16384):
            raise Denied('invalid reconciliation decision')
        if decision['outcome'] == 'completed':
            result = decision.get('result')
            if (not isinstance(result, dict) or result.get('isError')
                    or not isinstance(result.get('content'), list)):
                raise Denied('completion requires a successful observed tool receipt')
        elif 'result' in decision:
            raise Denied('absence decision cannot include a completion receipt')
        if not self.path.is_absolute() or self.path.parent.resolve() != self.path.parent:
            raise Denied('untrusted reconciliation path')
        with self.locked():
            # The request-directory trust check and journal lock above also cover
            # this lifecycle-lock location. Both locks stay held through fsync.
            descriptor = os.open(self.path.with_suffix('.lifecycle-lock'),
                os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
            try:
                info = os.fstat(descriptor)
                if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                    raise Denied('untrusted lifecycle lock')
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                if os.path.lexists(self.path.with_suffix('.active')):
                    raise Denied('request active or cleanup unverified')
                state = self.load()
                index = decision['index']
                if index >= len(state['operations']):
                    raise Denied('unknown reconciliation operation')
                row = state['operations'][index]
                if row['status'] != 'started' or self.fingerprint(row) != decision['fingerprint']:
                    raise Denied('reconciliation is stale or operation already resolved')
                row['status'] = decision['outcome']
                if decision['outcome'] == 'completed':
                    row['result'] = decision['result']
                row['reconciliation'] = {'outcome': decision['outcome'],
                    'evidence': decision['evidence'], 'prior_fingerprint': decision['fingerprint'],
                    'recorded_at': datetime.datetime.now(datetime.timezone.utc).isoformat()}
                self.save(state)
            except OSError as error:
                raise Denied('reconciliation unavailable while lifecycle state is busy or untrusted') from error
            finally:
                os.close(descriptor)

    def execute(self, name, arguments, action):
        with self.locked():
            state = self.load()
            for row in reversed(state['operations']):
                if same_operation(row, name, arguments):
                    if row['status'] == 'not_applied':
                        break  # operator proved absence; preserve this row and append a new attempt
                    if row['status'] != 'completed':
                        raise ReconciliationRequired('latest operation requires reconciliation')
                    return row['result']
            if any(row['status'] == 'started' for row in state['operations']):
                raise ReconciliationRequired('uncertain operation requires reconciliation before further mutations')
            row = {'tool': name, 'arguments': arguments, 'status': 'started'}
            state['operations'].append(row)
            self.save(state)  # must reach durable storage BEFORE the effect
            result = action()
            if not (isinstance(result, dict) and result.get('isError')):
                row.update(status='completed', result=result)
                self.save(state)
            return result

    def observe_hook(self, event):
        name = event.get('tool_name')
        arguments = event.get('tool_input')
        identifier = event.get('tool_use_id')
        phase = event.get('hook_event_name')
        if (not isinstance(name, str) or not isinstance(arguments, dict)
                or not isinstance(identifier, str) or not identifier
                or phase not in ('PreToolUse', 'PostToolUse', 'PostToolUseFailure')):
            raise Denied('invalid primary operation event')
        if read_only_operation(name, arguments):
            return
        with self.locked():
            state = self.load()
            matching = [row for row in state['operations'] if row.get('primary_id') == identifier]
            if phase == 'PreToolUse':
                if matching or any(row['status'] == 'started' for row in state['operations']):
                    raise ReconciliationRequired('unfinished primary operation requires reconciliation')
                if external_operation(name, arguments) and any(
                        same_operation(row, name, arguments)
                        and row['status'] == 'completed' for row in state['operations']):
                    raise CompletedOperation('external operation already completed for this request; use its prior receipt instead of repeating it')
                state['operations'].append({'tool': name, 'arguments': arguments,
                    'primary_id': identifier, 'status': 'started'})
                self.save(state)
                return
            if len(matching) != 1 or matching[0]['tool'] != name or matching[0]['arguments'] != arguments:
                raise Denied('unmatched primary result requires reconciliation')
            row = matching[0]
            if row['status'] == 'not_applied':
                raise Denied('late primary result conflicts with operator reconciliation')
            if phase == 'PostToolUseFailure':
                return  # failure is not evidence of absence of an effect
            if 'tool_response' not in event:
                raise Denied('missing primary result requires reconciliation')
            response = event['tool_response']
            if isinstance(response, dict) and response.get('isError'):
                return
            if not (isinstance(response, dict) and isinstance(response.get('content'), list)):
                response = {'content': [{'type': 'text', 'text': json.dumps(response)}]}
            if row['status'] == 'completed' and row['result'] != response:
                raise Denied('conflicting primary result requires reconciliation')
            row.update(status='completed', result=response)
            self.save(state)


class Policy:
    def __init__(self, config):
        self.cwd = Path(config['cwd']).resolve(strict=True)
        self.environment = {k: os.environ[k] for k in
                            ('HOME', 'PATH', 'USER', 'LOGNAME', 'LANG', 'TERM',
                             'DBUS_SESSION_BUS_ADDRESS', 'XDG_RUNTIME_DIR') if k in os.environ}
        self.environment.update(config.get('environment', {}))
        self.settings = config.get('settings', {})
        if not isinstance(self.settings, dict) or set(self.settings) - {'hooks', 'mcpServers'}:
            raise Denied('unsupported settings')
        hooks = self.settings.get('hooks', {})
        if not isinstance(hooks, dict) or set(hooks) - {'PreToolUse'}:
            raise Denied('unsupported hook event')
        self.hooks = []
        for group in hooks.get('PreToolUse', []):
            matcher = re.compile(group.get('matcher', '.*'))
            for hook in group.get('hooks', []):
                if hook.get('type') != 'command' or hook.get('async', False):
                    raise Denied('unsupported enforcement hook')
                command = shlex.split(hook['command'])
                if not command:
                    raise Denied('empty enforcement hook')
                self.hooks.append((matcher, command))
        self.read_roots = self._roots(config.get('read_roots', []))
        self.write_roots = self._roots(config.get('write_roots', []))
        # #1045: single files outside the roots that Read alone may open.
        self.read_allowances = self._allowances(config.get('read_allowances', []))
        self._node_install_cache = None
        self.build_vm_config = config.get('build_vm_config')
        # #1036: VM build scratch root and default build timeout, from the
        # daemon's policy (codex runs with a cleared environment).
        # A root the model could write through makes builds unavailable; the
        # bridge and its other tools still start.
        scratch = config.get('build_scratch_dir')
        refused = scratch is not None and (not Path(scratch).is_absolute() or any(
            Path(scratch).resolve() == root or root in Path(scratch).resolve().parents for root in self.write_roots))
        self._scratch = BuildScratch(scratch, refused=refused)
        configured_timeout = config.get('build_timeout_secs')
        self.build_timeout = min(max(int(configured_timeout), 1), COMMAND_TIMEOUT_MAX) \
            if configured_timeout is not None else BUILD_TIMEOUT_DEFAULT
        # #1041: which runner executes cargo/npm/npx. A VM configuration
        # selects the VM; only the explicit operator opt-out selects the host.
        # Anything else (no configuration found) leaves builds unavailable.
        requested = config.get('build_runner')
        if requested not in (None, 'vm', 'host', 'unavailable'):
            raise Denied('unsupported build runner')
        self.build_runner = ('vm' if self.build_vm_config
                             else 'host' if requested == 'host' else None)
        if self.build_vm_config:
            path = Path(self.build_vm_config)
            if not path.is_absolute() or any(path.resolve() == root or root in path.resolve().parents
                                             for root in self.write_roots):
                raise Denied('VM configuration must be outside model-writable scopes')
        self.handoff = None
        if config.get('handoff_path'):
            path = Path(config['handoff_path'])
            resolved = path.resolve()
            if (not path.is_absolute() or any(resolved == root or root in resolved.parents
                                              for root in self.read_roots + self.write_roots)
                    or self._read_allowance(str(path)) or self._read_allowance(str(resolved))):
                raise Denied('handoff state must be outside model tool scopes')
            self.handoff = HandoffJournal(path)
        self.tools = frozenset(config.get('allowed_tools', []))
        self.command_patterns = []
        for tool in self.tools:
            if tool in KNOWN_TOOLS:
                # Web tools are native Codex capabilities. Every other local
                # contract must have an executable bridge schema, not merely
                # a recognized name that vanishes from tools/list.
                if tool not in TOOL_SCHEMAS and tool not in {'WebSearch', 'WebFetch'}:
                    raise Denied('unsupported local tool in capability profile')
                continue
            if tool.startswith('mcp__') and len(tool.split('__', 2)) == 3:
                continue
            if tool.startswith('Bash(') and tool.endswith(')'):
                pattern = tool[5:-1]
                prefix = pattern.endswith('*')
                if prefix:
                    pattern = pattern[:-1].removesuffix(':').rstrip()
                if '*' in pattern or not pattern:
                    raise Denied('unsupported command pattern')
                self.command_patterns.append((shlex.split(pattern), prefix))
                continue
            raise Denied('unknown tool in capability profile')

    @staticmethod
    def _roots(roots):
        result = []
        for root in roots:
            path = Path(root).resolve(strict=True)
            if not path.is_dir() or path == Path('/'):
                raise Denied('scope must be a specific directory')
            result.append(path)
        return result

    @staticmethod
    def _allowances(entries):
        """Validate Read allowances: exactly Read, a normalized absolute directory, a name pattern."""
        if not isinstance(entries, list):
            raise Denied('read allowances must be a list')
        result = []
        for entry in entries:
            if not isinstance(entry, dict) or set(entry) != {'tools', 'directory', 'name_pattern'}:
                raise Denied('read allowance must declare tools, directory and name_pattern only')
            if entry['tools'] != ['Read']:
                raise Denied('read allowances grant Read only')
            directory, pattern = entry['directory'], entry['name_pattern']
            if (not isinstance(directory, str) or '\x00' in directory or not directory.startswith('/')
                    or any(part in ('', '.', '..') for part in directory.split('/')[1:])):
                raise Denied('read allowance directory must be absolute and normalized')
            if not isinstance(pattern, str) or not pattern:
                raise Denied('read allowance requires a name pattern')
            try:
                result.append((directory, re.compile(pattern)))
            except re.error as exc:
                raise Denied('invalid read allowance pattern') from exc
        return result

    def _read_allowance(self, name):
        """(directory, leaf) when `name` is exactly one allowed file, else None.

        The path must already be absolute and normalized: no `.`/`..` or empty
        component, so the file checked is the file opened, and nothing nested
        below the allowed directory matches.
        """
        if (not isinstance(name, str) or '\x00' in name or not name.startswith('/')
                or len(os.fsencode(name)) > MAX_PATH_BYTES
                or any(part in ('', '.', '..') for part in name.split('/')[1:])):
            return None
        directory, _, leaf = name.rpartition('/')
        for allowed, expression in self.read_allowances:
            if directory == allowed and expression.fullmatch(leaf):
                return directory, leaf
        return None

    def _read_allowed(self, directory, leaf):
        """Read one allowed file, typically in a shared directory such as /tmp.

        Every directory component is opened with O_NOFOLLOW. The directory
        must belong to root or this user and be writable by no one else unless
        sticky, and the leaf must satisfy `allowance_file_permitted`.
        """
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
        uid = os.getuid()
        descriptors = []
        try:
            try:
                descriptors.append(os.open('/', flags))
                for part in directory.split('/')[1:]:
                    descriptors.append(os.open(part, flags, dir_fd=descriptors[-1]))
                info = os.fstat(descriptors[-1])
            except OSError as exc:
                raise Denied('path cannot be accessed safely') from exc
            if info.st_uid not in (0, uid) or (info.st_mode & 0o022 and not info.st_mode & stat.S_ISVTX):
                raise Denied('read allowance directory is writable by other users')
            return self._read_at(descriptors[-1], leaf, owner=(uid, os.getgid()))
        finally:
            for fd in reversed(descriptors):
                os.close(fd)

    def before(self, tool, arguments):
        for matcher, command in self.hooks:
            if not matcher.fullmatch(tool):
                continue
            try:
                outcome = subprocess.run(command, input=json.dumps({
                    'tool_name': tool, 'tool_input': arguments, 'cwd': str(self.cwd)}),
                    text=True, capture_output=True, timeout=10,
                    cwd=self.cwd, env=self.environment)
                if outcome.returncode:
                    raise Denied('enforcement hook failed')
                if outcome.stdout.strip():
                    decision = json.loads(outcome.stdout)
                    if not isinstance(decision, dict) or not isinstance(decision.get('hookSpecificOutput', {}), dict):
                        raise Denied('invalid enforcement decision')
                    if (decision.get('decision') == 'block' or
                        decision.get('hookSpecificOutput', {}).get('permissionDecision') == 'deny'):
                        raise Denied('enforcement hook denied the operation')
            except (OSError, subprocess.TimeoutExpired, ValueError) as exc:
                raise Denied('enforcement hook failed closed') from exc

    def require(self, tool):
        if tool in self.tools or (tool == 'Bash' and self.command_patterns):
            return
        if tool.startswith('mcp__'):
            server, separator, name = tool[5:].partition('__')
            if separator and name and f'mcp__{server}__*' in self.tools:
                return
        raise Denied('tool is not permitted by this profile')

    def _relative(self, name, writing=False):
        if not isinstance(name, str) or not name or '\x00' in name:
            raise Denied('invalid path')
        # String form of Path(os.path.abspath(cwd / name)).relative_to(root):
        # abspath output is normalized and roots are resolved without a
        # trailing separator. Searches call this per entry, so avoid pathlib.
        candidate = os.path.abspath(os.path.join(str(self.cwd), name))
        if len(os.fsencode(candidate)) > MAX_PATH_BYTES:
            raise Denied('path exceeds length limit')
        roots = self.write_roots if writing else self.read_roots
        for root in sorted(roots, key=lambda p: len(p.parts), reverse=True):
            prefix = str(root)
            if candidate == prefix:
                raise Denied('operation requires a file')
            if not candidate.startswith(prefix + '/'):
                continue
            parts = tuple(candidate[len(prefix) + 1:].split('/'))
            if len(parts) > MAX_PATH_DEPTH:
                raise Denied('path exceeds depth limit')
            if any(p in CONTROL_PARTS or p == '.env' or p.startswith('.env.')
                   for p in parts):
                raise Denied('credential and control paths are excluded')
            return root, parts
        raise Denied('path is outside the permitted workspace')

    @contextmanager
    def parent(self, name, writing=False):
        root, parts = self._relative(name, writing)
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
        descriptors = []
        try:
            fd = os.open(root, flags)
            descriptors.append(fd)
            for part in parts[:-1]:
                if writing:
                    try:
                        os.mkdir(part, mode=0o700, dir_fd=fd)
                    except FileExistsError:
                        pass
                fd = os.open(part, flags, dir_fd=fd)
                descriptors.append(fd)
            yield fd, parts[-1]
        except OSError as exc:
            raise Denied('path cannot be accessed safely') from exc
        finally:
            for fd in reversed(descriptors):
                os.close(fd)

    def read(self, name, offset=None, limit=None, pages=None):
        self.require('Read')
        allowed = self._read_allowance(name)
        data = self._read_allowed(*allowed) if allowed else self._read_bytes(name)
        if data.startswith(b'%PDF-'):
            if offset is not None or limit is not None:
                raise Denied('use PDF page ranges instead of text line ranges')
            return self.read_pdf(data, pages)
        if pages is not None:
            raise Denied('page ranges apply only to PDF documents')
        mime = None
        if data.startswith(b'\x89PNG\r\n\x1a\n'):
            mime = 'image/png'
        elif data.startswith(b'\xff\xd8\xff'):
            mime = 'image/jpeg'
        elif data.startswith((b'GIF87a', b'GIF89a')):
            mime = 'image/gif'
        elif data.startswith(b'RIFF') and data[8:12] == b'WEBP':
            mime = 'image/webp'
        if mime:
            if offset is not None or limit is not None:
                raise Denied('text line ranges do not apply to images')
            import base64
            return {'content': [{'type': 'image', 'mimeType': mime,
                                 'data': base64.b64encode(data).decode('ascii')}]}
        text = data.decode('utf-8')
        if offset is None and limit is None:
            return text
        for value in (offset, limit):
            if value is not None and (type(value) is not int or value < 1):
                raise Denied('read offset and limit must be positive integers')
        start = (offset or 1) - 1
        return ''.join(text.splitlines(keepends=True)[start:None if limit is None else start + limit])

    def read_pdf(self, data, pages):
        import base64
        import tempfile
        import sys
        import signal
        import time
        selected = None
        if pages is not None:
            if not isinstance(pages, str) or not re.fullmatch(r'[1-9][0-9]{0,5}(?:-[1-9][0-9]{0,5})?', pages):
                raise Denied('PDF pages must be a page number or inclusive range')
            parts = [int(part) for part in pages.split('-')]
            first, last = parts[0], parts[-1]
            if last < first or last - first + 1 > 20:
                raise Denied('PDF reads support at most 20 pages per call')
            selected = (first, last)
        helper = Path(__file__).with_name('codex-command-sandbox.py')
        executables = [Path('/usr/bin/pdfinfo'), Path('/usr/bin/pdftoppm')]
        for executable in [helper, *executables]:
            if not executable.is_file() or any(executable.resolve() == root or root in executable.resolve().parents
                                              for root in self.write_roots):
                raise Denied('trusted PDF renderer or sandbox is unavailable')
        with tempfile.TemporaryDirectory(prefix='jarvis-pdf-') as temporary:
            root = Path(temporary)
            source = root / 'source'; source.mkdir(mode=0o700)
            output = root / 'output'; output.mkdir(mode=0o700)
            document = source / 'document.pdf'
            document.write_bytes(data)
            policy = root / 'policy.json'
            policy.write_text(json.dumps({'cwd': str(output), 'read_roots': [str(source)],
                'write_roots': [str(output)], 'runtime_reads': ['/etc/fonts'],
                'memory_limit_bytes': 512 * 1024 * 1024, 'file_limit_bytes': MAX_FILE_BYTES}))
            policy.chmod(0o600)
            deadline = time.monotonic() + 60

            def render(argv):
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise Denied('PDF rendering timed out')
                with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
                    process = subprocess.Popen([sys.executable, '-I', str(helper), str(policy), *argv],
                        cwd=output, env={'PATH': os.defpath, 'HOME': str(output), 'LANG': 'C.UTF-8'},
                        stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
                    try:
                        try:
                            status = process.wait(timeout=min(30, remaining))
                        except subprocess.TimeoutExpired as exc:
                            raise Denied('PDF rendering timed out') from exc
                        if status or os.fstat(stdout.fileno()).st_size > MAX_FILE_BYTES:
                            raise Denied('PDF rendering failed or exceeded its resource limit')
                        stdout.seek(0)
                        return stdout.read(MAX_FILE_BYTES + 1)
                    finally:
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        process.wait()

            info = render(['/usr/bin/pdfinfo', str(document)]).decode('utf-8', errors='replace')
            match = re.search(r'^Pages:\s+([0-9]+)\s*$', info, flags=re.MULTILINE)
            if not match or int(match[1]) < 1:
                raise Denied('PDF page count is unavailable')
            count = int(match[1])
            first, last = selected or (1, count)
            if last > count or last - first + 1 > 20:
                raise Denied('select an existing range of at most 20 PDF pages')
            content = []
            total = 0
            for page in range(first, last + 1):
                render(['/usr/bin/pdftoppm', '-f', str(page), '-l', str(page), '-singlefile',
                        '-scale-to', '1600', '-png', str(document), str(output / 'page')])
                with os.fdopen(os.open(output / 'page.png', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), 'rb') as stream:
                    info = os.fstat(stream.fileno())
                    if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_FILE_BYTES - total:
                        raise Denied('PDF render exceeds output size limit; request fewer pages')
                    image = stream.read(MAX_FILE_BYTES - total + 1)
                total += len(image)
                if total > MAX_FILE_BYTES or not image.startswith(b'\x89PNG\r\n\x1a\n'):
                    raise Denied('PDF renderer returned invalid or oversized image data')
                content.extend([{'type': 'text', 'text': f'Page {page} of {count}'},
                    {'type': 'image', 'mimeType': 'image/png', 'data': base64.b64encode(image).decode('ascii')}])
                (output / 'page.png').unlink()
            return {'content': content}

    def _read(self, name):
        return self._read_bytes(name).decode('utf-8')

    def _read_bytes(self, name):
        with self.parent(name) as (parent, leaf):
            return self._read_at(parent, leaf)

    def _read_at(self, directory, leaf, owner=None):
        """Read `leaf` in an already verified, O_NOFOLLOW-opened directory.

        With `owner` (uid, gid), the file must also satisfy `allowance_file_permitted`.
        """
        verification = file_verification(self.write_roots)
        try:
            fd = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                         dir_fd=directory)
            with os.fdopen(fd, 'rb') as stream:
                # Verified on the opened descriptor: regular, one hard link.
                metadata = verification.verify_regular_private_file(stream.fileno())
                if owner is not None and metadata is not None and not allowance_file_permitted(
                        metadata, *owner, self.write_roots):
                    metadata = None
                if metadata is None or metadata.st_size > MAX_FILE_BYTES:
                    raise Denied('read requires a bounded regular file')
                data = stream.read(MAX_FILE_BYTES + 1)
        except OSError as exc:
            raise Denied('path cannot be accessed safely') from exc
        if len(data) > MAX_FILE_BYTES:
            raise Denied('file exceeds size limit')
        return data

    def _write(self, name, content):
        self._write_bytes(name, content.encode('utf-8'))

    def _write_bytes(self, name, data):
        if len(data) > MAX_FILE_BYTES:
            raise Denied('file exceeds size limit')
        verification = file_verification(self.write_roots)
        with self.parent(name, writing=True) as (parent, leaf):
            try:
                existing = os.stat(leaf, dir_fd=parent, follow_symlinks=False)
                if not verification.regular_private_file(existing):
                    raise Denied('write requires a regular file')
                mode = stat.S_IMODE(existing.st_mode) & 0o777
            except FileNotFoundError:
                mode = 0o600
            temporary = '.jarvis-write-' + uuid.uuid4().hex
            try:
                fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL |
                             os.O_NOFOLLOW, mode, dir_fd=parent)
                with os.fdopen(fd, 'wb') as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
                os.replace(temporary, leaf, src_dir_fd=parent, dst_dir_fd=parent)
            finally:
                try:
                    os.unlink(temporary, dir_fd=parent)
                except FileNotFoundError:
                    pass

    def write(self, name, content):
        self.require('Write')
        self._write(name, content)

    def edit(self, name, old, new, replace_all=False):
        self.require('Edit')
        # Edit stays inside write scopes; Read allowances never apply to it.
        self._relative(name, writing=True)
        content = self.read(name)
        if not old or (content.count(old) != 1 and not replace_all) or old not in content:
            raise Denied('edit must identify exactly one match')
        self._write(name, content.replace(old, new, -1 if replace_all else 1))

    def files(self, path='.', excluded_dirs=()):
        for absolute, relative, _, _ in self._walk(path, excluded_dirs):
            yield absolute, relative

    def _walk(self, path='.', excluded_dirs=(), budget=None):
        """Yield (absolute, relative, directory fd, name) for scoped regular files.

        The descriptor is the entry's verified parent directory and stays open
        only until the generator resumes.
        """
        base = Path(os.path.abspath(self.cwd / path))
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
        verification = file_verification(self.write_roots)
        count = 0

        def sorted_names(fd):
            with os.scandir(fd) as entries:
                return iter(sorted(entry.name for entry in entries))

        # A synthetic leaf lets parent() verify and open every directory
        # component without following symlinks, including the search root.
        with self.parent(str(base / '__search_scope__')) as (root_fd, _):
            # Explicit depth-first stack in sorted pre-order: a deep tree can
            # neither exhaust the interpreter stack nor leak descriptors (#1042).
            stack = [(root_fd, Path('.'), sorted_names(root_fd))]
            try:
                while stack:
                    fd, relative, names = stack[-1]
                    name = next(names, None)
                    if name is None:
                        stack.pop()
                        if fd != root_fd:
                            os.close(fd)
                        continue
                    count += 1
                    if count > MAX_WALK_ENTRIES:
                        if budget is not None:
                            raise SearchTruncated('entries')
                        raise Denied('search exceeds entry limit; narrow its path')
                    if budget is not None:
                        budget.check_time()
                    child = relative / name
                    absolute = str(base / child)
                    try:
                        self._relative(absolute)
                    except Denied:
                        continue
                    info = os.stat(name, dir_fd=fd, follow_symlinks=False)
                    if stat.S_ISDIR(info.st_mode):
                        if name in excluded_dirs:
                            continue
                        child_fd = os.open(name, flags, dir_fd=fd)
                        try:
                            stack.append((child_fd, child, sorted_names(child_fd)))
                        except BaseException:
                            os.close(child_fd)
                            raise
                    elif verification.regular_private_file(info):
                        yield absolute, str(child), fd, name
            finally:
                for fd, _, _ in stack:
                    if fd != root_fd:
                        os.close(fd)

    def glob(self, pattern, path='.'):
        self.require('Glob')
        if not isinstance(pattern, str) or len(pattern) > 1024:
            raise Denied('invalid glob pattern')
        return [relative for _, relative in self.files(path)
                if fnmatch.fnmatchcase(relative, pattern) or
                (pattern.startswith('**/') and fnmatch.fnmatchcase(relative, pattern[3:]))]

    def grep(self, pattern, path='.', ignore_case=False):
        """Line search in two bounded phases (#1038).

        Walk and read: the bridge walks and reads files itself, through the
        scoped, symlink- and hard-link-refusing path, into an in-memory file.
        The phase stops at SearchBudget's limits and keeps what it has.
        Match: only the model-supplied regular expression runs elsewhere, in a
        short-lived child killed after GREP_MATCH_SECONDS, so no pattern can
        stall this process.
        """
        import sys
        self.require('Grep')
        if not isinstance(pattern, str) or len(pattern) > 1024:
            raise Denied('invalid search pattern')
        budget = SearchBudget()
        candidate = Path(os.path.abspath(self.cwd / path))
        self._relative(str(candidate / '__search_scope__') if candidate.is_dir() else str(candidate))
        if candidate.is_file():
            candidates = [(str(candidate), candidate.name, None, None)]
        else:
            candidates = self._walk(path, budget=budget)
        source = os.memfd_create('jarvis-grep-input', os.MFD_CLOEXEC)
        output = os.memfd_create('jarvis-grep-output', os.MFD_CLOEXEC)
        process = None
        try:
            def emit(data):
                view = memoryview(data)
                while view:
                    view = view[os.write(source, view):]

            emit(json.dumps({'pattern': pattern, 'ignore_case': bool(ignore_case),
                             'limit': MAX_GREP_RESULTS}).encode() + b'\n')
            paths = []
            truncated = None
            try:
                for absolute, relative, directory, leaf in candidates:
                    budget.check_time()
                    try:
                        # Walked entries are read through their verified parent
                        # descriptor, never by re-resolving a path string.
                        data = (self._read_bytes(absolute) if directory is None
                                else self._read_at(directory, leaf))
                    except Denied:
                        continue
                    budget.consume(len(data))
                    emit(b'%d\n' % len(data))
                    emit(data)
                    paths.append(relative)
            except SearchTruncated as stop:
                truncated = stop.reason
            finally:
                close = getattr(candidates, 'close', None)
                if close:
                    close()
            os.lseek(source, 0, os.SEEK_SET)
            started = MATCH_CLOCK()
            process = subprocess.Popen([sys.executable, '-I', '-S', '-c', GREP_MATCHER, str(os.getpid())],
                stdin=source, stdout=output, stderr=subprocess.DEVNULL, cwd='/', env={})
            # Snapshot after Popen, whose own cleanup may reap older children.
            # Dispatch is serial, so the only child reaped before the second
            # snapshot is this matcher.
            usage_before = CHILD_RUSAGE()
            try:
                status = process.wait(timeout=GREP_MATCH_SECONDS)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise SearchLimit(matching_timeout_message(
                    usage_before, CHILD_RUSAGE(), MATCH_CLOCK() - started)) from None
            if status != 0:
                raise Denied('search expression could not be evaluated')
            os.lseek(output, 0, os.SEEK_SET)
            raw = b''
            while len(raw) <= 4 * MAX_FILE_BYTES:
                chunk = os.read(output, 1024 * 1024)
                if not chunk:
                    break
                raw += chunk
            if len(raw) > 4 * MAX_FILE_BYTES:
                raise Denied('search output exceeds limit')
            outcome = json.loads(raw)
            if outcome.get('invalid'):
                raise Denied('invalid search expression')
            results = SearchResults({'path': paths[index], 'line': number, 'text': text}
                                    for index, number, text in outcome['matches'])
            if truncated:
                results.note = SearchBudget.note(truncated, len(paths))
            return results
        finally:
            if process is not None:
                if process.poll() is None:
                    process.kill()
                process.wait()
            os.close(source)
            os.close(output)

    def check_service_argv(self, argv):
        name = Path(argv[0]).name
        if name == 'augmentagent' and len(argv) > 2 and argv[1] == 'gmail':
            if argv[2] not in {'search', 'list-attachments', 'get-attachment', 'accounts',
                               'compose', 'update-draft'}:
                raise Denied('this Gmail operation is not available through the approval-gated agent')
        inputs = {'--body-file', '--reply-to-body-file', '--attach', '--attachment',
                  '--input', '-F', '--body-file'}
        outputs = {'--out', '--output'}
        forbidden = {'--db', '--wiki-dir', '--config', '--config-file'}
        index = 1
        while index < len(argv):
            token = argv[index]
            flag, sep, value = token.partition('=')
            if token.startswith('-F') and token != '-F':
                flag, sep, value = '-F', '=', token[2:]
            if flag in forbidden:
                raise Denied('service configuration overrides are not permitted')
            if flag in inputs | outputs:
                if not sep:
                    index += 1
                    if index >= len(argv):
                        raise Denied('missing file argument')
                    value = argv[index]
                if value == '-':
                    raise Denied('use a scoped file instead of service stdin')
                self._relative(value, writing=flag in outputs)
                resolved = (self.cwd / value).resolve()
                self._relative(str(resolved), writing=flag in outputs)
            index += 1

    def mark_command_started(self):
        """The command's process (host or VM) is about to start."""
        self._command_started = True

    def run_command(self, command, timeout=None):
        # #1041: the runner is decided before anything executes. It leads the
        # outcome, so truncation cannot drop it, and it is attached to any
        # failure raised after the process started (a timeout, a broker
        # failure mid-build). Refusals before that carry no runner.
        self._command_started = False
        argv = self.command_argv(command)
        build = Path(argv[0]).name in ('cargo', 'npm', 'npx')
        if build and self.build_runner is None:
            raise Readiness('build_vm_unavailable')
        runner = self.build_runner if build else 'host'
        if timeout is None:
            timeout = self.build_timeout if build else COMMAND_TIMEOUT_DEFAULT
        try:
            outcome = (self.run_vm_build(argv, timeout) if runner == 'vm'
                       else self._run_host_command(argv, build, timeout))
        except BaseException as error:
            if self._command_started:
                error.jarvis_runner = runner
            raise
        return {'runner': runner, **outcome}

    def _run_host_command(self, argv, build, timeout):
        import shutil
        import tempfile
        import signal
        import time
        import sys
        executable = shutil.which(argv[0], path=self.environment.get('PATH', os.defpath))
        if not executable:
            raise Denied('configured command is not installed')
        executable = Path(executable).absolute()
        resolved_executable = executable.resolve(strict=True)
        if any(resolved_executable == root or root in resolved_executable.parents for root in self.write_roots):
            raise Denied('command executable is in a model-writable directory')
        service = Path(argv[0]).name in ('augmentagent', 'aa-gh')
        if service:
            self.check_service_argv(argv)
        argv[0] = str(executable)
        helper = Path(__file__).with_name('codex-command-sandbox.py')
        if not helper.is_file():
            raise Denied('command sandbox helper is unavailable')
        with tempfile.TemporaryDirectory(prefix='jarvis-command-') as temporary:
            directory = Path(temporary)
            snapshot = BuildSnapshot(self, directory / 'workspace') if build else None
            run_cwd = snapshot.root if snapshot else self.cwd
            run_env = dict(self.environment)
            runtime_reads = [str(resolved_executable)]
            dependency_roots = []
            if Path(argv[0]).name == 'git':
                if any(arg.split('=', 1)[0] in ('--ext-diff', '--textconv') for arg in argv[1:]):
                    raise Denied('external Git diff helpers are not permitted')
                git_env = {'PATH': self.environment.get('PATH', os.defpath),
                    'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null',
                    'GIT_OPTIONAL_LOCKS': '0'}
                paths = subprocess.run([str(executable), 'rev-parse', '--absolute-git-dir', '--git-common-dir'],
                    cwd=self.cwd, env=git_env, capture_output=True, text=True, timeout=10)
                if paths.returncode:
                    raise Denied('Git metadata is unavailable for the scoped workspace')
                metadata = [(self.cwd / line).resolve(strict=True) for line in paths.stdout.splitlines()]
                runtime_reads.extend(str(path) for path in metadata)
                run_env.update(git_env)
                run_env.update({'GIT_DIR': str(metadata[0]), 'GIT_WORK_TREE': str(self.cwd)})
                argv = [argv[0], '--no-pager', '-c', 'core.hooksPath=/dev/null',
                        '-c', 'core.fsmonitor=false', *argv[1:]]
            if snapshot:
                if Path(argv[0]).name in ('npm', 'npx'):
                    for relative, dependencies in self.node_dependency_roots():
                        target = run_cwd / relative
                        target.parent.mkdir(parents=True, exist_ok=True)
                        target.symlink_to(dependencies, target_is_directory=True)
                        dependency_roots.append(str(dependencies))
                home = Path(self.environment.get('HOME', str(Path.home())))
                cargo_home = Path(self.environment.get('CARGO_HOME', str(home / '.cargo')))
                rustup_home = Path(self.environment.get('RUSTUP_HOME', str(home / '.rustup')))
                toolchains = rustup_home / 'toolchains'
                isolated_cargo = run_cwd / '.cargo-home'
                isolated_rustup = run_cwd / '.rustup-home'
                isolated_cargo.mkdir(); isolated_rustup.mkdir()
                for name in ('registry', 'git'):
                    cache = cargo_home / name
                    if cache.is_dir():
                        runtime_reads.append(str(cache))
                        (isolated_cargo / name).symlink_to(cache, target_is_directory=True)
                if toolchains.is_dir():
                    runtime_reads.append(str(toolchains))
                    (isolated_rustup / 'toolchains').symlink_to(toolchains, target_is_directory=True)
                # Rustup reads settings even with a selected toolchain. Copy only
                # the public default selector, not host path overrides or state.
                settings = rustup_home / 'settings.toml'
                if settings.is_file():
                    import tomllib
                    default = tomllib.loads(settings.read_text()).get('default_toolchain')
                    if isinstance(default, str):
                        (isolated_rustup / 'settings.toml').write_text(
                            'version = "12"\ndefault_toolchain = ' + json.dumps(default) + '\n')
                run_env.update({'CARGO_HOME': str(isolated_cargo), 'RUSTUP_HOME': str(isolated_rustup),
                    'CARGO_TARGET_DIR': str(run_cwd / 'target'), 'CARGO_NET_OFFLINE': 'true',
                    'TMPDIR': str(run_cwd / '.build-tmp'), 'NPM_CONFIG_CACHE': str(run_cwd / '.npm-cache'),
                    'NPM_CONFIG_USERCONFIG': str(run_cwd / '.build-tmp/npm-user.conf'),
                    'NPM_CONFIG_GLOBALCONFIG': str(run_cwd / '.build-tmp/npm-global.conf')})
                (run_cwd / '.build-tmp').mkdir()
                (run_cwd / '.build-tmp/npm-user.conf').touch()
                (run_cwd / '.build-tmp/npm-global.conf').touch()
            policy_file = directory / 'policy.json'
            policy_file.write_text(json.dumps({'cwd': str(run_cwd),
                'read_roots': [str(run_cwd), *dependency_roots] if snapshot else [str(p) for p in self.read_roots],
                'write_roots': [str(run_cwd)] if snapshot else [],
                'runtime_reads': runtime_reads}))
            policy_file.chmod(0o600)
            with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
                launch = argv if service else [sys.executable, '-I', str(helper), str(policy_file), *argv]
                self.mark_command_started()
                process = subprocess.Popen(launch,
                    cwd=run_cwd, env=run_env, stdin=subprocess.DEVNULL,
                    stdout=stdout, stderr=stderr, start_new_session=True)
                deadline = time.monotonic() + min(max(timeout, 1), 900)
                try:
                    while process.poll() is None:
                        if time.monotonic() > deadline:
                            raise Denied('command timed out; inspect progress before retrying')
                        if os.fstat(stdout.fileno()).st_size + os.fstat(stderr.fileno()).st_size > MAX_FILE_BYTES:
                            raise Denied('command exceeded output limit')
                        try:
                            process.wait(timeout=0.1)
                        except subprocess.TimeoutExpired:
                            pass
                    # Stop background writers before reconciling the snapshot.
                    # Waiting only for the top-level process leaves descendants
                    # able to race the source-copy validation below.
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    if snapshot:
                        snapshot.sync()
                    stdout.seek(0); stderr.seek(0)
                    return {'exit_code': process.returncode,
                            'stdout': stdout.read(MAX_FILE_BYTES).decode(errors='replace'),
                            'stderr': stderr.read(MAX_FILE_BYTES).decode(errors='replace')}
                finally:
                    # A child may exit after launching background descendants.
                    # The sandbox forbids setsid/setpgid, so group cleanup covers
                    # them as well. No persistent terminal is exposed.
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait()

    def _vm_helper(self):
        import importlib.util
        helper = Path(__file__).with_name('codex-build-vm.py')
        if not helper.is_file():
            raise Denied('VM build runner is unavailable')
        spec = importlib.util.spec_from_file_location('jarvis_build_vm', helper)
        vm = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(vm)
        return vm

    def run_vm_build(self, argv, timeout):
        import tempfile
        # Refused before any process starts: never fall back to /tmp or the host.
        scratch = self._scratch
        scratch.open()
        vm = self._vm_helper()
        try:
            runtime = vm.Runtime.load(Path(self.build_vm_config))
            artifacts = [runtime.config[key] for key in ('qemu', 'kernel', 'busybox', 'firmware',
                'data_dir', 'library_dir', 'module_dir')] + runtime.config['modules']
            if any(Path(path).resolve() == root or root in Path(path).resolve().parents
                   for path in artifacts for root in self.write_roots):
                raise Denied('VM runtime artifacts must be outside model-writable scopes')
            # The allowlisted host command chooses a fixed guest executable.
            # Host CLI shims and absolute checkout paths are not guest paths.
            name = Path(argv[0]).name
            guest = ['/toolchain/bin/cargo' if name == 'cargo' else '/usr/bin/' + name]
            source = str(self.cwd)
            def guest_path(value):
                if value == source or value.startswith(source + '/'):
                    return '/workspace' + value[len(source):]
                return value
            for value in argv[1:]:
                if value.startswith('-') and '=' in value:
                    flag, argument = value.split('=', 1)
                    guest.append(flag + '=' + guest_path(argument))
                else:
                    guest.append(guest_path(value))
            install = name == 'npm' and npm_subcommand(guest) in {
                'ci', 'install', 'i', 'update', 'up', 'uninstall', 'remove', 'rm', 'rebuild'}
            dependencies = []
            if name in ('npm', 'npx'):
                manifests = self.node_manifest_state()
                if self._node_install_cache and self._node_install_cache[0] == manifests:
                    dependencies = self._node_install_cache[1]
                elif not install:
                    dependencies = self.node_dependency_roots()
                else:
                    # An install can repair a missing or changed dependency tree;
                    # do not require a matching linked-checkout lock first.
                    dependencies = self.local_node_dependencies(self.cwd)
            with tempfile.TemporaryDirectory(prefix='jarvis-vm-build-', dir=scratch.tmp) as temporary:
                snapshot = BuildSnapshot(self, Path(temporary) / 'workspace')
                view = Policy({'cwd': str(snapshot.root), 'read_roots': [str(snapshot.root)],
                    'allowed_tools': ['Read']})
                initial_manifests = view.node_manifest_state() if name in ('npm', 'npx') else None
                if dependencies and initial_manifests != manifests:
                    raise Denied('dependency manifests changed before build; retry')
                if install:
                    for relative, source in dependencies:
                        copy_dependency_tree(source, snapshot.root / relative)
                build_environment = {}
                build_cache = None
                if name == 'cargo':
                    # The session's build-cache image carries the Cargo home
                    # (seeded in the guest from the read-only operator registry
                    # once per session) and the target directory across its
                    # commands, so later builds are incremental.
                    build_cache = str(scratch.cache)
                    build_environment = {
                        'CARGO_NET_OFFLINE': 'true' if {'--offline', '--frozen'} & set(guest) else 'false'}
                downloads = {}
                self.mark_command_started()
                result = vm.run(runtime, snapshot.root, guest, build_environment, timeout=timeout,
                    node_workspaces=[] if install else dependencies, download_info=downloads,
                    scratch_dir=str(scratch.tmp), build_cache=build_cache)
                if initial_manifests is not None and initial_manifests != self.node_manifest_state():
                    raise Denied('dependency manifests changed during build; retry before syncing sources')
                snapshot.sync()
                if install and result['exit_code'] == 0:
                    # Never install guest-produced executables into the host
                    # checkout. Retain this bridge's private dependency copy and
                    # mount it read-only for subsequent build/test commands.
                    owner = tempfile.TemporaryDirectory(prefix='jarvis-node-install-', dir=scratch.tmp)
                    try:
                        roots = []
                        for relative, source in self.local_node_dependencies(snapshot.root):
                            destination = Path(owner.name) / relative
                            copy_dependency_tree(source, destination)
                            roots.append((relative, destination))
                        installed_state = view.node_manifest_state()
                        if installed_state != self.node_manifest_state():
                            raise Denied('dependency manifests changed during install; retry before building')
                        previous = self._node_install_cache
                        # Dispatch is serial in serve(). Clear before eviction so
                        # a cleanup error cannot leave a pointer to a deleted cache.
                        self._node_install_cache = None
                        if previous:
                            previous[2].cleanup()
                        import weakref
                        weakref.finalize(self, owner.cleanup)
                        self._node_install_cache = (installed_state, roots, owner)
                    except Exception:
                        owner.cleanup()
                        raise
                return result
        except vm.Unavailable as exc:
            if str(exc) == 'VM build cache is full':
                raise Readiness('build_cache_full', scratch.cache, 'the session build cache image is full') from exc
            if scratch.volume_full():
                raise Readiness('build_cache_full', scratch.root, 'the scratch volume is full') from exc
            raise Denied(str(exc)) from exc
        except Readiness:
            raise
        except Denied:
            if scratch.volume_full():
                raise Readiness('build_cache_full', scratch.root, 'the scratch volume is full')
            raise
        except (OSError, ValueError, KeyError) as exc:
            if scratch.volume_full():
                raise Readiness('build_cache_full', scratch.root, 'the scratch volume is full') from exc
            raise Denied('VM runtime or source reconciliation is unavailable') from exc

    def close(self):
        if self._node_install_cache:
            self._node_install_cache[2].cleanup()
            self._node_install_cache = None
        self._scratch.close()

    def node_manifest_state(self):
        return {name: self._read_bytes(name) for name in self.dependency_manifests(self.cwd)}

    def node_dependency_roots(self):
        dependencies = self.local_node_dependencies(self.cwd)
        if dependencies or not (self.cwd / '.git').is_file():
            return dependencies
        # A linked worktree does not contain gitignored installed packages.
        # Discover only its registered main checkout, never arbitrary siblings.
        import shutil
        executable = shutil.which('git', path=self.environment.get('PATH', os.defpath))
        if not executable or any(Path(executable).resolve().is_relative_to(root) for root in self.write_roots):
            raise Denied('trusted Git dependency discovery is unavailable')
        git_env = {'PATH': os.defpath, 'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_OPTIONAL_LOCKS': '0'}
        def git(*args):
            result = subprocess.run([executable, '-c', 'core.fsmonitor=false', *args],
                cwd=self.cwd, env=git_env, capture_output=True, text=True, timeout=10)
            if result.returncode:
                raise Denied('Git dependency checkout discovery failed')
            return result.stdout
        common = (self.cwd / git('rev-parse', '--git-common-dir').strip()).resolve(strict=True)
        source = common.parent
        registered = git('worktree', 'list', '--porcelain', '-z').split('\0')
        if (common.name != '.git' or source == self.cwd
                or f'worktree {self.cwd}' not in registered
                or f'worktree {source}' not in registered
                or any(source == root or source.is_relative_to(root) for root in self.write_roots)):
            raise Denied('dependency source is not a separate registered checkout')
        source_policy = Policy({'cwd':str(source), 'read_roots':[str(source)],
            'write_roots':[], 'allowed_tools':['Read']})
        # Compare resolution inputs, not scripts: editing a regression test or
        # build script must not force a new install of identical dependencies.
        fields = ('name', 'version', 'dependencies', 'devDependencies', 'optionalDependencies',
                  'peerDependencies', 'peerDependenciesMeta', 'workspaces', 'overrides',
                  'engines', 'os', 'cpu', 'packageManager', 'bundledDependencies', 'bundleDependencies')
        try:
            manifests = self.dependency_manifests(self.cwd)
            if 'package-lock.json' not in manifests:
                raise Denied('dependency checkout requires matching lockfiles and manifests')
            root_lock = json.loads(self._read_bytes('package-lock.json'))
            if not isinstance(root_lock, dict):
                raise Denied('dependency lockfile must be an object')
            dependencies = []
            for relative in sorted(manifests):
                current = self._read_bytes(relative)
                original = source_policy._read_bytes(relative)
                if Path(relative).name == 'package.json':
                    current, original = json.loads(current), json.loads(original)
                    current = {key:current.get(key) for key in fields}
                    original = {key:original.get(key) for key in fields}
                if current != original:
                    raise Denied('dependency checkout resolution differs; install matching dependencies first')
                if Path(relative).name == 'package.json':
                    package = Path(relative).parent
                    # Independent nested projects need their own matching lock;
                    # npm workspaces can instead be covered by the root lock.
                    locked = (str(package / 'package-lock.json') in manifests
                        or str(package / 'npm-shrinkwrap.json') in manifests
                        or str(package) in root_lock.get('packages', {}))
                    dependency = source / package / 'node_modules'
                    if locked and dependency.exists():
                        if dependency.is_symlink() or not dependency.is_dir():
                            raise Denied('dependency directory must be a real directory')
                        dependencies.append((str(package / 'node_modules'), dependency))
                        if len(dependencies) > 16:
                            raise Denied('too many npm workspace dependency roots')
        except Denied:
            raise
        except (OSError, ValueError, AttributeError, TypeError) as exc:
            raise Denied('dependency checkout manifests are unavailable or invalid') from exc
        return dependencies

    def dependency_manifests(self, root):
        manifests = set()
        visited = 0
        for base, directories, files in os.walk(root, followlinks=False):
            visited += len(directories) + len(files) + 1
            if visited > 10000:
                raise Denied('dependency manifest discovery exceeds entry limit')
            directories[:] = [name for name in directories
                if name not in BuildSnapshot.EXCLUDED and name not in CONTROL_PARTS
                and name != '.env' and not name.startswith('.env.')
                and not (Path(base) / name).is_symlink()
                and not (Path(base) / name / '.git').exists()]
            for name in files:
                if name in ('package.json', 'package-lock.json', 'npm-shrinkwrap.json'):
                    path = Path(base) / name
                    if path.is_symlink():
                        raise Denied('dependency manifest must not be a symlink')
                    manifests.add(str(path.relative_to(root)))
        return manifests

    def local_node_dependencies(self, root):
        dependencies = []
        visited = 0
        for base, directories, _ in os.walk(root, followlinks=False):
            visited += len(directories) + 1
            if visited > 10000:
                raise Denied('dependency discovery exceeds entry limit')
            descend = []
            for name in directories:
                path = Path(base) / name
                try:
                    self._relative(str(self.cwd / path.relative_to(root)))
                except Denied:
                    continue
                if name == 'node_modules':
                    if path.is_symlink():
                        raise Denied('dependency directory must not be a symlink')
                    dependencies.append((str(path.relative_to(root)), path))
                    if len(dependencies) > 16:
                        raise Denied('too many npm workspace dependency roots')
                elif name not in BuildSnapshot.EXCLUDED and not path.is_symlink():
                    descend.append(name)
            directories[:] = descend
        return dependencies

    def command_argv(self, command):
        argv = literal_command_argv(command)
        for tokens, prefix in self.command_patterns:
            if argv == tokens or (prefix and argv[:len(tokens)] == tokens):
                return argv
        raise Denied('command is not permitted by this profile')


def npm_subcommand(argv):
    """Locate npm's command while respecting leading path/workspace options."""
    index = 1
    while index < len(argv):
        value = argv[index]
        if value in ('--prefix', '--workspace', '-w', '--cache', '--userconfig', '--globalconfig'):
            index += 2
        elif value.startswith('-'):
            index += 1
        else:
            return value
    return None


def copy_dependency_tree(source, destination):
    """Copy untrusted package data without following links or copying devices.

    Symlinks remain data for the guest, including npm's .bin/workspace links.
    No host operation dereferences them. Guest execution has already terminated
    before its dependency outputs are collected here.
    """
    import shutil
    source, destination = Path(source), Path(destination)
    if source.is_symlink() or not source.is_dir():
        raise Denied('dependency copy requires a regular directory root')
    count = 0
    for _, directories, files in os.walk(source, followlinks=False):
        count += len(directories) + len(files)
        if count > 100000:
            raise Denied('dependency copy exceeds entry limit')
    total = 0
    entries = 0
    def copy_regular(source, target):
        nonlocal total, entries
        descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, 'rb') as reader:
            info = os.fstat(reader.fileno())
            total += info.st_size
            entries += 1
            if not stat.S_ISREG(info.st_mode) or total > 2 * 1024**3 or entries > 100000:
                raise Denied('dependency copy exceeds limits or contains a nonregular file')
            with open(target, 'xb') as writer:
                shutil.copyfileobj(reader, writer, 1024 * 1024)
            os.chmod(target, info.st_mode & 0o777)
        return target
    # copytree does not follow directory symlinks when symlinks=True.
    shutil.copytree(source, destination, symlinks=True, copy_function=copy_regular)


class BuildSnapshot:
    """Disposable source copy for commands that execute project-supplied code.

    Secrets and repository control paths are excluded. Source edits made by
    formatters/code generators are reconciled through the ordinary scoped writer;
    build outputs never become executable artifacts in the live service checkout.
    """
    EXCLUDED = {'target', 'node_modules', '.build-tmp', '.npm-cache', '.cargo-home', '.rustup-home', 'dist', 'build', '__pycache__'}

    def __init__(self, policy, root):
        self.policy = policy
        self.root = root
        root.mkdir(mode=0o700)
        self.original = {}
        for absolute, relative in policy.files(excluded_dirs=self.EXCLUDED):
            data = policy._read_bytes(absolute)
            destination = root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(data)
            info = os.stat(absolute, follow_symlinks=False)
            destination.chmod(info.st_mode & 0o777)
            # #1036: keep source mtimes so a session's persistent Cargo target
            # sees unchanged files as fresh and rebuilds incrementally.
            os.utime(destination, ns=(info.st_atime_ns, info.st_mtime_ns))
            self.original[relative] = data

    def sync(self):
        if 'Write' not in self.policy.tools:
            return
        view = Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
                       'write_roots': [], 'allowed_tools': ['Read']})
        changes = []
        present = set()
        for absolute, relative in view.files(excluded_dirs=self.EXCLUDED):
            present.add(relative)
            data = view._read_bytes(absolute)
            if self.original.get(relative) != data:
                changes.append((relative, data))
        for relative in self.original.keys() - present:
            candidate = self.root / relative
            parts = Path(relative).parts
            if candidate.exists() or any(self.root.joinpath(*parts[:i]).is_symlink()
                                         for i in range(1, len(parts) + 1)):
                raise Denied('source replaced by nonregular path during build')
            changes.append((relative, None))
        planned = []
        for relative, data in changes:
            destination = self.policy.cwd / relative
            if relative in self.original:
                if self.policy._read_bytes(str(destination)) != self.original[relative]:
                    raise Denied('source changed during build; reconcile before retrying')
            elif destination.exists() or destination.is_symlink():
                raise Denied('source path appeared during build; reconcile before retrying')
            text = None
            if data is not None:
                try:
                    text = data.decode('utf-8')
                except UnicodeError:
                    pass
            # Native Write hooks accept text, not deletion or arbitrary bytes.
            # Never pretend those effects are an empty/encoded text write. The
            # production source-build profile has no such hooks; custom hooked
            # profiles require an explicit byte-aware contract before widening.
            if text is None and any(matcher.fullmatch('Write') for matcher, _ in self.policy.hooks):
                raise Denied('binary/deletion reconciliation cannot represent the configured Write hook contract')
            self.policy._relative(str(destination), writing=True)
            if text is not None:
                self.policy.before('Write', {'file_path': str(destination), 'content': text})
            planned.append((str(destination), data))
        for destination, data in planned:
            if data is None:
                with self.policy.parent(destination, writing=True) as (parent, leaf):
                    info = os.stat(leaf, dir_fd=parent, follow_symlinks=False)
                    if not stat.S_ISREG(info.st_mode):
                        raise Denied('deletion requires a regular source file')
                    os.unlink(leaf, dir_fd=parent)
            else:
                self.policy._write_bytes(destination, data)


TOOL_SCHEMAS = {
    'Bash': {'command': {'type': 'string'}, 'timeout': {'type': 'integer', 'minimum': 1, 'maximum': 900,
             'description': 'Maximum runtime in seconds.'}},
    'Read': {'file_path': {'type': 'string'}, 'offset': {'type': 'integer', 'minimum': 1},
             'limit': {'type': 'integer', 'minimum': 1}, 'pages': {'type': 'string',
             'description': 'PDF page number or inclusive range, for example 2-5; at most 20 pages.'}},
    'Glob': {'pattern': {'type': 'string'}, 'path': {'type': 'string'}},
    'Grep': {'pattern': {'type': 'string'}, 'path': {'type': 'string'}, 'ignore_case': {'type': 'boolean'}},
    'Write': {'file_path': {'type': 'string'}, 'content': {'type': 'string'}},
    'Edit': {'file_path': {'type': 'string'}, 'old_string': {'type': 'string'},
             'new_string': {'type': 'string'}, 'replace_all': {'type': 'boolean'}},
}
TOOL_REQUIRED = {'Bash': ['command'], 'Read': ['file_path'], 'Glob': ['pattern'],
    'Grep': ['pattern'], 'Write': ['file_path', 'content'], 'Edit': ['file_path', 'old_string', 'new_string']}


class Remote:
    """MCP transport owned by the broker, never by the model's shell.

    Requests are sent exactly once. In particular, connection/session failures
    are not permission to repeat a tools/call with potentially external effects.
    """
    def __init__(self, config, policy):
        def expand(value):
            if isinstance(value, str):
                def substitute(match):
                    if match[1] not in policy.environment:
                        raise Denied('required MCP environment value is missing')
                    return policy.environment[match[1]]
                return re.sub(r'\$\{([A-Za-z_][A-Za-z0-9_]*)\}', substitute, value)
            if isinstance(value, list):
                return [expand(item) for item in value]
            if isinstance(value, dict):
                return {key: expand(item) for key, item in value.items()}
            return value
        config = expand(config)
        self.config = config
        self.process = None
        self.buffer = b''
        self.sequence = 0
        self.closed = False
        self.session = None
        self.protocol = '2024-11-05'
        self.timeout = min(float(config.get('timeout', 30)), 120)
        if self.timeout <= 0:
            raise Denied('invalid MCP timeout')
        if config.get('type', 'stdio') == 'http':
            from urllib.parse import urlparse
            parsed = urlparse(config.get('url', ''))
            if parsed.scheme not in ('http', 'https') or not parsed.hostname or parsed.username:
                raise Denied('invalid MCP endpoint')
        elif config.get('type', 'stdio') == 'stdio':
            env = dict(policy.environment)
            env.update(config.get('env', {}))
            self.process = subprocess.Popen([config['command'], *config.get('args', [])],
                cwd=policy.cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, start_new_session=True, bufsize=0)
        else:
            raise Denied('unsupported MCP transport')
        try:
            result = self.request('initialize', {'protocolVersion': self.protocol,
                'capabilities': {}, 'clientInfo': {'name': 'jarvis-tool-bridge', 'version': '1'}})
            self.protocol = result['protocolVersion']
            self.notify('notifications/initialized', {})
        except Exception:
            self.close()
            raise

    def notify(self, method, params):
        self.exchange({'jsonrpc': '2.0', 'method': method, 'params': params}, False)

    def request(self, method, params):
        self.sequence += 1
        message = {'jsonrpc': '2.0', 'id': self.sequence, 'method': method, 'params': params}
        reply = self.exchange(message, True)
        if reply.get('id') != self.sequence or 'error' in reply or not isinstance(reply.get('result'), dict):
            raise Denied('MCP returned an invalid or failed response')
        return reply['result']

    def exchange(self, message, expect_response):
        import time
        import select
        data = (json.dumps(message) + '\n').encode()
        if self.process is None:
            from urllib.error import URLError
            try:
                return self.http(data, expect_response)
            except TimeoutError as error:
                raise Readiness('mcp_timeout') from error
            except URLError as error:
                if isinstance(error.reason, TimeoutError):
                    raise Readiness('mcp_timeout') from error
                raise
        if self.process.poll() is not None:
            raise Denied('MCP process is unavailable; request was not replayed')
        self.process.stdin.write(data)
        if not expect_response:
            return None
        deadline = time.monotonic() + self.timeout
        while True:
            if b'\n' in self.buffer:
                line, self.buffer = self.buffer.split(b'\n', 1)
                reply = json.loads(line)
                if 'method' in reply:
                    # No sampling, elicitation or other reverse RPC capability
                    # was advertised. Reject requests rather than delegating
                    # new authority to an external server.
                    if 'id' in reply:
                        rejection = {'jsonrpc': '2.0', 'id': reply['id'], 'error': {
                            'code': -32601, 'message': 'Client capability not available'}}
                        self.process.stdin.write((json.dumps(rejection) + '\n').encode())
                    continue
                if 'id' not in reply:
                    continue
                return reply
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.process.stdout], [], [], max(0, remaining))[0]:
                raise Readiness('mcp_timeout')
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise Denied('MCP connection closed; request was not replayed')
            self.buffer += chunk
            if len(self.buffer) > MAX_FILE_BYTES:
                raise Denied('MCP response exceeds size limit')

    def http(self, data, expect_response):
        import urllib.request
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, req, fp, code, msg, headers, newurl):
                return None
        headers = dict(self.config.get('headers', {}))
        headers.update({'Content-Type': 'application/json',
                        'Accept': 'application/json, text/event-stream',
                        'MCP-Protocol-Version': self.protocol})
        if self.session is not None:
            headers['Mcp-Session-Id'] = self.session
        request = urllib.request.Request(self.config['url'], data=data, headers=headers, method='POST')
        with urllib.request.build_opener(NoRedirect()).open(request, timeout=self.timeout) as response:
            session = response.headers.get('Mcp-Session-Id')
            if session:
                self.session = session
            if not expect_response:
                return None
            if 'text/event-stream' in response.headers.get('Content-Type', ''):
                size = 0
                event = []
                for line in response:
                    size += len(line)
                    if size > MAX_FILE_BYTES:
                        raise Denied('MCP event stream exceeds size limit')
                    if line.strip() == b'' and event:
                        item = json.loads(b'\n'.join(event))
                        event = []
                        if 'id' in item:
                            return item
                    elif line.startswith(b'data:'):
                        event.append(line[5:].lstrip().rstrip(b'\r\n'))
                raise Denied('MCP stream ended before its result')
            body = response.read(MAX_FILE_BYTES + 1)
            if len(body) > MAX_FILE_BYTES:
                raise Denied('MCP response exceeds size limit')
            return json.loads(body)

    def tools(self):
        result = []
        cursor = None
        seen = set()
        for _ in range(100):
            page = self.request('tools/list', {'cursor': cursor} if cursor else {})
            result.extend(page.get('tools', []))
            cursor = page.get('nextCursor')
            if not cursor:
                return result
            if cursor in seen:
                raise Denied('MCP tool cursor did not advance')
            seen.add(cursor)
        raise Denied('MCP tool list exceeds page limit')

    def close(self):
        import signal
        if self.closed:
            return
        self.closed = True
        if self.process is not None:
            try:
                os.killpg(self.process.pid, signal.SIGTERM)
                self.process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait()
            except ProcessLookupError:
                self.process.wait()
            finally:
                self.process.stdin.close()
                self.process.stdout.close()


class Server:
    def __init__(self, policy, startup_failure=None):
        self.policy = policy
        self.remotes = {}
        self.remote_tools = {}
        self.discovered = False
        # One receipt per JSON-RPC tool call in this bridge process. The
        # caller may resend a line after losing its reply; never rerun it.
        self.call_receipts = {}
        # A Readiness found while starting; reported on every tool method.
        self.startup_failure = startup_failure

    def close(self):
        try:
            for remote in self.remotes.values():
                remote.close()
        finally:
            self.policy.close()

    def discover(self):
        if self.discovered:
            return
        try:
            for name, config in self.policy.settings.get('mcpServers', {}).items():
                prefix = f'mcp__{name}__'
                if not any(t.startswith(prefix) for t in self.policy.tools):
                    continue
                if not re.fullmatch(r'[A-Za-z0-9_-]+', name):
                    raise Denied('invalid MCP server name')
                remote = Remote(config, self.policy)
                self.remotes[name] = remote
                for tool in remote.tools():
                    leaf = tool.get('name', '')
                    if not re.fullmatch(r'[A-Za-z0-9_.-]+', leaf):
                        raise Denied('invalid MCP tool name')
                    full = prefix + leaf
                    try:
                        self.policy.require(full)
                    except Denied:
                        continue
                    self.remote_tools[full] = (name, leaf, dict(tool, name=full))
            missing = [tool for tool in self.policy.tools if tool.startswith('mcp__')
                       and not tool.endswith('__*') and tool not in self.remote_tools]
            if missing:
                raise Readiness('mcp_tools')
            self.discovered = True
        except Readiness:
            self.close()
            raise
        except Exception as exc:
            self.close()
            raise Readiness('mcp_start') from exc

    def tools(self):
        self.discover()
        local = [{'name': name, 'description': 'Scoped Jarvis ' + name,
                 'inputSchema': {'type': 'object', 'properties': fields,
                                 'required': TOOL_REQUIRED[name], 'additionalProperties': False}}
                for name, fields in TOOL_SCHEMAS.items()
                if name in self.policy.tools or (name == 'Bash' and self.policy.command_patterns)]
        return local + [definition for _, _, definition in self.remote_tools.values()]

    def call(self, name, arguments):
        self.policy.require(name)
        if name in TOOL_SCHEMAS:
            fields = TOOL_SCHEMAS[name]
            if (not isinstance(arguments, dict) or set(arguments) - set(fields)
                    or set(TOOL_REQUIRED[name]) - set(arguments)):
                raise Denied('invalid tool argument fields')
            types = {'string': str, 'integer': int, 'boolean': bool}
            for key, value in arguments.items():
                field = fields[key]
                if type(value) is not types[field['type']]:
                    raise Denied('invalid tool argument type')
                if ('minimum' in field and value < field['minimum']) or ('maximum' in field and value > field['maximum']):
                    raise Denied('tool argument outside supported range')
        self.policy.before(name, arguments)
        if name == 'Bash':
            self.policy.command_argv(arguments['command'])
        if external_operation(name, arguments) and self.policy.handoff and not read_only_operation(name, arguments):
            return self.policy.handoff.execute(name, arguments,
                lambda: self.execute(name, arguments))
        return self.execute(name, arguments)

    def execute(self, name, arguments):
        if name == 'Bash':
            outcome = self.policy.run_command(arguments['command'], arguments.get('timeout'))
            return {'isError': outcome['exit_code'] != 0,
                    'content': [{'type': 'text', 'text': json.dumps(outcome)}]}
        if name.startswith('mcp__'):
            self.discover()
            if name not in self.remote_tools:
                raise Denied('configured MCP tool is unavailable')
            server, leaf, _ = self.remote_tools[name]
            return self.remotes[server].request('tools/call', {'name': leaf, 'arguments': arguments})
        if name == 'Glob':
            return json.dumps(self.policy.glob(arguments['pattern'], path=arguments.get('path', '.')))
        if name == 'Grep':
            hits = self.policy.grep(arguments['pattern'], path=arguments.get('path', '.'),
                                    ignore_case=arguments.get('ignore_case', False))
            if not getattr(hits, 'note', None):
                return json.dumps(hits)
            # Partial results stay parseable in the first block; the note follows.
            return {'content': [{'type': 'text', 'text': json.dumps(hits)},
                                {'type': 'text', 'text': hits.note}]}
        if name == 'Read':
            return self.policy.read(arguments['file_path'], arguments.get('offset'), arguments.get('limit'), arguments.get('pages'))
        if name == 'Write':
            self.policy.write(arguments['file_path'], arguments['content'])
            return 'File written.'
        if name == 'Edit':
            self.policy.edit(arguments['file_path'], arguments['old_string'], arguments['new_string'], arguments.get('replace_all', False))
            return 'File edited.'
        raise Denied('tool execution is not implemented')

    def dispatch(self, request):
        method = request.get('method')
        if self.startup_failure is not None and method in ('initialize', 'tools/list', 'tools/call'):
            raise self.startup_failure
        if method == 'initialize':
            self.discover()
            return {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}},
                    'serverInfo': {'name': 'jarvis-tools', 'version': '1'}}
        if method == 'ping':
            return {}
        if method == 'tools/list':
            return {'tools': self.tools()}
        if method == 'tools/call':
            params = request.get('params')
            if (not isinstance(params, dict) or not isinstance(params.get('name'), str)
                    or not isinstance(params.get('arguments', {}), dict)):
                raise InvalidParams('tools/call requires a string name and object arguments')
            try:
                output = self.call(params['name'], params.get('arguments', {}))
                return output if isinstance(output, dict) else {'content': [{'type': 'text', 'text': output}]}
            except ReconciliationRequired:
                return {'isError': True, 'content': [{'type': 'text', 'text':
                    'An earlier operation has an uncertain outcome. Use read-only tools to inspect current state. '
                    'Do not repeat or start external changes until that outcome is reconciled.'}]}
            except (Readiness, SearchLimit) as error:
                return {'isError': True, 'content': [{'type': 'text', 'text': runner_prefix(error) + str(error)}]}
            except (Denied, KeyError, TypeError, UnicodeError, OSError, ValueError,
                    RecursionError, MemoryError) as error:
                return {'isError': True, 'content': [{'type': 'text',
                        'text': runner_prefix(error) + 'Operation denied or invalid for the configured profile.'}]}
        raise Denied('unsupported protocol method')


def runner_prefix(error):
    """`[runner=vm] ` for a command that failed after its process started (#1041)."""
    runner = getattr(error, 'jarvis_runner', None)
    return f'[runner={runner}] ' if runner in ('vm', 'host') else ''


class InvalidParams(Exception):
    """JSON-RPC -32602: the method exists but its params have the wrong shape."""


class OversizedRequest:
    """A request line longer than MAX_REQUEST_BYTES; only its head is kept."""
    def __init__(self, prefix):
        self.prefix = prefix


def read_request_lines(stream, limit=MAX_REQUEST_BYTES):
    """Yield newline-framed requests, holding at most `limit + 1` bytes of a line.

    An oversized line is detected once that much has been read; its remainder
    is skipped in 1 MiB reads and never accumulated.
    """
    while True:
        line = stream.readline(limit + 1)
        if not line:
            return
        if len(line) > limit and not line.endswith(b'\n'):
            prefix = line[:256]
            while line and not line.endswith(b'\n'):
                line = stream.readline(1024 * 1024)
            yield OversizedRequest(prefix)
        else:
            yield line


def rpc_error(identifier, code, message):
    return {'jsonrpc': '2.0', 'id': identifier, 'error': {'code': code, 'message': message}}


def oversized_response(request):
    # Codex serializes jsonrpc then id first. Trust an id only in that exact
    # head position, never one found inside params, so no other pending call
    # can be completed by mistake. Otherwise the id is unknown (null).
    # Integers follow JSON's grammar (no leading zeros), so json.loads accepts
    # every match; safe_dispatch still contains any failure here.
    match = re.match(rb'\{"jsonrpc":"2\.0","id":(-?(?:0|[1-9][0-9]{0,17})|"[A-Za-z0-9_.:-]{1,128}")[,}]', request.prefix)
    identifier = json.loads(match[1]) if match else None
    return json.dumps(rpc_error(identifier, -32600, 'Request exceeds size limit'))


def safe_dispatch(server, line):
    """Serve one request line; return the serialized response, or None.

    Nothing a client sends may terminate the bridge (#1042). Unparsable input
    gets -32700 and a non-request -32600, both with a null id because none can
    be trusted. Codex's rmcp client (3.2.0 in codex-cli 0.154) parses id-less
    errors as JsonRpcError with id None and drops them, and never answers an
    error, so these replies cannot complete, wedge or echo against a pending
    call. Notifications and client responses carry no id and are never answered.
    """
    identifier = None
    try:
        if isinstance(line, OversizedRequest):
            return oversized_response(line)
        if not line.strip():
            return None
        try:
            request = json.loads(line.decode('utf-8'))
        except (ValueError, RecursionError, MemoryError):
            return json.dumps(rpc_error(None, -32700, 'Parse error'))
        if not isinstance(request, dict):
            return json.dumps(rpc_error(None, -32600, 'Invalid Request'))
        if 'id' not in request:
            return None
        if 'method' not in request and ('result' in request or 'error' in request):
            return None
        candidate = request['id']
        if not (candidate is None or type(candidate) in (str, int)
                or type(candidate) is float and math.isfinite(candidate)):
            return json.dumps(rpc_error(None, -32600, 'Invalid Request'))
        identifier = candidate
        if request.get('jsonrpc') != '2.0' or not isinstance(request.get('method'), str):
            return json.dumps(rpc_error(identifier, -32600, 'Invalid Request'))
        receipt_key = None
        if request['method'] == 'tools/call':
            if identifier is None:
                return json.dumps(rpc_error(identifier, -32600, 'Tool call requires an ID'))
            receipt_key = (type(identifier), identifier)
            params = json.dumps(request.get('params'), sort_keys=True,
                                separators=(',', ':'), ensure_ascii=False)
            digest = hashlib.sha256(params.encode()).digest()
            prior = server.call_receipts.get(receipt_key)
            if prior is not None:
                if prior[0] != digest:
                    return json.dumps(rpc_error(identifier, -32600, 'Tool call ID was reused with different arguments'))
                return prior[1] or json.dumps({'jsonrpc': '2.0', 'id': identifier,
                    'result': {'isError': True, 'content': [{'type': 'text',
                    'text': 'Earlier tool call outcome is uncertain; inspect current state before another change.'}]}})
            if len(server.call_receipts) >= 1024:
                return json.dumps(rpc_error(identifier, -32000, 'Tool call limit reached'))
            server.call_receipts[receipt_key] = (digest, None)
        try:
            response = {'jsonrpc': '2.0', 'id': identifier, 'result': server.dispatch(request)}
        except Readiness as error:
            response = rpc_error(identifier, -32001, str(error))
        except InvalidParams:
            response = rpc_error(identifier, -32602, 'Invalid params')
        except Denied:
            response = rpc_error(identifier, -32601, 'Unsupported method')
        serialized = json.dumps(response)
        if receipt_key is not None:
            # Read results can be large; keep memory bounded. A duplicate of
            # an oversized result is refused without repeating its effects.
            server.call_receipts[receipt_key] = (digest, serialized if len(serialized) <= 65536 else None)
        return serialized
    except Exception:
        # Never echo internal details; the tool journal owns effect uncertainty.
        return json.dumps(rpc_error(identifier, -32603, 'Internal error'))


def serve(config_path):
    import json
    import sys
    descriptor = os.open(config_path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise Denied('policy must be a private owner-controlled regular file')
        config = json.load(stream)
    import signal
    import select
    import threading
    def stop(signum, frame):
        raise SystemExit(128 + signum)
    signal.signal(signal.SIGTERM, stop)
    parent = os.getppid()
    # PR_SET_PDEATHSIG follows the spawning *thread*, which may retire while
    # a multithreaded Codex process remains healthy. A pidfd follows the whole
    # process and cannot be confused by PID reuse.
    parent_fd = os.pidfd_open(parent)
    if os.getppid() != parent:
        os.close(parent_fd)
        raise Denied('cannot bind bridge lifetime to parent')
    def watch_parent():
        watcher = select.poll()
        watcher.register(parent_fd, select.POLLIN)
        events = watcher.poll()
        if any(flags & (select.POLLIN | select.POLLHUP) for _, flags in events):
            os.kill(os.getpid(), signal.SIGTERM)
    threading.Thread(target=watch_parent, daemon=True).start()
    policy = Policy(config)
    startup_failure = None
    try:
        # Verify the shared file rule before any tool runs, against this
        # policy's write roots. Otherwise every file tool would only return
        # a silent generic denial.
        file_verification(policy.write_roots)
    except Denied as error:
        print(f'Jarvis tool bridge is not ready: {error}.', file=sys.stderr, flush=True)
        startup_failure = Readiness('mcp_start')
    server = Server(policy, startup_failure)
    try:
        for line in read_request_lines(sys.stdin.buffer):
            response = safe_dispatch(server, line)
            if response is not None:
                print(response, flush=True)
    finally:
        server.close()
        os.close(parent_fd)


if __name__ == '__main__':
    import sys
    if len(sys.argv) == 3 and sys.argv[1] in ('--handoff-status', '--handoff-reconcile'):
        try:
            journal = HandoffJournal(sys.argv[2])
            if sys.argv[1] == '--handoff-status':
                print(json.dumps(journal.inspect()))
            else:
                payload = sys.stdin.read(1024 * 1024 + 1)
                if len(payload) > 1024 * 1024:
                    raise Denied('reconciliation input exceeds limit')
                journal.reconcile(json.loads(payload))
                print('Reconciliation recorded.')
        except Exception:
            print('Handoff recovery refused: invalid/stale decision, untrusted state, or unverified request cleanup.', file=sys.stderr)
            sys.exit(2)
    elif len(sys.argv) == 3 and sys.argv[1] == '--handoff-hook':
        try:
            HandoffJournal(sys.argv[2]).observe_hook(json.load(sys.stdin))
        except CompletedOperation:
            print('External operation already completed for this request; use its prior result instead of repeating it.', file=sys.stderr)
            sys.exit(2)
        except Exception:
            # Claude uses exit 2 to deny PreToolUse, while ordinary script
            # failures can be non-blocking. Never expose journal payloads here.
            print('Handoff checkpoint unavailable or uncertain; reconciliation required.', file=sys.stderr)
            sys.exit(2)
    else:
        serve(sys.argv[1])
