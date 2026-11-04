"""Actual exec-stop/grant mechanics; not a root admission or CI proof."""
import importlib.util
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
import stat

ROOT=Path(__file__).resolve().parents[2]
def load(name,file):
    spec=importlib.util.spec_from_file_location(name,ROOT/'scripts'/file)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module
planner=load('worker_entry_planner_tests','ci-cargo-planner.py')


class WorkerEntryDiagnosticTests(unittest.TestCase):
    def admission(self,stdin_flags):
        scope={'scope_id':str(uuid.uuid4())};binding={};command=str(uuid.uuid4())
        flags={'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        native={'version':1,'kind':'ci-unit-policy-activation','sha256':'a'*64,'size_bytes':100,
            'activation_returncode':0,**flags}
        policy={'version':1,'kind':'ci-repository-worker-policy-receipt','binding':binding,'command':command,
            'scope_id':scope['scope_id'],'pid':401,'policy':native}
        rows=[{'number':n,'device':1,'inode':n+1,'type':kind,'uid':0,'gid':0,'flags':access,'position':0}
            for n,kind,access in ((0,stat.S_IFCHR,stdin_flags),(1,stat.S_IFIFO,os.O_WRONLY),
                                 (2,stat.S_IFIFO,os.O_WRONLY),(4,stat.S_IFREG,os.O_RDWR))]
        entry={'version':1,'kind':'ci-unit-exec-entry','pid':401,'birth':9,'tracer':400,
            'uids':[1000]*4,'gids':[1000]*4,'groups':[],'no_new_privs':1,'seccomp':2,'seccomp_filters':2,
            'capabilities':{'CapEff':0,'CapPrm':0,'CapAmb':0},'descriptors':[0,1,2,4],'fd_table':rows,
            'policy_activation':policy,'sealed_relay':{'device':1,'inode':5,'uid':0,'gid':0,'seals':15,
                'sha256':'b'*64,'size_bytes':160},**flags}
        value={'version':1,'kind':'ci-repository-worker-admission','binding':binding,'command':command,
            'scope_id':scope['scope_id'],'supervisor_pid':400,'entry':entry,
            'namespaces':{name:name+':[1]' for name in ('mnt','ipc','net','user','pid')},**flags}
        return value,binding,command,scope

    def test_receipt_grammar_accepts_original_readwrite_devnull_without_normalizing(self):
        # Metadata-only fixture: this is not a live root admission token.
        value,binding,command,scope=self.admission(os.O_RDWR)
        self.assertIs(planner.validate_worker_entry(value,binding,command,scope),value)
        self.assertEqual(value['entry']['fd_table'][0]['flags'],os.O_RDWR)

    def test_receipt_grammar_refuses_writeonly_stdin(self):
        with self.assertRaises(planner.FreshnessHold):
            planner.validate_worker_entry(*self.admission(os.O_WRONLY))

    def test_worker_entry_failure_has_bounded_stage_and_stable_evidence(self):
        import hashlib
        progress={}
        with self.assertRaises(planner.FreshnessHold) as caught:
            planner.validate_worker_entry({},None,None,{},progress)
        self.assertEqual(progress['stage'],'worker_entry_header')
        self.assertEqual(caught.exception.evidence_sha256,hashlib.sha256(b'worker_entry_header').hexdigest())


