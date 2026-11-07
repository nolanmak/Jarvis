import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import ast
import ctypes
import selectors
import types

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('ci_worker_scope',ROOT/'ci-worker-scope.py')
scope=importlib.util.module_from_spec(spec);spec.loader.exec_module(scope)


class WorkerScopeTests(unittest.TestCase):
    def test_input_capture_requires_its_own_stopped_worker_scope(self):
        input_spec=importlib.util.spec_from_file_location('scoped_input_manifest',ROOT/'ci-input-manifest.py')
        inputs=importlib.util.module_from_spec(input_spec);input_spec.loader.exec_module(inputs)
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);root.chmod(0o700);generated=root/'generated';generated.mkdir();(generated/'input').write_text('input')
            first=scope.WorkerScope(root,os.getuid(),os.getgid(),['true'])
            journal=inputs.ChangeJournal({'generated':generated},scope=first)
            with self.assertRaises(inputs.FreshnessHold):journal.finish()
            actual=scope.WorkerScope(root,os.getuid(),os.getgid(),['true'])
            journal=inputs.ChangeJournal({'generated':generated},scope=actual)
            process=subprocess.Popen(actual.argv);actual.attach(process);process.wait(timeout=5);stopped=actual.verify()
            self.assertEqual(journal.finish(stopped)['guest_worker_quiescence'],stopped.observation())
            second=scope.WorkerScope(root,os.getuid(),os.getgid(),['true'])
            journal=inputs.ChangeJournal({'generated':generated},scope=second)
            with self.assertRaises(inputs.FreshnessHold):journal.finish(stopped)
            with self.assertRaises(inputs.FreshnessHold):inputs.ChangeJournal({'generated':generated},scope=actual)
    def test_actual_guest_runner_stops_detached_writes_before_returning(self):
        if os.environ.get('CI_GUEST_RUNNER_CHILD')!='1':
            environment=os.environ.copy();environment['CI_GUEST_RUNNER_CHILD']='1'
            result=subprocess.run([sys.executable,__file__,'WorkerScopeTests.test_actual_guest_runner_stops_detached_writes_before_returning'],
                env=environment,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=10)
            self.assertEqual(result.returncode,0,(result.stdout+result.stderr).decode())
            return
        self.assertEqual(ctypes.CDLL(None).prctl(36,1,0,0,0),0)
        vm_spec=importlib.util.spec_from_file_location('ci_scope_vm',ROOT/'codex-build-vm.py')
        vm=importlib.util.module_from_spec(vm_spec);vm_spec.loader.exec_module(vm)
        # Execute the production guest function. Guest boot/mounts are outside
        # this host process fixture; the real VM replay covers that boundary.
        functions=[node for node in ast.parse(vm.GUEST_RUNNER).body if isinstance(node,ast.FunctionDef)
                   and node.name in ('run_worker','_run_worker')]
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);root.chmod(0o700);marker=root/'late-write';pid_file=root/'child-pid';release=root/'release'
            child='import os,time,sys;from pathlib import Path;os.setsid();Path(sys.argv[1]).write_text(str(os.getpid()))\nwhile not Path(sys.argv[3]).exists():time.sleep(.001)\nPath(sys.argv[2]).write_text("late")'
            parent='import subprocess,sys,time;from pathlib import Path;subprocess.Popen([sys.executable,"-c",sys.argv[1],sys.argv[2],sys.argv[3],sys.argv[4]],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\nfor _ in range(2000):\n if Path(sys.argv[2]).exists() and Path(sys.argv[2]).stat().st_size:break\n time.sleep(.001)\nelse:raise SystemExit(2)'
            def spawn(argv,**kwargs):
                kwargs['cwd']=str(root)
                return subprocess.Popen(argv,**kwargs)
            processes=types.SimpleNamespace(Popen=spawn,DEVNULL=subprocess.DEVNULL,PIPE=subprocess.PIPE)
            namespace={'subprocess':processes,'selectors':selectors,'signal':signal,'os':os,'Path':Path,
                'environment':{'PATH':os.defpath},'worker':lambda:None,'job':{'ci_context':True},
                'ci_workers':scope,'guest_worker_receipts':[],'worker_control':root,'uid':os.getuid(),'gid':os.getgid()}
            exec(compile(ast.Module(body=functions,type_ignores=[]),'actual-guest-worker','exec'),namespace)
            try:
                result=namespace['run_worker']([sys.executable,'-c',parent,child,str(pid_file),str(marker),str(release)])
                self.assertEqual(result['exit_code'],0)
                release.write_text('after return')
                deadline=time.monotonic()+.3
                while not marker.exists() and time.monotonic()<deadline:time.sleep(.001)
                self.assertFalse(marker.exists(),'guest runner returned before its detached writer was stopped')
            finally:
                if pid_file.exists():
                    try:
                        pid=int(pid_file.read_text());waited,_=os.waitpid(pid,os.WNOHANG)
                        if waited==0:os.kill(pid,signal.SIGKILL);os.waitpid(pid,0)
                    except (ProcessLookupError,ChildProcessError):pass
    def test_supervised_worker_cannot_leave_a_detached_writer_after_capture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);root.chmod(0o700);marker=root/'late-write';pid_file=root/'child-pid';release=root/'release'
            child='import os,time,sys;from pathlib import Path;os.setsid();Path(sys.argv[1]).write_text(str(os.getpid()))\nwhile not Path(sys.argv[3]).exists():time.sleep(.001)\nPath(sys.argv[2]).write_text("late")'
            parent='import subprocess,sys,time;from pathlib import Path;subprocess.Popen([sys.executable,"-c",sys.argv[1],sys.argv[2],sys.argv[3],sys.argv[4]],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\nfor _ in range(2000):\n if Path(sys.argv[2]).exists() and Path(sys.argv[2]).stat().st_size:break\n time.sleep(.001)\nelse:raise SystemExit(2)'
            worker=scope.WorkerScope(root,os.getuid(),os.getgid(),[sys.executable,'-c',parent,child,str(pid_file),str(marker),str(release)])
            process=subprocess.Popen(worker.argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            worker.attach(process)
            try:
                self.assertEqual(process.wait(timeout=5),0)
                stopped=worker.verify();self.assertTrue(stopped.matches(worker))
                self.assertEqual(stopped.observation()['state'],'all_descendants_reaped')
                release.write_text('after return');time.sleep(.05);self.assertFalse(marker.exists(),'snapshot window retained a detached writer')
                self.assertTrue(pid_file.is_file(),'detached-writer fixture never started')
                self.assertFalse(Path('/proc',pid_file.read_text()).exists(),'writer was not reaped')
                with self.assertRaises(scope.FreshnessHold):worker.verify()
            finally:
                if process.poll() is None:process.terminate();process.wait(timeout=5)
                process.stdout.close();process.stderr.close()

    def test_a_missing_forged_or_live_receipt_never_proves_worker_quiescence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);root.chmod(0o700)
            for kind in ['missing','symlink','shared','hardlink','live']:
                worker=scope.WorkerScope(root,os.getuid(),os.getgid(),['true'])
                class Process:
                    def poll(self):return None if kind=='live' else 0
                worker.attach(Process())
                if kind=='symlink':worker.receipt.symlink_to('/dev/null')
                elif kind!='missing':
                    worker.receipt.write_bytes(b'all-descendants-reaped\n');worker.receipt.chmod(0o600)
                    if kind=='shared':worker.receipt.chmod(0o660)
                    if kind=='hardlink':os.link(worker.receipt,worker.directory/'alias')
                with self.assertRaises(scope.FreshnessHold,msg=kind):worker.verify()

    def test_scope_never_selects_root_or_unowned_control(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);root.chmod(0o700)
            with self.assertRaises(scope.FreshnessHold):scope.WorkerScope(root,0,os.getgid(),['true'])
            root.chmod(0o770)
            with self.assertRaises(scope.FreshnessHold):scope.WorkerScope(root,os.getuid(),os.getgid(),['true'])


if __name__=='__main__':unittest.main()
