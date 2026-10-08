"""Deadline and process ownership for trusted compute initialization commands."""
import importlib.util
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest

SPEC = importlib.util.spec_from_file_location('compute_initialization', Path(__file__).parents[1] / 'code-mode-compute.py')
compute = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compute)
SUPERVISOR = Path(__file__).parents[1] / 'provider-supervisor.py'


class InitializationTests(unittest.TestCase):
    def phase(self, *args):
        self.assertTrue(hasattr(compute, 'run_host_phase'), 'initialization needs supervised deadline enforcement')
        return compute.run_host_phase(*args)

    def test_expired_initialization_cannot_launch_or_create_scratch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaises(compute.ComputeError) as error:
                self.phase([sys.executable, '-c', f'open({str(root / "started")!r},"w").close()'], root, time.monotonic() - 1)
            self.assertEqual(error.exception.code, 'timeout')
            self.assertEqual(list(root.iterdir()), [])

    def test_timeout_reaps_detached_descendants_and_removes_phase_scratch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = f"""import os,time
child = os.fork()
if child == 0:
    os.setsid()
    open({str(root / 'detached.pid')!r},'w').write(str(os.getpid()))
else:
    open({str(root / 'worker.pid')!r},'w').write(str(os.getpid()))
time.sleep(30)
"""
            started = time.monotonic()
            with self.assertRaises(compute.ComputeError) as error:
                self.phase([sys.executable, '-c', source], root, started + 1)
            self.assertEqual(error.exception.code, 'timeout')
            self.assertLess(time.monotonic() - started, 6)
            for name in ('worker.pid', 'detached.pid'):
                pid = int((root / name).read_text())
                self.assertFalse(Path(f'/proc/{pid}').exists(), f'initialization left descendant {pid}')
            self.assertFalse(list(root.glob('compute-initialize-*')))

    def test_unverifiable_shutdown_never_uses_an_unbounded_wait(self):
        from unittest.mock import Mock, patch
        with tempfile.TemporaryDirectory() as tmp:
            process = Mock()
            process.poll.return_value = None
            process.wait.side_effect = subprocess.TimeoutExpired('fixture', 1)
            with patch.object(compute.subprocess, 'Popen', return_value=process):
                with self.assertRaises(compute.ComputeError) as error:
                    self.phase(['/unused-fixture'], Path(tmp), time.monotonic() + 1)
            self.assertEqual(error.exception.code, 'cleanup_unverified')
            self.assertTrue(process.kill.called)
            self.assertTrue(all(0 < call.kwargs['timeout'] <= 5 for call in process.wait.call_args_list))

    def test_admission_preserves_deadline_and_does_not_reuse_failed_format(self):
        from types import SimpleNamespace
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            backend = compute.VMBackend('/unused-runtime', root)
            backend.scratch = SimpleNamespace(cache_bytes=3*1024**3, cache=root/'cache.img', tmp=root,
                _open_root=lambda: os.open(root, os.O_RDONLY | os.O_DIRECTORY), open=lambda: None)
            deadline = time.monotonic() + 10
            with patch.object(compute, 'run_host_phase', side_effect=compute.ComputeError('timeout', 'fixture')) as phase:
                with self.assertRaises(compute.ComputeError) as error:
                    with backend.admit(deadline): self.fail('failed formatter admitted a workload')
                self.assertEqual(error.exception.code, 'timeout')
                self.assertEqual(phase.call_args.args[-1], deadline)
                self.assertEqual(phase.call_args.args[0][0], '/usr/sbin/mke2fs')
                self.assertFalse(backend._formatted)
            with patch.object(compute, 'run_host_phase') as phase:
                with backend.admit(deadline): pass
                self.assertTrue(backend._formatted)
                with backend.admit(deadline): pass
                self.assertEqual(phase.call_count, 1)

    def test_wrong_parent_refuses_command_before_launch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            result = subprocess.run([sys.executable, '-I', SUPERVISOR, root / 'receipt', sys.executable,
                '-c', f'open({str(root / "started")!r},"w").close()'],
                env={'PATH':os.defpath, 'JARVIS_SUPERVISOR_PARENT_PID':str(os.getpid() + 10000000)},
                capture_output=True, timeout=5)
            self.assertNotEqual(result.returncode, 0, 'supervisor ignored its originating parent identity')
            self.assertFalse((root / 'started').exists())

    def test_killed_initializer_owner_cannot_leave_worker_or_detached_child(self):
        self.assertTrue(hasattr(compute, 'run_host_phase'), 'initialization needs supervised parent-death handling')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workload = root / 'workload.py'
            workload.write_text(f"""import os,time
child=os.fork()
if child==0:os.setsid()
else:open({str(root / 'supervisor.pid')!r},'w').write(str(os.getppid()))
open({str(root)!r}+('/child.pid' if child==0 else '/worker.pid'),'w').write(str(os.getpid()))
time.sleep(30)
""")
            owner_source = f"""import importlib.util,time
from pathlib import Path
s=importlib.util.spec_from_file_location('compute',{compute.__file__!r});m=importlib.util.module_from_spec(s);s.loader.exec_module(m)
m.run_host_phase([{sys.executable!r},{str(workload)!r}],Path({str(root)!r}),time.monotonic()+25)
"""
            owner = subprocess.Popen([sys.executable, '-c', owner_source], stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.addCleanup(lambda: owner.poll() is None and owner.kill())
            sentinel = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(30)'],
                                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.addCleanup(lambda: (sentinel.poll() is None and sentinel.kill(), sentinel.wait(timeout=5)))
            descriptors = []
            try:
                deadline = time.monotonic() + 5
                while not all((root / name).exists() for name in ('worker.pid', 'child.pid', 'supervisor.pid')):
                    self.assertIsNone(owner.poll(), 'initializer died before exercising ownership')
                    self.assertLess(time.monotonic(), deadline, 'workload failed to start')
                    time.sleep(.01)
                for name in ('worker.pid', 'child.pid', 'supervisor.pid'):
                    descriptors.append(os.pidfd_open(int((root / name).read_text())))
                owner.kill(); owner.wait(timeout=5)
                poller = select.poll()
                for descriptor in descriptors: poller.register(descriptor, select.POLLIN)
                remaining = set(descriptors)
                deadline = time.monotonic() + 5
                while remaining and time.monotonic() < deadline:
                    for descriptor, events in poller.poll(50):
                        if events & select.POLLIN:
                            remaining.discard(descriptor); poller.unregister(descriptor)
                self.assertFalse(remaining, 'formatter descendants survived initializer SIGKILL')
                self.assertIsNone(sentinel.poll(), 'cleanup killed an unrelated process')
            finally:
                for descriptor in descriptors:
                    # Test failure must not leave a synthetic worker behind.
                    try: signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    except ProcessLookupError: pass
                    os.close(descriptor)
                if owner.poll() is None: owner.kill()
                owner.wait(timeout=5)


if __name__ == '__main__':
    unittest.main()
