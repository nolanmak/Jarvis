"""Combined real vfork, static V3 continuation and original-supervised tool.

This test-only composition cannot issue a source verdict or runtime admission.
"""
import ctypes
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace

CAPSULE=struct.Struct('<8sIiIIIIIIQqQ')
RESPONSE=struct.Struct('<8sII')
SEALS=fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL


def load(name,path):
    spec=importlib.util.spec_from_file_location(name,path)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    return module


def tool_bootstrap(args):
    body_ready,body_hold=map(int,args)
    libc=ctypes.CDLL(None,use_errno=True)
    assert libc.prctl(38,1,0,0,0)==0 and libc.ptrace(0,0,None,None)==0
    os.kill(os.getpid(),signal.SIGSTOP)
    body="import os,sys;os.write(1,b'DELEGATED_TOOL_OUTPUT\\n');os.write(int(sys.argv[1]),b'R');assert os.read(int(sys.argv[2]),1)==b'x';os._exit(23)"
    os.execve(sys.executable,[sys.executable,'-I','-c',body,str(body_ready),str(body_hold)],{})


def tool_supervisor(args):
    path,receipt,*numbers=args
    executed,grant,body_ready,body_hold,report=map(int,numbers)
    supervisor=load('combined_original_tool_supervisor',path)
    def on_exec(child):
        os.write(executed,struct.pack('i',child.pid))
        assert os.read(grant,1)==b'G'
    raw=supervisor.supervise(receipt,[sys.executable,'-I',str(Path(__file__).resolve()),'--tool-bootstrap',
        str(body_ready),str(body_hold)],forward_fds=(body_ready,body_hold),on_exec=on_exec,return_raw_status=True)
    assert Path(receipt).read_bytes()==b'all-descendants-reaped\n'
    os.write(report,struct.pack('i',raw))


