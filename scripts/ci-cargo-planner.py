#!/usr/bin/env python3
"""Private guest V3 transport. No passing-CI proof or protected-cache activation.

Only the admitted root controller creates a listener. Planner requests run in
fresh, single-threaded root helpers; requested environments apply after dropping
to a unique unit UID. Frozen unit views and complete input/output provenance are
still required before this transport can run protected production Cargo.
"""
import array
import ctypes
import errno
import fcntl
import hashlib
import hmac
import importlib.util
import json
import math
import os
from pathlib import Path
import select
import signal
import socket
import stat
import struct
import subprocess
import sys
import threading
import time
import uuid

BASE=Path(__file__).resolve().parent
def _load(name,file):
    spec=importlib.util.spec_from_file_location(name,BASE/file)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module
legacy=_load('planner_wire','ci-cargo-relay.py')
staging=_load('planner_stdin','ci-cargo-staging.py')
cache=_load('planner_cache','ci-cache-generation.py')
FreshnessHold=cache.FreshnessHold
STAGES={'staging_ready':1,'tool_started':2,'finished':3,'error':4,'signaled':5}
_TOKEN=object()
_PHASE='planner_startup'
_CHILD_DIAGNOSTIC=None


def diagnostic(value):
    phases={'planner_startup','planner_unit','planner_capture','planner_authorize','planner_drop',
        'planner_arm_exec','planner_unit_exec','planner_client_start','planner_client_exec',
        'planner_probe_stdin','planner_probe_jobserver','planner_probe_cancel','planner_probe_supervisor'}
    categories={'OSError','ValueError','KeyError','FreshnessHold','StagingUnavailable','RuntimeError','Unavailable'}
    value=value if type(value) is dict else {}
    return {'stage':value.get('stage') if value.get('stage') in phases else 'planner_startup',
        'category':value.get('category') if value.get('category') in categories else 'Unavailable',
        'errno':value.get('errno') if type(value.get('errno')) is int and 0<=value['errno']<=4095 else None}


def event(stage,value):
    if stage not in STAGES or type(value) is not int or not 0<=value<=0xffffffff:raise ValueError('invalid V3 event')
    return legacy.RESPONSE.pack(b'JARVRSP3',STAGES[stage],value)


def decode_event(data):
    if len(data)!=legacy.RESPONSE.size:raise ValueError('invalid V3 event size')
    magic,stage,value=legacy.RESPONSE.unpack(data)
    if magic!=b'JARVRSP3' or stage not in STAGES.values():raise ValueError('invalid V3 event')
    return next(name for name,number in STAGES.items() if number==stage),value


def payload(fd):return legacy._payload(fd,expected_magic=b'JARVARG3')


def _source(target,binding,command):
    if os.getresuid()!=(0,0,0) or os.getresgid()!=(0,0,0):raise FreshnessHold('invalidation_unsafe')
    cache.validate_binding(binding);target=Path(target)
    if (target.resolve()!=target or target.name!='target' or target.parent.name!=binding['generation']
            or target.stat().st_uid not in (0,1000)
            or cache._read_record(target.parent/'control.json')!={'version':1,'binding':binding,'active_command':command}):
        raise FreshnessHold('generation_mismatch')
    return target


class _Admission:
    def __init__(self,token,target,binding,command,root):
        if token is not _TOKEN:raise FreshnessHold('invalidation_unsafe')
        self.token=token;self.owner=os.getpid();self.target=target;self.binding=dict(binding);self.command=command;self.root=root


def admit(target,binding,command,root):
    target=_source(target,binding,command);root=Path(root)
    if root.resolve()!=root:raise FreshnessHold('invalidation_unsafe')
    info=root.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid!=0 or stat.S_IMODE(info.st_mode)!=0o700 or any(root.iterdir()):
        raise FreshnessHold('invalidation_unsafe')
    return _Admission(_TOKEN,target,binding,command,root)


