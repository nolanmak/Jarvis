#!/usr/bin/env python3
"""Install the optional local Runpod adapter as a private macOS LaunchAgent."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import secrets
import stat
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = 'com.nolanmak.augmentagent.runpod-adapter'


def private_file(path):
    flags = os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise SystemExit(f'{path} must exist as an owner-private file') from error
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise SystemExit(f'{path} must be an owner-private regular file (mode 0600)')
        return stream.read()


def atomic_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    candidate = path.with_name(path.name + '.' + secrets.token_hex(8))
    try:
        descriptor = os.open(candidate, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'wb') as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.replace(candidate, path)
    finally:
        candidate.unlink(missing_ok=True)


def credential_keys(content):
    values = {}
    for line in content.splitlines():
        if not line or line.startswith('#'):
            continue
        key, marker, value = line.partition('=')
        if marker != '=' or key not in {'RUNPOD_API_KEY', 'ADAPTER_API_KEY'} or not value or key in values:
            raise SystemExit('runpod-adapter.env contains an invalid or duplicate key')
        values[key] = value
    if set(values) != {'RUNPOD_API_KEY', 'ADAPTER_API_KEY'}:
        raise SystemExit('runpod-adapter.env needs RUNPOD_API_KEY and ADAPTER_API_KEY')
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--render-only', type=Path, metavar='OUTPUT',
                        help='write a candidate plist to OUTPUT without changing the installed job')
    args = parser.parse_args()
    if sys.platform != 'darwin' and args.render_only is None:
        raise SystemExit('Runpod adapter LaunchAgent installation requires macOS')
    if args.render_only is not None and not args.render_only.is_absolute():
        raise SystemExit('--render-only output must be an absolute path')
    os.umask(0o077)
    home = Path(os.environ.get('HOME', str(Path.home())))
    config = Path(os.environ.get('XDG_CONFIG_HOME', home / '.config')) / 'augmentagent'
    data = Path(os.environ.get('XDG_DATA_HOME', home / '.local/share')) / 'augmentagent/runpod-adapter'
    log_dir = Path(os.environ.get('XDG_STATE_HOME', home / '.local/state')) / 'augmentagent'
    for path in (home, config, data, log_dir):
        if not path.is_absolute():
            raise SystemExit('Runpod adapter paths must be absolute')
    credential = config / 'runpod-adapter.env'
    routes = config / 'runpod-routes.json'
    credential_keys(private_file(credential))
    try:
        route_data = json.loads(private_file(routes))
    except ValueError as error:
        raise SystemExit('runpod-routes.json must be valid JSON') from error
    if not isinstance(route_data, dict):
        raise SystemExit('runpod-routes.json must contain a JSON object')
    state = data / 'state'
    server = data / 'server.py'
    plist = home / 'Library/LaunchAgents' / (LABEL + '.plist')
    job = {
        'Label': LABEL,
        'WorkingDirectory': str(data),
        'ProgramArguments': [sys.executable, '-u', str(ROOT / 'scripts/start-runpod-adapter.py'),
                             str(credential), str(server)],
        'EnvironmentVariables': {
            'HOME': str(home),
            'PATH': '/usr/bin:/bin:/usr/sbin:/sbin',
            'PYTHONDONTWRITEBYTECODE': '1',
            'RUNPOD_ADAPTER_ROUTES': str(routes),
            'RUNPOD_ADAPTER_JOURNAL': str(state / 'jobs.sqlite3'),
            'RUNPOD_ADAPTER_HOST': '127.0.0.1',
            'RUNPOD_ADAPTER_PORT': '20129',
        },
        'RunAtLoad': True,
        'KeepAlive': {'SuccessfulExit': False},
        'ThrottleInterval': 5,
        'Umask': 0o077,
        'StandardOutPath': str(log_dir / 'runpod-adapter.log'),
        'StandardErrorPath': str(log_dir / 'runpod-adapter.log'),
    }
    encoded = plistlib.dumps(job)
    if args.render_only is not None:
        atomic_write(args.render_only, encoded)
    else:
        state.mkdir(parents=True, exist_ok=True, mode=0o700)
        log_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(state, 0o700)
        os.chmod(log_dir, 0o700)
        atomic_write(server, (ROOT / 'scripts/runpod-adapter/server.py').read_bytes())
        candidate = plist.with_name(plist.name + '.' + secrets.token_hex(8) + '.new')
        atomic_write(candidate, encoded)
        subprocess.run(['/bin/bash', str(ROOT / 'scripts/lib/install-launchd-plist.sh'),
                        LABEL, str(plist), str(candidate), 'true'], check=True)
    print(f'Runpod adapter service configured at {plist}; accounts and journal retained.')


if __name__ == '__main__':
    main()
