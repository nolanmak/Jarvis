#!/usr/bin/env python3
"""Constrained tools for provider adapters. Policy is supplied by Jarvis, never the model.

Filesystem operations use directory descriptors and refuse symlinks at every
component. Commands are parsed into argv; model text is never evaluated by a shell.
The execution transport is wired separately from this policy core.
"""
import os
import json
import subprocess
import fnmatch
import re
import shlex
import stat
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
    }

    def __init__(self, category):
        super().__init__('JARVIS_READINESS:' + category + ' ' + self.MESSAGES[category])


class ReconciliationRequired(Denied):
    """Prior effects are uncertain; reads may gather evidence, writes must wait."""


FILE_TOOLS = {'Read', 'Write', 'Edit', 'Glob', 'Grep', 'LS'}
KNOWN_TOOLS = FILE_TOOLS | {'WebSearch', 'WebFetch', 'NotebookEdit'}
CONTROL_PARTS = {'.git', '.codex', '.claude', '.ssh', '.gnupg', '.aws', '.azure'}
MAX_FILE_BYTES = 8 * 1024 * 1024


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
    if name in ('Read', 'Glob', 'Grep', 'LS', 'WebSearch', 'WebFetch'):
        return True
    # These are explicit query contracts in augmentagent-mcp-memory, not
    # arbitrary server annotations or a prefix-based read exemption.
    if name in ('mcp__memory__memory_search', 'mcp__memory__memory_recent',
                'mcp__memory__search_conversation_history', 'mcp__memory__read_conversation_thread'):
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
                        or row.get('status') not in ('started', 'completed')
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

    def execute(self, name, arguments, action):
        with self.locked():
            state = self.load()
            for row in reversed(state['operations']):
                if same_operation(row, name, arguments):
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
        self.build_vm_config = config.get('build_vm_config')
        if self.build_vm_config:
            path = Path(self.build_vm_config)
            if not path.is_absolute() or any(path.resolve() == root or root in path.resolve().parents
                                             for root in self.write_roots):
                raise Denied('VM configuration must be outside model-writable scopes')
        self.handoff = None
        if config.get('handoff_path'):
            path = Path(config['handoff_path'])
            resolved = path.resolve()
            if not path.is_absolute() or any(resolved == root or root in resolved.parents
                                             for root in self.read_roots + self.write_roots):
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
        candidate = Path(os.path.abspath(self.cwd / name))
        roots = self.write_roots if writing else self.read_roots
        for root in sorted(roots, key=lambda p: len(p.parts), reverse=True):
            try:
                rel = candidate.relative_to(root)
            except ValueError:
                continue
            if not rel.parts:
                raise Denied('operation requires a file')
            if any(p in CONTROL_PARTS or p == '.env' or p.startswith('.env.')
                   for p in rel.parts):
                raise Denied('credential and control paths are excluded')
            return root, rel.parts
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
        data = self._read_bytes(name)
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
            fd = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                         dir_fd=parent)
            with os.fdopen(fd, 'rb') as stream:
                metadata = os.fstat(stream.fileno())
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > MAX_FILE_BYTES:
                    raise Denied('read requires a bounded regular file')
                data = stream.read(MAX_FILE_BYTES + 1)
                if len(data) > MAX_FILE_BYTES:
                    raise Denied('file exceeds size limit')
                return data

    def _write(self, name, content):
        self._write_bytes(name, content.encode('utf-8'))

    def _write_bytes(self, name, data):
        if len(data) > MAX_FILE_BYTES:
            raise Denied('file exceeds size limit')
        with self.parent(name, writing=True) as (parent, leaf):
            try:
                existing = os.stat(leaf, dir_fd=parent, follow_symlinks=False)
                if not stat.S_ISREG(existing.st_mode):
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
        content = self.read(name)
        if not old or (content.count(old) != 1 and not replace_all) or old not in content:
            raise Denied('edit must identify exactly one match')
        self._write(name, content.replace(old, new, -1 if replace_all else 1))

    def files(self, path='.', excluded_dirs=()):
        base = Path(os.path.abspath(self.cwd / path))
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
        count = 0

        def walk(fd, relative):
            nonlocal count
            with os.scandir(fd) as entries:
                names = sorted(entry.name for entry in entries)
            for name in names:
                count += 1
                if count > 10000:
                    raise Denied('search exceeds entry limit; narrow its path')
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
                        yield from walk(child_fd, child)
                    finally:
                        os.close(child_fd)
                elif stat.S_ISREG(info.st_mode):
                    yield absolute, str(child)

        # A synthetic leaf lets parent() verify and open every directory
        # component without following symlinks, including the search root.
        with self.parent(str(base / '__search_scope__')) as (fd, _):
            yield from walk(fd, Path('.'))

    def glob(self, pattern, path='.'):
        self.require('Glob')
        if not isinstance(pattern, str) or len(pattern) > 1024:
            raise Denied('invalid glob pattern')
        return [relative for _, relative in self.files(path)
                if fnmatch.fnmatchcase(relative, pattern) or
                (pattern.startswith('**/') and fnmatch.fnmatchcase(relative, pattern[3:]))]

    def grep(self, pattern, path='.', ignore_case=False):
        self.require('Grep')
        if not isinstance(pattern, str) or len(pattern) > 1024:
            raise Denied('invalid search pattern')
        try:
            expression = re.compile(pattern, re.IGNORECASE if ignore_case else 0)
        except re.error as exc:
            raise Denied('invalid search expression') from exc
        results = []
        candidate = Path(os.path.abspath(self.cwd / path))
        self._relative(str(candidate / '__search_scope__') if candidate.is_dir() else str(candidate))
        candidates = [(str(candidate), candidate.name)] if candidate.is_file() else self.files(path)
        for absolute, relative in candidates:
            try:
                text = self._read(absolute)
            except (Denied, UnicodeError):
                continue
            for number, line in enumerate(text.splitlines(), 1):
                if expression.search(line):
                    results.append({'path': relative, 'line': number, 'text': line[:2000]})
                    if len(results) >= 1000:
                        return results
        return results

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

    def run_command(self, command, timeout=120):
        import shutil
        import tempfile
        import signal
        import time
        import sys
        argv = self.command_argv(command)
        build = Path(argv[0]).name in ('cargo', 'npm', 'npx')
        if build and self.build_vm_config:
            return self.run_vm_build(argv, timeout)
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
                    dependencies = self.cwd / 'node_modules'
                    if dependencies.is_symlink():
                        raise Denied('dependency directory must not be a symlink')
                    if dependencies.is_dir():
                        (run_cwd / 'node_modules').symlink_to(dependencies, target_is_directory=True)
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

    def run_vm_build(self, argv, timeout):
        import importlib.util
        import tempfile
        helper = Path(__file__).with_name('codex-build-vm.py')
        if not helper.is_file():
            raise Denied('VM build runner is unavailable')
        spec = importlib.util.spec_from_file_location('jarvis_build_vm', helper)
        vm = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(vm)
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
            dependencies = self.node_dependency_roots() if name in ('npm', 'npx') else []
            with tempfile.TemporaryDirectory(prefix='jarvis-vm-build-') as temporary:
                snapshot = BuildSnapshot(self, Path(temporary) / 'workspace')
                result = vm.run(runtime, snapshot.root, guest, {}, timeout=timeout,
                    node_workspaces=dependencies)
                snapshot.sync()
                return result
        except vm.Unavailable as exc:
            raise Denied(str(exc)) from exc
        except Denied:
            raise
        except (OSError, ValueError, KeyError) as exc:
            raise Denied('VM runtime or source reconciliation is unavailable') from exc

    def node_dependency_roots(self):
        dependencies = []
        visited = 0
        for base, directories, _ in os.walk(self.cwd, followlinks=False):
            visited += len(directories) + 1
            if visited > 10000:
                raise Denied('dependency discovery exceeds entry limit')
            descend = []
            for name in directories:
                path = Path(base) / name
                try:
                    self._relative(str(path))
                except Denied:
                    continue
                if name == 'node_modules':
                    if path.is_symlink():
                        raise Denied('dependency directory must not be a symlink')
                    dependencies.append((str(path.relative_to(self.cwd)), path))
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
            destination.chmod(os.stat(absolute, follow_symlinks=False).st_mode & 0o777)
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
    def __init__(self, policy):
        self.policy = policy
        self.remotes = {}
        self.remote_tools = {}
        self.discovered = False

    def close(self):
        for remote in self.remotes.values():
            remote.close()

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
            outcome = self.policy.run_command(arguments['command'], arguments.get('timeout', 120))
            return {'isError': outcome['exit_code'] != 0,
                    'content': [{'type': 'text', 'text': json.dumps(outcome)}]}
        if name.startswith('mcp__'):
            self.discover()
            if name not in self.remote_tools:
                raise Denied('configured MCP tool is unavailable')
            server, leaf, _ = self.remote_tools[name]
            return self.remotes[server].request('tools/call', {'name': leaf, 'arguments': arguments})
        if name in ('Glob', 'Grep'):
            action = self.policy.glob if name == 'Glob' else self.policy.grep
            options = {'path': arguments.get('path', '.')}
            if name == 'Grep':
                options['ignore_case'] = arguments.get('ignore_case', False)
            return json.dumps(action(arguments['pattern'], **options))
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
        if method == 'initialize':
            self.discover()
            return {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}},
                    'serverInfo': {'name': 'jarvis-tools', 'version': '1'}}
        if method == 'ping':
            return {}
        if method == 'tools/list':
            return {'tools': self.tools()}
        if method == 'tools/call':
            try:
                params = request['params']
                output = self.call(params['name'], params.get('arguments', {}))
                return output if isinstance(output, dict) else {'content': [{'type': 'text', 'text': output}]}
            except ReconciliationRequired:
                return {'isError': True, 'content': [{'type': 'text', 'text':
                    'An earlier operation has an uncertain outcome. Use read-only tools to inspect current state. '
                    'Do not repeat or start external changes until that outcome is reconciled.'}]}
            except Readiness as error:
                return {'isError': True, 'content': [{'type': 'text', 'text': str(error)}]}
            except (Denied, KeyError, TypeError, UnicodeError, OSError, ValueError):
                return {'isError': True, 'content': [{'type': 'text',
                        'text': 'Operation denied or invalid for the configured profile.'}]}
        raise Denied('unsupported protocol method')


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
    server = Server(Policy(config))
    try:
        for line in sys.stdin:
            request = json.loads(line)
            if 'id' not in request:
                continue
            try:
                response = {'jsonrpc': '2.0', 'id': request['id'], 'result': server.dispatch(request)}
            except Readiness as error:
                response = {'jsonrpc': '2.0', 'id': request['id'],
                            'error': {'code': -32001, 'message': str(error)}}
            except Denied:
                response = {'jsonrpc': '2.0', 'id': request['id'],
                            'error': {'code': -32601, 'message': 'Unsupported method'}}
            print(json.dumps(response), flush=True)
    finally:
        server.close()
        os.close(parent_fd)


if __name__ == '__main__':
    import sys
    if len(sys.argv) == 3 and sys.argv[1] == '--handoff-hook':
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
