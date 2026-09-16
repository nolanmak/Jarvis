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
        executable = shutil.which(argv[0], path=self.environment.get('PATH', os.defpath))
        if not executable:
            raise Denied('configured command is not installed')
        executable = Path(executable).resolve(strict=True)
        if any(executable == root or root in executable.parents for root in self.write_roots):
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
            policy_file = directory / 'policy.json'
            policy_file.write_text(json.dumps({'cwd': str(self.cwd),
                'read_roots': [str(p) for p in self.read_roots], 'write_roots': []}))
            policy_file.chmod(0o600)
            with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
                launch = argv if service else [sys.executable, '-I', str(helper), str(policy_file), *argv]
                process = subprocess.Popen(launch,
                    cwd=self.cwd, env=self.environment, stdin=subprocess.DEVNULL,
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
    'Bash': {'command': {'type': 'string'}},
    'Read': {'file_path': {'type': 'string'}},
    'Glob': {'pattern': {'type': 'string'}},
    'Grep': {'pattern': {'type': 'string'}},
    'Write': {'file_path': {'type': 'string'}, 'content': {'type': 'string'}},
    'Edit': {'file_path': {'type': 'string'}, 'old_string': {'type': 'string'},
             'new_string': {'type': 'string'}},
}


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
            return self.http(data, expect_response)
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
                raise Denied('MCP timed out; request was not replayed')
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
                raise Denied('required MCP tools are unavailable')
            self.discovered = True
        except Exception:
            self.close()
            raise

    def tools(self):
        self.discover()
        local = [{'name': name, 'description': 'Scoped Jarvis ' + name,
                 'inputSchema': {'type': 'object', 'properties': fields,
                                 'required': list(fields), 'additionalProperties': False}}
                for name, fields in TOOL_SCHEMAS.items()
                if name in self.policy.tools or (name == 'Bash' and self.policy.command_patterns)]
        return local + [definition for _, _, definition in self.remote_tools.values()]

    def call(self, name, arguments):
        self.policy.require(name)
        self.policy.before(name, arguments)
        if name == 'Bash':
            outcome = self.policy.run_command(arguments['command'])
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
    import ctypes
    import signal
    def stop(signum, frame):
        raise SystemExit(128 + signum)
    signal.signal(signal.SIGTERM, stop)
    parent = os.getppid()
    if ctypes.CDLL(None).prctl(1, signal.SIGTERM, 0, 0, 0) != 0 or os.getppid() != parent:
        raise Denied('cannot bind bridge lifetime to parent')
    server = Server(Policy(config))
    try:
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
    finally:
        server.close()


if __name__ == '__main__':
    import sys
    serve(sys.argv[1])
