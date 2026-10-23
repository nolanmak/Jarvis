#!/usr/bin/env python3
"""Install renderer, fetch, or WhatsApp as a per-user macOS LaunchAgent."""

import argparse
import os
from pathlib import Path
import plistlib
import secrets
import shutil
import stat
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
SOCKET_KEYS = {
    'renderer': ('AUGMENTAGENT_RENDERER_SOCK', 'renderer.sock'),
    'fetch': ('FETCH_SOCKET', 'fetch.sock'),
    'wa-sidecar': ('AUGMENTAGENT_WA_SOCK', 'wa.sock'),
}


def runtime_directory():
    return Path(f'/tmp/augmentagent-{os.getuid()}')


def private_directory(path):
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise SystemExit(f'{path} must be an owner-private directory')
    if info.st_mode & 0o077:
        path.chmod(0o700)


def atomic_write(path, content):
    candidate = path.with_name(path.name + '.' + secrets.token_hex(8))
    try:
        descriptor = os.open(candidate, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'wb') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(candidate, path)
    finally:
        candidate.unlink(missing_ok=True)


def validate_fetch_credentials(path):
    if not path.exists() and not path.is_symlink():
        return
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0))
    with os.fdopen(descriptor, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise SystemExit('fetch credential file must be owner-private (mode 0600)')
        content = stream.read(16385)
    if len(content) > 16384:
        raise SystemExit('fetch credential file is too large')
    seen = set()
    for line in content.decode('utf-8').splitlines():
        if not line or line.startswith('#'):
            continue
        key, separator, value = line.partition('=')
        if separator != '=' or key not in {'FIRECRAWL_API_KEY', 'BRIGHTDATA_API_KEY', 'BRIGHTDATA_ZONE'} or not value or key in seen:
            raise SystemExit('fetch credential file has an invalid or duplicate key')
        seen.add(key)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('sidecar', choices=tuple(SOCKET_KEYS))
    parser.add_argument('--render-only', type=Path, metavar='OUTPUT',
                        help='write a candidate plist without changing a loaded job')
    args = parser.parse_args()
    if sys.platform != 'darwin' and args.render_only is None:
        raise SystemExit('sidecar LaunchAgent installation requires macOS')
    if args.render_only is not None and not args.render_only.is_absolute():
        raise SystemExit('--render-only output must be absolute')
    os.umask(0o077)
    home = Path(os.environ.get('HOME', str(Path.home())))
    state = Path(os.environ.get('XDG_STATE_HOME', home / '.local/state')) / 'augmentagent'
    config = Path(os.environ.get('XDG_CONFIG_HOME', home / '.config')) / 'augmentagent'
    socket_dir = runtime_directory()
    launch_agents = home / 'Library/LaunchAgents'
    if not home.is_absolute() or not state.is_absolute():
        raise SystemExit('sidecar home and state paths must be absolute')
    key, filename = SOCKET_KEYS[args.sidecar]
    socket = socket_dir / filename
    if len(os.fsencode(socket)) >= 100:
        raise SystemExit('sidecar socket path exceeds the supported Unix socket limit')
    label = f'com.nolanmak.augmentagent.{args.sidecar}'
    node = shutil.which('node') or '/usr/bin/node'
    if args.sidecar == 'wa-sidecar':
        binary = ROOT / 'sidecars/wa-sidecar/wa-sidecar'
        command = [str(binary)]
        cwd = binary.parent
    elif args.sidecar == 'renderer':
        entry = ROOT / 'sidecars/renderer/server.mjs'
        command = [node, str(entry)]
        cwd = entry.parent
    else:
        entry = ROOT / 'sidecars/fetch/dist/index.js'
        command = [node, str(entry)]
        cwd = entry.parent.parent
    plist = launch_agents / f'{label}.plist'
    path = ':'.join(dict.fromkeys((str(Path(node).parent), '/opt/homebrew/bin',
                                   '/usr/local/bin', '/usr/bin', '/bin')))
    job = {
        'Label': label,
        'WorkingDirectory': str(cwd),
        'ProgramArguments': ([sys.executable, '-u', str(ROOT / 'scripts/start-sidecar.py'),
                              args.sidecar, str(socket), *command]
                             if args.sidecar != 'wa-sidecar' else command),
        'EnvironmentVariables': {
            'HOME': str(home), 'PATH': path, key: str(socket),
            'XDG_STATE_HOME': str(state.parent), 'PYTHONDONTWRITEBYTECODE': '1',
        },
        'RunAtLoad': True,
        'KeepAlive': True,
        'ThrottleInterval': 5,
        'Umask': 0o077,
        'StandardOutPath': str(state / f'{args.sidecar}.log'),
        'StandardErrorPath': str(state / f'{args.sidecar}.log'),
    }
    if args.sidecar == 'fetch':
        job['EnvironmentVariables']['AUGMENTAGENT_FETCH_CREDENTIALS'] = str(config / 'fetch.env')
        job['EnvironmentVariables']['DOTENV_CONFIG_PATH'] = '/dev/null'
    encoded = plistlib.dumps(job)
    if args.render_only is not None:
        atomic_write(args.render_only, encoded)
        return
    if args.sidecar == 'wa-sidecar':
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise SystemExit('run sidecars/wa-sidecar/setup.sh before installing')
    elif args.sidecar == 'renderer':
        if not (cwd / 'node_modules/@remotion/renderer').is_dir():
            raise SystemExit('run sidecars/renderer/setup.sh before installing')
    elif not entry.is_file() or not (cwd / 'node_modules').is_dir():
        raise SystemExit('run npm ci and npm run build in sidecars/fetch before installing')
    if args.sidecar != 'wa-sidecar' and not Path(node).is_file():
        raise SystemExit('Node.js is required for this sidecar')
    private_directory(state)
    private_directory(socket_dir)
    private_directory(launch_agents)
    if args.sidecar == 'fetch':
        private_directory(config)
        validate_fetch_credentials(config / 'fetch.env')
    candidate = plist.with_name(plist.name + '.' + secrets.token_hex(8) + '.new')
    atomic_write(candidate, encoded)
    subprocess.run(['/bin/bash', str(ROOT / 'scripts/lib/install-launchd-plist.sh'),
                    label, str(plist), str(candidate), 'true'], check=True)
    print(f'Installed {label}; inspect with augmentagent service --unit augmentagent-{args.sidecar}.service status')


if __name__ == '__main__':
    main()
