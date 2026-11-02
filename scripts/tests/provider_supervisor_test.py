"""Real descriptor inheritance and cleanup through the production supervisor."""
import ctypes
import fcntl
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import time
import unittest

SUPERVISOR = Path(__file__).resolve().parents[1] / 'provider-supervisor.py'
DRIVER = r'''
import importlib.util,json,os,sys,time
from pathlib import Path
spec=importlib.util.spec_from_file_location('supervisor',sys.argv[1])
supervisor=importlib.util.module_from_spec(spec);spec.loader.exec_module(supervisor)
options=json.loads(sys.argv[3])
raw_status=options.pop('raw_status',False)
observer_fault=options.pop('observer_fault',False)
def await_fixture():
    ready=Path(sys.argv[2]).with_name('pids')
    deadline=time.monotonic()+3
    while not ready.exists() or not ready.stat().st_size:
        if time.monotonic()>deadline:raise RuntimeError('fixture did not start')
        time.sleep(.005)
if options.pop('fault_after_spawn',False):
    original=os.close
    target=options['close_after_spawn'][0]
    def fail_close(fd):
        original(fd)
        if fd==target:
            await_fixture()
            raise OSError(5,'injected close failure')
    supervisor.os.close=fail_close
try:
    extra={'return_raw_status':True} if raw_status else {}
    if observer_fault:
        def observed(child):
            await_fixture()
            raise OSError(5,'injected observer failure')
        extra['on_spawn']=observed
    result=supervisor.supervise(sys.argv[2],sys.argv[4:],
        forward_fds=tuple(options['forward_fds']),
        close_after_spawn=tuple(options['close_after_spawn']),**extra)
    if raw_status:
        print('RAW_STATUS='+str(result),flush=True)
        result=0
except Exception:
    result=125
raise SystemExit(result)
'''


