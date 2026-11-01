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
import fcntl
import functools
import importlib.util
import tempfile
import uuid


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


@functools.lru_cache(maxsize=4)
def helper(filename):
    spec = importlib.util.spec_from_file_location('native_provision_helper', Path(__file__).with_name(filename))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def copy_runtime_file(destination, relative, source, records):
    relative = str(helper('native-provider-vm.py').safe_relative(relative))
    data = Path(source).read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if relative in records:
        if records[relative][0] != digest:
            raise ValueError('runtime union contains conflicting file content; full reprovision required')
        return
    if len(records) >= 8192 or sum(value[1] for value in records.values()) + len(data) > 512 * 1024**2:
        raise ValueError('runtime union exceeds declared provisioning budget')
    target = destination / relative
    target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    with open(target, 'xb') as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    target.chmod(0o555 if Path(source).stat().st_mode & 0o111 else 0o444)
    records[relative] = (digest, len(data))


def copy_base_runtime(base, destination, records):
    helper('native-provider-vm.py').validate_runtime(base)
    manifest = json.loads((Path(base) / 'manifest.json').read_text())
    for entry in manifest['files']:
        copy_runtime_file(destination, entry['path'], Path(base) / entry['path'], records)
    return set(manifest.get('packages', []))


def native_companions(native):
    native = Path(native).resolve()
    if native.name != 'codex':
        return []
    companion = native.with_name('codex-code-mode-host')
    info = companion.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022 or not info.st_mode & 0o111 or info.st_size > 128 * 1024**2:
        raise ValueError('Codex code-mode companion is not a trusted executable')
    return [companion]


def stage_runtime(destination, native, bases=()):
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
    companions = native_companions(native)
    binaries = [python, Path(native).resolve(), *companions, *[path for path in sources if '.so' in path.name]]
    for binary in binaries:
        sources.update(dependencies(binary))
    destination.mkdir(mode=0o700)
    records = {}
    companion_manifest = {}
    for base in bases:
        companion_manifest.update(json.loads((Path(base) / 'manifest.json').read_text()).get('native_companions', {}))
        packages.update(copy_base_runtime(base, destination, records))
    for source in sorted(sources):
        canonical = source.resolve()
        info = canonical.stat()
        if info.st_uid != 0 or info.st_mode & 0o022 or not canonical.is_relative_to('/usr'):
            raise ValueError('runtime source is not a trusted distribution file')
        relative = str(source).removeprefix('/usr/').removeprefix('/')
        if str(source).startswith('/lib'):
            relative = str(source).lstrip('/')
        copy_runtime_file(destination, relative, canonical, records)
    copy_runtime_file(destination, 'bin/python3', python, records)
    if companions:
        native_digest = hashlib.sha256(Path(native).resolve().read_bytes()).hexdigest()
        mappings = []
        for companion in companions:
            relative = 'libexec/codex/' + native_digest + '/' + companion.name
            copy_runtime_file(destination, relative, companion, records)
            mappings.append({'name': companion.name, 'path': relative, 'sha256': records[relative][0]})
        companion_manifest[native_digest] = mappings
    manifest = {'version': 1, 'packages': sorted(packages), 'native_companions': companion_manifest,
                'files': [{'path': path, 'sha256': details[0]} for path, details in sorted(records.items())]}
    (destination / 'manifest.json').write_text(json.dumps(manifest, sort_keys=True))
    (destination / 'manifest.json').chmod(0o400)


def sync_runtime(root):
    # Persist all immutable bytes and directory entries before publishing the
    # policy pointer. Retained old generations remain valid for active runs.
    for directory, _, files in os.walk(root, topdown=False):
        for name in files:
            descriptor = os.open(Path(directory) / name, os.O_RDONLY | os.O_NOFOLLOW)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
        descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)


def discard_unpublished_generation(guest, policy_path):
    """Delete only our own generation after proving it is not published."""
    if not guest.exists():
        return
    try:
        current=json.loads(helper('native-provider-vm.py').private_bytes(policy_path,2*1024**2))
    except FileNotFoundError:
        current={}
    except Exception:
        # An unreadable/uncertain pointer cannot justify deleting its runtime.
        return
    if not isinstance(current,dict) or current.get('guest_runtime')==str(guest):
        return
    shutil.rmtree(guest)


