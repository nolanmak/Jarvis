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
import tempfile
import unittest

CAPSULE=struct.Struct('<8sIiIIIIIIQqQ')
CHILD='''import ctypes,errno,fcntl,json,os,signal,socket,struct,sys,time
class Prepared(ctypes.Structure):_fields_=[('config_fd',ctypes.c_int),('connection_fd',ctypes.c_int)]
l=ctypes.CDLL(sys.argv[1],use_errno=True)
l.jarvis_ci_prepare_status_continuation.argtypes=[ctypes.c_int,ctypes.c_int64,ctypes.POINTER(Prepared)]
l.jarvis_ci_close_status_continuation.argtypes=[ctypes.POINTER(Prepared)]
l.jarvis_ci_exec_status_proxy.argtypes=[ctypes.c_char_p,ctypes.c_int,ctypes.c_int64]
l.jarvis_ci_exec_status_proxy_fd.argtypes=[ctypes.c_int,ctypes.c_int,ctypes.c_int64]
s=socket.socket(fileno=int(sys.argv[2]));mode=sys.argv[3]
s.set_inheritable(False);flags=fcntl.fcntl(s.fileno(),fcntl.F_GETFD)
mask=signal.pthread_sigmask(signal.SIG_BLOCK,{signal.SIGUSR1})
image_fd=None
if mode.startswith('fd-'):
 image_fd=os.memfd_create('native-status-test',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING|0x10)
 with open(sys.argv[4],'rb') as source:os.write(image_fd,source.read())
 os.fchmod(image_fd,0o555)
 if mode!='fd-unsealed':fcntl.fcntl(image_fd,fcntl.F_ADD_SEALS,0x3f)
 image_info=os.fstat(image_fd);image_flags=fcntl.fcntl(image_fd,fcntl.F_GETFD)
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
 if mode in ('denied','fd-denied'):assert l.jarvis_ci_deny_exec()==0
 image=sys.argv[4].encode() if mode=='denied' else b'/does-not-exist/jarvis-status-image'
 if image_fd is None:assert l.jarvis_ci_exec_status_proxy(image,s.fileno(),int(time.monotonic()*1000)+2000)==-1
 else:
  assert l.jarvis_ci_exec_status_proxy_fd(image_fd,s.fileno(),int(time.monotonic()*1000)+2000)==-1
 assert ctypes.get_errno()==(errno.EPERM if mode in ('denied','fd-denied','fd-unsealed') else errno.ENOENT)
 if image_fd is not None:
  assert os.fstat(image_fd)==image_info and fcntl.fcntl(image_fd,fcntl.F_GETFD)==image_flags
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
    def test_sealed_descriptor_exec_survives_replaced_source_path(self):
        library=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        code='''import ctypes,importlib.util,os,pathlib,sys,time
spec=importlib.util.spec_from_file_location('staging',sys.argv[1]);staging=importlib.util.module_from_spec(spec);spec.loader.exec_module(staging)
source=pathlib.Path(sys.argv[3]);source.write_bytes(pathlib.Path(sys.argv[2]).with_name('ci_cargo_status_proxy').read_bytes())
fd=os.open(source,os.O_RDONLY|os.O_CLOEXEC)
with staging.capture_executable(fd) as image:
 os.close(fd);source.unlink();source.write_bytes(b'replaced source is not executable')
 library=ctypes.CDLL(sys.argv[2],use_errno=True)
 launch=library.jarvis_ci_exec_status_proxy_fd;launch.argtypes=[ctypes.c_int,ctypes.c_int,ctypes.c_int64]
 launch(image.fileno(),int(sys.argv[4]),int(time.monotonic()*1000)+3000)
 raise AssertionError(('descriptor exec returned',ctypes.get_errno()))
'''
        parent,child=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
        with parent,child,tempfile.TemporaryDirectory() as directory:
            parent.sendall(struct.pack('<8sII',b'JARVRSP3',2,0))
            parent.sendall(struct.pack('<8sII',b'JARVRSP3',3,23))
            result=subprocess.run([sys.executable,'-I','-c',code,
                str(Path(__file__).resolve().parents[1]/'ci-cargo-staging.py'),str(library),
                str(Path(directory)/'image'),str(child.fileno())],pass_fds=(child.fileno(),),
                capture_output=True,text=True,timeout=5)
            self.assertEqual(result.returncode,23,result.stderr)
            self.assertEqual(result.stdout,'')

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
    def test_descriptor_exec_refusal_preserves_borrowed_image_mask_and_channel(self):self.run_mode('fd-denied')
    def test_unsealed_descriptor_refuses_without_changing_original_descriptors(self):self.run_mode('fd-unsealed')


if __name__=='__main__':unittest.main()
