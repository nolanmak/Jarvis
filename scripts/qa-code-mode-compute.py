#!/usr/bin/env python3
"""Issue #1434 acceptance driver. Executes the real CLI, never runtime helpers.

Reports are private, reproducible evidence, not public failure-issue payloads.
An absent check remains incomplete: adding a passing smoke case cannot make an
entire acceptance row green. `all` requires every named check below.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]
FIXTURES = REPO / 'scripts/tests/fixtures/code-mode-compute'
LOG_LIMIT = 16 * 1024 * 1024
REQUIREMENTS = {
    'AC01': ['disabled', 'owner_contracts', 'missing_runtime', 'host_optout', 'unsupported_platform'],
    'AC02': ['xlsx', 'host_package_integrity'],
    'AC03': ['xlsx'],
    'AC04': ['fresh_task', 'changed_constraints', 'failed_preparation'],
    'AC05': ['request_contracts', 'gateway_contracts', 'source_only', 'transitive_url', 'altered_wheel'],
    'AC06': ['selected_readonly', 'host_canaries', 'secret_fds'],
    'AC07': ['network', 'execution_gateway_absent', 'xlsx'],
    'AC08': ['orchestration_boundary', 'workload_rpc'],
    'AC09': ['long_running', 'timeout', 'legacy_runner'],
    'AC10': ['task_deadline', 'host_deadline_contracts', 'call_timeout_recovery', 'request_contracts', 'core_contracts', 'repair_deadline', 'invalid_config'],
    'AC11': ['initialization_contracts', 'cleanup_allowance', 'cli_startup_signals', 'cli_stalled_startup', 'cli_signals', 'cancel_download', 'cancel_install', 'cancel_export', 'detached_descendants'],
    'AC12': ['managed_helpers', 'initialization_contracts', 'owner_crash', 'helper_crash', 'partial_startup', 'periodic_recovery', 'retention_contracts'],
    'AC13': ['retention_contracts', 'cli_contracts', 'log_limit', 'memory_limit', 'process_limit', 'disk_limit', 'concurrent_admission'],
    'AC14': ['symlink', 'hardlink', 'fifo', 'unix_socket', 'traversal', 'device', 'racing_output', 'output_limit'],
    'AC15': ['request_contracts', 'chaining', 'foreign_handle', 'artifact_contracts', 'byte_boundaries'],
    'AC16': ['owner_repair'],
    'AC17': ['export_transaction', 'full_log_transfer', 'private_audit', 'retention_contracts', 'preparation_audit', 'cancelled_logs', 'policy_timings_audit'],
    'AC18': ['disabled', 'legacy_runner', 'deno_regression', 'bridge_regression', 'build_vm_regression', 'dry_run'],
}
# Selection never changes the all-mode acceptance contract above.
GROUPS = {
    'smoke': {'disabled', 'arithmetic', 'chaining', 'private_audit', 'xlsx'},
    'security': {name for ac in ('AC01', 'AC05', 'AC06', 'AC07', 'AC08', 'AC13', 'AC14', 'AC15') for name in REQUIREMENTS[ac]},
    'lifecycle': {name for ac in ('AC04', 'AC09', 'AC10', 'AC11', 'AC12', 'AC16', 'AC17') for name in REQUIREMENTS[ac]},
}
GROUPS['all'] = {name for cases in REQUIREMENTS.values() for name in cases} | {'arithmetic'}


def coverage_report(requirements, results):
    return {ac: {'complete': bool(names) and all(results.get(name, {}).get('status') == 'passed' for name in names),
                 'checks': {name: results.get(name, {'status': 'not_run'}) for name in names}}
            for ac, names in requirements.items()}


def verify_suite_output(exit_code, output):
    if exit_code != 0 or re.search(r'skipped[= ]|\.\.\. ignored|\b[1-9]\d* (?:ignored|skipped)\b', output):
        return False
    rust = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', output)
    if rust:
        return sum(int(passed) for passed, _, _ in rust) > 0 and all(int(failed) == int(ignored) == 0 for _, failed, ignored in rust)
    python = re.search(r'Ran (\d+) tests? in ', output)
    if python:
        return int(python[1]) > 0 and bool(re.search(r'^OK\s*$', output, re.M))
    deno = re.search(r'ok \| (\d+) passed \| (\d+) failed', output)
    return bool(deno and int(deno[1]) > 0 and int(deno[2]) == 0)


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def write_json(path, value):
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, prefix='.report-', delete=False) as stream:
        temporary = Path(stream.name)
        json.dump(value, stream, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def checked(condition, message):
    if not condition:
        raise AssertionError(message)


def command(argv, cwd, env, log, timeout):
    """Bound logs and elapsed time; terminate only this command's process group."""
    started = time.monotonic()
    process = subprocess.Popen([str(arg) for arg in argv], cwd=cwd, env=env,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, start_new_session=True)
    failure = None
    total = 0
    try:
        with log.open('xb') as stream, selectors.DefaultSelector() as selector:
            log.chmod(0o600)
            selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map():
                if time.monotonic() - started >= timeout:
                    failure = 'command deadline exceeded'
                    break
                for key, _ in selector.select(0.1):
                    data = os.read(key.fileobj.fileno(), 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    available = LOG_LIMIT - total
                    stream.write(data[:available])
                    total += len(data)
                    if total > LOG_LIMIT:
                        failure = 'command log limit exceeded'
                        break
                if failure:
                    break
            if failure is None:
                process.wait(timeout=max(.001, timeout - (time.monotonic() - started)))
    except subprocess.TimeoutExpired:
        failure = 'command exit deadline exceeded'
    finally:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
        process.stdout.close()
    return {'argv': [str(arg) for arg in argv], 'cwd': str(cwd), 'executed': True, 'exitCode': process.returncode,
            'elapsedSecs': round(time.monotonic() - started, 3), 'failure': failure, 'log': str(log)}


def base_environment(root, args, deno):
    for name in ('home', 'state', 'config', 'wiki'):
        (root / name).mkdir(mode=0o700, exist_ok=True)
    env = {'PATH': os.environ.get('PATH', os.defpath), 'HOME': str(root / 'home'),
           'XDG_STATE_HOME': str(root / 'state'), 'XDG_CONFIG_HOME': str(root / 'config'),
           'AUGMENTAGENT_DB': str(root / 'qa.db'), 'AUGMENTAGENT_DENO_BIN': str(deno),
           'AUGMENTAGENT_BUILD_VM_CONFIG': str(args.vm_config),
           'AUGMENTAGENT_COMPUTE_ENABLED': 'true', 'AUGMENTAGENT_COMPUTE_TIMEOUT_SECS': '90',
           'AUGMENTAGENT_CODE_MODE_COMPUTE_TIMEOUT_SECS': '180', 'LANG': 'C.UTF-8', 'NO_COLOR': '1'}
    pip = os.environ.get('AUGMENTAGENT_COMPUTE_PIP_RUNTIME') or os.environ.get('JARVIS_TEST_COMPUTE_PIP')
    if pip:
        env['AUGMENTAGENT_COMPUTE_PIP_RUNTIME'] = str(Path(pip).absolute())
    return env


def program(code, outputs=(), selected=False, **fields):
    request = {'runtime': 'python', 'dependencies': [], 'code': code, 'outputs': list(outputs), **fields}
    binding = "request.inputs=[{artifactId:computeInputs.selected,name:'selected.txt'}];" if selected else ''
    return f'async function main(){{const request={json.dumps(request)};{binding}return await tools.compute.run(request);}} main();'


class Harness:
    def __init__(self, args, deno):
        self.args, self.deno = args, deno
        self.results = {}

    def cli(self, name, source, *, files=None, overrides=None, expect_exit=0, expected_error=None, verify=None, maximum=185, runner='vm'):
        root = self.args.output_dir / name
        root.mkdir(mode=0o700)
        inputs = {}
        for alias, data in (files or {}).items():
            path = root / f'input-{alias}'
            path.write_bytes(data)
            path.chmod(0o600)
            inputs[alias] = str(path)
        script = root / 'program.ts'
        script.write_text(source)
        script.chmod(0o600)
        scratch = Path(tempfile.mkdtemp(prefix='compute-qa-', dir=self.args.scratch_root))
        env = base_environment(root, self.args, self.deno)
        env['AUGMENTAGENT_BUILD_SCRATCH_DIR'] = str(scratch)
        env.update(overrides or {})
        output = root / 'artifacts'
        report = root / 'cli-report.json'
        argv = [self.args.bin, 'code-mode', 'compute-run', '--program', script, '--inputs', json.dumps(inputs),
                '--output-dir', output, '--report', report]
        self.results[name] = {'status': 'failed', 'command': {'argv': [str(arg) for arg in argv], 'executed': False},
                              'report': str(report), 'scratch': str(scratch)}
        try:
            receipt = command(argv, root, env, root / 'command.log', maximum)
            self.results[name]['command'] = receipt
            checked(receipt['failure'] is None, receipt['failure'])
            checked(receipt['exitCode'] == expect_exit, f'expected CLI exit {expect_exit}, got {receipt["exitCode"]}')
            if expect_exit == 2:
                checked(not report.exists(), 'invalid configuration wrote an execution report')
                return
            value = json.loads(report.read_text())
            checked(value['schemaVersion'] == 1, 'unknown report schema')
            checked(value['ok'] == (expect_exit == 0), 'CLI exit/report success disagree')
            checked(value['cleanup']['cleanupVerified'] is True, 'CLI did not verify cleanup')
            checked(not (root / 'qa.db').exists(), 'compute CLI opened its database')
            if runner is not None:
                checked(any(record.get('runner') == runner for record in value['records']), f'expected {runner} runner evidence')
            if expected_error:
                checked(((value.get('final') or {}).get('error') or {}).get('code') == expected_error,
                        f'compute result did not return {expected_error}')
                checked(not value['artifacts'] and not list(output.iterdir()), 'failed computation published artifacts')
            if verify:
                verify(value, root)
        finally:
            sessions = list(scratch.glob('jarvis-vm-session-*'))
            leases = list((scratch / 'compute-artifacts').glob('task-*'))
            checked(not sessions and not leases, 'task scratch remains; preserved for diagnosis')
            shutil.rmtree(scratch)

    def suite(self, name, argv, *, public=False):
        if public and not self.args.public_packages:
            raise AssertionError('required public registry case needs --public-packages')
        root = self.args.output_dir / name
        root.mkdir(mode=0o700)
        env = base_environment(root, self.args, self.deno)
        for key, fallback in [('CARGO_HOME', Path.home() / '.cargo'), ('RUSTUP_HOME', Path.home() / '.rustup')]:
            env[key] = os.environ.get(key, str(fallback))
        # Compilation uses the operator's existing native-library cache. This
        # applies only to associated test suites; CLI workload environments stay
        # private. Forcing CARGO_NET_OFFLINE makes ort-sys emit a link-error stub
        # even when its downloaded library already exists.
        env['XDG_CACHE_HOME'] = os.environ.get('XDG_CACHE_HOME', str(Path.home() / '.cache'))
        for key in ('ORT_LIB_PATH', 'ORT_LIB_LOCATION', 'ORT_PREFER_DYNAMIC_LINK', 'ORT_DYLIB_PATH'):
            if key in os.environ:
                env[key] = os.environ[key]
        target = os.environ.get('CARGO_TARGET_DIR')
        if argv[0] == 'cargo':
            checked(target is not None, 'associated Rust suites require an explicit worktree-specific CARGO_TARGET_DIR')
        target = target or str(root / 'unused-cargo-target')
        env.update(CARGO_TARGET_DIR=target, CARGO_BUILD_JOBS='2',
                   JARVIS_TEST_VM_CONFIG=str(self.args.vm_config), JARVIS_TEST_COMPUTE_SCRATCH=str(self.args.scratch_root),
                   JARVIS_TEST_BUILD_SCRATCH=str(self.args.scratch_root), REQUIRE_ENFORCEABLE_SANDBOX='1')
        if public:
            env['JARVIS_TEST_COMPUTE_PIP'] = env.get('AUGMENTAGENT_COMPUTE_PIP_RUNTIME', '')
            env['JARVIS_TEST_PACKAGE_NETWORK'] = '1'
        receipt = command(argv, REPO, env, root / 'command.log', 1200)
        self.results[name] = {'status': 'failed', 'command': receipt}
        checked(receipt['failure'] is None, receipt['failure'])
        checked(verify_suite_output(receipt['exitCode'], (root / 'command.log').read_text(errors='replace')),
                'required suite failed, skipped tests, or ran zero matching tests')

    def run_case(self, name):
        try:
            handler = CASES.get(name)
            if handler is None:
                self.results[name] = {'status': 'not_implemented', 'reason': f'No automated check registered for required case: {name}'}
                return
            handler(self, name)
            checked(self.results.get(name, {}).get('command', {}).get('exitCode') is not None,
                    'case produced no command evidence')
            self.results[name]['status'] = 'passed'
        except Exception as error:
            self.results.setdefault(name, {})['status'] = 'failed'
            self.results[name]['reason'] = f'{type(error).__name__}: {error}'
        finally:
            print(f'{name}: {self.results.get(name, {}).get("status", "failed")}', flush=True)


def final_value(value, _root):
    checked(value['final']['runner'] == 'vm' and value['final']['stdout'].strip() == '60', 'wrong VM result')


def arithmetic(h, name):
    h.cli(name, program("print(sum([10,20,30]))"), verify=final_value)


def chaining(h, name):
    first = {'runtime': 'python', 'dependencies': [], 'code': "open('/outputs/total.txt','w').write('60')", 'outputs': ['total.txt']}
    source = f'''async function main(){{const a=await tools.compute.run({json.dumps(first)});if(!a.ok)throw Error('first call failed');return await tools.compute.run({{runtime:'python',dependencies:[],inputs:[{{artifactId:a.artifacts[0].id,name:'prior.txt'}}],code:"print(open('/inputs/prior.txt').read())"}});}}main();'''
    h.cli(name, source, verify=final_value)


def private_audit(h, name):
    def verify(value, root):
        record = value['records'][0]
        log = record['logs']['stdout']
        checked(log['bytes'] == 70000 and log['responseTruncated'], 'full-log/truncation metadata incorrect')
        audit = root / value['auditDirectory']
        checked((audit.stat().st_mode & 0o777) == 0o700, 'audit directory is not private')
        data = (audit / log['file']).read_bytes()
        checked(data == b'x' * 70000 and hashlib.sha256(data).hexdigest() == log['sha256'], 'audit log bytes/digest differ')
        checked(len(value['final']['stdout'].encode()) <= 65536, 'public stdout exceeds its bound')
        checked(json.loads((audit / 'audit.json').read_text())['records'] == value['records'], 'audit records differ')
    h.cli(name, program("print('x'*70000,end='')"), verify=verify)


def xlsx(h, name):
    checked(h.args.public_packages, 'XLSX download/reuse requires --public-packages')
    def verify(value, root):
        expected = json.loads((FIXTURES / 'expected-summary.json').read_text())
        checked(json.loads((root / 'artifacts/summary.json').read_text()) == expected, 'spreadsheet summary differs')
        cold, warm = value['records']
        checked(cold['runner'] == warm['runner'] == 'vm', 'not a real VM execution')
        checked(not cold['environmentReused'] and warm['environmentReused'], 'cold/warm reuse flags incorrect')
        checked(cold['downloads']['requests'] > 0 and cold['downloads']['bytes'] > 0, 'cold path did not download')
        checked(warm['downloads'] == {'requests': 0, 'bytes': 0}, 'warm path accessed registry')
        checked(cold['dependencyLock'] == warm['dependencyLock'], 'lock changed within task')
        checked({entry['name'] for entry in cold['dependencyLock']} == {'openpyxl', 'et-xmlfile'}, 'transitive lock incomplete')
        checked(all(re.fullmatch('[0-9a-f]{64}', entry['sha256']) for entry in cold['dependencyLock']), 'invalid wheel digest')
    h.cli(name, (FIXTURES / 'sum-xlsx.ts').read_text(), files={'sheet': (FIXTURES / 'numbers.xlsx').read_bytes()}, verify=verify)


def selected_readonly(h, name):
    code = """from pathlib import Path
p=Path('/inputs/selected.txt')
assert p.read_text()=='60'
try:
    p.write_text('changed')
    raise AssertionError('selected input is writable')
except PermissionError:
    pass
print('60')
"""
    h.cli(name, program(code, selected=True), files={'selected': b'60'}, verify=final_value)


def network(h, name):
    with socket.socket() as tcp, socket.socket(type=socket.SOCK_DGRAM) as udp:
        tcp.bind(('0.0.0.0', 0)); tcp.listen(); tcp.setblocking(False)
        udp.bind(('0.0.0.0', 0)); udp.setblocking(False)
        code = f'''import socket, pathlib
assert set(p.name for p in pathlib.Path('/sys/class/net').iterdir()) == {{'lo'}}
for host in ['127.0.0.1','169.254.169.254','192.0.2.1']:
    s=socket.socket();s.settimeout(.1)
    try:
        s.connect((host,{tcp.getsockname()[1]}));raise AssertionError('external TCP connection succeeded')
    except OSError: pass
    finally: s.close()
    s=socket.socket(type=socket.SOCK_DGRAM)
    try: s.sendto(b'compute-qa-probe',(host,{udp.getsockname()[1]}))
    except OSError: pass
    finally: s.close()
try:
    socket.getaddrinfo('pypi.org',443);raise AssertionError('external DNS lookup succeeded')
except socket.gaierror: pass
print('60')
'''
        h.cli(name, program(code), verify=final_value)
        for listener in (tcp, udp):
            try:
                if listener is tcp:
                    connection, _ = listener.accept(); connection.close()
                else:
                    listener.recvfrom(1024)
                raise AssertionError('host listener observed an execution-phase connection')
            except BlockingIOError:
                pass


def host_canaries(h, name):
    canary_root = h.args.output_dir / 'host-canary-fixture'
    canary_root.mkdir(mode=0o700, exist_ok=True)
    canary = canary_root / 'private.txt'
    canary.write_text('SYNTHETIC_COMPUTE_HOST_CANARY')
    canary.chmod(0o600)
    code = f"""import os
try:
    open({str(canary)!r}).read();raise AssertionError('host file was visible')
except (FileNotFoundError,PermissionError):pass
assert 'COMPUTE_QA_HOST_CANARY' not in os.environ
assert os.getuid()!=0
print('60')
"""
    h.cli(name, program(code), overrides={'COMPUTE_QA_HOST_CANARY': 'SYNTHETIC_COMPUTE_HOST_CANARY'}, verify=final_value)
    checked(canary.read_text() == 'SYNTHETIC_COMPUTE_HOST_CANARY', 'host canary changed')
    value = json.loads((h.args.output_dir / name / 'cli-report.json').read_text())
    checked('SYNTHETIC_COMPUTE_HOST_CANARY' not in json.dumps(value), 'host canary leaked into result')


def full_log_transfer(h, name):
    import random
    expected = random.Random(1434).randbytes(8 * 1024 * 1024)
    def verify(value, root):
        record = value['records'][0]['logs']['stdout']
        checked(record['bytes'] == len(expected), 'full log was truncated before its limit')
        checked(record['sha256'] == hashlib.sha256(expected).hexdigest(), 'binary log digest changed')
        checked((root / value['auditDirectory'] / record['file']).read_bytes() == expected, 'private binary log changed in transfer')
        checked(len(value['final']['stdout'].encode()) <= 65536, 'public stdout exceeded its limit')
    h.cli(name, program('import os,random;os.write(1,random.Random(1434).randbytes(8*1024*1024))', timeoutSecs=20),
          maximum=25, verify=verify)


def workload_rpc(h, name):
    code = """import importlib.util,os
assert importlib.util.find_spec('tools') is None
from pathlib import Path
ports = list(Path('/sys/class/virtio-ports').iterdir())
assert ports
for port in ports:
    try:
        open('/dev/'+port.name,'wb');raise AssertionError('workload can forge a host result')
    except PermissionError:pass
print('{"id":77,"call":"send","args":[]}')
"""
    def verify(value, _):
        checked(len(value['records']) == 1 and value['final']['stdout'].strip() == '{"id":77,"call":"send","args":[]}',
                'workload stdout was interpreted as tools RPC')
    h.cli(name, program(code), verify=verify)


def gateway_absent(h, name):
    code = """from pathlib import Path
for name in ['/root/control','/root/pip.whl','/prepare.py']:
    try: assert not Path(name).exists(), 'preparation transport leaked into execution'
    except PermissionError: pass
print('60')
"""
    h.cli(name, program(code), verify=final_value)


def schema_denial(h, name, request, code):
    source = f"async function main(){{try{{await tools.compute.run({json.dumps(request)});return false;}}catch(e){{return String(e).includes({json.dumps(code)});}}}}main();"
    # Schema refusals have no execution records; check the typed RPC error and
    # zero side effects rather than inventing a guest runner for this case.
    root = h.args.output_dir / name
    def verify(value, _):
        checked(value['final'] is True and value['records'] == [] and value['artifacts'] == [], 'schema refusal lost its typed error or executed work')
    h.cli(name, source, expect_exit=1, verify=verify, runner=None)


def package_integrity(h, name):
    import site
    checked(h.args.public_packages, 'host-package integrity probe requires --public-packages')
    roots = sorted(set(site.getsitepackages() + [site.getusersitepackages(), '/usr/lib/python3/dist-packages']))
    def snapshot():
        result = {}
        for root in map(Path, roots):
            if not root.exists():
                result[str(root)] = 'absent'
                continue
            for directory, folders, files in os.walk(root, followlinks=False):
                for name in sorted(folders + files):
                    path = Path(directory) / name
                    info = path.lstat()
                    if path.is_symlink():
                        content = os.readlink(path)
                    elif path.is_file():
                        content = digest(path)
                    else:
                        content = None
                    result[str(path)] = [info.st_mode, info.st_size, info.st_mtime_ns, content]
        return result
    before = snapshot()
    xlsx(h, name)
    after = snapshot()
    checked(before == after, 'host Python package directories changed during CLI package preparation')
    h.results[name]['hostPackages'] = {'roots': roots, 'entries': len(before), 'sha256': hashlib.sha256(json.dumps(before, sort_keys=True).encode()).hexdigest()}


def export_denial(code):
    return lambda h, name: h.cli(name, program(code, outputs=['bad']), expect_exit=1, expected_error='output_denied')


def cargo(crate, test=None, library=False, binary=False, filter_name=None, ignored=False, skips=()):
    args = ['cargo', 'test', '-p', crate]
    if test: args += ['--test', test]
    if library: args += ['--lib']
    if binary: args += ['--bin', 'augmentagent']
    if filter_name: args += [filter_name]
    args += ['--', '--test-threads=1']
    if ignored: args += ['--ignored']
    for skip in skips: args += ['--skip', skip]
    return args


def suite(argv, public=False):
    return lambda h, name: h.suite(name, argv, public=public)


CASES = {
    'arithmetic': arithmetic, 'chaining': chaining, 'private_audit': private_audit, 'xlsx': xlsx,
    'selected_readonly': selected_readonly, 'network': network,
    'host_canaries': host_canaries, 'host_package_integrity': package_integrity,
    'full_log_transfer': full_log_transfer, 'workload_rpc': workload_rpc, 'execution_gateway_absent': gateway_absent,
    'traversal': lambda h, n: schema_denial(h, n, {'runtime':'python','dependencies':[],'code':'pass','outputs':['../outside']}, 'bad_args'),
    'memory_limit': lambda h, n: h.cli(n, program('bytearray(16*1024**3)'), expect_exit=1, expected_error='resource_limit'),
    'output_limit': lambda h, n: h.cli(n, program("f=open('/outputs/bad','wb');f.write(b'x'*(32*1024*1024+1));f.write(b'x');f.close()", outputs=['bad']), expect_exit=1, expected_error='resource_limit'),
    'disk_limit': lambda h, n: h.cli(n, program("from pathlib import Path\nfor i in range(20):Path('/work/chunk-'+str(i)).write_bytes(b'x'*(20*1024*1024))"), expect_exit=1, expected_error='resource_limit'),
    'process_limit': lambda h, n: h.cli(n, program("import os,time\nfor i in range(192):\n if os.fork()==0:\n  os.close(1);os.close(2);time.sleep(30);os._exit(0)"), expect_exit=1, expected_error='resource_limit'),

    'disabled': lambda h, n: h.cli(n, program('print(60)'), overrides={'AUGMENTAGENT_COMPUTE_ENABLED': 'false'}, expect_exit=1, expected_error='compute_disabled', runner='none'),
    'missing_runtime': lambda h, n: h.cli(n, program('print(60)'), overrides={'AUGMENTAGENT_BUILD_VM_CONFIG': '/missing-compute-qa-runtime.json'}, expect_exit=1, expected_error='sandbox_unavailable', runner='none'),
    'host_optout': lambda h, n: h.cli(n, program('print(60)'), overrides={'AUGMENTAGENT_BUILD_VM': 'host'}, verify=final_value),
    'foreign_handle': lambda h, n: h.cli(n, program('print(60)', **{'inputs': [{'artifactId': 'foreign-task', 'name': 'input'}]}), expect_exit=1, expected_error='input_denied', runner='none'),
    'long_running': lambda h, n: h.cli(n, (FIXTURES / 'long-running.ts').read_text(), verify=lambda v, _: checked(v['final'] == {'completed': True}, 'long workload did not finish')),
    'timeout': lambda h, n: h.cli(n, program('import time\ntime.sleep(20)', timeoutSecs=2), overrides={'AUGMENTAGENT_COMPUTE_TIMEOUT_SECS': '2'}, expect_exit=1, expected_error='timeout', maximum=7),
    'invalid_config': lambda h, n: h.cli(n, 'throw Error("must not execute")', overrides={'AUGMENTAGENT_COMPUTE_TIMEOUT_SECS': '0'}, expect_exit=2),
    'log_limit': lambda h, n: h.cli(n, program("import os\nos.write(1,b'x'*(9*1024*1024))"), expect_exit=1, expected_error='resource_limit'),
    'symlink': export_denial("import os\nos.symlink('/etc/passwd','/outputs/bad')"),
    'hardlink': export_denial("import os\nopen('/outputs/original','w').write('fixture')\nos.link('/outputs/original','/outputs/bad')"),
    'fifo': export_denial("import os\nos.mkfifo('/outputs/bad')"),
    'unix_socket': export_denial("import socket\ns=socket.socket(socket.AF_UNIX);s.bind('/outputs/bad')"),
    'core_contracts': suite(cargo('augmentagent-channel-core', library=True, filter_name='code_mode::compute::tests')),
    'retention_contracts': suite(cargo('augmentagent-channel-core', library=True, filter_name='code_mode::compute::retention')),
    'orchestration_boundary': suite(cargo('augmentagent-channel-core', test='code_mode_compute', filter_name='orchestration_cannot_import_host_or_remote_modules')),
    'cli_contracts': suite(cargo('augmentagent-cli', test='code_mode_compute', skips=('real_cli_',))),
    'owner_contracts': suite(cargo('augmentagent-cli', binary=True, filter_name='compute_tool::tests', skips=('real_owner_query',))),
    'owner_repair': suite(cargo('augmentagent-cli', binary=True, filter_name='compute_tool::tests::real_owner_query', ignored=True), public=True),
    'cli_signals': suite(cargo('augmentagent-cli', test='code_mode_compute', filter_name='real_cli_signals', ignored=True)),
    'export_transaction': suite(cargo('augmentagent-cli', test='code_mode_compute', filter_name='real_cli_report_failure', ignored=True)),
    'cancel_export': suite(cargo('augmentagent-cli', test='code_mode_compute', filter_name='real_cli_export_cancellation', ignored=True)),
    'cli_startup_signals': suite(cargo('augmentagent-cli', test='code_mode_compute', filter_name='real_cli_startup_signals', ignored=True)),
    'cli_stalled_startup': suite(cargo('augmentagent-cli', test='code_mode_compute', filter_name='real_cli_stalled_startup', ignored=True)),
    'owner_crash': suite(cargo('augmentagent-channel-core', test='code_mode_compute', filter_name='killed_owner_recovery', ignored=True)),
    'helper_crash': suite(cargo('augmentagent-channel-core', test='code_mode_compute', filter_name='killed_helper_recovery', ignored=True)),
    'partial_startup': suite(cargo('augmentagent-channel-core', library=True, filter_name='compute_recovery_keeps_locked_initialization_and_legacy_sessions')),
    'periodic_recovery': suite(cargo('augmentagent-channel-core', library=True, filter_name='periodic_worker_recovers_compute_scratch_at_startup_and_next_tick')),
    'legacy_runner': suite(cargo('augmentagent-channel-core', test='code_mode_runner')),
    'dry_run': suite(cargo('augmentagent-cli', test='code_mode_dry_run')),
    'managed_helpers': suite(cargo('augmentagent-channel-core', test='code_mode_compute', filter_name='helper_files_live')),
    'initialization_contracts': suite([sys.executable, '-m', 'unittest', 'scripts.tests.compute_initialization_test', '-v']),
    'host_deadline_contracts': suite([sys.executable, '-m', 'unittest', 'scripts.tests.code_mode_compute_test.TaskLifecycleContractTests', '-v']),
    'cleanup_allowance': suite(cargo('augmentagent-channel-core', test='code_mode_compute', filter_name='stalled_helper')),
    'call_timeout_recovery': suite(cargo('augmentagent-channel-core', test='code_mode_compute', filter_name='real_call_timeout', ignored=True)),
    'task_deadline': suite(cargo('augmentagent-cli', test='code_mode_compute', filter_name='real_cli_task_deadline', ignored=True)),
    'request_contracts': suite([sys.executable, '-m', 'unittest', 'scripts.tests.code_mode_compute_test.RequestContractTests', '-v']),
    'artifact_contracts': suite([sys.executable, '-m', 'unittest', 'scripts.tests.code_mode_compute_test.ArtifactCapabilityTests', '-v']),
    'gateway_contracts': suite([sys.executable, '-m', 'unittest', 'scripts.tests.build_dependency_proxy_test', '-v']),
    'build_vm_regression': suite([sys.executable, '-m', 'unittest', 'scripts.tests.codex_build_vm_test', 'scripts.tests.codex_tool_bridge_test.BuildScratchTests', '-v'], public=True),
    'deno_regression': lambda h, n: h.suite(n, [h.deno, 'test', '--no-lock', '--allow-run=deno', '--allow-read=.', 'sidecars/code-mode-runner/runner_test.ts']),
}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ('bin', 'vm-config', 'scratch-root', 'output-dir'):
        parser.add_argument('--' + option, type=Path, required=True)
    parser.add_argument('--require-vm', action='store_true')
    parser.add_argument('--public-packages', action='store_true')
    parser.add_argument('--cases', choices=('all', 'smoke', 'security', 'lifecycle'), default='all')
    args = parser.parse_args(argv)
    for key in ('bin', 'vm_config', 'scratch_root', 'output_dir'):
        setattr(args, key, getattr(args, key).absolute())
    checked(not args.output_dir.exists() or not any(args.output_dir.iterdir()), 'QA output directory must be new or empty')
    args.output_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    args.output_dir.chmod(0o700)
    report = {'schemaVersion': 1, 'issue': 1434, 'mode': args.cases, 'ok': False, 'acceptanceComplete': False, 'cases': {}}
    try:
        checked(args.bin.is_file() and os.access(args.bin, os.X_OK), 'compiled CLI binary is missing or not executable')
        checked(args.vm_config.is_file(), 'VM runtime manifest is missing')
        checked(args.scratch_root.is_dir() and not args.scratch_root.is_symlink(), 'scratch root must be a real directory')
        info = args.scratch_root.stat()
        checked(info.st_uid == os.getuid() and info.st_mode & 0o777 == 0o700, 'scratch root must be owner-private (0700)')
        if args.require_vm:
            checked(sys.platform == 'linux', '--require-vm requires Linux KVM')
            descriptor = os.open('/dev/kvm', os.O_RDWR | os.O_CLOEXEC)
            os.close(descriptor)
        deno = os.environ.get('AUGMENTAGENT_DENO_BIN') or shutil.which('deno')
        checked(deno, 'Deno executable is missing')
        deno = Path(shutil.which(deno) or deno).absolute()
        report['git'] = {'head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=REPO, text=True).strip(),
                         'dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=REPO))}
        report['binary'] = {'path': str(args.bin), 'sha256': digest(args.bin)}
        report['runtimeManifest'] = {'path': str(args.vm_config), 'sha256': digest(args.vm_config)}
        versions = args.output_dir / 'versions'; versions.mkdir(mode=0o700)
        env = base_environment(versions, args, deno)
        report['versions'] = []
        for name, cmd in [('cli', [args.bin, '--version']), ('deno', [deno, '--version']), ('python', ['/usr/bin/python3', '--version'])]:
            receipt = command(cmd, versions, env, versions / f'{name}.log', 10)
            checked(receipt['exitCode'] == 0 and not receipt['failure'], f'{name} version probe failed')
            receipt['value'] = (versions / f'{name}.log').read_text().strip()
            report['versions'].append(receipt)
        harness = Harness(args, deno)
        selected = GROUPS[args.cases] - ({'xlsx'} if args.cases == 'smoke' and not args.public_packages else set())
        # Real guests and public registry cases always execute serially.
        for name in sorted(selected):
            harness.run_case(name)
            report['cases'] = harness.results
            report['acceptance'] = coverage_report(REQUIREMENTS, harness.results)
            write_json(args.output_dir / 'report.json', report)
        unchanged = report['binary']['sha256'] == digest(args.bin)
        report['binaryUnchangedDuringQa'] = unchanged
        report['acceptanceComplete'] = unchanged and not report['git']['dirty'] and all(row['complete'] for row in report['acceptance'].values())
        report['ok'] = unchanged and all(case['status'] == 'passed' for case in harness.results.values())
        if args.cases == 'all':
            report['ok'] = report['ok'] and report['acceptanceComplete']
    except Exception as error:
        report['prerequisiteFailure'] = f'{type(error).__name__}: {error}'
    finally:
        report.setdefault('acceptance', coverage_report(REQUIREMENTS, report['cases']))
        write_json(args.output_dir / 'report.json', report)
    print(json.dumps({'ok': report['ok'], 'acceptanceComplete': report['acceptanceComplete'], 'report': str(args.output_dir / 'report.json')}))
    return 0 if report['ok'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
