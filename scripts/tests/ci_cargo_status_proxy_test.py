"""Black-box V3 continuation protocol. Configuration is not an exec permit."""
import array
import contextlib
import fcntl
import os
from pathlib import Path
import platform
import signal
import socket
import struct
import subprocess
import sys
import time
import unittest

CAPSULE=struct.Struct('<8sIiIIIIIIQqQ')
RESPONSE=struct.Struct('<8sII')
SEALS=fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL
BOOTSTRAP='''import array,ctypes,os,signal,socket,sys
+channel=socket.socket(fileno=int(sys.argv[2]));connection=int(sys.argv[3])
+signal.signal(signal.SIGUSR1,signal.SIG_IGN)
+signal.pthread_sigmask(signal.SIG_BLOCK,signal.valid_signals()-{signal.SIGKILL,signal.SIGSTOP})
+channel.sendall(b'blocked')
+data,control,flags,_=channel.recvmsg(8,socket.CMSG_SPACE(4))
+assert data==b'config' and not flags and len(control)==1
+level,kind,body=control[0];assert (level,kind)==(socket.SOL_SOCKET,socket.SCM_RIGHTS)
+fds=array.array('i');fds.frombytes(body);assert len(fds)==1
+os.set_inheritable(fds[0],True);os.set_inheritable(connection,True)
+for value in sys.argv[4:]:os.set_inheritable(int(value),False)
+channel.close()
+os.execve(sys.argv[1],[sys.argv[1],str(fds[0])],{})
'''.replace('\n+','\n')