def _sealed(value):
    body=json.dumps(value,sort_keys=True,separators=(',',':')).encode()
    if len(body)>16384:raise FreshnessHold('metadata_torn')
    fd=os.memfd_create('ci-planner-bootstrap',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
    try:
        if os.write(fd,body)!=len(body):raise OSError(errno.EIO,'bootstrap write failed')
        fcntl.fcntl(fd,fcntl.F_ADD_SEALS,legacy.SEALS);return fd
    except BaseException:os.close(fd);raise


def _bootstrap(fd,kind):
    if os.getresuid()!=(0,0,0) or os.getresgid()!=(0,0,0):raise FreshnessHold('invalidation_unsafe')
    info=os.fstat(fd)
    if (not stat.S_ISREG(info.st_mode) or info.st_uid!=0 or not 1<=info.st_size<=16384
            or fcntl.fcntl(fd,fcntl.F_GET_SEALS)&legacy.SEALS!=legacy.SEALS):raise FreshnessHold('metadata_torn')
    value=json.loads(os.pread(fd,info.st_size+1,0),object_pairs_hook=cache._unique_object)
    if (type(value) is not dict or value.get('version')!=3 or type(value.get('version')) is not int
            or value.get('kind')!=kind or type(value.get('parent')) is not int or value['parent']!=os.getppid()
            or type(value.get('deadline')) not in (int,float) or not math.isfinite(value['deadline'])
            or not time.monotonic()<value['deadline']<=time.monotonic()+901):raise FreshnessHold('metadata_torn')
    _source(value['target'],value['binding'],value['command'])
    return value


def _parent_guard(parent):
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(1,signal.SIGKILL,0,0,0)!=0 or os.getppid()!=parent:raise FreshnessHold('cleanup_uncertain')


def _release(fd,cancel,deadline):
    while True:
        remaining=deadline-time.monotonic()
        if remaining<=0:raise FreshnessHold('input_census_unknown')
        ready=select.select([fd,cancel],[],[],min(.05,remaining))[0]
        if cancel in ready:raise FreshnessHold('cleanup_uncertain')
        if fd in ready:
            if os.read(fd,4)!=b'go\n':raise FreshnessHold('cleanup_uncertain')
            return


def _control(endpoint,stage,value=0,descriptor=None):
    data=legacy.RESPONSE.pack(b'JARVCTL3',stage,value)
    rights=[] if descriptor is None else [(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[descriptor]))]
    endpoint.sendmsg([data],rights)


