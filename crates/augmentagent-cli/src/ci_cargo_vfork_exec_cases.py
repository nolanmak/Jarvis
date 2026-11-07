"""Original-supervised Linux vfork/exec feasibility; no CI or runtime grant."""
import array
import ctypes
import errno
import importlib.util
import json
import os
from pathlib import Path
import select
import signal
import socket
import struct
import sys
import threading
import time
from types import SimpleNamespace


def load(name,path):
    spec=importlib.util.spec_from_file_location(name,path)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    return module


def native(path):
    lib=ctypes.CDLL(path,use_errno=True)
    lib.jarvis_ci_exec_listener.argtypes=[];lib.jarvis_ci_exec_listener.restype=ctypes.c_int
    lib.jarvis_ci_exec_respond.argtypes=[ctypes.c_int,ctypes.c_uint64,ctypes.c_int]
    lib.jarvis_ci_policy_receive.argtypes=[ctypes.c_int,ctypes.POINTER(ctypes.c_uint64),
        ctypes.POINTER(ctypes.c_uint32),ctypes.POINTER(ctypes.c_int32),
        ctypes.POINTER(ctypes.c_uint64),ctypes.POINTER(ctypes.c_int32)]
    return lib


def launcher(args):
    library,transport,producer,*arguments=args
    lib=native(library);listener=lib.jarvis_ci_exec_listener()
    if listener<0:raise OSError(ctypes.get_errno(),'original listener unavailable')
    with socket.socket(fileno=int(transport)) as channel:
        channel.sendmsg([b'listener'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[listener]))])
    os.close(listener)
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.ptrace(0,0,None,None)!=0:raise RuntimeError('original exec observer unavailable')
    os.kill(os.getpid(),signal.SIGSTOP)
    os.execve(producer,[producer,*arguments],{})


def frame(fd,size,deadline):
    data=b''
    while len(data)<size:
        remaining=deadline-time.monotonic()
        if remaining<=0 or not select.select([fd],[],[],remaining)[0]:raise RuntimeError('probe frame timeout')
        block=os.read(fd,size-len(data))
        if not block:raise RuntimeError('probe frame truncated')
        data+=block
    return data