class Endpoint:
    def __init__(self,test,*,changes=None,sealed=True,extra=b'',timeout_ms=2500,
            extra_fds=(),cloexec_fds=(),stdout=subprocess.PIPE,stderr=subprocess.PIPE,socket_type=socket.SOCK_SEQPACKET):
        library=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        self.binary=library.parent/'ci_cargo_status_proxy'
        self.server,client=socket.socketpair(socket.AF_UNIX,socket_type)
        self.server.settimeout(2)
        parent,child=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
        parent.settimeout(2)
        self.process=subprocess.Popen([sys.executable,'-I','-c',BOOTSTRAP,str(self.binary),str(child.fileno()),str(client.fileno()),*map(str,cloexec_fds)],
            pass_fds=(child.fileno(),client.fileno(),*extra_fds),stdout=stdout,stderr=stderr,
            env={k:v for k,v in os.environ.items() if k in ('HOME','PATH','LANG','LC_ALL')})
        child.close()
        test.addCleanup(self.close)
        assert parent.recv(8)==b'blocked'
        values=dict(magic=b'JARVPXY3',version=1,connection=client.fileno(),client_pid=self.process.pid,
            client_uid=os.getuid(),client_gid=os.getgid(),server_pid=os.getpid(),server_uid=os.getuid(),
            server_gid=os.getgid(),cookie=struct.unpack('Q',client.getsockopt(socket.SOL_SOCKET,57,8))[0],
            deadline=int(time.monotonic()*1000)+timeout_ms,reserved=0)
        values.update(changes or {})
        fd=os.memfd_create('status-continuation',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            os.write(fd,CAPSULE.pack(*values.values())+extra)
            if sealed:fcntl.fcntl(fd,fcntl.F_ADD_SEALS,SEALS)
            parent.sendmsg([b'config'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[fd]))])
        finally:os.close(fd);parent.close();client.close()

    def send(self,stage,value=0,magic=b'JARVRSP3'):
        self.server.sendall(RESPONSE.pack(magic,stage,value))

    def finish(self,expected):
        out,err=self.process.communicate(timeout=3)
        assert self.process.returncode==expected,(self.process.returncode,out,err)
        assert (out,err)==(b'',b''),(out,err)

    def close(self):
        if self.process.poll() is None:
            self.process.kill();self.process.communicate(timeout=2)
        self.server.close()


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native helper build')
class StatusProxyTests(unittest.TestCase):
    def test_image_is_static_and_finishes_only_after_tool_start(self):
        binary=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve().parent/'ci_cargo_status_proxy'
        data=binary.read_bytes();self.assertEqual(data[:6],b'\x7fELF\x02\x01')
        off,=struct.unpack_from('<Q',data,32);size,count=struct.unpack_from('<HH',data,54)
        self.assertNotIn(3,[struct.unpack_from('<I',data,off+i*size)[0] for i in range(count)])
        endpoint=Endpoint(self);self.assertIsNone(endpoint.process.poll())
        endpoint.send(2);endpoint.send(3,23);endpoint.finish(23)

    def test_pending_signals_are_forwarded_before_tool_start(self):
        endpoint=Endpoint(self);os.kill(endpoint.process.pid,signal.SIGUSR2)
        self.assertEqual(RESPONSE.unpack(endpoint.server.recv(16)),(b'JARVSIG3',signal.SIGUSR2,0))
        self.assertIsNone(endpoint.process.poll())
        endpoint.send(2);endpoint.send(5,signal.SIGUSR1);endpoint.finish(-signal.SIGUSR1)

    def test_signals_after_tool_start_preserve_native_parent_wait_status(self):
        endpoint=Endpoint(self);os.kill(endpoint.process.pid,signal.SIGUSR2)
        self.assertEqual(RESPONSE.unpack(endpoint.server.recv(16)),(b'JARVSIG3',signal.SIGUSR2,0))
        endpoint.send(2);os.kill(endpoint.process.pid,signal.SIGUSR1)
        self.assertEqual(RESPONSE.unpack(endpoint.server.recv(16)),(b'JARVSIG3',signal.SIGUSR1,0))
        endpoint.send(5,signal.SIGTERM);endpoint.finish(-signal.SIGTERM)

    def test_out_of_order_and_malformed_control_cannot_claim_success(self):
        for frames in (((3,0),),((2,1),),((4,2),),((2,0),(2,0)),((2,0),(3,256)),((2,0),(5,signal.SIGCHLD))):
            with self.subTest(frames=frames):
                e=Endpoint(self)
                for stage,value in frames:e.send(stage,value)
                e.finish(125)
        e=Endpoint(self);e.send(2,magic=b'JARVRSP2');e.finish(125)
        e=Endpoint(self);e.server.sendall(RESPONSE.pack(b'JARVRSP3',2,0)+b'extra');e.finish(125)

    def test_configuration_binds_actual_identity_and_socket(self):
        for changes in ({'client_pid':1},{'client_uid':0},{'client_gid':0},{'server_pid':1},
                {'server_uid':0},{'server_gid':0},{'cookie':1},{'reserved':1},{'version':2}):
            with self.subTest(changes=changes):Endpoint(self,changes=changes).finish(125)
        Endpoint(self,socket_type=socket.SOCK_STREAM).finish(125)

    def test_capsule_must_be_sealed_and_exactly_sized(self):
        Endpoint(self,sealed=False).finish(125)
        Endpoint(self,extra=b'extra').finish(125)

    def test_disconnect_and_absolute_deadline_fail_closed(self):
        e=Endpoint(self);e.server.close();e.finish(125)
        Endpoint(self,timeout_ms=-1).finish(125)
        Endpoint(self,timeout_ms=950000).finish(125)
        Endpoint(self,timeout_ms=100).finish(125)

    def test_received_rights_are_rejected_as_malformed_control(self):
        e=Endpoint(self);fd=os.memfd_create('unexpected-rights',os.MFD_CLOEXEC)
        try:e.server.sendmsg([RESPONSE.pack(b'JARVRSP3',2,0)],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[fd]))])
        finally:os.close(fd)
        e.finish(125)

    def test_binary_stdio_extra_fifo_and_exec_closure_are_preserved(self):
        with contextlib.ExitStack() as stack:
            pipes=[]
            for _ in range(4):
                r,w=os.pipe()
                pipes.append((stack.enter_context(os.fdopen(r,'rb',buffering=0)),
                    stack.enter_context(os.fdopen(w,'wb',buffering=0))))
            (out_r,out_w),(err_r,err_w),(extra_r,extra_w),(clo_r,clo_w)=pipes
            e=Endpoint(self,extra_fds=(extra_w.fileno(),clo_w.fileno()),cloexec_fds=(clo_w.fileno(),),
                stdout=out_w,stderr=err_w)
            extra_w.close();clo_w.close()
            os.kill(e.process.pid,signal.SIGUSR2)
            self.assertEqual(RESPONSE.unpack(e.server.recv(16)),(b'JARVSIG3',signal.SIGUSR2,0))
            import select
            self.assertFalse(select.select([extra_r],[],[],0)[0])
            self.assertTrue(select.select([clo_r],[],[],0)[0]);self.assertEqual(clo_r.read(),b'')
            output=b'out\x00\xff\n';error=b'err\xfe\x00\n'
            out_w.write(output);err_w.write(error);out_w.close();err_w.close()
            e.send(2);e.send(3,23)
            self.assertEqual(e.process.wait(timeout=3),23)
            self.assertEqual(out_r.read(),output);self.assertEqual(err_r.read(),error)
            self.assertEqual(extra_r.read(),b'')


if __name__=='__main__':unittest.main()
