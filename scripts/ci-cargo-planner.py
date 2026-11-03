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
import re
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
unit_views=_load('planner_views','ci-cargo-views.py')
kernel_observer=_load('planner_kernel_observer','ci-cargo-observer.py')
unit_views.FreshnessHold=cache.FreshnessHold
import shutil
FreshnessHold=cache.FreshnessHold
STAGES={'staging_ready':1,'tool_started':2,'finished':3,'error':4,'signaled':5}
_TOKEN=object()
_PHASE='planner_startup'
_CHILD_DIAGNOSTIC=None


def diagnostic(value):
    phases={'planner_startup','planner_unit','planner_capture','planner_authorize','planner_drop',
        'planner_arm_exec','planner_unit_exec','planner_client_start','planner_client_exec',
        'planner_probe_stdin','planner_probe_jobserver','planner_probe_cancel','planner_probe_signal','planner_probe_supervisor',
        'planner_probe_credentials','planner_probe_filter','planner_probe_exec_start','planner_probe_stream','planner_probe_wait','planner_probe_status','planner_request_receive','planner_request_cleanup','planner_tool_credentials','planner_tool_metadata','planner_tool_socket','planner_tool_jobserver','planner_tool_fds','planner_tool_environment','planner_tool_stdin','planner_probe_view','planner_tool_view','planner_view_prepare','planner_view_enter','planner_view_namespace','planner_view_runtime','planner_view_source','planner_view_inputs','planner_view_proc','planner_view_devices','planner_view_root','planner_view_publish','planner_cargo_request','planner_cargo_artifact','planner_cargo_runtime','planner_cargo_execute','planner_cargo_compile','planner_cargo_build_script','planner_cargo_application','planner_kernel_observer'}
    categories={'OSError','ValueError','KeyError','FreshnessHold','StagingUnavailable','RuntimeError','TimeoutExpired','CalledProcessError','Unavailable'}
    value=value if type(value) is dict else {}
    return {'stage':value.get('stage') if value.get('stage') in phases else 'planner_startup',
        'category':value.get('category') if value.get('category') in categories else 'Unavailable',
        'errno':value.get('errno') if type(value.get('errno')) is int and 0<=value['errno']<=4095 else None}


def failed_probe_diagnostic(error,request_errors):
    selected=None
    for line in error[:8192].splitlines():
        try:value=json.loads(line)
        except ValueError:continue
        if type(value) is dict and type(value.get('planner_diagnostic')) is dict:
            candidate=diagnostic(value['planner_diagnostic'])
            if selected is None or candidate['stage']!='planner_probe_status':selected=candidate
    if request_errors and (selected is None or selected['stage']=='planner_probe_status'):
        selected=diagnostic(request_errors[0])
    return selected


def guarded_probe(source):
    # This is only the fixed, measured probe body. No exception text, command
    # arguments or private payload is included in its bounded failure output.
    return ("import json,sys\n_PROBE_PHASE='planner_probe_credentials'\ntry:\n"
        + ' exec(compile('+repr(source)+',\"fixed-planner-probe\",\"exec\"),globals())\n'
        + "except BaseException as error:\n"
          " category='OSError' if isinstance(error,OSError) else 'Unavailable'\n"
          " number=getattr(error,'errno',None)\n"
          " number=number if type(number) is int and 0<=number<=4095 else None\n"
          " print(json.dumps({'planner_diagnostic':{'stage':_PROBE_PHASE,'category':category,'errno':number}}),file=sys.stderr)\n"
          " sys.exit(125)\n")


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


def receive_control(endpoint):
    frame,extra,flags,_=endpoint.recvmsg(16,socket.CMSG_SPACE(4),socket.MSG_CMSG_CLOEXEC)
    received=[]
    try:
        for level,kind,body in extra:
            if level!=socket.SOL_SOCKET or kind!=socket.SCM_RIGHTS or len(body)%4:
                raise ValueError('invalid original control frame')
            values=array.array('i');values.frombytes(body);received.extend(values)
        if not frame and not extra and not flags&~socket.MSG_CMSG_CLOEXEC:return None
        if len(frame)!=16 or flags&~socket.MSG_CMSG_CLOEXEC or len(received)>1:
            raise ValueError('incomplete original control frame')
        magic,stage,child_pid=legacy.RESPONSE.unpack(frame)
        if magic!=b'JARVCTL3' or stage not in (1,2,3):raise ValueError('foreign original control frame')
        return stage,child_pid,received
    except BaseException:legacy._close(received);raise


def fixture_out_dir(environment):
    value=environment.get(b'OUT_DIR')
    if value is None:return None
    if re.fullmatch(rb'/workspace/target/debug/build/private-cargo-probe-[0-9a-f]{16}/out',value) is None:
        raise FreshnessHold('input_census_unknown')
    return os.fsdecode(value.removeprefix(b'/workspace/target/'))