def main(args):
    planner_path,supervisor_path,kernel_path,library,producer,proxy,wrong,mode,receipt=args
    kernel=load('combined_kernel_boundary',kernel_path)
    planner=load('combined_original_controller',planner_path)
    supervisor=load('combined_original_parent_supervisor',supervisor_path)
    lib=kernel.native(library);planner.UnitPolicy=lambda:SimpleNamespace(library=lib)
    server,client=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
    server.settimeout(3)
    alternate_server,alternate_client=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
    client_info=os.fstat(client.fileno())
    expected_cookie=struct.unpack('Q',client.getsockopt(socket.SOL_SOCKET,57,8))[0]
    chosen=alternate_client if mode=='swapped-channel' else client
    # Retain only the server endpoint during execution; otherwise EOF is false.
    chosen_fd=chosen.fileno()
    pipes=[os.pipe() for _ in range(4)]
    (ready_r,ready_w),(hold_r,hold_w),(release_r,release_w),(extra_r,extra_w)=pipes
    os.write(extra_w,b'+')
    observations={'tool_calls':0,'tool_exec_observed':False,'tool_cleanup':False,'root_role_admission_proved':False}
    process_holder=[]
    with tempfile.TemporaryDirectory(prefix='v3-combined-tool-') as directory:
        tool_receipt=Path(directory)/'cleanup'
        def inspect(child):
            cmdline=Path(f'/proc/{child}/cmdline').read_bytes().split(b'\0')
            assert len(cmdline)==3 and cmdline[-1]==b'' and cmdline[1].isdigit()
            assert Path(f'/proc/{child}/environ').read_bytes()==b''
            config_fd=int(cmdline[1]);assert 3<=config_fd<=65535
            descriptor=os.open(f'/proc/{child}/fd/{config_fd}',os.O_RDONLY|os.O_CLOEXEC)
            try:
                assert os.fstat(descriptor).st_size==64
                assert fcntl.fcntl(descriptor,fcntl.F_GET_SEALS)&SEALS==SEALS
                row=CAPSULE.unpack(os.pread(descriptor,65,0))
            finally:os.close(descriptor)
            assert row[0:2]==(b'JARVPXY3',1) and row[3:6]==(child,os.getuid(),os.getgid())
            assert row[6:9]==(os.getpid(),os.getuid(),os.getgid()) and row[11]==0
            assert int(time.monotonic()*1000)<row[10]<=int(time.monotonic()*1000)+10000
            actual=os.stat(f'/proc/{child}/fd/{row[2]}')
            matches=(actual.st_dev,actual.st_ino)==(client_info.st_dev,client_info.st_ino) and row[9]==expected_cookie
            observations['continuation_matches_original_channel']=matches
            observations['original_connection_closed_at_exec']=not Path(f'/proc/{child}/fd/{chosen_fd}').exists()
            assert observations['original_connection_closed_at_exec']
            return matches

        def resume(child,childfd,detach,deadline):
            # This real signal is queued before proxy code, then forwarded to
            # the actual tool after its own observed exec (SIGCONT is nonfatal).
            signal.pidfd_send_signal(childfd,signal.SIGCONT);detach()
            assert RESPONSE.unpack(server.recv(16))==(b'JARVSIG3',signal.SIGCONT,0)
            observations['pre_start_signal_forwarded']=signal.SIGCONT
            local=[os.pipe() for _ in range(5)]
            (exec_r,exec_w),(grant_r,grant_w),(body_r,body_w),(wait_r,wait_w),(report_r,report_w)=local
            process=None;toolpidfd=None
            try:
                observations['tool_calls']+=1
                process=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--tool-supervisor',
                    supervisor_path,str(tool_receipt),*map(str,(exec_w,grant_r,body_w,wait_r,report_w))],
                    pass_fds=(exec_w,grant_r,body_w,wait_r,report_w),
                    env={k:v for k,v in os.environ.items() if k in ('HOME','PATH','LANG','LC_ALL')})
                process_holder.append(process)
                pid,=struct.unpack('i',kernel.frame(exec_r,4,deadline))
                toolpidfd=os.pidfd_open(pid)
                observations['tool_exec_observed']=True;observations['tool_pid']=pid
                server.sendall(RESPONSE.pack(b'JARVRSP3',2,0))
                os.write(grant_w,b'G')
                assert kernel.frame(body_r,1,deadline)==b'R'
                signal.pidfd_send_signal(toolpidfd,signal.SIGCONT)
                if mode in ('signal','cancel-after-start'):
                    forwarded=signal.SIGUSR1 if mode=='signal' else signal.SIGTERM
                    signal.pidfd_send_signal(childfd,forwarded)
                    assert RESPONSE.unpack(server.recv(16))==(b'JARVSIG3',forwarded,0)
                    signal.pidfd_send_signal(toolpidfd,forwarded)
                    observations['routed_signal']=forwarded
                else:os.write(wait_w,b'x')
                assert process.wait(timeout=3)==0
                raw,=struct.unpack('i',kernel.frame(report_r,4,deadline))
                assert raw==(-signal.SIGUSR1 if mode=='signal' else -signal.SIGTERM if mode=='cancel-after-start' else 23)
                assert tool_receipt.read_bytes()==b'all-descendants-reaped\n'
                observations.update(tool_cleanup=True,tool_raw_status=raw)
                server.sendall(RESPONSE.pack(b'JARVRSP3',5 if raw<0 else 3,abs(raw)))
            finally:
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:process.wait(timeout=2)
                    except subprocess.TimeoutExpired:process.kill();process.wait(timeout=2)
                if toolpidfd is not None:os.close(toolpidfd)
                for pair in local:
                    for fd in pair:os.close(fd)

        production=mode=='production-deny'
        gate_mode='wrong-image' if mode=='wrong-image' else 'cancel' if mode=='cancel-before-exec' else 'finish'
        gate=(planner.ExecMediation(time.monotonic()+12,os.getuid()) if production else
            kernel.experiment(planner,proxy,gate_mode,ready_r,hold_w,release_r,ready_w,extra_r,
                inspect_exec=inspect,resume_exec=resume,reject_continuation=mode=='swapped-channel'))
        def spawned(child):
            gate.spawned(child);client.close();alternate_client.close()
        try:
            target=wrong if mode=='wrong-image' else proxy
            argv=[sys.executable,'-I',kernel_path,'--launcher',library,str(gate.child_fd),producer,
                target,str(chosen_fd),str(release_w),str(int(time.monotonic()*1000)+10000),mode,str(ready_w),str(extra_r)]
            raw=supervisor.supervise(receipt,argv,
                forward_fds=(gate.child_fd,chosen_fd,release_w,ready_w,extra_r),on_spawn=spawned,on_exec=gate.executed,return_raw_status=True)
            gate.settle()
            assert not gate.errors,gate.errors
            assert raw==(112 if production else 0),('parent outcome',raw)
            if production:
                try:gate.check()
                except planner.FreshnessHold as hold:assert hold.reason=='input_census_unknown'
                else:raise AssertionError('production admitted unconfigured vfork')
                result={'production_refusal':True,'parent_released_before_proxy_termination':False}
            else:
                if mode in ('cancel-before-exec','inherited-exec-deny'):
                    entered=struct.unpack('4i',kernel.frame(release_r,16,gate.deadline))
                    if mode=='inherited-exec-deny':
                        assert gate.child is None and entered[0:2]==(0x4a415256,gate.pid) and entered[3]==41
                        assert gate.vfork_seen and not gate.proof['native_exec_stop']
                        gate.child=entered[2]
                        observations['inherited_exec_denial_observed']=True
                    else:assert entered==(0x4a415256,gate.pid,gate.child,41)
                finished=struct.unpack('4i',kernel.frame(release_r,16,gate.deadline))
                expected=signal.SIGUSR1 if mode=='signal' else signal.SIGTERM if mode=='cancel-after-start' else signal.SIGKILL if mode in ('wrong-image','swapped-channel') else (93 if mode in ('cancel-before-exec','inherited-exec-deny') else 23)<<8
                assert finished==(0x52534c54,gate.pid,gate.child,expected),finished
                result=dict(gate.proof,production_refusal=False,original_parent_wait_status=finished[3])
                try:gate.check()
                except planner.FreshnessHold as hold:assert hold.reason=='input_census_unknown'
                else:raise AssertionError('test composition leaked a source verdict')
            assert observations['tool_calls']==(1 if mode in ('finish','signal','cancel-after-start') else 0)
            assert Path(receipt).read_bytes()==b'all-descendants-reaped\n'
            print(json.dumps(dict(result,**observations,mode=mode,original_cleanup=True,conformance_only=True,
                production_native_handoff=False,full_ci_pass=False,shipping_authority=False)))
        finally:
            gate.close()
            if not production:os.close(gate.proxy_fd)
            for sock in (server,client,alternate_server,alternate_client):sock.close()
            for pair in pipes:
                for fd in pair:os.close(fd)


if __name__=='__main__':
    if sys.argv[1]=='--tool-bootstrap':tool_bootstrap(sys.argv[2:])
    elif sys.argv[1]=='--tool-supervisor':tool_supervisor(sys.argv[2:])
    else:main(sys.argv[1:])
