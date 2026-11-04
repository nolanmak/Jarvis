"""Real Rust std::process spawning under the deny-exec preload experiment."""
import importlib.util
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('cargo_relay', ROOT / 'ci-cargo-relay.py')
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)
FIXTURE = 'self_improve::shared_ci::acceptance::cargo_relay_spawn_interface_fixture'


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(any(os.environ.get(name) for name in ('AUGMENTAGENT_CARGO_RELAY_LIBRARY',
    'AUGMENTAGENT_RELAY_FIXTURE_BINARY')),'requires managed native relay build; mandatory through the Rust adapter')
class NativeRelayTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix='jcr-native-', dir='/tmp')))
        self.root.chmod(0o700)
        self.library = Path(os.environ['AUGMENTAGENT_CARGO_RELAY_LIBRARY']).resolve(strict=True)
        self.binary = Path(os.environ['AUGMENTAGENT_RELAY_FIXTURE_BINARY']).resolve(strict=True)

    def launch(self, fd, *, extra_fds=(), extra_env=None):
        environment = {'PATH': os.defpath, 'LD_PRELOAD': str(self.library),
            'JARVIS_CARGO_RELAY_FD': str(fd), 'JARVIS_CARGO_RELAY_FIXTURE': '1'}
        environment.update(extra_env or {})
        return subprocess.run([str(self.binary), '--exact', FIXTURE, '--nocapture', '--test-threads=1'],
            cwd=self.root, env=environment, pass_fds=(fd, *extra_fds), capture_output=True, timeout=20)

    def test_real_rust_spawn_drains_large_output_and_preserves_exit_env_cwd_and_jobserver(self):
        with relay.Relay(self.root, library=self.library, timeout=10) as server:
            fd = server.config_fd()
            read, write = os.pipe()
            try:
                os.write(write, b'+')
                result = self.launch(fd, extra_fds=(read,), extra_env={'JARVIS_RELAY_TEST_JOB_FD': str(read)})
            finally:
                os.close(fd); os.close(read); os.close(write)
            self.assertEqual(result.returncode, 0, (result.stdout+result.stderr).decode(errors='replace'))
            self.assertIn(b'RAW_EXEC_DENIED', result.stdout)
            self.assertIn(b'RELAY_STD_COMPLETE', result.stdout)
            self.assertEqual(len(server.completed), 2)
            self.assertTrue(all(item['cleanup'] == 'all-descendants-reaped' for item in server.completed))

    def test_missing_listener_never_falls_back_to_native_execution(self):
        with relay.Relay(self.root, library=self.library) as server:
            fd = server.config_fd()
        read, write = os.pipe()
        try:
            os.write(write, b'+')
            result = self.launch(fd, extra_fds=(read,), extra_env={'JARVIS_RELAY_TEST_JOB_FD': str(read)})
        finally:
            os.close(fd); os.close(read); os.close(write)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'RAW_EXEC_DENIED', result.stdout)
        self.assertNotIn(b'RELAY_STD_COMPLETE', result.stdout)

    def test_python_async_popen_returns_before_the_delegated_child_exits(self):
        # Keep CPython's default process-launch settings. The child requires a
        # parent acknowledgement, so retaining a vfork parent until tool exit
        # is a real protocol deadlock, not merely slower synchronous work.
        policy=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve(strict=True)
        program='''import ctypes,subprocess,sys,time
from pathlib import Path
assert ctypes.CDLL(sys.argv[1],use_errno=True).jarvis_ci_restrict_unit()==0
marker=Path(sys.argv[2]);started=time.monotonic()
body="import sys,time;from pathlib import Path;print('READY',flush=True);p=Path(sys.argv[1]);"+"\\nwhile not p.exists():time.sleep(.01)\\nsys.exit(23)"
child=subprocess.Popen(['/usr/bin/python3','-I','-c',body,str(marker)],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,text=True)
assert child.poll() is None,'Popen waited for delegated child termination'
assert child.stdout.readline()=='READY\\n'
marker.write_text('parent resumed')
output,_=child.communicate(timeout=2)
assert child.returncode==23,(child.returncode,output)
print('PYTHON_ASYNC_RELAY_COMPLETE',flush=True)
'''
        with relay.Relay(self.root,library=self.library,timeout=3) as server:
            fd=server.config_fd()
            try:
                result=subprocess.run(['/usr/bin/python3','-I','-c',program,str(policy),str(self.root/'ack')],
                    cwd=self.root,env={'PATH':os.defpath,'LD_PRELOAD':str(self.library),'JARVIS_CARGO_RELAY_FD':str(fd)},
                    pass_fds=(fd,),capture_output=True,text=True,timeout=10)
            finally:os.close(fd)
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)
        self.assertEqual(result.stdout,'PYTHON_ASYNC_RELAY_COMPLETE\n')
        self.assertTrue((self.root/'ack').is_file())
        self.assertEqual(len(server.completed),1)
        self.assertEqual(server.completed[0]['cleanup'],'all-descendants-reaped')

    def test_unsealed_configuration_refuses_before_rust_main(self):
        fd = os.memfd_create('unsealed-config', os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
        try:
            os.write(fd, bytes(relay.CONFIG.size))
            result = self.launch(fd)
        finally: os.close(fd)
        self.assertEqual(result.returncode, 125)
        self.assertNotIn(b'RAW_EXEC_DENIED', result.stdout)

    def test_regular_file_cannot_replace_a_sealed_configuration(self):
        with relay.Relay(self.root, library=self.library) as server:
            sealed = server.config_fd()
            try: data = os.pread(sealed, relay.CONFIG.size, 0)
            finally: os.close(sealed)
            path = self.root / 'regular-config'
            path.write_bytes(data)
            with path.open('rb') as stream:
                result = self.launch(stream.fileno())
            self.assertEqual(result.returncode, 125)
            self.assertNotIn(b'RAW_EXEC_DENIED', result.stdout)
            self.assertEqual(server.completed, [])

    def test_unknown_inheritable_regular_fd_refuses_before_delegating(self):
        with relay.Relay(self.root, library=self.library) as server:
            fd = server.config_fd()
            read, write = os.pipe()
            private = os.open(self.root / 'private', os.O_CREAT | os.O_RDWR, 0o600)
            try:
                os.write(write, b'+')
                result = self.launch(fd, extra_fds=(read, private),
                    extra_env={'JARVIS_RELAY_TEST_JOB_FD': str(read)})
            finally:
                os.close(fd); os.close(read); os.close(write); os.close(private)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b'RAW_EXEC_DENIED', result.stdout)
            self.assertNotIn(b'RELAY_STD_COMPLETE', result.stdout)
            self.assertEqual(server.completed, [])

    def signal_case(self, mode, marker):
        with relay.Relay(self.root, library=self.library, timeout=10) as server:
            fd = server.config_fd()
            try: result = self.launch(fd, extra_env={'JARVIS_RELAY_TEST_MODE': mode})
            finally: os.close(fd)
            self.assertEqual(result.returncode, 0, (result.stdout+result.stderr).decode(errors='replace'))
            self.assertIn(marker, result.stdout)
            self.assertEqual(len(server.completed), 2 if mode == 'signal-exit' else 1)
            self.assertTrue(all(item['cleanup'] == 'all-descendants-reaped' for item in server.completed))

    def test_child_inherits_ignored_signals_and_blocked_mask(self):
        self.signal_case('signal-state', b'RELAY_SIGNAL_STATE_COMPLETE')

    def test_signal_termination_is_not_an_ordinary_exit_143(self):
        self.signal_case('signal-exit', b'RELAY_SIGNAL_EXIT_COMPLETE')

    def test_signal_to_child_pid_reaches_the_actual_tool_handler(self):
        self.signal_case('signal-forward', b'RELAY_SIGNAL_FORWARD_COMPLETE')


if __name__ == '__main__':
    unittest.main()
