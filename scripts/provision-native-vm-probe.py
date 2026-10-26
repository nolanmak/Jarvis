#!/usr/bin/env python3
"""Prepare private, explicit native VM artifacts; never install daemon policy."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import stat


def dependencies(path):
    result = subprocess.run(['/usr/bin/ldd', str(path)], capture_output=True, text=True,
                            env={'PATH': '/usr/bin:/bin', 'LC_ALL': 'C'}, timeout=15)
    if result.returncode and 'not a dynamic executable' not in result.stderr + result.stdout:
        raise RuntimeError('cannot enumerate trusted executable dependencies')
    return [Path(match) for match in re.findall(r'(?:=>\s+|^\s*)(/[^\s]+)\s+\(', result.stdout, re.M)]


def package(path):
    result = subprocess.run(['dpkg-query', '-S', str(path)], capture_output=True, text=True, check=True)
    matches = [line.rsplit(': ', 1)[0] for line in result.stdout.splitlines() if line.endswith(': ' + str(path))]
    if len(matches) != 1:
        raise RuntimeError('runtime source package identity unavailable')
    return matches[0]


def stage_runtime(destination, native):
    python = Path(sys.executable).resolve()
    if not str(python).startswith('/usr/bin/python3.'):
        raise ValueError('provision with distribution /usr/bin/python3')
    stdlib = Path('/usr/lib') / ('python' + '.'.join(map(str, sys.version_info[:2])))
    packages = {package(python), package(stdlib / 'json/__init__.py'), package(stdlib / 'encodings/__init__.py')}
    sources = {python}
    for name in sorted(packages):
        listing = subprocess.run(['dpkg-query', '-L', name], capture_output=True, text=True, check=True).stdout
        for raw in listing.splitlines():
            path = Path(raw)
            if path.name in ('sitecustomize.py', 'usercustomize.py'):
                continue
            if path.is_file() and (path == python or path.is_relative_to(stdlib)):
                sources.add(path)
    binaries = [python, Path(native).resolve(), *[path for path in sources if '.so' in path.name]]
    for binary in binaries:
        sources.update(dependencies(binary))
    destination.mkdir(mode=0o700)
    records = []
    for source in sorted(sources):
        canonical = source.resolve()
        info = canonical.stat()
        if info.st_uid != 0 or info.st_mode & 0o022 or not canonical.is_relative_to('/usr'):
            raise ValueError('runtime source is not a trusted distribution file')
        # Preserve the loader's requested path under /usr (the guest /lib and
        # /lib64 aliases point there), while reading only canonical package bytes.
        relative = str(source).removeprefix('/usr/').removeprefix('/')
        if str(source).startswith('/lib'):
            relative = str(source).lstrip('/')
        target = destination / relative
        target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
        shutil.copyfile(canonical, target)
        target.chmod(0o555 if info.st_mode & 0o111 else 0o444)
        records.append({'path': relative, 'sha256': hashlib.sha256(target.read_bytes()).hexdigest()})
    target = destination / 'bin/python3'
    shutil.copyfile(python, target)
    target.chmod(0o555)
    records.append({'path': 'bin/python3', 'sha256': hashlib.sha256(target.read_bytes()).hexdigest()})
    manifest = {'version': 1, 'packages': sorted(packages), 'files': records}
    (destination / 'manifest.json').write_text(json.dumps(manifest, sort_keys=True))
    (destination / 'manifest.json').chmod(0o400)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--runtime-config', type=Path, required=True)
    parser.add_argument('--provider', choices=['codex', 'claude', 'qwen', 'glm'], required=True)
    parser.add_argument('--native-executable', type=Path, required=True)
    parser.add_argument('--auth-file', type=Path)
    parser.add_argument('--allow-auth-refresh', action='store_true', help='explicit CAS token-only refresh grant')
    parser.add_argument('--gateway-policy', type=Path, required=True,
                        help='explicit reviewed authority/method/route policy, never inferred')
    parser.add_argument('--environment-key', action='append', default=[])
    parser.add_argument('--memory-mb', type=int, default=1024)
    args = parser.parse_args()
    root = args.directory.absolute()
    root.mkdir(mode=0o700)  # new only: never replace an operator's existing policy
    guest = root / 'runtime'
    stage_runtime(guest, args.native_executable)
    state = root / 'state'
    state.mkdir(mode=0o700)
    if args.allow_auth_refresh and (args.provider not in ('codex', 'claude') or args.auth_file is None):
        raise ValueError('OAuth refresh requires a supported explicit credential file')
    auth_file = args.auth_file.resolve() if args.auth_file else None
    if args.allow_auth_refresh:
        credentials = state / 'credentials'
        credentials.mkdir(mode=0o700)
        descriptor = os.open(args.auth_file, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        with os.fdopen(descriptor, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_nlink != 1 or info.st_size > 1024 * 1024:
                raise ValueError('credential import source is not private')
            contents = stream.read(1024 * 1024 + 1)
            json.loads(contents)
        auth_file = credentials / (args.provider + '.json')
        auth_file.write_bytes(contents)
        auth_file.chmod(0o600)
        receipt = credentials / (args.provider + '-import.json')
        receipt.write_text(json.dumps({'version': 1, 'provider': args.provider,
            'source_digest': hashlib.sha256(contents).hexdigest(), 'mode': 'explicit-private-import'}))
        receipt.chmod(0o600)
    profile = {'refresh_auth': args.allow_auth_refresh, 'gateway': json.loads(args.gateway_policy.read_text()),
               'auth_file': str(auth_file) if auth_file else None,
               'auth_relative': '.claude/.credentials.json' if args.provider == 'claude' else '.codex/auth.json',
               'environment': args.environment_key}
    policy = {'version': 1, 'runtime_config': str(args.runtime_config.resolve()),
              'guest_runtime': str(guest), 'state_root': str(state), 'memory_mb': args.memory_mb,
              'providers': {args.provider: profile}}
    path = root / 'policy.json'
    path.write_text(json.dumps(policy, sort_keys=True))
    path.chmod(0o600)
    print('Prepared private native VM policy:', path)
    print('No daemon configuration, authentication data or provider requests were changed.')


if __name__ == '__main__':
    main()
