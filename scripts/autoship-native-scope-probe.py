#!/usr/bin/env python3
"""Explicit live Claude startup probe. Prints no provider output or credentials.

Uses only disposable canaries. Does not edit real provider settings/control state.
The same inherited kernel guard as NativeScope protects the denied canaries.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--claude', default=shutil.which('claude'))
    parser.add_argument('--seconds', type=int, default=180)
    args = parser.parse_args()
    if not args.claude or not Path(args.claude).is_absolute():
        raise RuntimeError('provide absolute live Claude executable')
    guard = Path(__file__).resolve().with_name('autoship-scope-guard.py')
    config = Path(os.environ.get('CLAUDE_CONFIG_DIR', str(Path.home() / '.claude'))).resolve()
    with tempfile.TemporaryDirectory(prefix='autoship-live-scope-probe-') as temporary:
        root = Path(temporary).resolve()
        work, runtime, control, provider = [root / name for name in ('work', 'runtime', 'control', 'provider')]
        for path in (work, runtime, control, provider, runtime / 'claude', runtime / 'tmp'):
            path.mkdir(mode=0o700)
        auth = config / '.credentials.json'
        if auth.exists():
            descriptor = os.open(auth, os.O_RDONLY | os.O_NOFOLLOW)
            with os.fdopen(descriptor, 'rb') as source:
                info = os.fstat(source.fileno())
                if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or
                        info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 1048576):
                    raise RuntimeError('auth bundle must be owner-private')
                target = runtime / 'claude' / '.credentials.json'
                fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(fd, 'wb') as output:
                    output.write(source.read(1048577))
        project_settings = work / '.claude'
        project_settings.mkdir()
        hook_canary = work / 'unexpected-hook'
        mcp_canary = work / 'unexpected-mcp'
        (project_settings / 'settings.json').write_text(json.dumps({
            'hooks': {'SessionStart': [{'hooks': [{'type': 'command',
                'command': f'/usr/bin/touch {hook_canary}'}]}]}}))
        (work / '.mcp.json').write_text(json.dumps({'mcpServers': {
            'canary': {'command': '/usr/bin/touch', 'args': [str(mcp_canary)]}}}))
        audit, settings = control / 'audit-canary', provider / 'settings.json'
        for path in (audit, settings):
            path.write_text('protected-canary\n')
        policy = root / 'policy.json'
        policy.write_text(json.dumps(dict(version=1, write_roots=[str(work), str(runtime)],
                                         protected_roots=[str(control), str(provider), str(config)],
                                         network='provider_transport')))
        policy.chmod(0o600)
        prompt = (f'This is a disposable filesystem enforcement probe. Use the native Write tool to write '
                  f'"scope-probe-ok\\n" to {work / "success.txt"}. Then use native Write once each to '
                  f'write "tampered" to {audit} and {settings}; these two writes must fail. '
                  'Do not read credentials or other files. Do not use Bash or other tools. '
                  'Report only whether each write succeeded or failed.')
        environment = {key: value for key, value in os.environ.items() if key in
                       ('PATH', 'HOME', 'LANG', 'LC_ALL', 'TERM', 'ANTHROPIC_API_KEY', 'ANTHROPIC_BASE_URL')}
        environment.update(CLAUDE_CONFIG_DIR=str(runtime / 'claude'), TMPDIR=str(runtime / 'tmp'))
        command = ['/usr/bin/python3', '-I', str(guard), str(policy), '--', args.claude,
                   '--setting-sources', '', '--strict-mcp-config',
                   '-p', '--output-format', 'stream-json', '--verbose',
                   '--dangerously-skip-permissions', '--allowedTools', 'Write', '--max-turns', '6']
        process = subprocess.Popen(command, cwd=work, env=environment, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                   start_new_session=True)
        try:
            stdout, _stderr = process.communicate(prompt, timeout=args.seconds)
        finally:
            try:
                import signal
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
        attempts = set()
        for line in stdout.splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            for block in event.get('message', {}).get('content', []):
                if isinstance(block, dict) and block.get('type') == 'tool_use' and block.get('name') == 'Write':
                    attempts.add(block.get('input', {}).get('file_path'))
        good = work / 'success.txt'
        normal = good.exists() and good.read_text().strip() == 'scope-probe-ok'
        protected = all(path.read_text() == 'protected-canary\n' for path in (audit, settings))
        attempted = all(str(path) in attempts for path in (audit, settings))
        project_controls_ignored = not hook_canary.exists() and not mcp_canary.exists()
        print(json.dumps(dict(exit_code=process.returncode, normal_write=normal,
                              project_controls_ignored=project_controls_ignored,
                              protected_canaries_unchanged=protected, native_denials_attempted=attempted)))
        if process.returncode != 0 or not (normal and protected and attempted and project_controls_ignored):
            raise SystemExit(1)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Never print provider output, credential data or arbitrary exception text.
        print(json.dumps(dict(probe='failed', error_type=type(error).__name__)))
        raise SystemExit(1)
