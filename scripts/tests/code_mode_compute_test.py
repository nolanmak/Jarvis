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


if __name__ == '__main__':
    unittest.main()
