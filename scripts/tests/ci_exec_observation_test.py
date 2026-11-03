"""Positive kernel exec events, stdin staging and original descendant cleanup."""
import ctypes
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT=Path(__file__).resolve().parents[1]
DRIVER=r'''
import importlib.util,json,os,sys
from pathlib import Path
spec=importlib.util.spec_from_file_location('original_supervisor',sys.argv[1])
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
root=Path(sys.argv[2]);options=json.loads(sys.argv[3])
def spawned(child):
 (root/'pids').write_text(str(child.pid))
 if options.get('ready_fd') is not None:os.close(options['ready_fd'])
def executed(child):
 assert not (root/'tool-ran').exists(),'callback followed tool execution'
 event={'pid':child.pid,'exe':os.readlink('/proc/'+str(child.pid)+'/exe')}
 with (root/'exec-events').open('a') as output:output.write(json.dumps(event)+'\n')
 if options.get('observer_fault'):raise OSError(5,'injected exec observer failure')
try:
 status=module.supervise(root/'cleanup',sys.argv[4:],forward_fds=tuple(options.get('forward',[])),
  return_raw_status=True,on_spawn=spawned,on_exec=object() if options.get('invalid_observer') else executed)
except BaseException:status=125
print('RAW_STATUS='+str(status),flush=True)
'''
TRACE=r'''
import ctypes,os,signal,sys
libc=ctypes.CDLL(None,use_errno=True);libc.ptrace.restype=ctypes.c_long
if libc.ptrace(0,0,None,None)!=0:raise OSError(ctypes.get_errno(),'TRACEME failed')
os.kill(os.getpid(),signal.SIGSTOP)
'''


