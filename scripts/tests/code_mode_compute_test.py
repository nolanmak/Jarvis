"""Compute requests must be validated before any VM or package transport exists."""
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('compute', Path(__file__).parents[1] / 'code-mode-compute.py')
compute = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compute)


def request(**changes):
    return {'runtime': 'python', 'dependencies': [], 'code': 'print(1)', **changes}


class RequestContractTests(unittest.TestCase):
    def denied(self, value, code='bad_args', maximum=600):
        with self.assertRaises(compute.ComputeError) as error:
            compute.validate_request(value, maximum)
        self.assertEqual(error.exception.code, code)

    def test_defaults_and_normalized_resolution_request(self):
        got = compute.validate_request(request(dependencies=['Open_PyXL == 3.1.5', 'pandas>=2,<3']))
        self.assertEqual(got['dependencies'], ['open-pyxl==3.1.5', 'pandas<3,>=2'])
        self.assertEqual(got['timeoutSecs'], 600)
        self.assertEqual(got['inputs'], [])
        self.assertEqual(got['outputs'], [])

    def test_unknown_fields_and_wrong_types_have_no_authority(self):
        for value in (None, [], True, request(runtime='node'), request(code=1),
                      request(registry='https://attacker.invalid'), request(sessionId='other'),
                      request(dependencies='pandas'), request(inputs={}), request(outputs='out'),
                      {'code': 'pass'}, request(code='\0')):
            with self.subTest(value=value):
                self.denied(value)

    def test_source_byte_limit_includes_multibyte(self):
        compute.validate_request(request(code='é' * (128 * 1024)))
        self.denied(request(code='é' * (128 * 1024) + 'a'))

    def test_dependency_policy_rejects_flags_paths_urls_extras_markers_and_duplicates(self):
        for deps in (['--index-url=https://attacker.invalid'], ['/tmp/package'], ['../pkg'],
                     ['foo @ https://attacker.invalid/a.whl'], ['foo[extra]'], ['foo; python_version>"3"'],
                     ['foo\nbar'], ['foo\rbar'], ['foo===1'], ['foo==not-a-version'],
                     ['Foo_Bar', 'foo-bar'], ['foo..bar', 'foo-bar'], [True]):
            with self.subTest(deps=deps):
                self.denied(request(dependencies=deps), 'dependency_policy_denied')

    def test_count_limits(self):
        compute.validate_request(request(dependencies=[f'pkg{i}' for i in range(32)]))
        self.denied(request(dependencies=[f'pkg{i}' for i in range(33)]))
        for key in ('inputs', 'outputs'):
            items = [f'f{i}' if key == 'outputs' else {'artifactId': f'id{i}', 'name': f'f{i}'} for i in range(33)]
            compute.validate_request(request(**{key: items[:32]}))
            self.denied(request(**{key: items}))

    def test_names_reject_traversal_unicode_duplicates_and_nonfiles(self):
        for name in ('', '.', '..', '../out', '/tmp/out', 'a/b', 'a\\b', 'a\0b', 'é', 'a' * 129, 1):
            with self.subTest(name=name):
                self.denied(request(outputs=[name]))
                self.denied(request(inputs=[{'artifactId': 'abc', 'name': name}]))
        self.denied(request(outputs=['same', 'same']))
        self.denied(request(inputs=[{'artifactId': 'a', 'name': 'same'}, {'artifactId': 'b', 'name': 'same'}]))
        compute.validate_request(request(outputs=['a' * 128, 'report.json']))

    def test_input_handles_are_required_and_do_not_accept_host_paths(self):
        for entry in ({'name': 'f'}, {'artifactId': '', 'name': 'f'},
                      {'artifactId': 'a', 'name': 'f', 'path': '/etc/passwd'},
                      {'artifactId': 1, 'name': 'f'}):
            self.denied(request(inputs=[entry]))

    def test_timeout_is_integer_bounded_and_never_clamped(self):
        for value in (0, -1, 601, 1.5, True, '20', None):
            with self.subTest(value=value):
                self.denied(request(timeoutSecs=value))
        self.assertEqual(compute.validate_request(request(timeoutSecs=1))['timeoutSecs'], 1)
        self.assertEqual(compute.validate_request(request(), 900)['timeoutSecs'], 900)

    def test_operator_maximum_is_validated_before_use(self):
        for maximum in (0, -1, 901, True, '600', 2.5):
            self.denied(request(), 'sandbox_unavailable', maximum)

    def test_validation_does_not_mutate_caller_request(self):
        value = request(dependencies=['OpenPyXL'])
        compute.validate_request(value)
        self.assertEqual(value, request(dependencies=['OpenPyXL']))




