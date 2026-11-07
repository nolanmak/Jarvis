"""Actual kernel cases for the inherited namespace/async-I/O unit policy."""
import ctypes
import errno
import os
import resource
import json
import signal
import struct
import sys
import threading

resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
mode = sys.argv[2]
before = libc.prctl(39, 0, 0, 0, 0)
library = ctypes.CDLL(sys.argv[1], use_errno=True)
assert libc.prctl(39, 0, 0, 0, 0) == before, 'library load activated policy'


def denied(number, *args, expected=errno.EPERM):
    ctypes.set_errno(0)
    result = libc.syscall(number, *args)
    assert result == -1 and ctypes.get_errno() == expected, (number, result, ctypes.get_errno())


def cross_memory(listener):
    class Iovec(ctypes.Structure):
        _fields_=[('base',ctypes.c_void_p),('size',ctypes.c_size_t)]
    for name in ('process_vm_readv','process_vm_writev'):
        fn=getattr(libc,name);fn.restype=ctypes.c_ssize_t
        fn.argtypes=[ctypes.c_int,ctypes.POINTER(Iovec),ctypes.c_ulong,
            ctypes.POINTER(Iovec),ctypes.c_ulong,ctypes.c_ulong]
    libc.ptrace.restype=ctypes.c_long
    libc.ptrace.argtypes=[ctypes.c_uint,ctypes.c_int,ctypes.c_void_p,ctypes.c_void_p]
    commands=os.pipe();responses=os.pipe()
    child=os.fork()
    if not child:
        os.close(commands[1]);os.close(responses[0])
        value=ctypes.create_string_buffer(b'ORIGINAL')
        os.write(responses[1],struct.pack('Q',ctypes.addressof(value)))
        assert os.read(commands[0],1)==b'1' and value.raw[:8]==b'changed!'
        value.value=b'ORIGINAL';os.write(responses[1],b'ok')
        assert os.read(commands[0],1)==b'2' and value.raw[:8]==b'ORIGINAL'
        os._exit(0)
    os.close(commands[0]);os.close(responses[1])
    address,=struct.unpack('Q',os.read(responses[0],8))
    source=ctypes.create_string_buffer(b'changed!');observed=ctypes.create_string_buffer(8)
    local=Iovec(ctypes.addressof(source),8);remote=Iovec(address,8)
    # Valid related-process controls distinguish policy from kernel permissions
    # or invalid iovec pointers. The victim verifies both actual byte states.
    assert libc.process_vm_writev(child,ctypes.byref(local),1,ctypes.byref(remote),1,0)==8
    os.write(commands[1],b'1');assert os.read(responses[0],2)==b'ok'
    assert libc.ptrace(0x4206,child,None,None)==0 # SEIZE.
    assert libc.ptrace(0x4207,child,None,None)==0 # INTERRUPT.
    found,status=os.waitpid(child,0x40000000)
    assert found==child and os.WIFSTOPPED(status)
    assert libc.ptrace(17,child,None,None)==0 # DETACH.
    descriptor=library.jarvis_ci_exec_listener() if listener else library.jarvis_ci_restrict_unit()
    assert descriptor>=0 and (listener or descriptor==0)
    ctypes.set_errno(0)
    assert libc.process_vm_writev(child,ctypes.byref(local),1,ctypes.byref(remote),1,0)==-1
    assert ctypes.get_errno()==errno.EPERM
    ctypes.set_errno(0)
    assert libc.ptrace(0x4206,child,None,None)==-1 and ctypes.get_errno()==errno.EPERM
    # Remote advisory operations are refused too; invalid/unsupported flags
    # cannot be mistaken for a positive remote-madvise compatibility control.
    pidfd=os.pidfd_open(child)
    try:denied(440,pidfd,ctypes.byref(remote),1,0,0)
    finally:os.close(pidfd)
    denied(101,ctypes.c_ulong(1<<32),child,0,0) # High bits must not alias TRACEME.
    readback=Iovec(ctypes.addressof(observed),8)
    assert libc.process_vm_readv(child,ctypes.byref(readback),1,ctypes.byref(remote),1,0)==8
    assert observed.raw==b'ORIGINAL'
    os.write(commands[1],b'2');assert os.waitpid(child,0)==(child,0)
    os.close(commands[1]);os.close(responses[0])
    if listener:os.close(descriptor)
    print(json.dumps({'mode':mode,'pre_policy_remote_write_bytes':8,'pre_policy_attach':True,
        'post_policy_remote_write_errno':errno.EPERM,'post_policy_attach_errno':errno.EPERM,
        'post_policy_remote_read_bytes':8,'victim_bytes_unchanged':True,'victim_reaped':True,
        'full_ci_pass':False,'shipping_authority':False}))