def fixture_request(request):
    """Map the pinned Cargo build-script/library/binary graph; unknown work holds.

    This is an execution mapping experiment, not generic unit discovery or an
    input-closure authority. Preserve the original command and its virtual paths.
    """
    file,argv,environment,cwd,search,_ignored,_blocked=request
    # POSIX spawnp/execvp preserve their search request for files containing '/'.
    # Such files ignore PATH; keep the original executable and payload untouched.
    if cwd!=b'/workspace' or not argv or argv[0]!=file:
        raise FreshnessHold('input_census_unknown')
    # cargo_run.rs at Cargo commit29ea6fb6a strips the caller's cwd prefix.
    # Both literal paths resolve to this one planned mount under /workspace.
    if file in (b'/workspace/target/debug/private-cargo-probe',b'target/debug/private-cargo-probe') and len(argv)==1:
        return {'kind':'application','alias':'debug/private-cargo-probe'}
    if re.fullmatch(rb'/workspace/target/debug/build/private-cargo-probe-[0-9a-f]{16}/build-script-build',file) and len(argv)==1:
        generated=fixture_out_dir(environment)
        if generated is None:raise FreshnessHold('input_census_unknown')
        return {'kind':'build_script','alias':os.fsdecode(file.removeprefix(b'/workspace/target/')),
            'generated_root':generated}
    if file!=b'/toolchain/bin/rustc':raise FreshnessHold('input_census_unknown')
    args=argv[1:]
    if args in ([b'-vV'],[b'--version']):return {'kind':'information'}
    if args and args[0]==b'-':
        # Cargo's target-information probe consumes stdin and emits no artifacts.
        words={b'-',b'--crate-name',b'___',b'--crate-type',b'bin',b'rlib',b'dylib',
            b'cdylib',b'staticlib',b'proc-macro',b'-Wwarnings',b'--print=file-names',
            b'--print=sysroot',b'--print=split-debuginfo',b'--print=crate-name',b'--print=cfg'}
        if all(arg in words for arg in args) and any(arg.startswith(b'--print=') for arg in args):
            return {'kind':'information'}
        raise FreshnessHold('input_census_unknown')
    fields={};codegen={};source=None;index=0
    while index<len(args):
        arg=args[index];index+=1
        if arg.startswith((b'--error-format=',b'--json=')):
            key,_,value=arg.partition(b'=')
            if key in fields:raise FreshnessHold('input_census_unknown')
            fields[key]=value;continue
        if arg in (b'src/main.rs',b'src/lib.rs',b'build.rs') and source is None:source=arg;continue
        if arg in (b'--edition=2021',b'--emit=dep-info,link',b'--emit=dep-info,metadata,link'):
            if arg in fields:raise FreshnessHold('input_census_unknown')
            fields[arg]=True;continue
        if arg in (b'--crate-name',b'--crate-type',b'--out-dir',b'-L',b'-C',b'--check-cfg',b'--error-format',b'--json',b'--extern'):
            if index>=len(args):raise FreshnessHold('input_census_unknown')
            value=args[index];index+=1
            if arg==b'-C':
                key,separator,number=value.partition(b'=')
                if (not separator or key in codegen or key not in (b'embed-bitcode',b'debuginfo',b'metadata',b'extra-filename',b'incremental',b'strip')
                        or key==b'incremental'):
                    raise FreshnessHold('input_census_unknown')
                if (key==b'embed-bitcode' and number!=b'no' or key==b'debuginfo' and number!=b'0'
                        or key==b'strip' and number!=b'debuginfo'
                        or key==b'metadata' and re.fullmatch(b'[0-9a-f]{16}',number) is None
                        or key==b'extra-filename' and re.fullmatch(b'-[0-9a-f]{16}',number) is None):
                    raise FreshnessHold('input_census_unknown')
                codegen[key]=number
            elif arg==b'--check-cfg':
                if value not in (b'cfg(docsrs,test)',b'cfg(feature, values())'):
                    raise FreshnessHold('input_census_unknown')
            else:
                if arg in fields:raise FreshnessHold('input_census_unknown')
                fields[arg]=value
            continue
        raise FreshnessHold('input_census_unknown')
    if source not in (b'src/main.rs',b'src/lib.rs',b'build.rs'):raise FreshnessHold('input_census_unknown')
    role={b'src/main.rs':'binary',b'src/lib.rs':'library',b'build.rs':'build-script'}[source]
    if b'extra-filename' not in codegen or b'metadata' not in codegen:raise FreshnessHold('input_census_unknown')
    suffix=codegen[b'extra-filename']
    out=(b'/workspace/target/debug/build/private-cargo-probe'+suffix
        if role=='build-script' else b'/workspace/target/debug/deps')
    emit=b'--emit=dep-info,metadata,link' if role=='library' else b'--emit=dep-info,link'
    required={b'--crate-name':b'build_script_build' if role=='build-script' else b'private_cargo_probe',
        b'--crate-type':b'lib' if role=='library' else b'bin',
        b'--out-dir':out,b'-L':b'dependency=/workspace/target/debug/deps',b'--edition=2021':True,emit:True}
    if (not source or any(fields.get(key)!=value for key,value in required.items())
            or (b'--emit=dep-info,link' in fields and b'--emit=dep-info,metadata,link' in fields)
            or fields.get(b'--error-format',b'json')!=b'json'
            or fields.get(b'--json',b'diagnostic-rendered-ansi,artifacts,future-incompat')!=b'diagnostic-rendered-ansi,artifacts,future-incompat'):
        raise FreshnessHold('input_census_unknown')
    prefix=os.fsdecode(out.removeprefix(b'/workspace/target/'))+'/'
    name=os.fsdecode(required[b'--crate-name']+suffix);base=prefix+name
    outputs=([prefix+'lib'+name+'.rlib',prefix+'lib'+name+'.rmeta',base+'.d']
        if role=='library' else [base,base+'.d'])
    dependencies=[]
    if b'--extern' in fields:
        if role!='binary' or re.fullmatch(rb'private_cargo_probe=/workspace/target/debug/deps/libprivate_cargo_probe-[0-9a-f]{16}\.rlib',fields[b'--extern']) is None:
            raise FreshnessHold('input_census_unknown')
        dependencies=[os.fsdecode(fields[b'--extern'].split(b'=',1)[1].removeprefix(b'/workspace/target/'))]
    generated=fixture_out_dir(environment)
    if role=='library' and generated is None or role=='build-script' and generated is not None:
        raise FreshnessHold('input_census_unknown')
    return {'kind':'compile','role':role,'outputs':outputs,'dependencies':dependencies,'generated_root':generated}


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
        self.views=None;self.completed=[];self.errors=[];self._stop=threading.Event();self._slots=threading.BoundedSemaphore(4)
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
        unit_views.validate_stdio(descriptors[1:4])
        _,_,environment,*_=payload(descriptors[0])
        staging.normalize_jobserver(environment,dict(zip(numbers[3:count],descriptors[4:])))
        return numbers[:count]

    def _handle(self,connection,ordinal):
        descriptors=[];phase=['planner_request_receive']
        try:
            connection.settimeout(self.timeout)
            numbers=self._receive(connection,descriptors);phase[0]='planner_unit'
            self._run(connection,ordinal,descriptors,numbers,phase)
        except (OSError,ValueError,RuntimeError,struct.error,subprocess.SubprocessError,FreshnessHold,staging.StagingUnavailable) as error:
            self.errors.append(diagnostic({'stage':getattr(error,'unit_view_stage',phase[0]),
                'category':type(error).__name__,'errno':getattr(error,'errno',None)}))
            try:connection.sendall(event('error',errno.EINVAL))
            except OSError:pass
        finally:
            legacy._close(descriptors);connection.close();self._slots.release()

    def _run(self,connection,ordinal,descriptors,numbers,phase):
        directory=self.root/'units'/str(uuid.uuid4());directory.mkdir(mode=0o700)
        control,child_control=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
        cancel,stop=os.pipe2(os.O_CLOEXEC);release,grant=os.pipe2(os.O_CLOEXEC);exec_release,exec_grant=os.pipe2(os.O_CLOEXEC)
        deadline=time.monotonic()+self.timeout
        value={'version':3,'kind':'unit','parent':os.getpid(),'target':str(self.admission.target),
            'binding':self.admission.binding,'command':self.admission.command,'directory':str(directory),
            'ordinal':ordinal,'deadline':deadline,'library':str(self.library),'numbers':numbers,
            'extras':descriptors[4:]}
        view=None;bootstrap=None;child=None;pidfd=None;cancelled=False;ready=False;sealed=False;authorized=False;started=False
        signal_count=0;cancelled_at=None;control_eof=False;original_pid=None
        try:
            if self.views is not None:
                phase[0]='planner_view_prepare';view=self.views.prepare(ordinal,payload(descriptors[0]))
                if view is not None:value['view']=view
            bootstrap=_sealed(value)
            argv=[sys.executable,'-I',str(Path(__file__).resolve()),'--unit',
                *map(str,(bootstrap,descriptors[0],cancel,release,exec_release,child_control.fileno()))]
            child=subprocess.Popen(argv,stdin=descriptors[1],stdout=descriptors[2],stderr=descriptors[3],
                pass_fds=tuple([bootstrap,*descriptors,cancel,release,exec_release,child_control.fileno()]),
                env={'PATH':os.defpath,'JARVIS_SUPERVISOR_PARENT_PID':str(os.getpid())})
            with self._lock:self._children.add(child)
            os.close(bootstrap);bootstrap=None;child_control.close();legacy._close(descriptors);descriptors.clear()
            phase[0]='planner_capture'
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
                    message=receive_control(control)
                    if message is None:
                        control_eof=True
                        continue
                    stage,child_pid,received=message
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
                        sealed=True;phase[0]='planner_authorize'
                    elif stage==3 and authorized and not started and not received and child_pid==original_pid:
                        if not cancelled:
                            connection.sendall(event('tool_started',0));os.write(exec_grant,b'go\n');started=True;phase[0]='planner_unit_exec'
                    else:legacy._close(received);raise ValueError('out-of-order original control frame')
                    legacy._close(received)
                if ready and sealed and not authorized and not cancelled:
                    # Do not grant an exec while an already queued client
                    # cancellation is waiting to be read at this boundary.
                    if not select.select([connection],[],[],0)[0]:os.write(grant,b'go\n');authorized=True
                if cancelled and child.poll() is None and cancelled_at is None:
                    os.write(stop,b'cancel');child.terminate();cancelled_at=time.monotonic()
                if view is not None and started:self.views.running(view)
                status=child.poll()
                if status is not None:break
                if cancelled_at is not None and time.monotonic()-cancelled_at>5:
                    raise FreshnessHold('cleanup_uncertain')
            phase[0]='planner_request_cleanup'
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
            if view is not None:
                phase[0]='planner_view_publish';self.views.finish(view,directory,raw)
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
        if 'view' in value:
            namespaces={name:{'controller':os.readlink('/proc/self/ns/'+name),
                'unit':os.readlink(f'/proc/{child.pid}/ns/'+name)} for name in ('mnt','ipc','net')}
            if any(row['controller']==row['unit'] for row in namespaces.values()):raise FreshnessHold('invalidation_unsafe')
            cache._write_record(Path(value['directory'])/'view-namespace.json',{'version':1,
                'binding':value['binding'],'command':value['command'],'ordinal':value['ordinal'],
                'pid':child.pid,'namespaces':namespaces})
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
    if 'view' in value:
        _PHASE='planner_view_enter';view=value['view']
        mounts=cache._read_record(Path(view['mounts']),max_bytes=16*1024**2)
        if (mounts.get('binding')!=value['binding'] or mounts.get('command')!=value['command']
                or mounts.get('ordinal')!=value['ordinal']):raise FreshnessHold('generation_mismatch')
        unit_views.enter(view['root'],2000+value['ordinal'],view['source'],view['artifacts'],mounts['entries'])
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


