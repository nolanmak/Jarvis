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


FILE_TOOLS = {'Read', 'Write', 'Edit', 'Glob', 'Grep', 'LS'}
KNOWN_TOOLS = FILE_TOOLS | {'WebSearch', 'WebFetch', 'NotebookEdit'}
CONTROL_PARTS = {'.git', '.codex', '.claude', '.ssh', '.gnupg', '.aws', '.azure'}
MAX_FILE_BYTES = 8 * 1024 * 1024


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
        self.tools = frozenset(config.get('allowed_tools', []))
        self.command_patterns = []
        for tool in self.tools:
            if tool in KNOWN_TOOLS:
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
                    if (decision.get('decision') == 'block' or
                        decision.get('hookSpecificOutput', {}).get('permissionDecision') == 'deny'):
                        raise Denied('enforcement hook denied the operation')
            except (OSError, subprocess.TimeoutExpired, ValueError) as exc:
                raise Denied('enforcement hook failed closed') from exc

    def require(self, tool):
        if tool not in self.tools:
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

    def read(self, name):
        self.require('Read')
        return self._read(name)

    def _read(self, name):
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
                return data.decode('utf-8')

    def _write(self, name, content):
        data = content.encode('utf-8')
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

    def edit(self, name, old, new):
        self.require('Edit')
        content = self.read(name)
        if not old or content.count(old) != 1:
            raise Denied('edit must identify exactly one match')
        self._write(name, content.replace(old, new, 1))

    def files(self, path='.'):
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

    def grep(self, pattern, path='.'):
        self.require('Grep')
        if not isinstance(pattern, str) or len(pattern) > 1024:
            raise Denied('invalid search pattern')
        try:
            expression = re.compile(pattern)
        except re.error as exc:
            raise Denied('invalid search expression') from exc
        results = []
        for absolute, relative in self.files(path):
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

    def command_argv(self, command):
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
        for tokens, prefix in self.command_patterns:
            if argv == tokens or (prefix and argv[:len(tokens)] == tokens):
                return argv
        raise Denied('command is not permitted by this profile')


TOOL_SCHEMAS = {
    'Read': {'file_path': {'type': 'string'}},
    'Glob': {'pattern': {'type': 'string'}},
    'Grep': {'pattern': {'type': 'string'}},
    'Write': {'file_path': {'type': 'string'}, 'content': {'type': 'string'}},
    'Edit': {'file_path': {'type': 'string'}, 'old_string': {'type': 'string'},
             'new_string': {'type': 'string'}},
}


class Server:
    def __init__(self, policy):
        self.policy = policy

    def tools(self):
        return [{'name': name, 'description': 'Scoped Jarvis ' + name,
                 'inputSchema': {'type': 'object', 'properties': fields,
                                 'required': list(fields), 'additionalProperties': False}}
                for name, fields in TOOL_SCHEMAS.items() if name in self.policy.tools]

    def call(self, name, arguments):
        self.policy.require(name)
        self.policy.before(name, arguments)
        if name in ('Glob', 'Grep'):
            import json
            action = self.policy.glob if name == 'Glob' else self.policy.grep
            return json.dumps(action(arguments['pattern']))
        if name == 'Read':
            return self.policy.read(arguments['file_path'])
        if name == 'Write':
            self.policy.write(arguments['file_path'], arguments['content'])
            return 'File written.'
        if name == 'Edit':
            self.policy.edit(arguments['file_path'], arguments['old_string'], arguments['new_string'])
            return 'File edited.'
        raise Denied('tool execution is not implemented')

    def dispatch(self, request):
        method = request.get('method')
        if method == 'initialize':
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
                return {'content': [{'type': 'text', 'text': output}]}
            except (Denied, KeyError, TypeError, UnicodeError):
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
    server = Server(Policy(config))
    for line in sys.stdin:
        request = json.loads(line)
        if 'id' not in request:
            continue
        try:
            response = {'jsonrpc': '2.0', 'id': request['id'], 'result': server.dispatch(request)}
        except Denied:
            response = {'jsonrpc': '2.0', 'id': request['id'],
                        'error': {'code': -32601, 'message': 'Unsupported method'}}
        print(json.dumps(response), flush=True)


if __name__ == '__main__':
    import sys
    serve(sys.argv[1])
