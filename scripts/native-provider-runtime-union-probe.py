#!/usr/bin/env python3
"""Opt-in disposable runtime-union and real CLI help smoke; no credentials/model.
Run under a bounded transient unit. This never installs or edits daemon policy.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def write(path, value):
    fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as output:
        json.dump(value, output, sort_keys=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--execute', action='store_true', required=True)
    parser.add_argument('--base-policy', type=Path, required=True)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--claude', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    vm = load('union_vm', 'native-provider-vm.py')
    provision = load('union_provision', 'provision-native-vm-probe.py')
    original = vm.private_bytes(args.base_policy, 2 * 1024**2)
    baseline = json.loads(original)
    if baseline.get('version') != 1 or set(baseline['providers']) != {'codex'}:
        raise ValueError('probe requires a single Codex baseline')
    executables = {name: path.resolve(strict=True) for name, path in [('codex', args.codex), ('claude', args.claude)]}
    for executable in executables.values():
        info = executable.stat()
        if not executable.is_file() or info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
            raise ValueError('untrusted native executable')
    root = args.output.absolute()
    root.mkdir(mode=0o700)
    profile = root / 'profile'
    profile.mkdir(mode=0o700)
    (profile / 'state').mkdir(mode=0o700)
    copied = dict(baseline, state_root=str(profile / 'state'))
    policy_path = profile / 'policy.json'
    write(policy_path, copied)
    # This unused placeholder grants no actual provider authority. Help must
    # produce zero gateway exchanges; credentials are absent from both guests.
    gateway = {'version': 1, 'authorities': [{'authority': 'api.example.com:443',
        'tls': True, 'routes': [{'path_prefix': '/unused', 'exact': True, 'methods': ['GET']}]}]}
    gateway_path = root / 'unused-gateway.json'
    write(gateway_path, gateway)
    old_runtime = Path(baseline['guest_runtime'])
    vm.validate_runtime(old_runtime)
    old_manifest = json.loads((old_runtime / 'manifest.json').read_text())
    command = ['/usr/bin/python3', '-I', str(Path(__file__).with_name('provision-native-vm-probe.py')),
        '--extend-policy', str(policy_path), '--provider', 'claude', '--native-executable', str(executables['claude']),
        '--gateway-policy', str(gateway_path)]
    result = subprocess.run(command, capture_output=True, timeout=180)
    if result.returncode:
        raise RuntimeError('disposable profile extension failed; no credential output retained')
    extended = json.loads(vm.private_bytes(policy_path, 2 * 1024**2))
    if set(extended['providers']) != {'codex', 'claude'} or extended['providers']['codex'] != baseline['providers']['codex']:
        raise ValueError('existing provider grants changed')
    for key in baseline.keys() - {'providers', 'guest_runtime', 'state_root'}:
        if extended[key] != baseline[key]:
            raise ValueError('extension changed global policy')
    runtime = Path(extended['guest_runtime'])
    vm.validate_runtime(runtime)
    manifest = json.loads((runtime / 'manifest.json').read_text())
    entries = {entry['path']: entry['sha256'] for entry in manifest['files']}
    for entry in old_manifest['files']:
        if entries.get(entry['path']) != entry['sha256']:
            raise ValueError('extension dropped or changed a prior runtime file')
    dependencies = {}
    for provider, executable in executables.items():
        libraries = provision.dependencies(executable)
        for library in libraries:
            relative = str(library).removeprefix('/usr/').removeprefix('/')
            if entries.get(relative) != digest(library):
                raise ValueError('actual CLI dependency missing from runtime union')
        dependencies[provider] = len(libraries)
    observations = []
    for provider, executable in executables.items():
        turn = root / provider
        turn.mkdir(mode=0o700)
        session = turn / 'session'
        session.mkdir(mode=0o700)
        launch = {'version': 1, 'provider': provider, 'runtime_config': extended['runtime_config'],
            'guest_runtime': str(runtime), 'memory_mb': extended['memory_mb'],
            'session_directory': str(session), 'resume': False, 'executable': str(executable),
            'argv': ['--help'], 'environment': {'LANG': 'C.UTF-8'}, 'auth_file': None,
            'auth_relative': extended['providers'][provider]['auth_relative'], 'refresh_auth': False,
            'gateway': gateway, 'mcp_argv': None, 'assets': []}
        launch_path = turn / 'launch.json'
        write(launch_path, launch)
        environment = {'PATH': '/usr/bin:/bin', 'JARVIS_NATIVE_VM_READY': str(turn / 'ready'),
            'JARVIS_NATIVE_VM_CONTINUE': str(turn / 'continue')}
        process = subprocess.Popen(['/usr/bin/python3', '-I', str(Path(__file__).with_name('native-provider-vm.py')), str(launch_path)],
            env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 45
            while not (turn / 'ready').exists():
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError('native help startup failed')
                time.sleep(.02)
            vm.atomic(turn / 'continue', b'admitted\n')
            stdout, stderr = process.communicate(timeout=60)
            if process.returncode or b'usage:' not in stdout.lower() or provider.encode() not in stdout.lower():
                raise RuntimeError('actual native help observation failed')
            completion = json.loads(vm.private_bytes(session / 'complete.json', 2 * 1024**2))
            if (completion['exit_code'] != 0 or completion['launch_sha256'] != digest(launch_path)
                    or completion['worker_quiescence']['remaining_workers'] != 0
                    or completion['gateway_transport']['exchanges'] or completion['gateway_transport']['host_failures']):
                raise ValueError('native help completion or no-network evidence invalid')
            observations.append({'provider': provider, 'executable_sha256': digest(executable),
                'help_sha256': hashlib.sha256(stdout).hexdigest(), 'completion_sha256': digest(session / 'complete.json'),
                'auth_mounted': False, 'gateway_exchanges': 0})
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    if vm.private_bytes(args.base_policy, 2 * 1024**2) != original:
        raise ValueError('source policy changed during probe')
    receipt = {'version': 1, 'status': 'passed', 'scope': 'runtime union and actual CLI help only; no inference or shipping authority',
        'baseline_files': len(old_manifest['files']), 'union_files': len(entries), 'actual_dependencies': dependencies,
        'source_policy_unchanged': True, 'observations': observations, 'manifest_sha256': digest(runtime / 'manifest.json')}
    write(root / 'receipt.json', receipt)
    print(json.dumps(receipt, sort_keys=True))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(json.dumps({'status': 'failed', 'category': type(error).__name__, 'scope': 'disposable no-auth CLI smoke'}))
        raise SystemExit(1)