class _ViewProbe:
    """Fixed native filesystem experiment, never a reusable producer record."""
    def __init__(self,admission):
        if (type(admission) is not _Admission or admission.token is not _TOKEN or admission.owner!=os.getpid()
                or os.getresuid()!=(0,0,0)):raise FreshnessHold('invalidation_unsafe')
        _source(admission.target,admission.binding,admission.command)
        self.admission=admission;self.owner=os.getpid();self.advanced=False;self.receipt=None
        self.root=admission.target/('.unit-view-probe-'+str(uuid.uuid4()));self.root.mkdir(mode=0o700)
        self.control={'version':1,'kind':'private-unit-view-probe','binding':admission.binding,
            'command':admission.command,'shipping_authority':False}
        cache._write_record(self.root/'control.json',{**self.control,'state':'preparing'})
        self.source=admission.target.parent/'source-projection'
        projected=cache._projection_manifest(cache._read_record(admission.target.parent/'source-projection.json',
            max_bytes=16*1024**2),admission.binding)
        if projected['state']!='sealed' or cache._projection_inventory(self.source,sealed=True)!=projected['entries']:
            raise FreshnessHold('input_census_unknown')
        self.inputs=self.root/'inputs-v1';self._inputs(self.inputs,b'OLD')
        self.before=unit_views.inventory(self.inputs,owner=0,sealed=True)
        self.input_stat=(self.inputs/'deps/input').stat()
        self.abstract=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM|socket.SOCK_CLOEXEC)
        self.abstract.bind(b'\0jarvis-unit-view-probe');self.abstract.listen(1)
        if Path('/tmp/ci-unit-probe-private-marker').exists():raise FreshnessHold('invalidation_unsafe')

    @staticmethod
    def _inputs(root,data):
        root.mkdir(mode=0o700);(root/'deps').mkdir(mode=0o700)
        (root/'deps/input').write_bytes(data);(root/'deps/input').chmod(0o444)
        (root/'deps').chmod(0o555);root.chmod(0o555)

    def prepare(self,ordinal,request=None):
        if self.owner!=os.getpid() or ordinal!=4:raise FreshnessHold('invalidation_unsafe')
        self.slot=self.root/'unit';self.slot.mkdir(mode=0o700)
        artifacts=self.slot/'artifacts'
        entries=unit_views.clone_inputs(self.inputs,artifacts,self.before,owner=0,unit=2004)
        root=self.slot/'root';root.mkdir(mode=0o700)
        manifest=self.slot/'mounts.json'
        cache._write_record(manifest,{'version':1,'binding':self.admission.binding,
            'command':self.admission.command,'ordinal':ordinal,'entries':entries})
        return {'root':str(root),'artifacts':str(artifacts),'source':str(self.source),'mounts':str(manifest),'ordinal':ordinal}

    def running(self,view):
        if self.advanced:return
        ready=Path(view['artifacts'])/'ready'
        if not ready.exists() and not ready.is_symlink():return
        if unit_views._read(ready,2004,False)[1:]!=(6,hashlib.sha256(b'ready\n').hexdigest()):
            raise FreshnessHold('input_census_unknown')
        self.next=self.root/'inputs-v2';self._inputs(self.next,b'NEW')
        self.after=unit_views.inventory(self.next,owner=0,sealed=True)
        descriptor=os.open(Path(view['artifacts'])/'ack',os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o444)
        try:
            if os.write(descriptor,b'published\n')!=10:raise OSError(errno.EIO,'probe acknowledgement failed')
            os.fsync(descriptor)
        finally:os.close(descriptor)
        self.advanced=True

    def finish(self,view,directory,raw):
        if not self.advanced or raw!=0:raise FreshnessHold('cleanup_uncertain')
        observed=cache._read_record(directory/'view-namespace.json')
        if (observed.get('binding')!=self.admission.binding or observed.get('command')!=self.admission.command
                or observed.get('ordinal')!=4):raise FreshnessHold('generation_mismatch')
        namespaces=observed['namespaces']
        self.original_pid=observed.get('pid')
        if type(self.original_pid) is not int or self.original_pid<=0:raise FreshnessHold('cleanup_uncertain')
        if set(namespaces)!={'mnt','ipc','net'} or any(row['controller']==row['unit'] for row in namespaces.values()):
            raise FreshnessHold('invalidation_unsafe')
        now=(self.inputs/'deps/input').stat();before=self.input_stat
        if (unit_views.inventory(self.inputs,owner=0,sealed=True)!=self.before
                or (now.st_ino,now.st_nlink,now.st_mtime_ns,now.st_ctime_ns,now.st_atime_ns)!=
                    (before.st_ino,before.st_nlink,before.st_mtime_ns,before.st_ctime_ns,before.st_atime_ns)):
            raise FreshnessHold('input_census_unknown')
        if Path('/tmp/ci-unit-probe-private-marker').exists():raise FreshnessHold('invalidation_unsafe')
        staged=self.root/'publication-preparing'
        output=unit_views.copy_output_roots(Path(view['artifacts']),staged,['unit-output'],owner=2004,controller=0)
        staged.rename(self.root/'published')
        descriptor=os.open(self.root,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
        cache._write_record(self.root/'control.json',{**self.control,'state':'sealed','outputs':output})
        self.receipt={'version':1,'input_bytes_preserved':True,'input_inode_metadata_preserved':True,
            'next_publication_distinct':self.before!=self.after,'namespaces':namespaces,'outputs':output,
            'original_unit_cleanup':True,'allocation_removed':False,'shipping_authority':False}

    def cleanup(self):
        if self.owner!=os.getpid() or self.receipt is None:raise FreshnessHold('cleanup_uncertain')
        _source(self.admission.target,self.admission.binding,self.admission.command)
        if cache._read_record(self.root/'control.json')!={**self.control,'state':'sealed','outputs':self.receipt['outputs']}:
            raise FreshnessHold('cleanup_uncertain')
        self.abstract.close();shutil.rmtree(self.root)
        descriptor=os.open(self.admission.target,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
        self.receipt['allocation_removed']=True


CARGO_FIXTURE_MANIFEST='[package]\nname="private-cargo-probe"\nversion="0.0.0"\nedition="2021"\n'
CARGO_FIXTURE_SOURCE='''fn main(){
 assert_eq!(private_cargo_probe::value(),7);
 let out=std::path::PathBuf::from(env!("OUT_DIR"));
 assert_eq!(std::fs::read(out.join("generated.rs")).unwrap(),b"pub const GENERATED:u8=7;\\n");
 assert!(out.join("empty").is_dir());
 assert!(std::fs::write(out.join("forbidden"),b"x").is_err());
 assert!(std::fs::rename(&out,out.with_extension("renamed")).is_err());
 let root=std::path::Path::new("target/debug/.fingerprint");
 std::fs::create_dir_all(root).unwrap();
 std::fs::write(root.join("unit-forged"),b"private unit only").unwrap();
 println!("ISOLATED");
}
'''
CARGO_FIXTURE_LIB='''include!(concat!(env!("OUT_DIR"),"/generated.rs"));
pub fn value()->u8{GENERATED}
'''
CARGO_FIXTURE_BUILD='''fn main(){
 let out=std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
 std::fs::create_dir_all(out.join("empty")).unwrap();
 std::fs::write(out.join("generated.rs"),b"pub const GENERATED:u8=7;\\n").unwrap();
 let metadata=std::path::Path::new("target/debug/.fingerprint");
 std::fs::create_dir_all(metadata).unwrap();
 std::fs::write(metadata.join("build-script-forged"),b"private unit only").unwrap();
 println!("cargo:rerun-if-changed=build.rs");
}
'''


class _CargoProbe:
    """Real Cargo mapping under the private probe capability; no reuse proof."""
    def __init__(self,admission):
        if (type(admission) is not _Admission or admission.token is not _TOKEN
                or admission.owner!=os.getpid() or os.getresuid()!=(0,0,0)):
            raise FreshnessHold('invalidation_unsafe')
        _source(admission.target,admission.binding,admission.command)
        self.admission=admission;self.owner=os.getpid();self.units={};self.publications={};self.files={};self.report=None
        self.generated_stats={};self.library_used=False
        self.source=admission.target.parent/'source-projection'
        for name,body in (('Cargo.toml',CARGO_FIXTURE_MANIFEST),('src/main.rs',CARGO_FIXTURE_SOURCE),
                ('src/lib.rs',CARGO_FIXTURE_LIB),('build.rs',CARGO_FIXTURE_BUILD)):
            if unit_views._read(self.source/name,0,True)[1:]!=(len(body.encode()),hashlib.sha256(body.encode()).hexdigest()):
                raise FreshnessHold('input_census_unknown')
        self.root=admission.target/('.cargo-view-probe-'+str(uuid.uuid4()));self.root.mkdir(mode=0o710)
        os.chown(self.root,0,1001)
        self.control={'version':1,'kind':'private-cargo-view-probe','binding':admission.binding,
            'command':admission.command,'shipping_authority':False}
        cache._write_record(self.root/'control.json',{**self.control,'state':'active'})
        self.target=self.root/'planner-target'
        for name in ('planner-target','home','cargo-home'):
            path=self.root/name;path.mkdir(mode=0o700);os.chown(path,1001,1001)

    @staticmethod
    def _directory(root,name,unit):
        current=root
        for part in unit_views.relative(name).parts:
            current=current/part
            if not current.exists():current.mkdir(mode=0o700);os.chown(current,unit,unit)
            unit_views._output_directory(current,unit)
        return current

    @staticmethod
    def _stat(path):
        info=path.stat()
        return (*unit_views._key(info),info.st_nlink,info.st_atime_ns)

    def _planner_file(self,name,expected):
        path=unit_views.literal(self.target/name)
        descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NOATIME|os.O_CLOEXEC)
        try:
            before=os.fstat(descriptor);digest=hashlib.sha256();count=0
            if (not stat.S_ISREG(before.st_mode) or before.st_uid!=1001 or before.st_size!=expected[3]
                    or before.st_size>unit_views.MAX_BYTES):raise cargo_hold('planner_cargo_artifact')
            while True:
                block=os.read(descriptor,1024*1024)
                if not block:break
                count+=len(block);digest.update(block)
                if count>before.st_size:raise cargo_hold('planner_cargo_artifact')
            if (count!=before.st_size or digest.hexdigest()!=expected[4]
                    or unit_views._key(os.fstat(descriptor))!=unit_views._key(before)
                    or unit_views._key(path.lstat())!=unit_views._key(before)):
                raise cargo_hold('planner_cargo_artifact')
        finally:os.close(descriptor)
        return path

    def _file_input(self,artifacts,unit,name,alias=None):
        if name not in self.files:raise cargo_hold('planner_cargo_artifact')
        label,row=self.files[name];publication=self.publications[label]
        if unit_views.inventory(publication['root'],owner=0,sealed=True)!=publication['manifest']:
            raise cargo_hold('planner_cargo_artifact')
        original=publication['root']/name;actual=self._planner_file(name,row)
        if alias is not None:
            entry=self.target/alias
            if entry.is_symlink() or not entry.is_file() or not entry.samefile(actual):
                raise cargo_hold('planner_cargo_artifact')
        virtual=alias or name
        self._directory(artifacts,str(Path(virtual).parent),unit)
        (artifacts/virtual).touch(mode=0o444);raw=os.fsencode(virtual).hex()
        return [raw,str(original),[raw,*row[1:]]]

    def _generated_input(self,artifacts,unit,name):
        publication=self.publications.get('generated')
        if publication is None or publication['roots']!=[name]:raise cargo_hold('planner_cargo_artifact')
        if unit_views.inventory(publication['root'],owner=0,sealed=True)!=publication['manifest']:
            raise cargo_hold('planner_cargo_artifact')
        original=publication['root']/name;manifest=unit_views.inventory(original,owner=0,sealed=True)
        self._directory(artifacts,str(Path(name).parent),unit)
        (artifacts/name).mkdir(mode=0o555);raw=os.fsencode(name).hex()
        return [raw,str(original),[raw,'tree',manifest]]

    def prepare(self,ordinal,request):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        try:mapped=fixture_request(request)
        except FreshnessHold as error:error.unit_view_stage='planner_cargo_request';raise
        slot=self.root/('unit-'+str(ordinal));slot.mkdir(mode=0o700)
        unit=2000+ordinal;artifacts=slot/'artifacts';artifacts.mkdir(mode=0o700);os.chown(artifacts,unit,unit)
        for name in ('debug','debug/deps'):
            path=artifacts/name;path.mkdir(mode=0o700);os.chown(path,unit,unit)
        entries=[]
        if mapped['kind'] in ('application','build_script'):
            label='binary' if mapped['kind']=='application' else 'build-script'
            if label not in self.publications:raise cargo_hold('planner_cargo_artifact')
            entries.append(self._file_input(artifacts,unit,self.publications[label]['files'][0],mapped['alias']))
        if mapped['kind']=='compile':
            for name in mapped['dependencies']:
                if name not in self.files or self.files[name][0]!='library':raise cargo_hold('planner_cargo_artifact')
                for artifact in self.publications['library']['files']:
                    if artifact.endswith(('.rlib','.rmeta')):entries.append(self._file_input(artifacts,unit,artifact))
                self.library_used=True
            self._directory(artifacts,str(Path(mapped['outputs'][0]).parent),unit)
        generated=mapped.get('generated_root')
        if mapped['kind']=='application':
            generated=self.publications['generated']['roots'][0]
        if generated is not None:
            if mapped['kind']=='build_script':self._directory(artifacts,generated,unit)
            else:entries.append(self._generated_input(artifacts,unit,generated))
        root=slot/'root';root.mkdir(mode=0o700);mounts=slot/'mounts.json'
        cache._write_record(mounts,{'version':1,'binding':self.admission.binding,
            'command':self.admission.command,'ordinal':ordinal,'entries':entries})
        view={'root':str(root),'artifacts':str(artifacts),'source':str(self.source),
            'mounts':str(mounts),'ordinal':ordinal}
        self.units[ordinal]={'view':view,'mapping':mapped,'finished':False}
        return view

    def running(self,view):pass

    def finish(self,view,directory,raw):
        ordinal=view['ordinal'];record=self.units[ordinal];mapping=record['mapping']
        if raw!=0:
            stage={'compile':'planner_cargo_compile','build_script':'planner_cargo_build_script',
                'application':'planner_cargo_application'}.get(mapping['kind'],'planner_cargo_compile')
            raise cargo_hold(stage)
        observed=cache._read_record(directory/'view-namespace.json')
        if (observed.get('binding')!=self.admission.binding or observed.get('command')!=self.admission.command
                or observed.get('ordinal')!=ordinal
                or set(observed['namespaces'])!={'mnt','ipc','net'}
                or any(row['controller']==row['unit'] for row in observed['namespaces'].values())):
            raise FreshnessHold('invalidation_unsafe')
        if mapping['kind'] in ('compile','build_script'):
            label=mapping['role'] if mapping['kind']=='compile' else 'generated'
            if label in self.publications:raise FreshnessHold('input_census_unknown')
            staged=self.root/('preparing-'+label);roots=[]
            if mapping['kind']=='compile':
                manifest=unit_views.copy_outputs(Path(view['artifacts']),staged,mapping['outputs'],owner=2000+ordinal,controller=0)
            else:
                roots=[mapping['generated_root']]
                manifest=unit_views.copy_output_roots(Path(view['artifacts']),staged,roots,owner=2000+ordinal,controller=0)
                forged=Path(view['artifacts'])/'debug/.fingerprint/build-script-forged'
                if unit_views._read(forged,2000+ordinal,False)[1:]!=(17,hashlib.sha256(b'private unit only').hexdigest()):
                    raise FreshnessHold('invalidation_unsafe')
            sealed=self.root/('sealed-'+label);staged.rename(sealed)
            parent=os.open(self.root,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
            try:os.fsync(parent)
            finally:os.close(parent)
            outputs=[os.fsdecode(bytes.fromhex(row[0])) for row in manifest if row[1]=='file']
            if mapping['kind']=='compile':outputs=mapping['outputs']
            self.publications[label]={'root':sealed,'manifest':manifest,'files':outputs,'roots':roots}
            cache._write_record(self.root/('publication-'+label+'.json'),{'version':1,
                'binding':self.admission.binding,'command':self.admission.command,'ordinal':ordinal,
                'kind':'private-cargo-output-probe','role':label,'outputs':manifest,
                'original_cleanup':True,'shipping_authority':False})
            for row in manifest:
                if row[0]=='2e':continue
                name=os.fsdecode(bytes.fromhex(row[0]))
                if row[1]=='directory':self._directory(self.target,name,1001)
                else:self.files[name]=(label,row)
            # Cargo may hardlink and change bookkeeping on these detached copies.
            # Its metadata must never share an inode with immutable unit inputs.
            for name in outputs:
                destination=self.target/name;self._directory(self.target,str(Path(name).parent),1001)
                descriptor=os.open(destination,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
                try:
                    mode,_size,_digest=unit_views._read(sealed/name,0,True,descriptor)
                    os.fchown(descriptor,1001,1001);os.fchmod(descriptor,0o755 if mode&0o111 else 0o644);os.fsync(descriptor)
                finally:os.close(descriptor)
            if label=='generated':
                tree=sealed/roots[0]
                for row in unit_views.inventory(tree,owner=0,sealed=True):
                    path=tree/Path(os.fsdecode(bytes.fromhex(row[0])));self.generated_stats[path]=self._stat(path)
        elif mapping['kind']=='application':
            forged=Path(view['artifacts'])/'debug/.fingerprint/unit-forged'
            if unit_views._read(forged,2000+ordinal,False)[1:]!=(17,hashlib.sha256(b'private unit only').hexdigest()):
                raise FreshnessHold('invalidation_unsafe')
        pid=observed.get('pid')
        if type(pid) is not int or pid<=0:raise FreshnessHold('cleanup_uncertain')
        record.update(finished=True,namespaces=observed['namespaces'],pid=pid)

    def run(self,value,relay):
        first=len(relay.completed);counts=[];script_counts=[];fingerprint=None
        for iteration in range(2):
            client=relay.config_fd();bootstrap=None;child=None;before=len(relay.completed)
            try:
                config=dict(value,kind='cargo',parent=os.getpid(),
                    cargo_root=str(self.root),source=str(self.source))
                bootstrap=_sealed(config)
                child=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--cargo',str(bootstrap),str(client)],
                    pass_fds=(bootstrap,client),env={'PATH':os.defpath},stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                output,error=child.communicate(timeout=max(.001,value['deadline']-time.monotonic()))
                if child.returncode!=0 or output!=b'ISOLATED\n' or len(error)>8192 or relay.errors:
                    global _CHILD_DIAGNOSTIC
                    _CHILD_DIAGNOSTIC=failed_probe_diagnostic(error,relay.errors)
                    raise FreshnessHold('invalidation_unsafe')
            finally:
                if child is not None and child.poll() is None:child.kill();child.wait(timeout=5)
                os.close(client)
                if bootstrap is not None:os.close(bootstrap)
            completed=relay.completed[before:]
            count=sum(self.units[unit['ordinal']]['mapping']['kind']=='compile' for unit in completed)
            if count!=(3 if iteration==0 else 0):raise FreshnessHold('input_census_unknown')
            scripts=sum(self.units[unit['ordinal']]['mapping']['kind']=='build_script' for unit in completed)
            if scripts!=(1 if iteration==0 else 0):raise FreshnessHold('input_census_unknown')
            script_counts.append(scripts)
            if sum(self.units[unit['ordinal']]['mapping']['kind']=='application' for unit in completed)!=1:
                raise FreshnessHold('invalidation_unsafe')
            counts.append(count)
            metadata=self.target/'debug/.fingerprint'
            current=unit_views.inventory(metadata,owner=1001,sealed=False)
            if ((metadata/'unit-forged').exists() or (metadata/'build-script-forged').exists()
                    or fingerprint is not None and current!=fingerprint):
                raise FreshnessHold('invalidation_unsafe')
            fingerprint=current
        if (set(self.publications)!={'build-script','generated','library','binary'} or not self.library_used
                or not self.generated_stats or any(not unit['finished'] for unit in self.units.values())
                or any(unit_views.inventory(pub['root'],owner=0,sealed=True)!=pub['manifest'] for pub in self.publications.values())
                or any(self._stat(path)!=before for path,before in self.generated_stats.items())):
            raise FreshnessHold('cleanup_uncertain')
        self.report={'version':1,'compiler_calls':counts,'build_script_calls':script_counts,'application_runs':2,
            'generated_tree_preserved':True,'library_input_mounted':True,
            'actual_metadata_preserved':True,'sealed_artifact_detached':True,
            'units':relay.completed[first:],'allocation_removed':False,
            'trusted_input_closure':False,'shipping_authority':False}

    def cleanup(self):
        if self.owner!=os.getpid() or self.report is None:raise FreshnessHold('cleanup_uncertain')
        _source(self.admission.target,self.admission.binding,self.admission.command)
        if cache._read_record(self.root/'control.json')!={**self.control,'state':'active'}:
            raise FreshnessHold('cleanup_uncertain')
        shutil.rmtree(self.root);self.report['allocation_removed']=True


def cargo_hold(stage):
    error=FreshnessHold('artifact_provenance_unknown');error.unit_view_stage=stage;return error


def _cargo(arguments):
    """Internal fixed Cargo bootstrap; requires the original root-sealed grant."""
    global _PHASE
    _PHASE='planner_client_start';bootstrap,configuration=map(int,arguments)
    value=_bootstrap(bootstrap,'cargo');_parent_guard(value['parent']);os.close(bootstrap)
    root=unit_views.literal(value['cargo_root']);source=unit_views.literal(value['source'])
    if (root.parent!=Path(value['target']) or not root.name.startswith('.cargo-view-probe-')
            or cache._read_record(root/'control.json')!={'version':1,'kind':'private-cargo-view-probe',
                'binding':value['binding'],'command':value['command'],'shipping_authority':False,'state':'active'}):
        raise FreshnessHold('invalidation_unsafe')
    _PHASE='planner_cargo_runtime';libc=ctypes.CDLL(None,use_errno=True)
    if libc.unshare(0x00020000)!=0:raise OSError(ctypes.get_errno(),'planner mount namespace unavailable')
    unit_views._mount('--make-rprivate','/');os.chdir('/')
    runtime=root/('toolchain-runtime-'+str(uuid.uuid4()));runtime.mkdir(mode=0o755)
    unit_views.toolchain_view('/toolchain',runtime)
    unit_views._mount('--rbind',runtime,'/toolchain')
    unit_views._mount('-o','remount,bind,ro,nosuid,nodev,noatime','/toolchain')
    unit_views._readonly(source,'/workspace')
    for original,destination in ((root/'planner-target',Path('/workspace/target')),
            (root/'home',Path('/home/worker')),(root/'cargo-home',Path('/ci-cargo-home'))):
        destination.mkdir(mode=0o755,parents=True,exist_ok=True)
        unit_views._mount('--bind',original,destination)
        unit_views._mount('-o','remount,bind,rw,nosuid,nodev,noatime',destination)
    environment={'PATH':'/toolchain/bin:/usr/bin:/bin','HOME':'/home/worker','CARGO_HOME':'/ci-cargo-home',
        'CARGO_TARGET_DIR':'/workspace/target','CARGO_INCREMENTAL':'0','CARGO_BUILD_JOBS':'1',
        'CARGO_PROFILE_DEV_DEBUG':'0','RUSTC':'/toolchain/bin/rustc',
        'LD_PRELOAD':value['library'],'JARVIS_CARGO_RELAY_FD':str(configuration)}
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    os.setgroups([]);os.setgid(1001);os.setuid(1001);os.umask(0o077);_parent_guard(value['parent'])
    os.set_inheritable(configuration,True);os.chdir('/workspace');_PHASE='planner_cargo_execute'
    os.execve('/toolchain/bin/cargo',['/toolchain/bin/cargo','run','--offline','--locked','--quiet'],environment)


_TOOL_PROBE=r'''
import ctypes,errno,hashlib,json,os,signal,socket,sys
metadata,socket_path,expected=sys.argv[1:];expected=int(expected)
is_view=os.environ.get('CI_PLANNER_PROBE_VIEW')=='1'
_PROBE_PHASE='planner_tool_credentials'
assert os.getresuid()==(expected,expected,expected) and os.getresgid()==(expected,expected,expected) and os.getgroups()==[]
assert ctypes.CDLL(None).prctl(39,0,0,0,0)==1
_PROBE_PHASE='planner_tool_metadata'
try:open(metadata,'a').write('bad')
except OSError as error:assert error.errno in ((errno.ENOENT,) if is_view else (errno.EACCES,errno.EPERM))
else:raise AssertionError('unit wrote planner metadata')
_PROBE_PHASE='planner_tool_socket'
with socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET) as endpoint:
 try:endpoint.connect(socket_path)
 except OSError as error:assert error.errno in ((errno.ENOENT,) if is_view else (errno.EACCES,errno.EPERM))
 else:raise AssertionError('unit reached root planner socket')
_PROBE_PHASE='planner_tool_jobserver'
flags=os.environ.get('CARGO_MAKEFLAGS')
if flags:
 assert flags=='-j --jobserver-fds=3,4 --jobserver-auth=3,4'
 assert os.read(3,1)==b'+';os.write(4,b'+')
_PROBE_PHASE='planner_tool_fds'
fds=[]
for raw in os.listdir('/proc/self/fd'):
 try:os.fstat(int(raw))
 except OSError:continue
 fds.append(int(raw))
assert sorted(fds)==([0,1,2,3,4] if flags else [0,1,2]),fds
_PROBE_PHASE='planner_tool_environment'
assert os.environb[b'RAW']==b'\xffvalue'
_PROBE_PHASE='planner_tool_stdin'
result={'uid':expected,'stdin_sha256':hashlib.sha256(sys.stdin.buffer.read()).hexdigest(),
 'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':bool(flags)}
if is_view:
 libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
 missing=libc.syscall(ctypes.c_long(257),ctypes.c_long(-100),
  ctypes.c_char_p(b'/workspace/target/deps/missing'),ctypes.c_long(os.O_RDONLY|os.O_CLOEXEC),ctypes.c_long(0))
 assert missing==-1 and ctypes.get_errno()==errno.ENOENT
 long_path=b'/workspace/'+b'x/'*2042
 assert len(long_path)==4095
 longest=libc.syscall(ctypes.c_long(257),ctypes.c_long(-100),ctypes.c_char_p(long_path),
  ctypes.c_long(os.O_RDONLY|os.O_CLOEXEC),ctypes.c_long(0))
 assert longest==-1 and ctypes.get_errno()==errno.ENOENT
 _PROBE_PHASE='planner_tool_view'
 from pathlib import Path
 import time
 input=Path('/workspace/target/deps/input')
 selected=lambda info:(info.st_ino,info.st_nlink,info.st_mode,info.st_size,info.st_mtime_ns,info.st_ctime_ns,info.st_atime_ns)
 before=selected(input.stat());assert input.read_bytes()==b'OLD'
 for operation in (lambda:input.open('wb'),lambda:input.unlink()):
  try:operation()
  except OSError as error:assert error.errno in (errno.EROFS,errno.EACCES,errno.EPERM,errno.EBUSY)
  else:raise AssertionError('frozen input changed')
 source=Path('/workspace/source.txt');assert source.read_bytes()==b'literal immutable protocol fixture\n'
 try:source.write_bytes(b'wrong source')
 except OSError as error:assert error.errno in (errno.EROFS,errno.EACCES,errno.EPERM)
 else:raise AssertionError('tracked source changed')
 with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as endpoint:
  try:endpoint.connect(b'\0jarvis-unit-view-probe')
  except OSError as error:assert error.errno in (errno.ENOENT,errno.ECONNREFUSED)
  else:raise AssertionError('unit reached foreign abstract socket')
 try:os.readlink('/proc/'+str(os.getppid())+'/root')
 except OSError as error:assert error.errno in (errno.ENOENT,errno.EACCES,errno.EPERM)
 else:raise AssertionError('unit reached root supervisor filesystem')
 Path('/tmp/ci-unit-probe-private-marker').write_bytes(b'private')
 Path('/workspace/target/ready-preparing').write_bytes(b'ready\n')
 Path('/workspace/target/ready-preparing').rename('/workspace/target/ready')
 deadline=time.monotonic()+10
 while True:
  try:ack=Path('/workspace/target/ack').read_bytes()
  except FileNotFoundError:ack=b''
  if ack==b'published\n':break
  assert time.monotonic()<deadline
  time.sleep(.01)
 assert input.read_bytes()==b'OLD' and selected(input.stat())==before
 output=Path('/workspace/target/unit-output');output.mkdir();(output/'empty').mkdir()
 (output/('generated-'+str(os.getpid())+'.bin')).write_bytes(b'sealed result\n')
 result['filesystem']={'input_stable':True,'input_metadata_stable':True,'source_read_only':True,
  'abstract_socket_isolated':True,'private_temp':True,'metadata_hidden':True,'output_written':True}
print(json.dumps(result),flush=True)
if os.environ.get('CI_PLANNER_PROBE_SIGNAL')=='1':signal.pause()
'''


_PLANNER_PROBE=r'''
import ctypes,errno,hashlib,json,os,signal,sys
mode,metadata,socket_path,expected_uid=sys.argv[1:];expected_uid=int(expected_uid)
_PROBE_PHASE='planner_probe_credentials'
assert os.getresuid()==(1001,1001,1001) and os.getresgid()==(1001,1001,1001) and os.getgroups()==[]
with open(metadata,'a') as output:output.write('planner\n')
_PROBE_PHASE='planner_probe_filter'
libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
argv=(ctypes.c_char_p*2)(b'true',None);environment=(ctypes.c_char_p*1)(None)
assert libc.syscall(ctypes.c_long(59),ctypes.c_char_p(b'/usr/bin/true'),argv,environment)==-1
assert ctypes.get_errno()==errno.EPERM
body=__FIXED_TOOL_PROGRAM__
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
  if mode=='signal':env[b'CI_PLANNER_PROBE_SIGNAL']=b'1'
  if mode=='view':env[b'CI_PLANNER_PROBE_VIEW']=b'1'
  if jobs:env[b'CARGO_MAKEFLAGS']=('-j --jobserver-fds=%d,%d --jobserver-auth=%d,%d'%(*jobs,*jobs)).encode()
  os.execve(b'/usr/bin/python3',[b'python3',b'-I',b'-c',body.encode(),os.fsencode(metadata),os.fsencode(socket_path),str(expected_uid).encode()],env)
 except BaseException:os._exit(126)
 os._exit(127)
os.close(source_read);os.close(output_write);os.close(start_write)
_PROBE_PHASE='planner_probe_exec_start'
assert os.read(start_read,1)==b'';os.close(start_read)
data=b'\xffstdin'*200000 if mode!='cancel' else b''
if mode=='cancel':os.kill(child,signal.SIGUSR1)
else:
 _PROBE_PHASE='planner_probe_stream'
 view=memoryview(data)
 while view:view=view[os.write(source_write,view):]
 os.close(source_write);source_write=None
_PROBE_PHASE='planner_probe_wait'
with os.fdopen(output_read,'rb') as output:
 if mode=='signal':
  captured=output.readline(8193);assert 0<len(captured)<=8192
  os.kill(child,signal.SIGUSR1)
  assert output.read(1)==b''
 else:captured=output.read(8193)
_,status=os.waitpid(child,0);code=os.waitstatus_to_exitcode(status)
if source_write is not None:os.close(source_write)
_PROBE_PHASE='planner_probe_status'
if mode=='cancel':
 assert code==125 and captured==b'',(code,captured)
 result={'cancelled_before_exec':True,'proxy_exit':code}
else:
 assert code==(-signal.SIGUSR1 if mode=='signal' else 0),(code,captured)
 result=json.loads(captured);assert result['stdin_sha256']==hashlib.sha256(data).hexdigest()
 if mode=='signal':result.update(signal_forwarded=int(signal.SIGUSR1),proxy_signal=-code)
 if jobs:assert os.read(jobs[0],1)==b'+'
for fd in jobs:os.close(fd)
print(json.dumps(result),flush=True)
'''
_PLANNER_PROBE=_PLANNER_PROBE.replace('__FIXED_TOOL_PROGRAM__',repr(guarded_probe(_TOOL_PROBE)))


def _planner(arguments):
    global _PHASE
    _PHASE='planner_client_start'
    bootstrap,configuration=map(int,arguments)
    value=_bootstrap(bootstrap,'planner');_parent_guard(value['parent'])
    if value.get('mode') not in ('stdin','jobserver','cancel','signal','view'):raise FreshnessHold('metadata_torn')
    os.close(bootstrap)
    environment={'PATH':'/usr/bin:/bin','HOME':'/tmp','LD_PRELOAD':value['library'],
        'JARVIS_CARGO_RELAY_FD':str(configuration)}
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    os.setgroups([]);os.setgid(1001);os.setuid(1001);_parent_guard(value['parent'])
    os.set_inheritable(configuration,True)
    _PHASE='planner_client_exec'
    os.execve('/usr/bin/python3',['python3','-I','-c',guarded_probe(_PLANNER_PROBE),value['mode'],
        value['metadata'],value['socket'],str(value['expected_uid'])],environment)


def _probe(arguments):
    global _PHASE,_CHILD_DIAGNOSTIC
    value=_bootstrap(int(arguments[0]),'probe');_parent_guard(value['parent'])
    root=Path(value['root']);admission=admit(value['target'],value['binding'],value['command'],root)
    results=[];view_probe=None;cargo_probe=None
    _PHASE='planner_kernel_observer'
    with kernel_observer.Observer(admission,deadline=value['deadline']) as observer, RootRelay(admission,library=value['library'],timeout=30) as relay:
        metadata=root/'planner-metadata';metadata.write_text('');os.chown(metadata,1001,1001);metadata.chmod(0o600)
        for ordinal,mode in enumerate(('stdin','jobserver','cancel','signal','view')):
            _PHASE='planner_probe_'+mode
            if mode=='view':view_probe=_ViewProbe(admission);relay.views=view_probe
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
                    _CHILD_DIAGNOSTIC=failed_probe_diagnostic(error,relay.errors)
                    raise FreshnessHold('invalidation_unsafe')
                results.append(json.loads(output))
            finally:
                if child is not None and child.poll() is None:child.kill();child.wait(timeout=5)
                os.close(client)
                if child_bootstrap is not None:os.close(child_bootstrap)
        if len(relay.completed)!=5 or relay.errors:raise FreshnessHold('cleanup_uncertain')
        for ordinal,completed in enumerate(relay.completed):
            if (completed['ordinal']!=ordinal or completed['uid']!=2000+ordinal
                    or completed['started']!=(ordinal!=2) or completed['cancelled']!=(ordinal==2)
                    or completed['original_cleanup'] is not True):raise FreshnessHold('cleanup_uncertain')
        original_units=list(relay.completed)
        cargo_probe=_CargoProbe(admission);relay.views=cargo_probe;cargo_probe.run(value,relay)
    view_probe.cleanup()
    cargo_probe.cleanup()
    _PHASE='planner_kernel_observer'
    compilers={item['mapping']['role']:(item['pid'],2000+ordinal)
        for ordinal,item in cargo_probe.units.items() if item['mapping']['kind']=='compile'}
    kernel=observer.finish(view_probe.original_pid,compilers)
    print(json.dumps({'version':3,'kind':'ci-planner-protocol-probe','binding':value['binding'],
        'command':value['command'],'cases':results,'units':original_units,'filesystem':view_probe.receipt,
        'cargo':cargo_probe.report,'kernel_observation':kernel,
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


def validate_filesystem_probe(value):
    keys={'version','input_bytes_preserved','input_inode_metadata_preserved','next_publication_distinct',
        'namespaces','outputs','original_unit_cleanup','allocation_removed','shipping_authority'}
    outputs=value.get('outputs') if type(value) is dict else None
    if (type(outputs) is not list or len(outputs)!=4 or type(outputs[3]) is not list
            or len(outputs[3])!=5 or type(outputs[3][0]) is not str):raise FreshnessHold('input_census_unknown')
    try:name=bytes.fromhex(outputs[3][0])
    except ValueError:raise FreshnessHold('input_census_unknown') from None
    if re.fullmatch(rb'unit-output/generated-[1-9][0-9]{0,19}\.bin',name) is None:
        raise FreshnessHold('input_census_unknown')
    expected_outputs=[['2e','directory',0o555],[b'unit-output'.hex(),'directory',0o555],
        [b'unit-output/empty'.hex(),'directory',0o555],
        [name.hex(),'file',0o444,14,hashlib.sha256(b'sealed result\n').hexdigest()]]
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or any(value[key] is not True for key in ('input_bytes_preserved','input_inode_metadata_preserved',
                'next_publication_distinct','original_unit_cleanup','allocation_removed'))
            or value['shipping_authority'] is not False or value['outputs']!=expected_outputs
            or type(value['namespaces']) is not dict or set(value['namespaces'])!={'mnt','ipc','net'}):
        raise FreshnessHold('input_census_unknown')
    for name,row in value['namespaces'].items():
        if (type(row) is not dict or set(row)!={'controller','unit'} or row['controller']==row['unit']
                or any(type(item) is not str or re.fullmatch(name+r':\[[0-9]{1,20}\]',item) is None for item in row.values())):
            raise FreshnessHold('input_census_unknown')
    return value


def validate_cargo_probe(value):
    keys={'version','compiler_calls','application_runs','actual_metadata_preserved','sealed_artifact_detached',
        'units','allocation_removed','trusted_input_closure','shipping_authority','build_script_calls',
        'generated_tree_preserved','library_input_mounted'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['compiler_calls']!=[3,0] or any(type(count) is not int for count in value['compiler_calls'])
            or value['build_script_calls']!=[1,0] or any(type(count) is not int for count in value['build_script_calls'])
            or type(value['application_runs']) is not int or value['application_runs']!=2
            or any(value[key] is not True for key in ('actual_metadata_preserved','sealed_artifact_detached',
                'allocation_removed','generated_tree_preserved','library_input_mounted'))
            or value['trusted_input_closure'] is not False or value['shipping_authority'] is not False
            or type(value['units']) is not list or not 6<=len(value['units'])<=32):
        raise FreshnessHold('input_census_unknown')
    for ordinal,unit in enumerate(value['units'],start=5):
        if (type(unit) is not dict or set(unit)!={'ordinal','uid','started','cancelled','returncode',
                'original_cleanup','input_closure_complete','shipping_authority'}
                or type(unit['ordinal']) is not int or unit['ordinal']!=ordinal
                or type(unit['uid']) is not int or unit['uid']!=2000+ordinal
                or unit['started'] is not True or unit['cancelled'] is not False
                or type(unit['returncode']) is not int or unit['returncode']!=0
                or unit['original_cleanup'] is not True or unit['input_closure_complete'] is not False
                or unit['shipping_authority'] is not False):raise FreshnessHold('cleanup_uncertain')
    return value


def validate_probe(value,binding,command,workers,writer,source_projection):
    keys={'version','kind','binding','command','cases','units','guest_worker_quiescence','filesystem',
        'input_closure_complete','cargo_metadata_isolation_established','shipping_authority','kernel_observation'}
    if (type(value) is not dict or set(value) not in (keys,keys|{'cargo'}) or type(value['version']) is not int or value['version']!=3
            or value['kind']!='ci-planner-protocol-probe' or value['binding']!=binding or value['command']!=command
            or value['input_closure_complete'] is not False or value['cargo_metadata_isolation_established'] is not False
            or value['shipping_authority'] is not False or type(value['cases']) is not list or len(value['cases'])!=5
            or type(value['units']) is not list or len(value['units'])!=5):raise FreshnessHold('input_census_unknown')
    validate_filesystem_probe(value['filesystem'])
    try:kernel_observer.validate(value['kernel_observation'],binding,command)
    except kernel_observer.ObservationUnavailable:raise FreshnessHold('input_census_unknown') from None
    if 'cargo' in value:validate_cargo_probe(value['cargo'])
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
                or type(unit['returncode']) is not int or (ordinal!=2 and unit['returncode']!=(-10 if ordinal==3 else 0))
                or unit['input_closure_complete'] is not False or unit['shipping_authority'] is not False):
            raise FreshnessHold('cleanup_uncertain')
        case=value['cases'][ordinal]
        if ordinal==2:
            if case!={'cancelled_before_exec':True,'proxy_exit':125}:raise FreshnessHold('input_census_unknown')
        else:
            case_keys={'uid','stdin_sha256','metadata_denied','root_socket_denied','control_fds_closed','jobserver'}
            if ordinal==3:case_keys|={'signal_forwarded','proxy_signal'}
            if ordinal==4:case_keys|={'filesystem'}
            if (type(case) is not dict or set(case)!=case_keys or type(case['uid']) is not int or case['uid']!=2000+ordinal
                    or case['stdin_sha256']!=expected or case['metadata_denied'] is not True
                    or case['root_socket_denied'] is not True or case['control_fds_closed'] is not True
                    or case['jobserver'] is not (ordinal==1)
                    or (ordinal==3 and (case['signal_forwarded']!=10 or case['proxy_signal']!=10))):
                raise FreshnessHold('input_census_unknown')
            if ordinal==4:
                flags=case['filesystem'];expected_flags={'input_stable','input_metadata_stable','source_read_only',
                    'abstract_socket_isolated','private_temp','metadata_hidden','output_written'}
                if type(flags) is not dict or set(flags)!=expected_flags or any(item is not True for item in flags.values()):
                    raise FreshnessHold('input_census_unknown')
    return value


if __name__=='__main__':
    try:
        if os.geteuid()!=0:raise FreshnessHold('invalidation_unsafe')
        if sys.argv[1:2]==['--unit']:result=_unit(sys.argv[2:])
        elif sys.argv[1:2]==['--stage']:result=_stage(sys.argv[2:])
        elif sys.argv[1:2]==['--planner']:result=_planner(sys.argv[2:])
        elif sys.argv[1:2]==['--cargo']:result=_cargo(sys.argv[2:])
        elif sys.argv[1:2]==['--probe']:result=_probe(sys.argv[2:])
        elif sys.argv[1:2]==['--probe-supervisor']:result=_probe_supervisor(sys.argv[2:])
        else:raise FreshnessHold('invalidation_unsafe')
    except BaseException as error:
        print(json.dumps({'planner_diagnostic':_CHILD_DIAGNOSTIC or diagnostic({
            'stage':getattr(error,'unit_view_stage',_PHASE),'category':type(error).__name__,'errno':getattr(error,'errno',None)})}),file=sys.stderr)
        result=125
    raise SystemExit(result)