@unittest.skipUnless(sys.platform=='linux','requires Linux ptrace/subreaper')
class ExecObservationTests(unittest.TestCase):
    def setUp(self):
        self.root=Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.assertEqual(ctypes.CDLL(None).prctl(36,1,0,0,0),0)

    def launch(self,program,*,forward=(),ready_fd=None,stdin=None,**options):
        options.update(forward=forward,ready_fd=ready_fd)
        passed=tuple(sorted(set(forward)|({ready_fd} if ready_fd is not None else set())))
        child=subprocess.Popen([sys.executable,'-I','-c',DRIVER,str(ROOT/'provider-supervisor.py'),
            str(self.root),json.dumps(options),sys.executable,'-I','-c',program],
            env={'PATH':os.defpath},pass_fds=passed,stdin=stdin,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        def cleanup():
            if child.poll() is None:child.terminate()
            try:child.communicate(timeout=6)
            finally:
                pids=self.root/'pids'
                if pids.exists():
                    pid=int(pids.read_text())
                    try:os.kill(pid,signal.SIGKILL)
                    except ProcessLookupError:pass
                    try:os.waitpid(pid,os.WNOHANG)
                    except ChildProcessError:pass
        self.addCleanup(cleanup)
        return child

    def finish(self,child,expected,*,events):
        output,error=child.communicate(timeout=6)
        self.assertEqual(child.returncode,0,error.decode())
        self.assertIn(('RAW_STATUS='+str(expected)+'\n').encode(),output)
        self.assertEqual((self.root/'cleanup').read_bytes(),b'all-descendants-reaped\n')
        path=self.root/'exec-events';observed=[json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        self.assertEqual(len(observed),events)
        for event in observed:
            self.assertEqual(event['pid'],int((self.root/'pids').read_text()))
            self.assertEqual(Path(event['exe']).resolve(),Path(sys.executable).resolve())
        return output

    def tool(self,body):
        return TRACE+f"os.execve(sys.executable,[sys.executable,'-I','-c',{body!r}],{{}})\n"

    def test_exec_event_precedes_short_lived_tool_and_preserves_actual_exit(self):
        child=self.launch(self.tool(f"from pathlib import Path;Path({str(self.root/'tool-ran')!r}).write_text('ran');raise SystemExit(17)"))
        self.finish(child,17,events=1);self.assertEqual((self.root/'tool-ran').read_text(),'ran')

    def test_exec_observer_preserves_signal_death_versus_numeric_exit(self):
        for body,expected in (('raise SystemExit(143)',143),('import os,signal;os.kill(os.getpid(),signal.SIGTERM)',-15)):
            with self.subTest(expected=expected):
                self.finish(self.launch(self.tool(body)),expected,events=1)
                (self.root/'cleanup').unlink();(self.root/'exec-events').unlink()

    def test_exec_failure_and_death_before_exec_never_emit_an_exec_event(self):
        for body,expected in (("raise SystemExit(127)",127),("os.kill(os.getpid(),signal.SIGKILL)",-9)):
            with self.subTest(expected=expected):
                self.finish(self.launch(TRACE+body),expected,events=0)
                (self.root/'cleanup').unlink()

    def test_failed_execve_never_becomes_a_started_tool(self):
        code=TRACE+"\ntry:os.execve('/no-ci-exec-fixture',['no-ci-exec-fixture'],{})\nexcept OSError:raise SystemExit(127)\n"
        self.finish(self.launch(code),127,events=0)

    def test_zero_bootstrap_exit_without_the_handshake_does_not_prove_a_tool_exec(self):
        self.finish(self.launch('raise SystemExit(0)'),0,events=0)

    def test_noncallable_observer_refuses_before_any_bootstrap_is_spawned(self):
        child=self.launch(f"open({str(self.root/'tool-ran')!r},'w').write('bad')",invalid_observer=True)
        self.finish(child,125,events=0)
        self.assertFalse((self.root/'pids').exists());self.assertFalse((self.root/'tool-ran').exists())

    def test_user_sigtrap_and_preexec_signal_are_not_kernel_exec_events(self):
        for number in (signal.SIGTRAP,signal.SIGUSR1):
            with self.subTest(number=number):
                body=f"open({str(self.root/'tool-ran')!r},'w').write('bad')"
                code=TRACE+f'os.kill(os.getpid(),{int(number)})\n'+self.tool(body)[len(TRACE):]
                self.finish(self.launch(code),-9,events=0)
                self.assertFalse((self.root/'tool-ran').exists());(self.root/'cleanup').unlink()

    def test_untraced_sigstop_cannot_forge_the_bootstrap_handshake(self):
        code='import os,signal;os.kill(os.getpid(),signal.SIGSTOP)\n'
        self.finish(self.launch(code),-9,events=0)

    def test_multithreaded_bootstrap_cannot_arm_the_original_exec_observer(self):
        code='import threading,time\nthreading.Thread(target=lambda:time.sleep(30),daemon=True).start()\n'+self.tool(
            f"open({str(self.root/'tool-ran')!r},'w').write('bad')")
        self.finish(self.launch(code),-9,events=0);self.assertFalse((self.root/'tool-ran').exists())

    def test_supervisor_death_kills_the_armed_stager_without_claiming_cleanup_or_exec(self):
        for phase in ('unarmed','armed'):
            with self.subTest(phase=phase):
                read,write=os.pipe2(os.O_CLOEXEC);pidfd=None;reaped=False
                ready=self.root/phase
                wait=f"import os;from pathlib import Path;Path({str(ready)!r}).write_text('ready');os.read({read},1)\n"
                code=wait+TRACE if phase=='unarmed' else TRACE+wait
                code+=self.tool(f"open({str(self.root/'tool-ran')!r},'w').write('bad')")[len(TRACE):]
                try:
                    child=self.launch(code,forward=(read,))
                    deadline=time.monotonic()+3
                    while not ready.exists() and child.poll() is None and time.monotonic()<deadline:time.sleep(.005)
                    self.assertTrue(ready.exists(),'bootstrap did not reach the controlled death interval')
                    pid=int((self.root/'pids').read_text());pidfd=os.pidfd_open(pid)
                    child.kill()
                    self.assertTrue(select.select([pidfd],[],[],3)[0],'stager survived its original supervisor')
                    child.communicate(timeout=4);self.assertEqual(child.returncode,-signal.SIGKILL)
                    waited,status=os.waitpid(pid,0);reaped=True
                    self.assertEqual(waited,pid);self.assertEqual(os.waitstatus_to_exitcode(status),-signal.SIGKILL)
                    self.assertFalse((self.root/'tool-ran').exists());self.assertFalse((self.root/'exec-events').exists())
                    self.assertFalse((self.root/'cleanup').exists(),'dead original supervisor cannot acknowledge cleanup')
                finally:
                    if pidfd is not None:
                        if not reaped:
                            try:signal.pidfd_send_signal(pidfd,signal.SIGKILL)
                            except ProcessLookupError:pass
                            try:os.waitpid(pid,0)
                            except ChildProcessError:pass
                        os.close(pidfd)
                    os.close(read);os.close(write)

    def test_exec_observer_failure_keeps_tool_stopped_until_cleanup(self):
        child=self.launch(self.tool(f"open({str(self.root/'tool-ran')!r},'w').write('bad')"),observer_fault=True)
        self.finish(child,-9,events=1);self.assertFalse((self.root/'tool-ran').exists())

    def test_staging_ready_unblocks_large_stdin_before_a_real_tool_exec(self):
        reader,writer=os.pipe2(os.O_CLOEXEC);ready,ack=os.pipe2(os.O_CLOEXEC)
        cancel,cancel_writer=os.pipe2(os.O_CLOEXEC)
        data=b'\xffstdin'*200000
        code=f'''import importlib.util,os,time
spec=importlib.util.spec_from_file_location('stage',{str(ROOT/'ci-cargo-staging.py')!r})
stage=importlib.util.module_from_spec(spec);spec.loader.exec_module(stage)
captured=stage.capture(0,{cancel},command_deadline=time.monotonic()+3)
os.dup2(captured.fileno(),0)
'''+self.tool("import hashlib,sys;print('INPUT='+hashlib.sha256(sys.stdin.buffer.read()).hexdigest(),flush=True)")
        try:
            child=self.launch(code,forward=(cancel,),ready_fd=ack,stdin=reader)
            os.close(reader);reader=None;os.close(ack);ack=None
            self.assertTrue(select.select([ready],[],[],3)[0],'staging-ready never closed the caller exec-start pipe')
            self.assertEqual(os.read(ready,1),b'')
            view=memoryview(data)
            while view:view=view[os.write(writer,view):]
            os.close(writer);writer=None
            output=self.finish(child,0,events=1)
            self.assertIn(('INPUT='+hashlib.sha256(data).hexdigest()+'\n').encode(),output)
        finally:
            for descriptor in (reader,writer,ready,ack,cancel,cancel_writer):
                if descriptor is not None:os.close(descriptor)

    def test_staging_cancellation_never_observes_or_executes_a_tool(self):
        reader,writer=os.pipe2(os.O_CLOEXEC);ready,ack=os.pipe2(os.O_CLOEXEC)
        cancel,cancel_writer=os.pipe2(os.O_CLOEXEC)
        code=f'''import importlib.util,os,time
spec=importlib.util.spec_from_file_location('stage',{str(ROOT/'ci-cargo-staging.py')!r})
stage=importlib.util.module_from_spec(spec);spec.loader.exec_module(stage)
try:captured=stage.capture(0,{cancel},command_deadline=time.monotonic()+3)
except stage.StagingUnavailable:raise SystemExit(125)
'''+self.tool(f"open({str(self.root/'tool-ran')!r},'w').write('bad')")
        try:
            child=self.launch(code,forward=(cancel,),ready_fd=ack,stdin=reader)
            os.close(reader);reader=None;os.close(ack);ack=None
            self.assertTrue(select.select([ready],[],[],3)[0]);self.assertEqual(os.read(ready,1),b'')
            os.write(cancel_writer,b'cancel')
            self.finish(child,125,events=0);self.assertFalse((self.root/'tool-ran').exists())
        finally:
            for descriptor in (reader,writer,ready,ack,cancel,cancel_writer):
                if descriptor is not None:os.close(descriptor)


if __name__=='__main__':unittest.main()