# Explicit opt-in: unit tests do not pretend to prove the VM boundary.
import os
import tempfile


@unittest.skipUnless(os.environ.get('JARVIS_TEST_VM_CONFIG'), 'requires provisioned KVM runtime')
class ComputeVMTests(unittest.TestCase):
    def test_dependency_free_execution_exports_after_vm_shutdown(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = compute.run_execution(os.environ['JARVIS_TEST_VM_CONFIG'], Path(tmp),
                request(code="from pathlib import Path\nPath('/outputs/result.json').write_text('{\"total\":60}')\nprint('computed')",
                        outputs=['result.json']), timeout=30)
            self.assertTrue(result['ok'], result)
            self.assertEqual(result['runner'], 'vm')
            self.assertEqual(result['stdout'].strip(), 'computed')
            self.assertEqual(result['files']['result.json'], b'{"total":60}')
            self.assertTrue(result['cleanupVerified'])

    def test_timeout_reaps_owned_vm_and_removes_command_scratch(self):
        import time
        with tempfile.TemporaryDirectory() as tmp:
            started = time.monotonic()
            with self.assertRaises(compute.ComputeError) as failure:
                compute.run_execution(os.environ['JARVIS_TEST_VM_CONFIG'], Path(tmp),
                                      request(code='import time; time.sleep(30)'), timeout=2)
            self.assertEqual(failure.exception.code, 'timeout')
            self.assertEqual(failure.exception.runner, 'vm')
            self.assertLess(time.monotonic() - started, 7)
            self.assertEqual(list(Path(tmp).iterdir()), [])
            for process in Path('/proc').iterdir():
                if process.name.isdigit():
                    try:
                        command = (process / 'cmdline').read_bytes()
                    except OSError:
                        continue
                    self.assertNotIn((tmp + '/compute-execution-').encode(), command)

    def test_symlink_hardlink_and_fifo_outputs_are_rejected(self):
        for code in ("import os; os.symlink('/etc/passwd','/outputs/bad')",
                     "from pathlib import Path; import os; Path('/outputs/other').write_text('x'); os.link('/outputs/other','/outputs/bad')",
                     "import os; os.mkfifo('/outputs/bad')"):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as tmp:
                result = compute.run_execution(os.environ['JARVIS_TEST_VM_CONFIG'], Path(tmp),
                                               request(code=code, outputs=['bad']), timeout=15)
                self.assertFalse(result['ok'])
                self.assertEqual(result['error'], 'output_denied')
                self.assertEqual(result['files'], {})
                self.assertTrue(result['cleanupVerified'])

    def test_execution_cannot_access_host_or_network(self):
        code = '''
import os, socket
from pathlib import Path
try:
    assert not Path('/root/control').exists()
except PermissionError:
    pass
assert not Path('/workspace').exists()
assert not Path('/home/nolan-makatche').exists()
assert 'SYNTHETIC_SECRET' not in os.environ
s = socket.socket()
s.settimeout(0.2)
try:
    s.connect(('169.254.169.254', 80))
    raise AssertionError('network connected')
except OSError:
    pass
print('isolated')
'''
        with tempfile.TemporaryDirectory() as tmp:
            result = compute.run_execution(os.environ['JARVIS_TEST_VM_CONFIG'], Path(tmp),
                                           request(code=code), timeout=30)
            self.assertTrue(result['ok'], result)
            self.assertEqual(result['stdout'].strip(), 'isolated')


class ArtifactCapabilityTests(unittest.TestCase):
    def test_input_is_a_private_snapshot_and_id_is_task_scoped(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'one').mkdir(mode=0o700); (root / 'two').mkdir(mode=0o700)
            source = root / 'source.csv'; source.write_bytes(b'a,b\n1,2\n')
            one = compute.ArtifactStore(root / 'one')
            artifact = one.import_file(source, 'sheet.csv')
            self.assertNotIn(str(source), artifact['id'])
            source.write_bytes(b'changed')
            resolved = one.resolve_inputs([{'artifactId': artifact['id'], 'name': 'renamed.csv'}])
            self.assertEqual(resolved, {'renamed.csv': b'a,b\n1,2\n'})
            two = compute.ArtifactStore(root / 'two')
            with self.assertRaises(compute.ComputeError) as failure:
                two.resolve_inputs([{'artifactId': artifact['id'], 'name': 'stolen.csv'}])
            self.assertEqual(failure.exception.code, 'input_denied')

    def test_import_rejects_links_and_special_files_without_reading(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); (root / 'store').mkdir(mode=0o700)
            store = compute.ArtifactStore(root / 'store')
            source = root / 'source'; source.write_bytes(b'canary')
            link = root / 'link'; link.symlink_to(source)
            parent = root / 'parent'; parent.symlink_to(root, target_is_directory=True)
            fifo = root / 'fifo'; os.mkfifo(fifo)
            hardlink = root / 'hardlink'; os.link(source, hardlink)
            for path in (link, parent / 'source', fifo, source, hardlink, root):
                with self.subTest(path=path), self.assertRaises(compute.ComputeError):
                    store.import_file(path, 'input')

    def test_unknown_handle_cannot_be_interpreted_as_a_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = compute.ArtifactStore(Path(tmp))
            for identifier in ('/etc/passwd', '../another-task/file', 'made-up-id'):
                with self.subTest(identifier=identifier), self.assertRaises(compute.ComputeError) as failure:
                    store.resolve_inputs([{'artifactId': identifier, 'name': 'file'}])
                self.assertEqual(failure.exception.code, 'input_denied')

    def test_untrusted_store_directory_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); child = root / 'child'; child.mkdir(mode=0o755)
            link = root / 'link'; link.symlink_to(root, target_is_directory=True)
            for directory in (child, link):
                with self.subTest(directory=directory), self.assertRaises(compute.ComputeError):
                    compute.ArtifactStore(directory)


