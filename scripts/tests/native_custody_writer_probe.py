"""Disposable native-helper ingress observations; never a custody authority.

Run directly with the helper path. Runtime validation is bypassed only to reach
the actual helper's lock and temporary-directory operations without a VM. File
permissions, leases, flock, mkdir and helper main() remain real kernel paths.
"""
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import types
import unittest


def child(helper, spec_path, report_path):
    definition = importlib.util.spec_from_file_location('native_writer_fixture', helper)
    module = importlib.util.module_from_spec(definition)
    definition.loader.exec_module(module)
    spec = json.loads(Path(spec_path).read_text())
    state = Path(spec['session_directory'])
    report = {'lock_open_returned': False, 'temporary_directory_created': False}

    class Runtime:
        @classmethod
        def load(cls, _path):
            return types.SimpleNamespace(config={})

    # This probe establishes neither runtime authenticity nor VM execution.
    module.module = lambda _name, filename: (
        types.SimpleNamespace(Runtime=Runtime)
        if filename == 'codex-build-vm.py' else types.SimpleNamespace())
    module.validate_runtime = lambda _path: None
    real_open, real_mkdir = os.open, os.mkdir

    def observed_open(path, flags, *args, **kwargs):
        descriptor = real_open(path, flags, *args, **kwargs)
        if Path(path) == state / 'lock' and flags & os.O_RDWR:
            report['lock_open_returned'] = True
        return descriptor

    def observed_mkdir(path, *args, **kwargs):
        result = real_mkdir(path, *args, **kwargs)
        if Path(path).parent == state:
            report['temporary_directory_created'] = True
        return result

    module.os.open, module.os.mkdir = observed_open, observed_mkdir
    try:
        module.main(spec_path)
        report['unexpected_completion'] = True
    except BaseException as error:
        report['exception'] = type(error).__name__
        report['errno'] = getattr(error, 'errno', None)
        filename = getattr(error, 'filename', None)
        report['refused_session_creation'] = bool(
            filename and Path(filename).parent == state
            and Path(filename).name.startswith('native-vm-launch-'))
    finally:
        module.os.open, module.os.mkdir = real_open, real_mkdir
    Path(report_path).write_text(json.dumps(report))


class NativeWriterProbe(unittest.TestCase):
    observations = []

    def exercise(self, mode):
        if os.geteuid() == 0:
            self.fail('requires ordinary owner UID; root bypasses the permission fixture')
        with tempfile.TemporaryDirectory(prefix='native-custody-helper-probe-') as temp:
            root = Path(temp)
            state = root / 'session'
            state.mkdir(mode=0o700)
            (state / 'owned.json').write_bytes(b'{"fixture":"preserve"}')
            (state / 'owned.json').chmod(0o600)
            (state / 'lock').touch(mode=0o600)
            original = (state / 'owned.json').read_bytes()
            declaration = {'version': 1, 'guest_runtime': '/fixture/unused',
                           'runtime_config': '/fixture/unused', 'memory_mb': 512,
                           'session_directory': str(state), 'resume': False,
                           'refresh_auth': False, 'auth_relative': '.fixture/auth'}
            spec = root / 'launch.json'
            spec.write_text(json.dumps(declaration))
            spec.chmod(0o600)
            result = root / 'result.json'
            descriptor = os.open(state / 'lock', os.O_RDONLY | os.O_NOFOLLOW)
            seen = []
            old_handler = signal.signal(signal.SIGIO, lambda *_: seen.append(True))
            process = None
            leased = False

            def restrict():
                (state / 'owned.json').chmod(0o400)
                (state / 'lock').chmod(0o400)
                state.chmod(0o500)

            try:
                if mode == 'pending-before-fence':
                    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_RDLCK)
                    leased = True
                elif mode == 'fenced-before-start':
                    restrict()
                process = subprocess.Popen(
                    [sys.executable, '-I', str(Path(__file__).resolve()), '--child',
                     str(HELPER), str(spec), str(result)],
                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL)
                if mode == 'pending-before-fence':
                    deadline = time.monotonic() + 5
                    while not seen and time.monotonic() < deadline:
                        self.assertIsNone(process.poll(), 'helper exited before lease break')
                        time.sleep(0.005)
                    self.assertTrue(seen, 'helper never reached real conflicting open')
                    restrict()
                    self.assertEqual(fcntl.fcntl(descriptor, fcntl.F_GETLEASE),
                                     fcntl.F_UNLCK)
                    fcntl.flock(descriptor, fcntl.LOCK_UN)
                    fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_UNLCK)
                    leased = False
                self.assertEqual(process.wait(timeout=5), 0)
                report = json.loads(result.read_text())
                self.assertEqual((state / 'owned.json').read_bytes(), original)
                self.assertEqual(sorted(p.name for p in state.iterdir()),
                                 ['lock', 'owned.json'])
                report.update(mode=mode, original_bytes_preserved=True,
                              grants_capacity=False, guest_or_provider_started=False)
                self.observations.append(report)
                return report
            finally:
                if process is not None and process.poll() is None:
                    process.kill()
                    process.wait()
                if leased:
                    fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_UNLCK)
                os.close(descriptor)
                signal.signal(signal.SIGIO, old_handler)
                state.chmod(0o700)
                for path in state.iterdir():
                    path.chmod(0o600)

    def test_normal_helper_reaches_real_temporary_directory_creation(self):
        report = self.exercise('normal')
        self.assertTrue(report['lock_open_returned'])
        self.assertTrue(report['temporary_directory_created'])
        # The fixture deliberately omits executable, stopping before any VM.
        self.assertEqual(report['exception'], 'KeyError')

    def test_restricted_helper_refuses_before_lock_open(self):
        report = self.exercise('fenced-before-start')
        self.assertFalse(report['lock_open_returned'])
        self.assertFalse(report['temporary_directory_created'])
        self.assertEqual(report['errno'], errno.EACCES)

    def test_delayed_real_helper_lock_open_cannot_create_session_data(self):
        report = self.exercise('pending-before-fence')
        self.assertTrue(report['lock_open_returned'])
        self.assertFalse(report['temporary_directory_created'])
        self.assertTrue(report['refused_session_creation'])
        self.assertEqual(report['errno'], errno.EACCES)


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--child':
        child(*sys.argv[2:])
    else:
        HELPER = Path(sys.argv.pop(1)).resolve()
        result = unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(NativeWriterProbe))
        print(json.dumps({'observations': NativeWriterProbe.observations,
                          'grants_capacity': False}))
        raise SystemExit(not result.wasSuccessful())
