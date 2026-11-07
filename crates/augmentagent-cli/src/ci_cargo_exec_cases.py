"""Controlled subprocess cases for the native exec-denial primitive."""
import ctypes,errno,os,resource,sys
resource.setrlimit(resource.RLIMIT_CORE,(0,0))
library=ctypes.CDLL(sys.argv[1],use_errno=True)
if library.jarvis_ci_deny_exec()!=0:raise OSError(ctypes.get_errno(),'exec filter unavailable')
libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
assert libc.prctl(39,0,0,0,0)==1
mode=sys.argv[2]
if mode=='x32':
    libc.syscall(0x40000000|39)
    raise SystemExit('foreign ABI was not rejected')
if mode=='fork':
    child=os.fork()
    if child:
        _,status=os.waitpid(child,0);assert status==0
        print('REAPED');raise SystemExit(0)
if mode=='execveat':
    argv=(ctypes.c_char_p*2)(b'true',None);env=(ctypes.c_char_p*1)(None)
    assert libc.syscall(322,-100,ctypes.c_char_p(b'/bin/true'),argv,env,0)==-1
    assert ctypes.get_errno()==errno.EPERM
else:
    try:os.execve('/bin/true',['true'],{})
    except PermissionError as error:assert error.errno==errno.EPERM
    else:raise SystemExit('exec unexpectedly returned')
os.write(1,b'DENIED\n')
if mode=='fork':os._exit(0)
