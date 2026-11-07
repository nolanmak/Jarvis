"""Kernel feasibility with the original exec observer; never CI authority."""
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
import sys
import threading
import time
from types import SimpleNamespace


def native(path):
    library = ctypes.CDLL(path, use_errno=True)
    library.jarvis_ci_exec_listener.argtypes = []
    library.jarvis_ci_exec_listener.restype = ctypes.c_int
    library.jarvis_ci_exec_receive.argtypes = [ctypes.c_int, ctypes.POINTER(ctypes.c_uint64),
        ctypes.POINTER(ctypes.c_uint32), ctypes.POINTER(ctypes.c_int32)]
    library.jarvis_ci_exec_respond.argtypes = [ctypes.c_int, ctypes.c_uint64, ctypes.c_int]
    return library


def launcher(arguments):
    library, early, executable, mode, transport = arguments
    library = native(library)
    listener = library.jarvis_ci_exec_listener()
    if listener < 0:
        raise OSError(ctypes.get_errno(), 'exec notification unavailable')
    # The one listener installs the complete unit guard too. Stacking the
    # standalone ERRNO filter here would hide these policy notifications.
    with socket.socket(fileno=int(transport)) as channel:
        channel.sendmsg([b'listener'], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array('i', [listener]))])
    os.close(listener)
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.ptrace(0, 0, None, None) != 0:
        raise OSError(ctypes.get_errno(), 'original exec stop unavailable')
    os.kill(os.getpid(), signal.SIGSTOP)
    if mode == 'static':
        os.execve(executable, [executable], {})
    environment = {'LD_PRELOAD': early} if mode == 'early' else {}
    expected = errno.ENOSYS if mode == 'listener-loss' else errno.EPERM
    body = '''import ctypes,errno,os,sys
l=ctypes.CDLL(None,use_errno=True); l.syscall.restype=ctypes.c_long
a=(ctypes.c_char_p*2)(b'true',None); e=(ctypes.c_char_p*1)(None)
for number,args in ((59,(ctypes.c_char_p(b'/bin/true'),a,e)),(322,(-100,ctypes.c_char_p(b'/bin/true'),a,e,0))):
 assert l.syscall(number,*args)==-1
 assert ctypes.get_errno()==int(sys.argv[1]),(number,ctypes.get_errno())
g=ctypes.CDLL(sys.argv[2],use_errno=True)
assert g.jarvis_ci_exec_listener()==-1 and ctypes.get_errno()==errno.EPERM
print('BODY_DENIED')
'''
    if mode == 'fork':
        body = body.replace('for number,args in', '''child=os.fork()
if child:
 assert os.waitpid(child,0)[1]==0
for number,args in''').replace("print('BODY_DENIED')", "if not child:os._exit(0)\nprint('BODY_DENIED')")
    if mode in ('vfork-policy','vfork-descendant','clone-policy','clone3-fallback'):
        routes={'vfork-policy':((58,0),),'vfork-descendant':((58,0),),'clone-policy':((56,0x4111),(56,0x111)),
            'clone3-fallback':((435,0),)}[mode]
        body='''import ctypes,errno
l=ctypes.CDLL(None,use_errno=True);l.syscall.restype=ctypes.c_long
for number,flags in %r:
 assert l.syscall(number,ctypes.c_ulong(flags),0,0,0,0)==-1
 assert ctypes.get_errno()==errno.ENOSYS,(number,ctypes.get_errno())
print('POLICY_DENIED')
''' % (routes,)
        if mode=='clone3-fallback':
            body=body.replace("print('POLICY_DENIED')",'''import threading
seen=[]
t=threading.Thread(target=lambda:seen.append('actual-thread'))
t.start();t.join(2)
assert not t.is_alive() and seen==['actual-thread']
print('POLICY_DENIED')''')
        if mode=='vfork-descendant':
            body=body.replace('for number,flags in', '''import os
child=os.fork()
if child:
 assert os.waitpid(child,0)[1]==0
else:
 for number,flags in''').replace(" assert l.syscall", "  assert l.syscall").replace(" assert ctypes.get_errno()", "  assert ctypes.get_errno()")
            body=body.replace("print('POLICY_DENIED')", " if not child:os._exit(0)\nprint('POLICY_DENIED')")
    if mode == 'search':
        try:
            os.execve('/does-not-exist/ci-first-exec-candidate', ['missing'], environment)
        except FileNotFoundError:
            pass
    try:
        os.execve(sys.executable, [sys.executable, '-I', '-c', body, str(expected), sys.argv[2]], environment)
    except PermissionError:
        if mode == 'cancel':
            return 125
        raise


