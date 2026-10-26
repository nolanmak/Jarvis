#!/usr/bin/env python3
"""Explicit authenticated Codex VM transport proof, not full Autoship/MCP proof.

Run under a bounded transient systemd unit. Only startup has a time budget;
productive inference has no whole-turn wall timeout. Provider JSON remains in
owner-private files, never in the public summary or terminal output.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import time
import uuid

PROVIDER = 'codex'
INFERENCE_KIND = 'responses'
ALLOW_WEBSOCKET = True
MODEL = 'gpt-5.6-terra'
TOKEN = 'AUTOSHIP_VM_OK'
PROMPT = 'Reply with exactly AUTOSHIP_VM_OK. Do not use tools, read files, or perform any other action.'
FEATURES = ('shell_tool', 'apps', 'plugins', 'multi_agent', 'browser_use', 'computer_use',
            'image_generation', 'view_image', 'skill_search', 'skill_mcp_dependency_install', 'shell_snapshot')
MAX_LOG = 32 * 1024 * 1024


def digest_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def private_json(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(descriptor) as stream:
        metadata = os.fstat(stream.fileno())
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or metadata.st_nlink != 1 or metadata.st_mode & 0o077 or metadata.st_size > 2 * 1024 * 1024):
            raise ValueError('input is not owner-private bounded JSON')
        return json.load(stream)


def private_write(path, value):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        json.dump(value, stream, sort_keys=True)
        stream.write('\n');stream.flush();os.fsync(stream.fileno())


def native_argv(session=None):
    args = ['exec'] + (['resume'] if session else [])
    args += ['--json', '--skip-git-repo-check', '--ignore-user-config', '--ignore-rules', '--strict-config',
             '-m', MODEL, '-c', 'approval_policy=never', '-c', 'project_doc_max_bytes=0',
             '-c', 'web_search=disabled', '-c', 'mcp_servers={}',
             '-c', 'default_permissions=transport_probe',
             '-c', 'permissions.transport_probe={filesystem={":minimal"="read"},network={enabled=false}}']
    for feature in FEATURES:
        args += ['-c', f'features.{feature}=false']
    args += [session] if session else ['-C', '/workspace']
    args += ['-']
    return args


def observations(path, expected_session=None):
    if path.stat().st_size > MAX_LOG:
        raise ValueError('provider output exceeds evidence inspection bound')
    session, messages, completed, failed, tools = None, [], 0, False, 0
    with path.open('rb') as stream:
        for line in stream:
            if len(line) > 2 * 1024 * 1024:
                raise ValueError('provider event exceeds inspection bound')
            if not line.strip():
                continue
            event = json.loads(line)
            kind = event.get('type')
            if kind == 'thread.started':
                observed = event.get('thread_id')
                if not isinstance(observed, str) or len(observed) > 256 or not re.fullmatch(r'[A-Za-z0-9_-]+', observed):
                    raise ValueError('invalid actual native session identity')
                if session is not None and session != observed:
                    raise ValueError('native session changed inside turn')
                session = observed
            if kind == 'turn.completed':
                completed += 1
            if kind in ('turn.failed', 'error'):
                failed = True
            if kind in ('item.started', 'item.updated', 'item.completed'):
                item = event.get('item', {})
                if kind == 'item.completed' and item.get('type') == 'agent_message':
                    messages.append(item.get('text'))
                if item.get('type') in ('command_execution', 'mcp_tool_call', 'web_search', 'tool_call'):
                    tools += 1
    if (not session or completed != 1 or failed or tools or not messages
            or not isinstance(messages[-1], str) or messages[-1].strip() != TOKEN):
        raise ValueError('native inference did not prove the fixed transport observation')
    if expected_session is not None and session != expected_session:
        raise ValueError('resume did not use the actual prior native session')
    return {'native_session': session, 'completed_turns': completed, 'tool_calls': tools,
            'exact_reply': True, 'stdout_sha256': digest_file(path)}


def inference_receipt(completion, launch_sha256):
    if completion.get('diagnostic_only') is True:
        raise ValueError('failed-launch observation cannot attest completed transport')
    if completion.get('launch_sha256') != launch_sha256:
        raise ValueError('gateway observation belongs to another launch')
    receipt = completion.get('gateway_transport', {})
    if receipt.get('version') != 1 or receipt.get('overflow') is not False:
        raise ValueError('bounded host gateway observations are missing')
    exchanges = receipt.get('exchanges')
    if not isinstance(exchanges, list) or len(exchanges) > 256:
        raise ValueError('invalid host exchange inventory')
    proven = []
    for exchange in exchanges:
        websocket = exchange.get('transport') == 'websocket'
        if (exchange.get('inference') == INFERENCE_KIND and exchange.get('authenticated_request') is True
                and exchange.get('remote_tls') is True and exchange.get('successful') is True
                and type(exchange.get('request_bytes')) is int and exchange['request_bytes'] > 0
                and type(exchange.get('response_bytes')) is int and exchange['response_bytes'] > 0
                and isinstance(exchange.get('route_sha256'), str) and re.fullmatch(r'[a-f0-9]{64}', exchange['route_sha256'])
                and (ALLOW_WEBSOCKET and websocket and exchange.get('method') == 'GET' and exchange.get('status') == 101
                     or not websocket and exchange.get('transport') == 'http' and exchange.get('method') == 'POST'
                     and type(exchange.get('status')) is int and 200 <= exchange['status'] < 300 and exchange.get('completed') is True)):
            proven.append(exchange)
    if not proven:
        raise ValueError('no host-observed authenticated inference exchange for this turn')
    return {'inference_exchanges': len(proven), 'transport_sha256': hashlib.sha256(json.dumps(receipt, sort_keys=True).encode()).hexdigest()}


def stop(process):
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def launch_turn(root, policy, profile, executable, iteration, session_id, startup_seconds, diagnose_route=False):
    turn = root / f'turn-{iteration}'
    turn.mkdir(mode=0o700)
    session = root / 'native-session'
    if iteration == 1:
        session.mkdir(mode=0o700)
    argv = native_argv(session_id)
    spec = {'version': 1, 'provider': PROVIDER, 'runtime_config': policy['runtime_config'],
            'guest_runtime': policy['guest_runtime'], 'memory_mb': policy['memory_mb'],
            'session_directory': str(session), 'resume': session_id is not None,
            'executable': str(executable), 'argv': argv, 'environment': {'LANG': 'C.UTF-8'},
            'auth_file': profile['auth_file'], 'auth_relative': profile['auth_relative'],
            'refresh_auth': False, 'gateway': profile['gateway'], 'mcp_argv': None, 'assets': [],
            'probe_route_diagnostics': diagnose_route}
    spec_path = turn / 'launch.json'
    private_write(spec_path, spec)
    launcher = Path(__file__).with_name('native-provider-vm.py').resolve()
    command = ['/usr/bin/python3', '-I', str(launcher), str(spec_path)]
    private_write(turn / 'command.json', {'launcher_argv': command, 'native_argv': argv,
        'model': MODEL, 'prompt': PROMPT, 'prompt_sha256': hashlib.sha256(PROMPT.encode()).hexdigest(),
        'executable_sha256': digest_file(executable), 'launcher_sha256': digest_file(launcher),
        'helper_sha256': {name: digest_file(launcher.with_name(name)) for name in
            ('native-provider-vm-guest.py', 'native-provider-gateway.py', 'codex-build-vm.py')}})
    environment = {'PATH': '/usr/bin:/bin', 'JARVIS_NATIVE_VM_READY': str(turn / 'ready'),
                   'JARVIS_NATIVE_VM_CONTINUE': str(turn / 'continue')}
    stdout_fd = os.open(turn / 'stdout.jsonl', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    stderr_fd = os.open(turn / 'stderr.log', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    admitted = False
    process = None
    started = time.monotonic()
    try:
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=stdout_fd, stderr=stderr_fd,
                                   env=environment, start_new_session=True)
        deadline = time.monotonic() + startup_seconds
        while not (turn / 'ready').exists():
            if process.poll() is not None:
                raise RuntimeError('native VM failed before readiness')
            if time.monotonic() >= deadline:
                raise TimeoutError('native VM startup budget exhausted before model admission')
            time.sleep(.02)
        private_write(turn / 'admission.json', {'version': 1, 'phase': 'explicit_transport_probe_admitted',
            'model': MODEL, 'command_sha256': digest_file(turn / 'command.json')})
        # Same private ready/continue protocol as production VM transport.
        descriptor = os.open(turn / 'continue', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'wb') as stream:
            stream.write(b'admitted\n');stream.flush();os.fsync(stream.fileno())
        admitted = True
        process.stdin.write(PROMPT.encode());process.stdin.close()
        # Intentionally no whole-inference wall timeout. External explicit
        # cancellation/resource failure remains observable and retains logs.
        if diagnose_route:
            diagnostic = turn / 'gateway-route-denials.jsonl'
            while process.poll() is None:
                if diagnostic.exists() and diagnostic.stat().st_size:
                    stop(process)
                    raise RuntimeError('explicit diagnostic stopped after first denied route')
                time.sleep(.02)
        code = process.wait()
        if code:
            raise RuntimeError('native VM/provider returned a nonzero status')
        result = observations(turn / 'stdout.jsonl', session_id)
        completion = private_json(session / 'complete.json')
        transport = inference_receipt(completion, digest_file(spec_path))
        quiescence = completion.get('worker_quiescence', {})
        if quiescence.get('remaining_workers') != 0 or quiescence.get('quiet_scans', 0) < 2:
            raise ValueError('native runtime export lacks worker quiescence')
        result.update({'iteration': iteration, 'exit_code': code, 'elapsed_seconds': round(time.monotonic()-started, 3),
            'stderr_sha256': digest_file(turn / 'stderr.log'), 'completion_sha256': digest_file(session / 'complete.json'),
            'model': MODEL, 'resumed': session_id is not None, 'worker_quiescence': quiescence,
            'host_inference': transport})
        private_write(turn / 'receipt.json', result)
        return result
    except BaseException as error:
        if process is not None:
            stop(process)
        private_write(turn / 'failure.json', {'version': 1, 'admitted': admitted,
            'native_dispatch_intent_present': (turn / 'vm-dispatch-intent').exists(),
            'phase': 'after_admission' if admitted else 'startup', 'category': type(error).__name__,
            'exit_code': process.returncode if process is not None else None,
            'stdout_sha256': digest_file(turn / 'stdout.jsonl'), 'stderr_sha256': digest_file(turn / 'stderr.log')})
        raise
    finally:
        os.close(stdout_fd);os.close(stderr_fd)


def validate_existing(root, policy_path, policy, profile, executable):
    root = root.absolute()
    for directory in (root, root / 'native-session'):
        metadata = directory.lstat()
        if (directory.resolve() != directory or not stat.S_ISDIR(metadata.st_mode)
                or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077):
            raise ValueError('existing evidence directory is not private')
    if any((root / name).exists() for name in ('turn-2', 'recovery-intent.json', 'recovery-summary.json')):
        raise ValueError('resume dispatch already exists; reconcile it instead of rebilling')
    original = private_json(root / 'probe.json')
    summary = private_json(root / 'summary.json')
    launch = private_json(root / 'turn-1/launch.json')
    command = private_json(root / 'turn-1/command.json')
    failure = private_json(root / 'turn-1/failure.json')
    completion = private_json(root / 'native-session/complete.json')
    if (original.get('kind') != f'authenticated_{PROVIDER}_vm_transport_only'
            or original.get('policy_sha256') != digest_file(policy_path)
            or original.get('executable') != str(executable) or original.get('model') != MODEL
            or original.get('auth_source_read_only') is not True
            or summary.get('status') != 'failed' or summary.get('auth_source_unchanged') is not True):
        raise ValueError('original failed proof identity does not match')
    expected = {'provider': PROVIDER, 'runtime_config': policy['runtime_config'],
        'guest_runtime': policy['guest_runtime'], 'memory_mb': policy['memory_mb'],
        'session_directory': str(root / 'native-session'), 'resume': False,
        'executable': str(executable), 'argv': native_argv(), 'environment': {'LANG': 'C.UTF-8'},
        'auth_file': profile['auth_file'], 'auth_relative': profile['auth_relative'],
        'refresh_auth': False, 'gateway': profile['gateway'], 'mcp_argv': None, 'assets': []}
    if any(launch.get(key) != value for key, value in expected.items()):
        raise ValueError('original native launch differs from declared transport')
    if (command.get('native_argv') != launch['argv'] or command.get('model') != MODEL
            or command.get('executable_sha256') != digest_file(executable)
            or command.get('prompt') != PROMPT
            or command.get('prompt_sha256') != hashlib.sha256(PROMPT.encode()).hexdigest()
            or failure.get('stdout_sha256') != digest_file(root / 'turn-1/stdout.jsonl')
            or failure.get('stderr_sha256') != digest_file(root / 'turn-1/stderr.log')):
        raise ValueError('original command or captured output changed')
    auth = completion.get('auth_generation')
    if not isinstance(auth, str) or not re.fullmatch(r'[a-f0-9]{64}', auth) or auth != digest_file(Path(profile['auth_file'])):
        raise ValueError('original authentication digest unavailable or changed')
    quiescence = completion.get('worker_quiescence', {})
    if completion.get('exit_code') != 0 or quiescence.get('remaining_workers') != 0 or quiescence.get('quiet_scans', 0) < 2:
        raise ValueError('original launch did not finish cleanly')
    transport = inference_receipt(completion, digest_file(root / 'turn-1/launch.json'))
    observation = observations(root / 'turn-1/stdout.jsonl')
    generation = completion.get('generation', '')
    if not re.fullmatch(r'generation-[a-f0-9]{32}', generation) or not (root / 'native-session' / generation).is_dir():
        raise ValueError('original exported generation unavailable')
    return dict(observation, host_inference=transport, original_auth_sha256=auth,
        original_summary_sha256=digest_file(root / 'summary.json'),
        original_completion_sha256=digest_file(root / 'native-session/complete.json'))


def resume_existing(root, policy_path, policy, profile, executable, startup_seconds):
    root = root.absolute()
    first = validate_existing(root, policy_path, policy, profile, executable)
    private_write(root / 'recovered-turn-1.json', first)
    # Durable single-dispatch intent: an interrupted second turn requires
    # reconciliation, never another blind paid call from this helper.
    private_write(root / 'recovery-intent.json', {'version': 1, 'phase': 'resume_existing_export',
        'recovered_first_sha256': digest_file(root / 'recovered-turn-1.json')})
    try:
        second = launch_turn(root, policy, profile, executable, 2, first['native_session'], startup_seconds)
        if digest_file(Path(profile['auth_file'])) != first['original_auth_sha256']:
            raise ValueError('authentication changed during recovered proof')
        if digest_file(root / 'summary.json') != first['original_summary_sha256']:
            raise ValueError('original failure summary changed')
        result = {'version': 1, 'status': 'passed', 'turns': 2, 'resume_proven': True,
            'new_model_turns': 1, 'auth_source_unchanged': True,
            'first_recovery_sha256': digest_file(root / 'recovered-turn-1.json'),
            'second_receipt_sha256': digest_file(root / 'turn-2/receipt.json'),
            'scope': 'recovered provider transport only; no shipping authority'}
    except BaseException as error:
        private_write(root / 'recovery-summary.json', {'version': 1, 'status': 'failed',
            'category': type(error).__name__, 'original_failure_preserved': True})
        raise
    private_write(root / 'recovery-summary.json', result)
    print(json.dumps(result, sort_keys=True), flush=True)
    return 0


def validate_profile(profile):
    pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--execute', action='store_true', required=True)
    parser.add_argument('--policy', type=Path, required=True)
    parser.add_argument('--executable', type=Path, required=True)
    parser.add_argument('--resume-existing-proof', type=Path, help='validate prior completed first turn and dispatch only its second turn')
    parser.add_argument('--resume', action='store_true', help='verify a second real native turn in the same isolated conversation')
    parser.add_argument('--output', type=Path, help='new private retained evidence directory')
    parser.add_argument('--startup-seconds', type=int, default=60)
    parser.add_argument('--diagnose-route', action='store_true', help='retain private query-free denied path and stop after the first route refusal')
    args = parser.parse_args()
    if not 10 <= args.startup_seconds <= 180:
        raise ValueError('startup budget out of range')
    policy = private_json(args.policy)
    profile = policy['providers'][PROVIDER]
    if policy.get('version') != 1 or profile.get('refresh_auth') or profile.get('environment'):
        raise ValueError('probe requires explicit read-only file authentication, no refresh or environment secrets')
    validate_profile(profile)
    auth = Path(profile['auth_file'])
    private_json(auth)  # validate ownership/shape; never persist credential values
    auth_before = digest_file(auth)
    executable = args.executable.resolve(strict=True)
    metadata = executable.stat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid not in (0, os.getuid()) or metadata.st_mode & 0o022:
        raise ValueError('native executable is not trusted')
    if args.resume_existing_proof is not None:
        if args.output is not None or args.resume or args.diagnose_route:
            raise ValueError('existing-proof recovery cannot combine with fresh output/resume/route diagnostics')
        return resume_existing(args.resume_existing_proof, args.policy, policy, profile, executable, args.startup_seconds)
    root = (args.output or Path(policy['state_root']) / (PROVIDER + '-transport-probe-' + uuid.uuid4().hex)).absolute()
    root.mkdir(mode=0o700)
    private_write(root / 'probe.json', {'version': 1, 'kind': f'authenticated_{PROVIDER}_vm_transport_only',
        'policy_sha256': digest_file(args.policy), 'executable': str(executable), 'model': MODEL,
        'resume_requested': args.resume, 'auth_source_read_only': True,
        'not_proven': ['MCP tool execution', 'full Autoship pipeline', 'cross-conversation OAuth refresh', 'production rollout']})
    print(json.dumps({'status': 'started', 'evidence_directory': str(root), 'model': MODEL}), flush=True)
    receipts = []
    try:
        first = launch_turn(root, policy, profile, executable, 1, None, args.startup_seconds, args.diagnose_route)
        receipts.append(first)
        if args.resume:
            receipts.append(launch_turn(root, policy, profile, executable, 2, first['native_session'], args.startup_seconds, args.diagnose_route))
        unchanged = digest_file(auth) == auth_before
        if not unchanged:
            raise RuntimeError('authentication source changed during read-only probe')
        summary = {'status': 'passed', 'evidence_directory': str(root), 'model': MODEL,
            'turns': len(receipts), 'resume_proven': args.resume, 'auth_source_unchanged': unchanged,
            'receipt_sha256': [digest_file(root / f'turn-{i+1}' / 'receipt.json') for i in range(len(receipts))],
            'scope': 'provider inference transport only; no MCP or Autoship LGTM authority'}
        private_write(root / 'summary.json', summary)
        print(json.dumps(summary, sort_keys=True), flush=True)
    except BaseException as error:
        summary = {'status': 'failed', 'evidence_directory': str(root), 'model': MODEL,
                   'completed_turns': len(receipts), 'category': type(error).__name__,
                   'auth_source_unchanged': digest_file(auth) == auth_before}
        private_write(root / 'summary.json', summary)
        print(json.dumps(summary, sort_keys=True), flush=True)
        return 1
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception:
        print(json.dumps({'status': 'preflight_failed', 'detail': 'private policy/executable validation failed; no credential values emitted'}))
        raise SystemExit(1)