def provision(args):
    runtime_helper = helper('native-provider-vm.py')
    policy_path = args.extend_policy
    baseline = None
    if policy_path:
        if policy_path.absolute() != policy_path.resolve():
            raise ValueError('provider policy path contains a symlink')
        policy_path = policy_path.resolve()
        baseline = runtime_helper.private_bytes(policy_path, 2 * 1024**2)
        policy = json.loads(baseline)
        if policy.get('version') != 1 or args.provider in policy.get('providers', {}):
            raise ValueError('provider already declared or policy version unsupported')
        root = policy_path.parent
        if root.stat().st_uid != os.getuid() or root.stat().st_mode & 0o077:
            raise ValueError('provisioning root is not private')
        state = Path(policy['state_root'])
        if state.resolve() != root / 'state' or state.is_symlink() or state.stat().st_uid != os.getuid() or state.stat().st_mode & 0o077:
            raise ValueError('extension must preserve the managed private state root')
        if any(value is not None for value in (args.runtime_config, args.memory_mb, args.max_state_bytes, args.max_sessions)):
            raise ValueError('extension cannot silently change global runtime/resource policy')
    else:
        if args.runtime_config is None:
            raise ValueError('new provisioning requires runtime configuration')
        root = args.directory.absolute()
        root.mkdir(mode=0o700)
        root = root.resolve()
        state = root / 'state'
        state.mkdir(mode=0o700)
        state_info = state.stat()
        root_identity = hashlib.sha256(json.dumps({'device': state_info.st_dev, 'inode': state_info.st_ino}, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        runtime_helper.atomic(state / 'authority-bootstrap.json', json.dumps({'version': 1, 'epoch': str(uuid.uuid4()), 'root_identity_sha256': root_identity}, sort_keys=True, separators=(',', ':')).encode())
        policy_path = root / 'policy.json'
        policy = {'version': 1, 'runtime_config': str(args.runtime_config.resolve()),
                  'state_root': str(state), 'memory_mb': args.memory_mb if args.memory_mb is not None else 1024,
                  'max_state_bytes': args.max_state_bytes if args.max_state_bytes is not None else 8 * 1024**3,
                  'max_sessions': args.max_sessions if args.max_sessions is not None else 16, 'providers': {}}
    if not 512 <= policy['memory_mb'] <= 4096 or policy['max_state_bytes'] < 1024**3 or policy['max_sessions'] < 1:
        raise ValueError('invalid declared native resource budget')
    lock = os.open(root / 'provision.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    published = False
    publication_attempted = False
    guest = root / ('runtime-' + uuid.uuid4().hex)
    try:
        info = os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            raise ValueError('provisioning lock is not private')
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if baseline is not None and runtime_helper.private_bytes(policy_path, 2 * 1024**2) != baseline:
            raise ValueError('provider policy changed before extension')
        gateway = json.loads(args.gateway_policy.read_text())
        helper('native-provider-gateway.py').Policy(gateway)  # no requests or inferred grants
        stage_runtime(guest, args.native_executable, [Path(policy['guest_runtime'])] if baseline else [])
        if args.allow_auth_refresh and (args.provider not in ('codex', 'claude') or args.auth_file is None):
            raise ValueError('OAuth refresh requires a supported explicit credential file')
        auth_file = args.auth_file.resolve() if args.auth_file else None
        contents = runtime_helper.private_bytes(args.auth_file, 1024 * 1024) if args.auth_file else None
        if contents is not None:
            json.loads(contents)
        if args.allow_auth_refresh:
            credentials = state / 'credentials'
            credentials.mkdir(mode=0o700, exist_ok=True)
            if credentials.is_symlink() or credentials.stat().st_uid != os.getuid() or credentials.stat().st_mode & 0o077:
                raise ValueError('credential destination is not private')
            auth_file = credentials / (args.provider + '.json')
            if auth_file.exists():
                if runtime_helper.private_bytes(auth_file, 1024 * 1024) != contents:
                    raise ValueError('unpublished credential differs; explicit reconciliation required')
            else:
                with open(auth_file, 'xb') as stream:
                    os.fchmod(stream.fileno(), 0o600)
                    stream.write(contents)
                    stream.flush()
                    os.fsync(stream.fileno())
            receipt = credentials / (args.provider + '-import.json')
            if not receipt.exists():
                runtime_helper.atomic(receipt, json.dumps({'version': 1, 'provider': args.provider,
                    'source_digest': hashlib.sha256(contents).hexdigest(), 'mode': 'explicit-private-import'}).encode())
        profile = {'refresh_auth': args.allow_auth_refresh, 'gateway': gateway,
                   'auth_file': str(auth_file) if auth_file else None,
                   'auth_relative': '.claude/.credentials.json' if args.provider == 'claude' else '.codex/auth.json',
                   'environment': args.environment_key}
        policy['providers'][args.provider] = profile
        policy['guest_runtime'] = str(guest)
        sync_runtime(guest)
        if baseline is not None and runtime_helper.private_bytes(policy_path, 2 * 1024**2) != baseline:
            raise ValueError('provider policy changed during extension')
        with tempfile.NamedTemporaryFile(dir=root, delete=False) as stream:
            temporary = Path(stream.name)
            os.fchmod(stream.fileno(), 0o600)
            stream.write(json.dumps(policy, sort_keys=True).encode())
            stream.flush()
            os.fsync(stream.fileno())
        publication_attempted = True
        os.replace(temporary, policy_path)
        published = True
        directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        return policy_path
    finally:
        # Once publication was attempted, completion can be uncertain (or
        # another actor may already have moved the pointer again). Retain the
        # generation for any workflow that could have observed it.
        if not published and not publication_attempted:
            discard_unpublished_generation(guest,policy_path)
        os.close(lock)


def runtime_matches_native(policy, native):
    """Validate the current generation before deciding whether refresh is needed."""
    runtime = Path(policy['guest_runtime'])
    helper('native-provider-vm.py').validate_runtime(runtime)
    native = Path(native).resolve(strict=True)
    info = native.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022 or not info.st_mode & 0o111 or info.st_size > 512 * 1024**2:
        raise ValueError('native executable is not trusted')
    companions = native_companions(native)
    if not companions:
        return True
    def digest(path):
        result = hashlib.sha256()
        with Path(path).open('rb') as stream:
            for chunk in iter(lambda:stream.read(1024*1024), b''):
                result.update(chunk)
        return result.hexdigest()
    manifest = json.loads((runtime/'manifest.json').read_text())
    declared = {item['path']:item['sha256'] for item in manifest['files']}
    entries = manifest.get('native_companions', {}).get(digest(native))
    if not isinstance(entries, list) or len(entries) != len(companions):
        return False
    for companion, entry in zip(companions, entries):
        relative = str(helper('native-provider-vm.py').safe_relative(entry['path']))
        if entry.get('name') != companion.name or declared.get(relative) != entry.get('sha256') or entry.get('sha256') != digest(companion):
            return False
    return True


def refresh_runtime_policy(policy_path, native, only_if_needed=False):
    """Publish only a new immutable runtime pointer; never import auth/grants."""
    policy_path = Path(policy_path)
    if policy_path.absolute() != policy_path.resolve():
        raise ValueError('runtime refresh policy path must be literal')
    root = policy_path.parent
    info = root.stat()
    if info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError('runtime refresh root is not private')
    runtime_helper = helper('native-provider-vm.py')
    baseline = runtime_helper.private_bytes(policy_path, 2 * 1024**2)
    policy = json.loads(baseline)
    if policy.get('version') != 1 or not policy.get('providers'):
        raise ValueError('runtime refresh requires an existing policy')
    lock = os.open(root/'provision.lock', os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW|os.O_CLOEXEC, 0o600)
    guest = root / ('runtime-' + uuid.uuid4().hex)
    published = False
    publication_attempted = False
    try:
        info = os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            raise ValueError('provisioning lock is not private')
        fcntl.flock(lock, fcntl.LOCK_EX|fcntl.LOCK_NB)
        if runtime_helper.private_bytes(policy_path, 2*1024**2) != baseline:
            raise ValueError('provider policy changed before runtime refresh')
        # Recheck under the same publication lock: concurrent updater ticks
        # must not create successive generations for one executable update.
        if only_if_needed and runtime_matches_native(policy, native):
            if runtime_helper.private_bytes(policy_path, 2*1024**2) != baseline:
                raise ValueError('provider policy changed during runtime inspection')
            return policy_path
        stage_runtime(guest, native, [Path(policy['guest_runtime'])])
        sync_runtime(guest)
        if runtime_helper.private_bytes(policy_path, 2*1024**2) != baseline:
            raise ValueError('provider policy changed during runtime refresh')
        policy['guest_runtime'] = str(guest)
        publication_attempted = True
        runtime_helper.atomic(policy_path, json.dumps(policy, sort_keys=True).encode())
        published = True
        return policy_path
    finally:
        os.close(lock)
        # Once publication was attempted, completion can be uncertain (or
        # another actor may already have moved the pointer again). Retain the
        # generation for any workflow that could have observed it.
        if not published and not publication_attempted:
            discard_unpublished_generation(guest,policy_path)


def main():
    parser = argparse.ArgumentParser()
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument('--directory', type=Path)
    target.add_argument('--extend-policy', type=Path, help='add one new profile, preserving old grants/state and unioning pinned runtime files')
    target.add_argument('--refresh-runtime-policy', type=Path, help='refresh immutable runtime only; preserve every existing grant and state path')
    parser.add_argument('--runtime-config', type=Path)
    parser.add_argument('--provider', choices=['codex', 'claude', 'qwen', 'glm'])
    parser.add_argument('--native-executable', type=Path, required=True)
    parser.add_argument('--auth-file', type=Path)
    parser.add_argument('--allow-auth-refresh', action='store_true', help='explicit serialized token-only refresh grant')
    parser.add_argument('--gateway-policy', type=Path,
                        help='explicit reviewed authority/method/route policy, never inferred')
    parser.add_argument('--environment-key', action='append', default=[])
    parser.add_argument('--memory-mb', type=int)
    parser.add_argument('--max-state-bytes', type=int)
    parser.add_argument('--max-sessions', type=int)
    args = parser.parse_args()
    if args.refresh_runtime_policy:
        if any(value is not None for value in (args.provider,args.gateway_policy,args.auth_file,args.runtime_config,args.memory_mb,args.max_state_bytes,args.max_sessions)) or args.allow_auth_refresh or args.environment_key:
            parser.error('runtime-only refresh cannot change provider, credentials, grants or resources')
        path = refresh_runtime_policy(args.refresh_runtime_policy, args.native_executable)
    else:
        if args.provider is None or args.gateway_policy is None:
            parser.error('provisioning requires explicit provider and gateway policy')
        path = provision(args)
    print('Prepared private native VM policy:', path)
    print('No daemon configuration, original credential files or provider requests were changed.')


if __name__ == '__main__':
    main()