def main(arguments):
    supervisor_path, library_path, early, executable, mode, receipt = arguments
    library = native(library_path)  # Missing API is the original RED condition.
    spec = importlib.util.spec_from_file_location('mediation_original_supervisor', supervisor_path)
    supervisor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(supervisor)
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    parent.settimeout(3)
    state = {'initial': True, 'listener': None, 'pid': None, 'ack': False}
    lock = threading.Lock()
    stop = threading.Event()
    errors = []
    records = []
    thread = None

    def notifications():
        try:
            poller = select.poll()
            poller.register(state['listener'], select.POLLIN)
            while not stop.is_set():
                events = poller.poll(20)
                if not events:
                    continue
                flags = events[0][1]
                if flags & (select.POLLERR | select.POLLNVAL):
                    raise RuntimeError('notification listener failed')
                if flags & select.POLLHUP and not flags & select.POLLIN:
                    return  # Last filtered task has exited and been reaped.
                identity, pid, number = ctypes.c_uint64(), ctypes.c_uint32(), ctypes.c_int32()
                if library.jarvis_ci_exec_receive(state['listener'], ctypes.byref(identity), ctypes.byref(pid), ctypes.byref(number)) != 0:
                    # A withdrawn syscall is no decision, not an authorization.
                    # https://docs.kernel.org/userspace-api/seccomp_filter.html
                    if ctypes.get_errno() in (errno.ENOENT, errno.EINTR):
                        continue
                    raise OSError(ctypes.get_errno(), 'notification receive failed')
                with lock:
                    assert number.value in (59, 322)
                    allow = state['initial'] and pid.value == state['pid'] and mode != 'cancel'
                    records.append({'pid': pid.value, 'nr': number.value, 'allowed': allow, 'after_ack': state['ack']})
                    # Invalid replies must not consume or authorize this request.
                    assert library.jarvis_ci_exec_respond(state['listener'], identity.value, 2) == -1
                    assert ctypes.get_errno() == errno.EINVAL
                    if library.jarvis_ci_exec_respond(state['listener'], identity.value, int(allow)) != 0:
                        raise OSError(ctypes.get_errno(), 'notification response failed')
        except BaseException as error:
            errors.append(str(error))

    def spawned(process):
        nonlocal thread
        state['pid'] = process.pid
        data, control, flags, _ = parent.recvmsg(32, socket.CMSG_SPACE(4))
        assert data == b'listener' and not flags and len(control) == 1
        level, kind, data = control[0]
        assert (level, kind) == (socket.SOL_SOCKET, socket.SCM_RIGHTS)
        descriptors = array.array('i')
        descriptors.frombytes(data)
        assert len(descriptors) == 1
        state['listener'] = descriptors[0]
        os.set_inheritable(state['listener'], False)
        thread = threading.Thread(target=notifications)
        thread.start()

    def executed(process):
        assert process.pid == state['pid']
        with lock:
            assert state['initial'] and not state['ack']
            state['initial'] = False
            state['ack'] = True
        if mode == 'listener-loss':
            stop.set()
            thread.join(2)
            assert not thread.is_alive()
            os.close(state['listener'])
            state['listener'] = None

    try:
        status = supervisor.supervise(receipt, [sys.executable, '-I', str(Path(__file__).resolve()), '--launcher',
            library_path, early, executable, mode, str(child.fileno())], forward_fds=(child.fileno(),),
            on_spawn=spawned, on_exec=executed, return_raw_status=True)
    finally:
        stop.set()
        if thread is not None:
            thread.join(2)
            assert not thread.is_alive()
        if state['listener'] is not None:
            os.close(state['listener'])
        parent.close()
        child.close()
    assert not errors, errors
    assert status == (125 if mode == 'cancel' else 0), status
    assert state['ack'] == (mode != 'cancel')
    initial = [row for row in records if row['allowed']]
    assert len(initial) == (0 if mode == 'cancel' else 2 if mode == 'search' else 1), records
    assert all(not row['after_ack'] for row in initial), records
    denied = [row for row in records if not row['allowed']]
    assert len(denied) == (1 if mode == 'cancel' else 0 if mode == 'listener-loss' else 4 if mode in ('early', 'fork') else 2), records
    if mode != 'cancel':
        assert all(row['after_ack'] for row in denied), records
    assert Path(receipt).read_text() == 'all-descendants-reaped\n'
    print(json.dumps({'first_exec_ack': state['ack'], 'notifications': records,
        'original_cleanup': True, 'shipping_authority': False}))