class RootRelay:
    def __init__(self,admission,*,library,timeout=30):
        if (os.geteuid()!=0 or type(admission) is not _Admission or admission.token is not _TOKEN
                or admission.owner!=os.getpid() or type(timeout) not in (int,float) or not 1<=timeout<=900):
            raise FreshnessHold('invalidation_unsafe')
        _source(admission.target,admission.binding,admission.command)
        self.admission=admission;self.root=admission.root;self.library=Path(library)
        info=self.library.lstat()
        if (self.library.resolve()!=self.library or not stat.S_ISREG(info.st_mode) or info.st_uid!=0
                or info.st_mode&0o022 or info.st_nlink!=1 or not 1<=info.st_size<=2*1024**2):raise FreshnessHold('authority_changed')
        self.timeout=timeout;self.path=self.root/'relay.sock';self.nonce=os.urandom(32)
        if len(os.fsencode(self.path))>=108:raise FreshnessHold('invalidation_unsafe')
        self.completed=[];self.errors=[];self._stop=threading.Event();self._slots=threading.BoundedSemaphore(4)
        self._threads=[];self._requests=0;self._children=set();self._lock=threading.Lock()

    def __enter__(self):
        os.chown(self.root,0,1001);self.root.chmod(0o710)
        (self.root/'units').mkdir(mode=0o700)
        self.listener=socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
        self.listener.bind(str(self.path));os.chown(self.path,0,1001);self.path.chmod(0o660)
        self.listener.listen(8);self.listener.settimeout(.05)
        self.thread=threading.Thread(target=self._accept,daemon=True);self.thread.start();return self

    def __exit__(self,*unused):
        self._stop.set();self.thread.join(timeout=2);self.listener.close()
        for thread in self._threads:thread.join(timeout=6)
        if self.thread.is_alive() or any(thread.is_alive() for thread in self._threads) or self._children:
            raise FreshnessHold('cleanup_uncertain')
        self.path.unlink()

    def config_fd(self):
        fd=os.memfd_create('ci-planner-client',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            body=legacy.CONFIG.pack(b'JARVCFG3',0,os.getpid(),int(self.timeout*1000),self.nonce,os.fsencode(self.path))
            if os.write(fd,body)!=len(body):raise OSError(errno.EIO,'client configuration write failed')
            fcntl.fcntl(fd,fcntl.F_ADD_SEALS,legacy.SEALS);return fd
        except BaseException:os.close(fd);raise

    def _accept(self):
        while not self._stop.is_set():
            try:connection,_=self.listener.accept()
            except socket.timeout:continue
            ordinal=self._requests;self._requests+=1
            if ordinal>=4096 or not self._slots.acquire(blocking=False):
                try:connection.sendall(event('error',errno.EBUSY))
                finally:connection.close()
                continue
            thread=threading.Thread(target=self._handle,args=(connection,ordinal),daemon=True)
            self._threads.append(thread);thread.start()

    def _receive(self,connection,descriptors):
        pid,uid,gid=struct.unpack('3i',connection.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
        if pid<=0 or uid!=1001 or gid!=1001:raise FreshnessHold('invalidation_unsafe')
        data,ancillary,flags,_=connection.recvmsg(legacy.REQUEST.size,socket.CMSG_SPACE(65*4),socket.MSG_CMSG_CLOEXEC)
        for level,kind,body in ancillary:
            if level!=socket.SOL_SOCKET or kind!=socket.SCM_RIGHTS:raise FreshnessHold('metadata_torn')
            values=array.array('i');values.frombytes(body[:len(body)-len(body)%4]);descriptors.extend(values)
        if len(data)!=legacy.REQUEST.size or flags&(socket.MSG_TRUNC|socket.MSG_CTRUNC):raise FreshnessHold('metadata_torn')
        magic,nonce,count,*numbers=legacy.REQUEST.unpack(data)
        if (magic!=b'JARVREQ3' or not hmac.compare_digest(nonce,self.nonce) or not 3<=count<=64
                or len(descriptors)!=count+1 or numbers[:3]!=[0,1,2] or any(numbers[count:])
                or len(set(numbers[:count]))!=count or any(not 3<=number<=65535 for number in numbers[3:count])):
            raise FreshnessHold('metadata_torn')
        _,_,environment,*_=payload(descriptors[0])
        staging.normalize_jobserver(environment,dict(zip(numbers[3:count],descriptors[4:])))
        return numbers[:count]

    def _handle(self,connection,ordinal):
        descriptors=[]
        try:
            connection.settimeout(self.timeout)
            numbers=self._receive(connection,descriptors);self._run(connection,ordinal,descriptors,numbers)
        except (OSError,ValueError,RuntimeError,struct.error,subprocess.SubprocessError,FreshnessHold,staging.StagingUnavailable):
            self.errors.append('protocol_or_original_cleanup_unverified')
            try:connection.sendall(event('error',errno.EINVAL))
            except OSError:pass
        finally:
            legacy._close(descriptors);connection.close();self._slots.release()

    def _run(self,connection,ordinal,descriptors,numbers):
        directory=self.root/'units'/str(uuid.uuid4());directory.mkdir(mode=0o700)
        control,child_control=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
        cancel,stop=os.pipe2(os.O_CLOEXEC);release,grant=os.pipe2(os.O_CLOEXEC);exec_release,exec_grant=os.pipe2(os.O_CLOEXEC)
        deadline=time.monotonic()+self.timeout
        value={'version':3,'kind':'unit','parent':os.getpid(),'target':str(self.admission.target),
            'binding':self.admission.binding,'command':self.admission.command,'directory':str(directory),
            'ordinal':ordinal,'deadline':deadline,'library':str(self.library),'numbers':numbers,
            'extras':descriptors[4:]}
        bootstrap=_sealed(value);child=None;pidfd=None;cancelled=False;ready=False;sealed=False;authorized=False;started=False
        signal_count=0;cancelled_at=None;control_eof=False;original_pid=None
        try:
            argv=[sys.executable,'-I',str(Path(__file__).resolve()),'--unit',
                *map(str,(bootstrap,descriptors[0],cancel,release,exec_release,child_control.fileno()))]
            child=subprocess.Popen(argv,stdin=descriptors[1],stdout=descriptors[2],stderr=descriptors[3],
                pass_fds=tuple([bootstrap,*descriptors,cancel,release,exec_release,child_control.fileno()]),
                env={'PATH':os.defpath,'JARVIS_SUPERVISOR_PARENT_PID':str(os.getpid())})
            with self._lock:self._children.add(child)
            os.close(bootstrap);bootstrap=None;child_control.close();legacy._close(descriptors);descriptors.clear()
            while True:
                if self._stop.is_set() or time.monotonic()>=deadline:cancelled=True
                readers=select.select([connection]+([] if control_eof else [control]),[],[],.02)[0]
                if connection in readers:
                    frame,extra,flags,_=connection.recvmsg(16,0)
                    if not frame:cancelled=True
                    else:
                        if extra or flags&(socket.MSG_TRUNC|socket.MSG_CTRUNC) or len(frame)!=16:raise ValueError('invalid client signal')
                        magic,number,reserved=legacy.RESPONSE.unpack(frame);signal_count+=1
                        if magic!=b'JARVSIG3' or reserved or number not in legacy.FORWARDED_SIGNALS or signal_count>4096:raise ValueError('invalid client signal')
                        if not started:cancelled=True
                        elif pidfd is not None:
                            try:signal.pidfd_send_signal(pidfd,number)
                            except ProcessLookupError:pass
                if control in readers:
                    frame,extra,flags,_=control.recvmsg(16,socket.CMSG_SPACE(4),socket.MSG_CMSG_CLOEXEC)
                    received=[]
                    for level,kind,body in extra:
                        if level!=socket.SOL_SOCKET or kind!=socket.SCM_RIGHTS:raise ValueError('invalid original control frame')
                        values=array.array('i');values.frombytes(body[:len(body)-len(body)%4]);received.extend(values)
                    if not frame and not extra and not flags:
                        control_eof=True
                        continue
                    if len(frame)!=16 or flags&(socket.MSG_TRUNC|socket.MSG_CTRUNC):
                        legacy._close(received);raise ValueError('incomplete original control frame')
                    magic,stage,child_pid=legacy.RESPONSE.unpack(frame)
                    if magic!=b'JARVCTL3':legacy._close(received);raise ValueError('foreign original control frame')
                    if stage==1 and not ready and len(received)==1 and child_pid>0:
                        pidfd=received.pop();signal.pidfd_send_signal(pidfd,0)
                        info=Path(f'/proc/self/fdinfo/{pidfd}').read_text().splitlines()
                        if next((line.split()[1] for line in info if line.startswith('Pid:')),None)!=str(child_pid):
                            raise FreshnessHold('cleanup_uncertain')
                        if int(Path(f'/proc/{child_pid}/stat').read_text().split()[3])!=child.pid:
                            raise FreshnessHold('cleanup_uncertain')
                        original_pid=child_pid
                        if not cancelled:connection.sendall(event('staging_ready',0))
                        ready=True
                    elif stage==2 and not sealed and not received:
                        observed=cache._read_record(directory/'stdin.json')
                        if (observed.get('command')!=self.admission.command or observed.get('binding')!=self.admission.binding
                                or observed.get('ordinal')!=ordinal or observed.get('shipping_authority') is not False):
                            raise FreshnessHold('input_census_unknown')
                        sealed=True
                    elif stage==3 and authorized and not started and not received and child_pid==original_pid:
                        if not cancelled:
                            connection.sendall(event('tool_started',0));os.write(exec_grant,b'go\n');started=True
                    else:legacy._close(received);raise ValueError('out-of-order original control frame')
                    legacy._close(received)
                if ready and sealed and not authorized and not cancelled:
                    # Do not grant an exec while an already queued client
                    # cancellation is waiting to be read at this boundary.
                    if not select.select([connection],[],[],0)[0]:os.write(grant,b'go\n');authorized=True
                if cancelled and child.poll() is None and cancelled_at is None:
                    os.write(stop,b'cancel');child.terminate();cancelled_at=time.monotonic()
                status=child.poll()
                if status is not None:break
                if cancelled_at is not None and time.monotonic()-cancelled_at>5:
                    raise FreshnessHold('cleanup_uncertain')
            record=cache._read_record(directory/'exit-status.json')
            cleanup=directory/'cleanup'
            info=cleanup.lstat()
            if (not stat.S_ISREG(info.st_mode) or info.st_uid!=0 or info.st_nlink!=1
                    or stat.S_IMODE(info.st_mode)!=0o600 or cleanup.read_bytes()!=b'all-descendants-reaped\n'
                    or record.get('command')!=self.admission.command or record.get('binding')!=self.admission.binding
                    or record.get('ordinal')!=ordinal or type(record.get('returncode')) is not int
                    or not -64<=record['returncode']<=255):raise FreshnessHold('cleanup_uncertain')
            raw=record['returncode']
            if status!=(raw if raw>=0 else 128-raw):raise FreshnessHold('cleanup_uncertain')
            self.completed.append({'ordinal':ordinal,'uid':2000+ordinal,'started':started,
                'cancelled':cancelled,'returncode':raw,'original_cleanup':True,
                'input_closure_complete':False,'shipping_authority':False})
            if not cancelled:
                if not started or record.get('exec_observed') is not True:connection.sendall(event('error',errno.EIO))
                else:connection.sendall(event('signaled' if raw<0 else 'finished',abs(raw)))
        finally:
            if child is not None:
                if child.poll() is None:child.terminate();child.wait(timeout=6)
                with self._lock:self._children.discard(child)
            legacy._close([cancel,stop,release,grant,exec_release,exec_grant]+([] if bootstrap is None else [bootstrap]))
            control.close();child_control.close()
            if pidfd is not None:os.close(pidfd)


def _unit(arguments):
    global _PHASE
    _PHASE='planner_unit'
    bootstrap,data,cancel,release,exec_release,control=map(int,arguments)
    value=_bootstrap(bootstrap,'unit');_parent_guard(value['parent'])
    descriptors=[bootstrap,data,cancel,release,exec_release,control,*value['extras']]
    for fd in descriptors:os.set_inheritable(fd,False)
    stage=dict(value,kind='stage',parent=os.getpid());stage_bootstrap=_sealed(stage)
    stage_identity=os.fstat(stage_bootstrap)
    endpoint=socket.socket(fileno=control);observed=False
    supervisor=_load('planner_original_supervisor','provider-supervisor.py')
    def spawned(child):
        pidfd=os.pidfd_open(child.pid)
        try:_control(endpoint,1,child.pid,pidfd)
        finally:os.close(pidfd)
    def executed(child):
        nonlocal observed
        status=Path(f'/proc/{child.pid}/status').read_text()
        expected=2000+value['ordinal']
        for label in ('Uid:','Gid:'):
            row=next(line for line in status.splitlines() if line.startswith(label))
            if [int(number) for number in row.split()[1:]]!=[expected]*4:raise FreshnessHold('invalidation_unsafe')
        _control(endpoint,3,child.pid);_release(exec_release,cancel,value['deadline']);observed=True
    try:
        forwarded=tuple([stage_bootstrap,data,cancel,release,control,*value['extras']])
        argv=[sys.executable,'-I',str(Path(__file__).resolve()),'--stage',*map(str,(stage_bootstrap,data,cancel,release,control))]
        raw=supervisor.supervise(Path(value['directory'])/'cleanup',argv,forward_fds=forwarded,
            close_after_spawn=(stage_bootstrap,),return_raw_status=True,on_spawn=spawned,on_exec=executed)
        stage_bootstrap=None
        cache._write_record(Path(value['directory'])/'exit-status.json',{'version':3,'binding':value['binding'],
            'command':value['command'],'ordinal':value['ordinal'],'returncode':raw,'exec_observed':observed})
        return raw if raw>=0 else 128-raw
    finally:
        if stage_bootstrap is not None:
            try:
                current=os.fstat(stage_bootstrap)
                if (current.st_dev,current.st_ino)==(stage_identity.st_dev,stage_identity.st_ino):os.close(stage_bootstrap)
            except OSError as error:
                if error.errno!=errno.EBADF:raise
        endpoint.close()


def _stage(arguments):
    global _PHASE
    bootstrap,data,cancel,release,control=map(int,arguments)
    value=_bootstrap(bootstrap,'stage');_parent_guard(value['parent'])
    if type(value['ordinal']) is not int or not 0<=value['ordinal']<4096:raise FreshnessHold('invalidation_unsafe')
    for fd in (bootstrap,data,cancel,release,control,*value['extras']):os.set_inheritable(fd,False)
    file,argv,environment,cwd,search,ignored,blocked=payload(data)
    environment,mapping,jobserver=staging.normalize_jobserver(environment,dict(zip(value['numbers'][3:],value['extras'])))
    _PHASE='planner_capture';captured=staging.capture(0,cancel,command_deadline=value['deadline'])
    observation={'version':3,'binding':value['binding'],'command':value['command'],'ordinal':value['ordinal'],
        'stdin':captured.observation(),'jobserver':jobserver,'shipping_authority':False}
    cache._write_record(Path(value['directory'])/'stdin.json',observation)
    with socket.socket(fileno=control) as endpoint:_control(endpoint,2)
    _PHASE='planner_authorize';_release(release,cancel,value['deadline'])
    cancel=fcntl.fcntl(cancel,fcntl.F_DUPFD_CLOEXEC,128)
    saved=[(target,fcntl.fcntl(source,fcntl.F_DUPFD_CLOEXEC,128)) for target,source in mapping]
    os.dup2(captured.fileno(),0)
    for target,source in saved:os.dup2(source,target,inheritable=True);os.close(source)
    environment.pop(b'JARVIS_CARGO_RELAY_FD',None)
    if b'LD_PRELOAD' in environment:
        libraries=[part for part in environment[b'LD_PRELOAD'].replace(b':',b' ').split() if part!=os.fsencode(value['library'])]
        if libraries:environment[b'LD_PRELOAD']=b' '.join(libraries)
        else:environment.pop(b'LD_PRELOAD')
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    _PHASE='planner_drop'
    uid=2000+value['ordinal'];os.setgroups([]);os.setgid(uid);os.setuid(uid);os.umask(0o077)
    _parent_guard(value['parent'])  # setuid/setgid clear the previous guard.
    os.chdir(cwd)
    for number in legacy.FORWARDED_SIGNALS:
        signal.signal(number,signal.SIG_IGN if ignored&(1<<(number-1)) else signal.SIG_DFL)
    signal.pthread_sigmask(signal.SIG_SETMASK,[number for number in legacy.FORWARDED_SIGNALS if blocked&(1<<(number-1))])
    if select.select([cancel],[],[],0)[0]:raise FreshnessHold('cleanup_uncertain')
    _PHASE='planner_arm_exec'
    if libc.ptrace(0,0,None,None)!=0:raise OSError(ctypes.get_errno(),'unit exec observation unavailable')
    os.kill(os.getpid(),signal.SIGSTOP)
    _PHASE='planner_unit_exec'
    if search is None or b'/' in file:os.execve(file,argv,environment)
    denied=False
    for directory in search.split(b':'):
        try:os.execve(os.path.join(directory or b'.',file),argv,environment)
        except OSError as error:
            if error.errno==errno.EACCES:denied=True
            elif error.errno not in (errno.ENOENT,errno.ENOTDIR):raise
    raise OSError(errno.EACCES if denied else errno.ENOENT,'unit exec unavailable')


_PLANNER_PROBE=r'''
import ctypes,errno,hashlib,json,os,signal,sys
mode,metadata,socket_path,expected_uid=sys.argv[1:];expected_uid=int(expected_uid)
assert os.getresuid()==(1001,1001,1001) and os.getresgid()==(1001,1001,1001) and os.getgroups()==[]
with open(metadata,'a') as output:output.write('planner\n')
libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
argv=(ctypes.c_char_p*2)(b'true',None);environment=(ctypes.c_char_p*1)(None)
assert libc.syscall(ctypes.c_long(59),ctypes.c_char_p(b'/usr/bin/true'),argv,environment)==-1
assert ctypes.get_errno()==errno.EPERM
body=r"""import ctypes,errno,hashlib,json,os,socket,sys
metadata,socket_path,expected=sys.argv[1:];expected=int(expected)
assert os.getresuid()==(expected,expected,expected) and os.getresgid()==(expected,expected,expected) and os.getgroups()==[]
assert ctypes.CDLL(None).prctl(39,0,0,0,0)==1
try:open(metadata,'a').write('bad')
except OSError as error:assert error.errno in (errno.EACCES,errno.EPERM)
else:raise AssertionError('unit wrote planner metadata')
with socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET) as endpoint:
 try:endpoint.connect(socket_path)
 except OSError as error:assert error.errno in (errno.EACCES,errno.EPERM)
 else:raise AssertionError('unit reached root planner socket')
flags=os.environ.get('CARGO_MAKEFLAGS')
if flags:
 assert flags=='-j --jobserver-fds=3,4 --jobserver-auth=3,4'
 assert os.read(3,1)==b'+';os.write(4,b'+')
fds=[]
for raw in os.listdir('/proc/self/fd'):
 try:os.fstat(int(raw))
 except OSError:continue
 fds.append(int(raw))
assert sorted(fds)==([0,1,2,3,4] if flags else [0,1,2]),fds
assert os.environb[b'RAW']==b'\xffvalue'
print(json.dumps({'uid':expected,'stdin_sha256':hashlib.sha256(sys.stdin.buffer.read()).hexdigest(),
 'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':bool(flags)}),flush=True)
"""
source_read,source_write=os.pipe2(os.O_CLOEXEC);output_read,output_write=os.pipe2(os.O_CLOEXEC)
start_read,start_write=os.pipe2(os.O_CLOEXEC)
jobs=os.pipe2(os.O_CLOEXEC) if mode=='jobserver' else ()
if jobs:os.write(jobs[1],b'+')
child=os.fork()
if child==0:
 try:
  os.dup2(source_read,0);os.dup2(output_write,1)
  for fd in jobs:os.set_inheritable(fd,True)
  env=dict(os.environb);env[b'RAW']=b'\xffvalue'
  if jobs:env[b'CARGO_MAKEFLAGS']=('-j --jobserver-fds=%d,%d --jobserver-auth=%d,%d'%(*jobs,*jobs)).encode()
  os.execve(b'/usr/bin/python3',[b'python3',b'-I',b'-c',body.encode(),os.fsencode(metadata),os.fsencode(socket_path),str(expected_uid).encode()],env)
 except BaseException:os._exit(126)
os._exit(127)
os.close(source_read);os.close(output_write);os.close(start_write)
assert os.read(start_read,1)==b'';os.close(start_read)
data=b'\xffstdin'*200000 if mode!='cancel' else b''
if mode=='cancel':os.kill(child,signal.SIGUSR1)
else:
 view=memoryview(data)
 while view:view=view[os.write(source_write,view):]
 os.close(source_write);source_write=None
with os.fdopen(output_read,'rb') as output:captured=output.read(8193)
_,status=os.waitpid(child,0);code=os.waitstatus_to_exitcode(status)
if source_write is not None:os.close(source_write)
if mode=='cancel':
 assert code==125 and captured==b'',(code,captured)
 result={'cancelled_before_exec':True,'proxy_exit':code}
else:
 assert code==0,(code,captured)
 result=json.loads(captured);assert result['stdin_sha256']==hashlib.sha256(data).hexdigest()
 if jobs:assert os.read(jobs[0],1)==b'+'
for fd in jobs:os.close(fd)
print(json.dumps(result),flush=True)
'''


def _planner(arguments):
    global _PHASE
    _PHASE='planner_client_start'
    bootstrap,configuration=map(int,arguments)
    value=_bootstrap(bootstrap,'planner');_parent_guard(value['parent'])
    if value.get('mode') not in ('stdin','jobserver','cancel'):raise FreshnessHold('metadata_torn')
    os.close(bootstrap)
    environment={'PATH':'/usr/bin:/bin','HOME':'/tmp','LD_PRELOAD':value['library'],
        'JARVIS_CARGO_RELAY_FD':str(configuration)}
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    os.setgroups([]);os.setgid(1001);os.setuid(1001);_parent_guard(value['parent'])
    os.set_inheritable(configuration,True)
    _PHASE='planner_client_exec'
    os.execve('/usr/bin/python3',['python3','-I','-c',_PLANNER_PROBE,value['mode'],
        value['metadata'],value['socket'],str(value['expected_uid'])],environment)


def _probe(arguments):
    global _PHASE,_CHILD_DIAGNOSTIC
    value=_bootstrap(int(arguments[0]),'probe');_parent_guard(value['parent'])
    root=Path(value['root']);admission=admit(value['target'],value['binding'],value['command'],root)
    results=[]
    with RootRelay(admission,library=value['library'],timeout=30) as relay:
        metadata=root/'planner-metadata';metadata.write_text('');os.chown(metadata,1001,1001);metadata.chmod(0o600)
        for ordinal,mode in enumerate(('stdin','jobserver','cancel')):
            _PHASE='planner_probe_'+mode
            client=relay.config_fd();child_bootstrap=None;child=None
            try:
                config=dict(value,kind='planner',parent=os.getpid(),deadline=time.monotonic()+30,
                    mode=mode,metadata=str(metadata),socket=str(relay.path),expected_uid=2000+ordinal)
                child_bootstrap=_sealed(config)
                child=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--planner',str(child_bootstrap),str(client)],
                    pass_fds=(child_bootstrap,client),env={'PATH':os.defpath},stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                output,error=child.communicate(timeout=35)
                if child.returncode!=0 or len(output)>8192 or len(error)>8192:
                    for line in error[:8192].splitlines():
                        try:decoded=json.loads(line)
                        except ValueError:continue
                        if type(decoded) is dict and 'planner_diagnostic' in decoded:
                            _CHILD_DIAGNOSTIC=diagnostic(decoded['planner_diagnostic'])
                    raise FreshnessHold('invalidation_unsafe')
                results.append(json.loads(output))
            finally:
                if child is not None and child.poll() is None:child.kill();child.wait(timeout=5)
                os.close(client)
                if child_bootstrap is not None:os.close(child_bootstrap)
        if len(relay.completed)!=3 or relay.errors:raise FreshnessHold('cleanup_uncertain')
        for ordinal,completed in enumerate(relay.completed):
            if (completed['ordinal']!=ordinal or completed['uid']!=2000+ordinal
                    or completed['started']!=(ordinal!=2) or completed['cancelled']!=(ordinal==2)
                    or completed['original_cleanup'] is not True):raise FreshnessHold('cleanup_uncertain')
    print(json.dumps({'version':3,'kind':'ci-planner-protocol-probe','binding':value['binding'],
        'command':value['command'],'cases':results,'units':relay.completed,
        'input_closure_complete':False,'cargo_metadata_isolation_established':False,'shipping_authority':False}))
    return 0


def _probe_supervisor(arguments):
    global _PHASE
    _PHASE='planner_probe_supervisor'
    value=_bootstrap(int(arguments[0]),'probe-supervisor');_parent_guard(value['parent'])
    probe=_sealed(dict(value,kind='probe',parent=os.getpid()))
    supervisor=_load('original_planner_probe_supervisor','provider-supervisor.py')
    try:
        return supervisor.supervise(value['receipt'],[sys.executable,'-I',str(Path(__file__).resolve()),'--probe',str(probe)],
            forward_fds=(probe,),close_after_spawn=(probe,))
    finally:
        # The supervisor closes the original bootstrap immediately on spawn;
        # process exit owns any exceptional pre-spawn descriptor cleanup.
        pass


def validate_probe(value,binding,command,workers,writer,source_projection):
    keys={'version','kind','binding','command','cases','units','guest_worker_quiescence',
        'input_closure_complete','cargo_metadata_isolation_established','shipping_authority'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=3
            or value['kind']!='ci-planner-protocol-probe' or value['binding']!=binding or value['command']!=command
            or value['input_closure_complete'] is not False or value['cargo_metadata_isolation_established'] is not False
            or value['shipping_authority'] is not False or type(value['cases']) is not list or len(value['cases'])!=3
            or type(value['units']) is not list or len(value['units'])!=3):raise FreshnessHold('input_census_unknown')
    scope=value['guest_worker_quiescence']
    if (scope not in workers['scopes'] or scope==writer['guest_worker_quiescence']
            or any(scope==item['scope'] for item in source_projection['access'])):raise FreshnessHold('cleanup_uncertain')
    expected=hashlib.sha256(b'\xffstdin'*200000).hexdigest()
    for ordinal,unit in enumerate(value['units']):
        if (type(unit) is not dict or set(unit)!={'ordinal','uid','started','cancelled','returncode',
                'original_cleanup','input_closure_complete','shipping_authority'}
                or type(unit['ordinal']) is not int or unit['ordinal']!=ordinal or type(unit['uid']) is not int
                or unit['uid']!=2000+ordinal or unit['started'] is not (ordinal!=2)
                or unit['cancelled'] is not (ordinal==2) or unit['original_cleanup'] is not True
                or type(unit['returncode']) is not int or (ordinal!=2 and unit['returncode']!=0)
                or unit['input_closure_complete'] is not False or unit['shipping_authority'] is not False):
            raise FreshnessHold('cleanup_uncertain')
        case=value['cases'][ordinal]
        if ordinal==2:
            if case!={'cancelled_before_exec':True,'proxy_exit':125}:raise FreshnessHold('input_census_unknown')
        elif (type(case) is not dict or set(case)!={'uid','stdin_sha256','metadata_denied','root_socket_denied',
                'control_fds_closed','jobserver'} or type(case['uid']) is not int or case['uid']!=2000+ordinal
                or case['stdin_sha256']!=expected or case['metadata_denied'] is not True
                or case['root_socket_denied'] is not True or case['control_fds_closed'] is not True
                or case['jobserver'] is not (ordinal==1)):raise FreshnessHold('input_census_unknown')
    return value


if __name__=='__main__':
    try:
        if os.geteuid()!=0:raise FreshnessHold('invalidation_unsafe')
        if sys.argv[1:2]==['--unit']:result=_unit(sys.argv[2:])
        elif sys.argv[1:2]==['--stage']:result=_stage(sys.argv[2:])
        elif sys.argv[1:2]==['--planner']:result=_planner(sys.argv[2:])
        elif sys.argv[1:2]==['--probe']:result=_probe(sys.argv[2:])
        elif sys.argv[1:2]==['--probe-supervisor']:result=_probe_supervisor(sys.argv[2:])
        else:raise FreshnessHold('invalidation_unsafe')
    except BaseException as error:
        print(json.dumps({'planner_diagnostic':_CHILD_DIAGNOSTIC or diagnostic({
            'stage':_PHASE,'category':type(error).__name__,'errno':getattr(error,'errno',None)})}),file=sys.stderr)
        result=125
    raise SystemExit(result)