@unittest.skipUnless(sys.platform == 'linux', 'requires Linux subreaper support')
class SupervisorDescriptorTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.receipt = self.root / 'cleanup'
        # A faulty supervisor must not leave a fixture orphan or zombie behind.
        self.assertEqual(ctypes.CDLL(None).prctl(36, 1, 0, 0, 0), 0)

    def launch(self, code, forward=(), close=(), fault=False, storage=(), raw_status=False, observer_fault=False):
        options = {'forward_fds': forward, 'close_after_spawn': close,
                   'fault_after_spawn': fault, 'raw_status': raw_status, 'observer_fault': observer_fault}
        env = {'PATH': os.defpath}
        if storage:
            env['JARVIS_STORAGE_LEASE_FD'] = str(storage[0])
            env['JARVIS_STORAGE_LEASE_FDS'] = ','.join(map(str, storage))
        child = subprocess.Popen([sys.executable, '-I', '-c', DRIVER,
            str(SUPERVISOR), str(self.receipt), json.dumps(options),
            sys.executable, '-I', '-c', code], env=env,
            pass_fds=tuple(sorted(set(forward) | set(storage))),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        def cleanup():
            if child.poll() is None:
                child.terminate()
            try:
                child.communicate(timeout=8)
            finally:
                pids = self.root / 'pids'
                if pids.exists():
                    for pid in map(int, pids.read_text().split()):
                        try: os.kill(pid, signal.SIGKILL)
                        except ProcessLookupError: pass
                        try: os.waitpid(pid, 0)
                        except ChildProcessError: pass
        self.addCleanup(cleanup)
        return child

    def test_exec_start_eof_precedes_large_output_and_tool_exit(self):
        start_read, start_write = os.pipe()
        job_read, job_write = os.pipe()
        self.addCleanup(os.close, start_read)
        self.addCleanup(os.close, job_write)
        os.write(job_write, b'+')
        tool = ('import os; '
                f'assert os.read({job_read},1)==b"+"; '
                'os.write(1,b"x"*(1024*1024)); raise SystemExit(17)')
        bootstrap = ('import os,sys; '
                     f'os.set_inheritable({start_write},False); '
                     f'os.execve(sys.executable,[sys.executable,"-I","-c",{tool!r}],{{}})')
        try:
            child = self.launch(bootstrap, (start_write, job_read), (start_write,))
        finally:
            os.close(start_write)
            os.close(job_read)
        # Reading stdout first would hide the deadlock: Rust Command.spawn()
        # waits for its CLOEXEC error pipe before it can drain captured output.
        with selectors.DefaultSelector() as ready:
            ready.register(start_read, selectors.EVENT_READ)
            self.assertTrue(ready.select(3), 'exec-start pipe stayed open in supervisor')
        self.assertEqual(os.read(start_read, 1), b'')
        output, error = child.communicate(timeout=5)
        self.assertEqual(child.returncode, 17, error.decode())
        self.assertEqual(output, b'x' * (1024 * 1024))
        self.assertEqual(self.receipt.read_bytes(), b'all-descendants-reaped\n')

    def test_storage_ownership_cannot_be_closed_as_exec_start_pipe(self):
        path = self.root / 'storage.lock'
        fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
        self.addCleanup(os.close, fd)
        fcntl.flock(fd, fcntl.LOCK_EX)
        effect = self.root / 'effect'
        child = self.launch(f'open({str(effect)!r},"w").write("ran")',
                            (fd,), (fd,), storage=(fd,))
        child.communicate(timeout=5)
        self.assertNotEqual(child.returncode, 0)
        self.assertFalse(effect.exists())
        with path.open('r+') as contender:
            with self.assertRaises(BlockingIOError):
                fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_post_spawn_close_failure_reaps_detached_descendants_before_receipt(self):
        for fault in ('close', 'observer'):
            with self.subTest(fault=fault):
                start_read, start_write = os.pipe()
                self.addCleanup(os.close, start_read)
                pids = self.root / 'pids'
                code = ('import os,time; '
                        f'os.close({start_write}); '
                        'os.close(1); os.close(2); '
                        'pid=os.fork(); '
                        'os.setsid() if pid==0 else None; '
                        f'open({str(pids)!r},"w").write(str(os.getpid())+" "+str(pid)) if pid else None; '
                        'time.sleep(30)')
                try:
                    child = self.launch(code, (start_write,), (start_write,), fault=fault=='close', observer_fault=fault=='observer')
                finally:
                    os.close(start_write)
                _, error = child.communicate(timeout=8)
                self.assertNotEqual(child.returncode, 0, error.decode())
                self.assertTrue(pids.exists(), 'fault must occur after descendants start')
                self.assertEqual(self.receipt.read_bytes(), b'all-descendants-reaped\n')
                for pid in map(int, pids.read_text().split()):
                    self.assertFalse(Path(f'/proc/{pid}').exists(), 'receipt preceded descendant cleanup')
                self.receipt.unlink()
                pids.unlink()

    def test_invalid_or_missing_descriptors_never_launch(self):
        for invalid in ((True,), (2,), (999999,), (3, 3)):
            with self.subTest(descriptors=invalid):
                effect = self.root / 'effect'
                # Bad descriptors are deliberately absent in the driver.
                options = json.dumps({'forward_fds': invalid, 'close_after_spawn': []})
                result = subprocess.run([sys.executable, '-I', '-c', DRIVER,
                    str(SUPERVISOR), str(self.receipt), options,
                    sys.executable, '-I', '-c', f'open({str(effect)!r},"w").write("ran")'],
                    env={'PATH': os.defpath}, capture_output=True, timeout=5)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(effect.exists())
                if self.receipt.exists(): self.receipt.unlink()

    def test_raw_status_distinguishes_sigterm_from_exit_143_after_original_cleanup(self):
        for code, expected in [('raise SystemExit(143)', 143),
                               ('import os,signal;os.kill(os.getpid(),signal.SIGTERM)', -15)]:
            with self.subTest(expected=expected):
                child = self.launch(code, raw_status=True)
                output, error = child.communicate(timeout=5)
                self.assertEqual(child.returncode, 0, error.decode())
                self.assertEqual(output, f'RAW_STATUS={expected}\n'.encode())
                self.assertEqual(self.receipt.read_bytes(), b'all-descendants-reaped\n')
                self.receipt.unlink()


if __name__ == '__main__':
    unittest.main()