def integrated(arguments):
    controller_path, supervisor_path, library_path, early, executable, mode, receipt = arguments
    spec = importlib.util.spec_from_file_location('actual_exec_controller', controller_path)
    planner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(planner)
    # The host fixture supplies the real constructor-free DSO. Production
    # bootstraps use UnitPolicy's checked root-owned literal /ci-tools inode.
    library = native(library_path)
    planner.UnitPolicy = lambda: SimpleNamespace(library=library)
    spec = importlib.util.spec_from_file_location('integrated_original_supervisor', supervisor_path)
    supervisor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(supervisor)
    cancellation = os.pipe() if mode == 'cancel' else None
    if cancellation is not None:os.write(cancellation[1],b'cancel')
    gate = planner.ExecMediation(time.monotonic()+10, os.getuid(),cancel=None if cancellation is None else cancellation[0])
    try:
        argv=[sys.executable, '-I', str(Path(__file__).resolve()), '--launcher',library_path, early, executable, mode, str(gate.child_fd)]
        if mode == 'cancel':argv=[sys.executable,'-I','-c','raise SystemExit(125)']
        result = supervisor.supervise(receipt, argv, forward_fds=(gate.child_fd,),
            on_spawn=gate.spawned, on_exec=gate.executed, return_raw_status=True)
        if mode == 'cancel':
            assert result == 125 and not gate.ack and not gate.records
            gate.check()
            assert Path(receipt).read_text() == 'all-descendants-reaped\n'
            print(json.dumps({'controller_hold':None,'cancelled_before_exec':True,'original_cleanup':True,'shipping_authority':False}))
            return
        assert result == 0 and gate.ack
        try:
            gate.check()
        except planner.FreshnessHold as hold:
            assert mode!='clone3-fallback'
            assert hold.reason == 'input_census_unknown'
        else:
            if mode!='clone3-fallback':
                raise AssertionError('unsupported route was accepted as a source verdict')
        assert not gate.errors
        assert len([r for r in gate.records if r['allowed']]) == (2 if mode == 'search' else 1)
        denied = [r for r in gate.records if not r['allowed']]
        if mode=='clone3-fallback':
            assert denied and all(r['disposition']=='clone3_fallback' for r in denied)
        else:
            assert len(denied) == (4 if mode in ('early','fork') else 1 if mode in ('vfork-policy','vfork-descendant') else 2)
        if mode=='vfork-descendant':assert denied[0]['pid']!=gate.pid
        assert all(r['after_ack'] for r in denied)
        assert Path(receipt).read_text() == 'all-descendants-reaped\n'
        print(json.dumps({'controller_hold':None if mode=='clone3-fallback' else 'input_census_unknown',
            'policy_records':[r for r in denied if r['nr'] not in (59,322)],
            'original_cleanup':True,'shipping_authority':False}))
    finally:
        gate.close()
        if cancellation is not None:
            for descriptor in cancellation:os.close(descriptor)


if __name__ == '__main__':
    if sys.argv[1] == '--launcher':
        sys.exit(launcher(sys.argv[2:]))
    if sys.argv[1] == '--controller':
        integrated(sys.argv[2:])
        sys.exit(0)
    main(sys.argv[1:])
