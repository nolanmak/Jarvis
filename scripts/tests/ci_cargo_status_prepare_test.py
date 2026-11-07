"""Real native continuation setup and failed-exec rollback, no admission grant."""
import ctypes
import errno
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
CHILD='''import ctypes,errno,fcntl,json,os,signal,socket,struct,sys,time
class Prepared(ctypes.Structure):_fields_=[('config_fd',ctypes.c_int),('connection_fd',ctypes.c_int)]
l=ctypes.CDLL(sys.argv[1],use_errno=True)
l.jarvis_ci_prepare_status_continuation.argtypes=[ctypes.c_int,ctypes.c_int64,ctypes.POINTER(Prepared)]
l.jarvis_ci_close_status_continuation.argtypes=[ctypes.POINTER(Prepared)]
l.jarvis_ci_exec_status_proxy.argtypes=[ctypes.c_char_p,ctypes.c_int,ctypes.c_int64]
s=socket.socket(fileno=int(sys.argv[2]));mode=sys.argv[3]
s.set_inheritable(False);flags=fcntl.fcntl(s.fileno(),fcntl.F_GETFD)
mask=signal.pthread_sigmask(signal.SIG_BLOCK,{signal.SIGUSR1})
before=set(os.listdir('/proc/self/fd'))
if mode=='prepare':
 p=Prepared();assert l.jarvis_ci_prepare_status_continuation(s.fileno(),int(time.monotonic()*1000)+2000,ctypes.byref(p))==0
 assert not fcntl.fcntl(p.config_fd,fcntl.F_GETFD)&fcntl.FD_CLOEXEC
 assert not fcntl.fcntl(p.connection_fd,fcntl.F_GETFD)&fcntl.FD_CLOEXEC
 assert p.config_fd!=p.connection_fd and p.connection_fd!=s.fileno()
 data=os.pread(p.config_fd,65,0);assert len(data)==64
 row=struct.unpack('<8sIiIIIIIIQqQ',data)
 assert row[:3]==(b'JARVPXY3',1,p.connection_fd) and row[3]==os.getpid()
 seals=fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL
 assert fcntl.fcntl(p.config_fd,fcntl.F_GET_SEALS)&seals==seals
 ctypes.set_errno(errno.E2BIG);l.jarvis_ci_close_status_continuation(ctypes.byref(p))
 assert ctypes.get_errno()==errno.E2BIG and (p.config_fd,p.connection_fd)==(-1,-1)
else:
 if mode=='denied':assert l.jarvis_ci_deny_exec()==0
 image=sys.argv[4].encode() if mode=='denied' else b'/does-not-exist/jarvis-status-image'
 assert l.jarvis_ci_exec_status_proxy(image,s.fileno(),int(time.monotonic()*1000)+2000)==-1
 assert ctypes.get_errno()==(errno.EPERM if mode=='denied' else errno.ENOENT)
assert fcntl.fcntl(s.fileno(),fcntl.F_GETFD)==flags
assert set(os.listdir('/proc/self/fd'))==before
assert signal.pthread_sigmask(signal.SIG_BLOCK,set())=={signal.SIGUSR1}|mask
signal.pthread_sigmask(signal.SIG_SETMASK,mask)
s.sendall(b'original-channel-intact')
print('PREPARE_ROLLBACK_OK')
'''


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native helper build')
class StatusPreparationTests(unittest.TestCase):
    def run_mode(self,mode):
        library=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        parent,child=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
        with parent,child:
            parent.settimeout(2)
            result=subprocess.run([sys.executable,'-I','-c',CHILD,str(library),str(child.fileno()),mode,str(library.parent/'ci_cargo_status_proxy')],
                pass_fds=(child.fileno(),),capture_output=True,text=True,timeout=5,
                env={k:v for k,v in os.environ.items() if k in ('HOME','PATH','LANG','LC_ALL')})
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual(result.stdout,'PREPARE_ROLLBACK_OK\n')
            self.assertEqual(parent.recv(64),b'original-channel-intact')

    def test_sealed_pid_bound_preparation_leaves_original_fd_and_mask_unchanged(self):self.run_mode('prepare')
    def test_missing_native_image_preserves_errno_mask_channel_and_fd_count(self):self.run_mode('missing')
    def test_kernel_exec_refusal_closes_temporaries_without_fallback(self):self.run_mode('denied')


if __name__=='__main__':unittest.main()