@unittest.skipUnless(os.environ.get('JARVIS_TEST_VM_CONFIG') and os.environ.get('JARVIS_TEST_COMPUTE_PIP'),
                     'requires KVM and a private pinned pip runtime')
class DependencyPreparationTests(unittest.TestCase):
    def test_task_reuses_real_environment_and_chains_an_exported_input(self):
        import json
        with tempfile.TemporaryDirectory() as tmp:
            backend = compute.VMBackend(os.environ['JARVIS_TEST_VM_CONFIG'], '/mnt/build/codex-vm',
                json.loads(Path(os.environ['JARVIS_TEST_COMPUTE_PIP']).read_text()))
            self.addCleanup(backend.close)
            artifacts = compute.ArtifactStore(Path(tmp))
            task = compute.ComputeTask(backend, artifacts, enabled=True)
            first = task.execute(request(dependencies=['openpyxl==3.1.5'], outputs=['input.xlsx'], code="""
import openpyxl
book = openpyxl.Workbook()
for n in (10,20,30): book.active.append([n])
book.save('/outputs/input.xlsx')
"""))
            self.assertTrue(first['ok'], first)
            second = task.execute(request(dependencies=['openpyxl==3.1.5'], outputs=['summary.json'],
                inputs=[{'artifactId': first['artifacts'][0]['id'], 'name': 'input.xlsx'}], code="""
import json,openpyxl
from pathlib import Path
book = openpyxl.load_workbook('/inputs/input.xlsx')
rows = list(book.active.values)
Path('/outputs/summary.json').write_text(json.dumps({'count':len(rows),'total':sum(row[0] for row in rows)}))
try:
    Path('/inputs/input.xlsx').write_bytes(b'mutated')
    raise AssertionError('input writable')
except OSError:
    pass
"""))
            self.assertTrue(second['ok'], second)
            self.assertTrue(second['environmentReused'])
            self.assertEqual(first['dependencyLock'], second['dependencyLock'])
            self.assertEqual(task.records[-1]['downloads']['requests'], 0)
            data = artifacts.resolve_inputs([{'artifactId': second['artifacts'][0]['id'], 'name': 'summary'}])
            self.assertEqual(json.loads(data['summary']), {'count':3,'total':60})

    def test_real_wheels_install_and_are_readonly_without_gateway_during_execution(self):
        import json
        bridge_spec = importlib.util.spec_from_file_location('compute_bridge', Path(__file__).parents[1] / 'codex-tool-bridge.py')
        bridge = importlib.util.module_from_spec(bridge_spec); bridge_spec.loader.exec_module(bridge)
        scratch = bridge.BuildScratch('/mnt/build/codex-vm')
        scratch.open()
        self.addCleanup(scratch.close)
        pip_runtime = json.loads(Path(os.environ['JARVIS_TEST_COMPUTE_PIP']).read_text())
        environment_id = 'a' * 32
        prepared = compute.run_preparation(os.environ['JARVIS_TEST_VM_CONFIG'], scratch.tmp, scratch.cache,
            pip_runtime, ['openpyxl==3.1.5'], environment_id, timeout=90)
        self.assertTrue(prepared['ok'], prepared)
        self.assertEqual({item['name'] for item in prepared['dependencyLock']}, {'openpyxl', 'et-xmlfile'})
        self.assertGreater(prepared['downloads']['requests'], 0)
        code = """
import openpyxl
from pathlib import Path
book = openpyxl.Workbook()
book.active.append([10]); book.active.append([20]); book.active.append([30])
book.save('/outputs/input.xlsx')
assert sum(row[0] for row in book.active.values) == 60
try:
    Path(openpyxl.__file__).write_text('mutated')
    raise AssertionError('dependency environment is writable')
except OSError:
    pass
try:
    assert not Path('/root/control').exists()
except PermissionError:
    pass
print('60')
"""
        executed = compute.run_execution(os.environ['JARVIS_TEST_VM_CONFIG'], scratch.tmp,
            request(code=code, outputs=['input.xlsx']), timeout=30,
            cache=scratch.cache, environment_id=environment_id)
        self.assertTrue(executed['ok'], executed)
        self.assertEqual(executed['stdout'].strip(), '60')
        self.assertTrue(executed['files']['input.xlsx'].startswith(b'PK'))
        self.assertEqual(executed['downloads']['requests'], 0)