def experiment(planner,proxy,mode,ready,hold,release,cloexec,extra):
    class TestOnlyGate(planner.ExecMediation):
        """Never used by the runtime; check() cannot issue a source verdict."""
        def __init__(self):
            super().__init__(time.monotonic()+10,os.getuid())
            self.vfork_seen=False;self.child=None;self.proxy_fd=os.open(proxy,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
            self.proof={'parent_released_before_proxy_termination':False,'shared_memory_observed':False,
                'cloexec_closed_at_exec_stop':False,'image_accepted':False,'native_exec_stop':False}

        def check(self):raise planner.FreshnessHold('input_census_unknown')

        def _notifications(self):
            libc=ctypes.CDLL(None,use_errno=True)
            libc.ptrace.restype=ctypes.c_long
            libc.ptrace.argtypes=[ctypes.c_uint,ctypes.c_int,ctypes.c_void_p,ctypes.c_void_p]
            childfd=None
            def trace(operation,pid,data=None):
                if libc.ptrace(operation,pid,None,data)!=0:raise OSError(ctypes.get_errno(),'probe ptrace')
            def wait(pid):
                while time.monotonic()<self.deadline:
                    found,status=os.waitpid(pid,os.WNOHANG|0x40000000) # __WALL, same tracer thread.
                    if found:return status
                    time.sleep(.002)
                raise RuntimeError('probe exec-stop timeout')
            def respond(identity,allow):
                if self.native.jarvis_ci_exec_respond(self.listener,identity,int(allow))!=0:
                    raise OSError(ctypes.get_errno(),'probe notification response')
            try:
                poller=select.poll();poller.register(self.listener,select.POLLIN)
                while not self._stop.is_set():
                    if time.monotonic()>=self.deadline:raise RuntimeError('probe listener deadline')
                    events=poller.poll(20)
                    if not events:continue
                    flags=events[0][1]
                    if flags&(select.POLLERR|select.POLLNVAL):raise RuntimeError('probe listener failed')
                    if flags&select.POLLHUP and not flags&select.POLLIN:return
                    identity,pid,number=ctypes.c_uint64(),ctypes.c_uint32(),ctypes.c_int32()
                    clone_flags,kind=ctypes.c_uint64(),ctypes.c_int32()
                    if self.native.jarvis_ci_policy_receive(self.listener,ctypes.byref(identity),ctypes.byref(pid),
                            ctypes.byref(number),ctypes.byref(clone_flags),ctypes.byref(kind))!=0:
                        if ctypes.get_errno() in (errno.ENOENT,errno.EINTR):continue
                        raise OSError(ctypes.get_errno(),'probe notification receive')
                    with self._lock:
                        if len(self.records)>=16:raise RuntimeError('probe notification bound')
                        if kind.value==1 and self.initial and pid.value==self.pid:
                            respond(identity.value,True)
                            self.records.append({'pid':pid.value,'nr':number.value,'stage':'initial_exec'})
                            continue
                        if kind.value==3 and number.value==58 and pid.value==self.pid and self.ack and not self.vfork_seen:
                            assert {p.name for p in Path(f'/proc/{self.pid}/task').iterdir()}=={str(self.pid)}
                            respond(identity.value,True);self.vfork_seen=True
                            self.records.append({'pid':pid.value,'nr':58,'stage':'fixture_vfork_only'})
                            continue
                        if kind.value!=1 or not self.vfork_seen or pid.value==self.pid or self.child is not None:
                            raise RuntimeError('unexpected probe notification')
                        self.child=pid.value;childfd=os.pidfd_open(self.child)
                        status=dict(line.split(':',1) for line in Path(f'/proc/{self.child}/status').read_text().splitlines())
                        assert int(status['PPid'])==self.pid and int(status['TracerPid'])==0
                        assert all(list(map(int,status[k].split()))==[os.getuid()]*4 for k in ('Uid','Gid'))
                        assert {p.name for p in Path(f'/proc/{self.child}/task').iterdir()}=={str(self.child)}
                        assert Path(f'/proc/{self.child}/cgroup').read_bytes()==Path(f'/proc/{self.pid}/cgroup').read_bytes()
                        for name in ('pid','mnt','ipc','net','user'):
                            assert os.readlink(f'/proc/{self.child}/ns/{name}')==os.readlink(f'/proc/{self.pid}/ns/{name}')
                        if mode=='cancel':
                            respond(identity.value,False)
                            self.records.append({'pid':self.child,'nr':number.value,'stage':'cancelled_exec'})
                            continue
                        if mode=='listener-loss':
                            os.close(self.listener);self.listener=None
                            self.records.append({'pid':self.child,'nr':number.value,'stage':'lost_listener'})
                            return
                        # The fixture's conditional CONTINUE follows actual
                        # attachment; it is not a runtime permission policy.
                        trace(0x4206,self.child,ctypes.c_void_p(0x10|0x100000)) # SEIZE, TRACEEXEC, EXITKILL.
                        respond(identity.value,True)
                        stopped=wait(self.child)
                        assert os.WIFSTOPPED(stopped) and os.WSTOPSIG(stopped)==signal.SIGTRAP and stopped>>16==4
                        former=ctypes.c_ulong();trace(0x4201,self.child,ctypes.byref(former))
                        assert former.value==self.child
                        with open(f'/proc/{self.child}/exe','rb') as image:
                            actual=os.fstat(image.fileno());expected=os.fstat(self.proxy_fd)
                            accepted=(actual.st_dev,actual.st_ino)==(expected.st_dev,expected.st_ino)
                        assert accepted==(mode!='wrong-image')
                        assert not Path(f'/proc/{self.child}/fd/{cloexec}').exists()
                        actual=os.stat(f'/proc/{self.child}/fd/{extra}');expected=os.fstat(extra)
                        assert (actual.st_dev,actual.st_ino)==(expected.st_dev,expected.st_ino)
                        assert not select.select([ready],[],[],0)[0], 'probe body ran before image gate'
                        entered=struct.unpack('4i',frame(release,16,self.deadline))
                        assert entered==(0x4a415256,self.pid,self.child,41),entered
                        assert not select.select([childfd],[],[],0)[0], 'vfork parent returned only at child death'
                        self.proof.update(parent_released_before_proxy_termination=True,shared_memory_observed=True,
                            cloexec_closed_at_exec_stop=True,image_accepted=accepted,native_exec_stop=True)
                        self.records.append({'pid':self.child,'nr':number.value,'stage':'verified_exec_stop','image_accepted':accepted})
                        if not accepted:
                            signal.pidfd_send_signal(childfd,signal.SIGKILL)
                            terminal=wait(self.child)
                            assert os.WIFSIGNALED(terminal) and os.WTERMSIG(terminal)==signal.SIGKILL
                        else:
                            trace(17,self.child) # DETACH only after the original exec-stop checks.
                            assert frame(ready,12,self.deadline)==b'PROXY_READY\n'
                            if mode=='signal':signal.pidfd_send_signal(childfd,signal.SIGTERM)
                            else:os.write(hold,b'x')
            except BaseException as error:
                self.errors.append(type(error).__name__+': '+str(error))
                for descriptor in (childfd,self.pidfd):
                    if descriptor is not None:
                        try:signal.pidfd_send_signal(descriptor,signal.SIGKILL)
                        except OSError:pass
            finally:
                if childfd is not None:os.close(childfd)
    return TestOnlyGate()


def main(args):
    planner_path,supervisor_path,library,producer,proxy,wrong,mode,receipt=args
    planner=load('vfork_original_controller',planner_path)
    supervisor=load('vfork_original_supervisor',supervisor_path)
    lib=native(library);planner.UnitPolicy=lambda:SimpleNamespace(library=lib)
    pipes=[os.pipe() for _ in range(5)]
    (ready_r,ready_w),(hold_r,hold_w),(clo_r,clo_w),(extra_r,extra_w),(release_r,release_w)=pipes
    os.write(extra_w,b'+')
    production=mode=='production-deny'
    gate=(planner.ExecMediation(time.monotonic()+10,os.getuid()) if production else
        experiment(planner,proxy,mode,ready_r,hold_w,release_r,clo_r,extra_r))
    try:
        target=wrong if mode=='wrong-image' else proxy
        argv=[sys.executable,'-I',str(Path(__file__).resolve()),'--launcher',library,str(gate.child_fd),producer,
            target,str(ready_w),str(hold_r),str(clo_r),str(extra_r),str(release_w),mode]
        raw=supervisor.supervise(receipt,argv,forward_fds=(gate.child_fd,ready_w,hold_r,clo_r,extra_r,release_w),
            on_spawn=gate.spawned,on_exec=gate.executed,return_raw_status=True)
        if mode!='production-deny':assert raw==0,('native outcome',raw)
        else:assert raw==112,raw
        if production:
            try:gate.check()
            except planner.FreshnessHold as hold:assert hold.reason=='input_census_unknown'
            else:raise AssertionError('expected original unsupported-route hold')
            proof={'parent_released_before_proxy_termination':False,'production_refusal':True}
        else:
            gate.settle();assert not gate.errors,gate.errors
            assert gate.vfork_seen and gate.child is not None
            if mode in ('cancel','listener-loss'):
                entered=struct.unpack('4i',frame(release_r,16,gate.deadline))
                assert entered==(0x4a415256,gate.pid,gate.child,41),entered
            finished=struct.unpack('4i',frame(release_r,16,gate.deadline))
            expected=signal.SIGTERM if mode=='signal' else signal.SIGKILL if mode=='wrong-image' else (93 if mode in ('cancel','listener-loss') else 23)<<8
            assert finished==(0x52534c54,gate.pid,gate.child,expected),finished
            proof=dict(gate.proof,original_parent_wait_status=finished[3],production_refusal=False)
            # Even this successful experiment must never become a source verdict.
            try:gate.check()
            except planner.FreshnessHold as hold:assert hold.reason=='input_census_unknown'
            else:raise AssertionError('feasibility leaked a source verdict')
        assert Path(receipt).read_text()=='all-descendants-reaped\n'
        print(json.dumps(dict(proof,mode=mode,original_cleanup=True,feasibility_only=True,
            notifications=gate.records,full_ci_pass=False,shipping_authority=False)))
    finally:
        gate.close()
        if not production:os.close(gate.proxy_fd)
        for pair in pipes:
            for fd in pair:os.close(fd)


if __name__=='__main__':
    if sys.argv[1]=='--launcher':launcher(sys.argv[2:])
    else:main(sys.argv[1:])