@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native policy helper')
class WorkerEntryTests(unittest.TestCase):
    def run_case(self,mode):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);parent,child=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
            with parent,child:
                process=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--probe',mode,directory,
                    os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'],str(child.fileno())],pass_fds=(child.fileno(),),
                    stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                child.close();pidfd=None
                try:
                    self.assertTrue(select.select([parent],[],[],5)[0])
                    frame=planner.receive_control(parent)
                    ready=mode not in ('extra','foreign-policy')
                    self.assertEqual(frame is not None,ready)
                    if ready:
                        stage,pid,received=frame;self.assertEqual(stage,3);self.assertEqual(len(received),1);pidfd=received[0]
                        self.assertFalse((root/'body').exists())
                        stat=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
                        self.assertEqual(stat[0],'t');self.assertEqual(int(stat[1]),process.pid)
                        self.assertEqual(select.select([pidfd],[],[],0)[0],[])
                        if mode=='grant':planner._control(parent,3,pid)
                        elif mode=='wrong-pid':planner._control(parent,3,pid+1)
                        elif mode=='eof':parent.close()
                        elif mode=='timeout':pass
                    out,err=process.communicate(timeout=6)
                    self.assertEqual(process.returncode,0,err.decode())
                    result=json.loads(out);self.assertEqual(result['body_started'],mode=='grant')
                    self.assertEqual(result['held'],mode!='grant');self.assertTrue(result['original_cleanup'])
                finally:
                    if pidfd is not None:os.close(pidfd)
                    if process.poll() is None:process.kill()
                    process.wait(timeout=3)
                    process.stdout.close();process.stderr.close()

    def test_body_waits_for_controller_grant(self):self.run_case('grant')
    def test_wrong_original_pid_never_releases_body(self):self.run_case('wrong-pid')
    def test_controller_loss_never_releases_body(self):self.run_case('eof')
    def test_original_deadline_never_releases_body(self):self.run_case('timeout')
    def test_extra_inherited_fd_blocks_entry(self):self.run_case('extra')
    def test_foreign_policy_receipt_blocks_entry(self):self.run_case('foreign-policy')


def probe(mode,directory,library,channel):
    import ctypes
    import fcntl
    staging=planner.staging;supervisor=load('worker_entry_original_supervisor','provider-supervisor.py')
    root=Path(directory);configuration=os.memfd_create('worker-configuration',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
    policy=os.memfd_create('worker-policy',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
    os.write(configuration,b'original sealed configuration');staging._seal(configuration)
    extra=os.open(os.devnull,os.O_RDONLY|os.O_CLOEXEC)
    context={'version':1,'kind':'test-worker-policy','command':'original'}
    bootstrap='''import ctypes,importlib.util,json,os,signal,sys
library,configuration,receipt,context,helper,marker,extra=sys.argv[1:]
configuration=int(configuration);receipt=int(receipt)
s=importlib.util.spec_from_file_location('worker_stage',helper);m=importlib.util.module_from_spec(s);s.loader.exec_module(m)
os.set_inheritable(receipt,False);os.set_inheritable(configuration,True)
lib=ctypes.CDLL(library,use_errno=True);assert lib.jarvis_ci_restrict_unit()==0
value=json.loads(context);value['pid']=os.getpid();m.publish_policy_receipt(receipt,value)
libc=ctypes.CDLL(None,use_errno=True);assert libc.ptrace(0,0,None,None)==0
os.kill(os.getpid(),signal.SIGSTOP)
os.execve(sys.executable,[sys.executable,'-I','-c',"from pathlib import Path;Path(__import__('sys').argv[1]).write_text('started')",marker],{})
'''
    held=False
    with socket.socket(fileno=int(channel)) as endpoint:
        def executed(child):
            nonlocal held
            try:
                expected={**context,'command':'foreign' if mode=='foreign-policy' else context['command'],'pid':child.pid}
                observed=staging.observe_worker_entry(child.pid,configuration,policy,expected,
                    uid=os.getuid(),gid=os.getgid(),groups=sorted(os.getgroups()))
                assert observed['input_closure_complete'] is observed['shipping_authority'] is False
                fd=os.pidfd_open(child.pid)
                try:planner._control(endpoint,3,child.pid,fd)
                finally:os.close(fd)
                planner.wait_original_worker_grant(endpoint,child.pid,time.monotonic()+1)
            except (planner.FreshnessHold,staging.StagingUnavailable):held=True;raise
        try:
            supervisor.supervise(root/'cleanup',[sys.executable,'-I','-c',bootstrap,library,str(configuration),str(policy),
                json.dumps(context),str(ROOT/'scripts/ci-cargo-staging.py'),str(root/'body'),str(extra)],
                forward_fds=(configuration,policy,*((extra,) if mode=='extra' else ())),on_exec=executed)
        except (planner.FreshnessHold,staging.StagingUnavailable):held=True
    for fd in (configuration,policy,extra):os.close(fd)
    print(json.dumps({'held':held,'body_started':(root/'body').exists(),
        'original_cleanup':(root/'cleanup').read_bytes()==b'all-descendants-reaped\n'}))


if __name__=='__main__':
    if len(sys.argv)>1 and sys.argv[1]=='--probe':probe(*sys.argv[2:])
    else:unittest.main()