def pidfd_copy(listener):
    import fcntl,socket
    server,client=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
    commands=os.pipe();responses=os.pipe();target=client.fileno()
    child=os.fork()
    if not child:
        server.close();os.close(commands[1]);os.close(responses[0])
        os.write(responses[1],client.getsockopt(socket.SOL_SOCKET,57,8))
        assert os.read(commands[0],1)==b'1'
        assert fcntl.fcntl(target,fcntl.F_GETFL)&os.O_NONBLOCK
        fcntl.fcntl(target,fcntl.F_SETFL,fcntl.fcntl(target,fcntl.F_GETFL)&~os.O_NONBLOCK)
        os.write(responses[1],b'R')
        assert os.read(commands[0],1)==b'2'
        assert fcntl.fcntl(target,fcntl.F_GETFL)&os.O_NONBLOCK
        os._exit(0)
    client.close();os.close(commands[0]);os.close(responses[1]);pidfd=os.pidfd_open(child)
    duplicate=None;descriptor=None;reaped=False
    try:
        cookie=os.read(responses[0],8);assert len(cookie)==8
        before_fd=libc.syscall(438,pidfd,target,0)
        assert before_fd>=0,('valid pre-policy pidfd_getfd',ctypes.get_errno())
        duplicate=socket.socket(fileno=before_fd)
        assert duplicate.getsockopt(socket.SOL_SOCKET,57,8)==cookie
        duplicate.sendall(b'copied-before');assert server.recv(32)==b'copied-before'
        duplicate.setblocking(False);os.write(commands[1],b'1')
        assert os.read(responses[0],1)==b'R'
        assert not fcntl.fcntl(duplicate.fileno(),fcntl.F_GETFL)&os.O_NONBLOCK
        descriptor=library.jarvis_ci_exec_listener() if listener else library.jarvis_ci_restrict_unit()
        assert descriptor>=0 and (listener or descriptor==0)
        ctypes.set_errno(0);new_fd=libc.syscall(438,pidfd,target,0);failure=ctypes.get_errno()
        if new_fd>=0:os.close(new_fd)
        assert new_fd==-1 and failure==errno.EPERM,('post-policy fd copy',new_fd,failure)
        # A syscall guard cannot revoke an already-issued file description.
        duplicate.setblocking(False)
        duplicate.sendall(b'retained-after');assert server.recv(32)==b'retained-after'
        os.write(commands[1],b'2');assert os.waitpid(child,0)==(child,0);reaped=True
        print(json.dumps({'mode':mode,'pre_policy_fd_copy':True,'same_socket_cookie':True,
            'pre_policy_shared_flags_verified':True,'post_policy_fd_copy_errno':failure,
            'retained_alias_usable_after_policy':True,'post_policy_shared_flags_verified':True,
            'victim_reaped':True,'descriptor_ingress_complete':False,
            'full_ci_pass':False,'shipping_authority':False}))
    finally:
        if not reaped:
            try:os.kill(child,signal.SIGKILL)
            except ProcessLookupError:pass
            os.waitpid(child,0)
        if duplicate is not None:duplicate.close()
        if listener and descriptor is not None and descriptor>=0:os.close(descriptor)
        os.close(pidfd);os.close(commands[1]);os.close(responses[0]);server.close()


if mode in ('pidfd-copy','pidfd-copy-listener'):
    pidfd_copy(mode=='pidfd-copy-listener')
    raise SystemExit(0)


if mode in ('cross-memory','cross-memory-listener'):
    cross_memory(mode=='cross-memory-listener')
    raise SystemExit(0)


ready = threading.Event()
release = threading.Event()
errors = []


def existing_thread():
    ready.set()
    release.wait()
    try:
        denied(208, 0, 0, 0, 0, 0)  # io_getevents, also checks TSYNC.
        denied(438,-1,0,0)
        denied(311,os.getpid(),0,0,0,0,ctypes.c_ulong(0))
        denied(435, 0, 0, expected=errno.ENOSYS)
    except BaseException as error:
        errors.append(error)


if mode == 'threads':
    thread = threading.Thread(target=existing_thread, daemon=True)
    thread.start()
    assert ready.wait(2)
if library.jarvis_ci_restrict_unit() != 0:
    raise OSError(ctypes.get_errno(), 'unit policy unavailable')
assert libc.prctl(39, 0, 0, 0, 0) == 1

if mode == 'namespace':
    # Explicit arguments distinguish policy denial from invalid kernel inputs.
    for number in (155, 161, 165, 166, 272, 308, 428, 429, 430, 431, 432, 433, 442):
        denied(number, 0, 0, 0, 0, 0, 0)
elif mode == 'async':
    for number in (206, 207, 208, 209, 210, 333, 425, 426, 427):
        denied(number, 0, 0, 0, 0, 0, 0)
elif mode == 'clone':
    for flag in (0x80, 0x20000, 0x2000000, 0x4000000, 0x8000000,
                 0x10000000, 0x20000000, 0x40000000, 1 << 32):
        denied(56, ctypes.c_ulong(flag | signal.SIGCHLD), 0, 0, 0, 0)
    denied(435, 0, 0, expected=errno.ENOSYS)
elif mode == 'fork':
    child = os.fork()
    if child == 0:
        denied(208, 0, 0, 0, 0, 0)
        denied(438,-1,0,0)
        os._exit(0)
    assert os.waitpid(child, 0)[1] == 0
elif mode == 'exec':
    # The first ordinary exec is allowed; the kernel policy survives exec.
    program = ('import ctypes,errno; l=ctypes.CDLL(None,use_errno=True); '
               'assert l.syscall(208,0,0,0,0,0)==-1; '
               'assert ctypes.get_errno()==errno.EPERM; '
               'assert l.syscall(438,-1,0,0)==-1; '
               'assert ctypes.get_errno()==errno.EPERM; print("GUARDED")')
    os.execve(sys.executable, [sys.executable, '-I', '-c', program], {})
elif mode == 'threads':
    release.set()
    thread.join(2)
    assert not thread.is_alive() and not errors, errors
    # Newly created threads still work after clone3 is refused with ENOSYS.
    after = threading.Thread(target=existing_thread)
    after.start()
    after.join(2)
    assert not after.is_alive() and not errors, errors
elif mode == 'x32':
    libc.syscall(0x40000000 | 39)
    raise SystemExit('foreign ABI was not rejected')
else:
    raise SystemExit('unknown unit policy case')
print('GUARDED')