class TaskLifecycleContractTests(unittest.TestCase):
    class Backend:
        fingerprint = 'trusted-runtime-fixture'
        def __init__(self):
            self.prepares = []; self.executions = []; self.preparation_failure = False
        def admit(self):
            from contextlib import nullcontext
            return nullcontext()
        def prepare(self, requirements, deadline):
            self.prepares.append((requirements, deadline))
            if self.preparation_failure:
                raise compute.ComputeError('dependency_unavailable', 'Unavailable fixture.')
            return {'environmentId': 'a' * 32,
                    'dependencyLock': [{'name': 'example', 'version': '1', 'sha256': 'a' * 64}],
                    'downloads': {'requests': 2, 'bytes': 64}}
        def execute(self, request, inputs, environment_id, deadline):
            self.executions.append((request, inputs, environment_id, deadline))
            return {'ok': True, 'runner': 'vm', 'exitCode': 0, 'error': None,
                    'stdout': 'computed', 'stderr': '', 'files': {'result.json': b'{"total":60}'},
                    'cleanupVerified': True}

    def setup_task(self, root, **options):
        backend = self.Backend()
        artifacts = compute.ArtifactStore(root)
        return backend, compute.ComputeTask(backend, artifacts, enabled=True, **options)

    def test_identical_constraints_reuse_lock_and_changed_constraints_prepare_again(self):
        with tempfile.TemporaryDirectory() as tmp:
            backend, task = self.setup_task(Path(tmp))
            spec = request(dependencies=['Example==1'], outputs=['result.json'])
            first = task.execute(spec); second = task.execute(spec)
            self.assertTrue(first['ok']); self.assertTrue(second['ok'])
            self.assertFalse(first['environmentReused']); self.assertTrue(second['environmentReused'])
            self.assertEqual(first['dependencyLock'], second['dependencyLock'])
            self.assertEqual(len(backend.prepares), 1)
            self.assertEqual(len(backend.executions), 2)
            task.execute(request(dependencies=['Example>=1'], outputs=['result.json']))
            self.assertEqual(len(backend.prepares), 2)

    def test_output_handle_can_be_selected_as_next_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            backend, task = self.setup_task(Path(tmp))
            first = task.execute(request(outputs=['result.json']))
            self.assertTrue(first['ok'])
            artifact = first['artifacts'][0]
            task.execute(request(inputs=[{'artifactId': artifact['id'], 'name': 'prior.json'}], outputs=['result.json']))
            self.assertEqual(backend.executions[-1][1], {'prior.json': b'{"total":60}'})

    def test_missing_input_fails_before_backend_or_resolution(self):
        with tempfile.TemporaryDirectory() as tmp:
            backend, task = self.setup_task(Path(tmp))
            result = task.execute(request(inputs=[{'artifactId': 'foreign', 'name': 'file'}]))
            self.assertFalse(result['ok']); self.assertEqual(result['error']['code'], 'input_denied')
            self.assertEqual(backend.prepares, []); self.assertEqual(backend.executions, [])

    def test_failed_preparation_is_never_reused(self):
        with tempfile.TemporaryDirectory() as tmp:
            backend, task = self.setup_task(Path(tmp))
            backend.preparation_failure = True
            first = task.execute(request(dependencies=['example'], outputs=['result.json']))
            self.assertFalse(first['ok'])
            backend.preparation_failure = False
            second = task.execute(request(dependencies=['example'], outputs=['result.json']))
            self.assertTrue(second['ok']); self.assertFalse(second['environmentReused'])
            self.assertEqual(len(backend.prepares), 2)

    def test_disabled_and_expired_tasks_never_start_backend(self):
        with tempfile.TemporaryDirectory() as tmp:
            now = [10.0]
            backend, task = self.setup_task(Path(tmp), task_timeout=2, clock=lambda: now[0])
            now[0] = 12.0
            result = task.execute(request())
            self.assertEqual(result['error']['code'], 'timeout')
            disabled = compute.ComputeTask(backend, task.artifacts, enabled=False)
            self.assertEqual(disabled.execute(request())['error']['code'], 'compute_disabled')
            self.assertEqual(backend.prepares, []); self.assertEqual(backend.executions, [])


class HostProtocolTests(unittest.TestCase):
    def test_disabled_helper_rejects_compute_without_reading_runtime(self):
        import json
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); (root / 'artifacts').mkdir(mode=0o700)
            policy = root / 'policy.json'
            policy.write_text(json.dumps({'runtime': '/absent/runtime.json', 'scratch': '/absent/scratch',
                'artifactRoot': str(root / 'artifacts'), 'enabled': False, 'callTimeoutSecs': 600,
                'taskTimeoutSecs': 1800, 'inputFiles': {}})); policy.chmod(0o600)
            result = subprocess.run([sys.executable, '-I', str(Path(compute.__file__).resolve()), '--serve', str(policy)],
                input=json.dumps({'execute':request()})+'\n'+json.dumps({'close':True})+'\n',
                text=True, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            frames = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertTrue(frames[0]['ready'])
            self.assertEqual(frames[1]['result']['error']['code'], 'compute_disabled')
            self.assertTrue(frames[2]['cleanupVerified'])


if __name__ == '__main__':
    unittest.main()
