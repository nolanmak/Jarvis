#!/usr/bin/env python3
"""Private guest V3 transport. No passing-CI proof or protected-cache activation.

Only the admitted root controller creates a listener. Planner requests run in
fresh, single-threaded root helpers; requested environments apply after dropping
to a unique unit UID. Frozen unit views and complete input/output provenance are
still required before this transport can run protected production Cargo.
"""
import array
import ctypes
import copy
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
import selectors
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
lookup_inputs=_load('planner_lookup_inputs','ci-lookup-inputs.py')
lookup_inputs.FreshnessHold=cache.FreshnessHold
unit_views.FreshnessHold=cache.FreshnessHold
import shutil
FreshnessHold=cache.FreshnessHold
STAGES={'staging_ready':1,'tool_started':2,'finished':3,'error':4,'signaled':5}
_TOKEN=object()
_WORKER_GROUP_TOKEN=object()
_PHASE='planner_startup'
_CHILD_DIAGNOSTIC=None


def diagnostic(value):
    phases={'planner_startup','planner_unit','planner_capture','planner_authorize','planner_drop',
        'planner_arm_exec','planner_unit_exec','planner_client_start','planner_client_exec',
        'planner_probe_stdin','planner_probe_jobserver','planner_probe_cancel','planner_probe_signal','planner_probe_supervisor',
        'planner_probe_credentials','planner_probe_filter','planner_probe_exec_start','planner_probe_stream','planner_probe_wait','planner_probe_status','planner_request_receive','planner_request_cleanup','planner_tool_credentials','planner_tool_metadata','planner_tool_socket','planner_tool_jobserver','planner_tool_fds','planner_tool_environment','planner_tool_stdin','planner_probe_view','planner_tool_view','planner_view_prepare','planner_view_enter','planner_view_namespace','planner_view_runtime','planner_view_source','planner_view_inputs','planner_view_proc','planner_view_devices','planner_view_root','planner_view_publish','planner_cargo_request','planner_cargo_artifact','planner_cargo_runtime','planner_cargo_execute','planner_cargo_compile','planner_cargo_build_script','planner_cargo_application','planner_cargo_supervisor','planner_cargo_retire','planner_cargo_retry','planner_kernel_observer'}
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
    if request_errors and (selected is None or selected['stage']=='planner_probe_status'
            or selected=={'stage':'planner_probe_stream','category':'OSError','errno':errno.EPIPE}):
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


def wait_original_worker_grant(endpoint,pid,deadline):
    while True:
        remaining=deadline-time.monotonic()
        if remaining<=0:raise FreshnessHold('cleanup_uncertain')
        if not select.select([endpoint],[],[],min(.05,remaining))[0]:continue
        received=[]
        try:
            frame=receive_control(endpoint)
            if frame is None:raise FreshnessHold('cleanup_uncertain')
            stage,original,received=frame
            if stage!=3 or original!=pid or received:raise FreshnessHold('generation_mismatch')
            return
        finally:legacy._close(received)


def fixture_out_dir(environment):
    value=environment.get(b'OUT_DIR')
    if value is None:return None
    if re.fullmatch(rb'/workspace/target/debug/build/private-cargo-probe-[0-9a-f]{16}/out',value) is None:
        raise FreshnessHold('input_census_unknown')
    return os.fsdecode(value.removeprefix(b'/workspace/target/'))


def rustc_unit(request,*,package,host_triple):
    """Decode a bounded direct-rustc request without granting artifact authority.

    The controller must separately admit the package/source graph, runtime and
    every referenced publication. The request hash binds execution syntax; it
    is not a compatible-unit key or complete input proof. Opaque tools hold.
    """
    def hold():raise FreshnessHold('input_census_unknown')
    def path(value,prefix):
        if (type(value) is not bytes or not value.startswith(prefix+b'/')
                or any(p in (b'',b'.',b'..') for p in value.split(b'/')[1:])):hold()
        return value[len(prefix)+1:]
    try:
        file,argv,environment,cwd,search,ignored,blocked=request
        triples=(b'x86_64-unknown-linux-gnu',b'aarch64-unknown-linux-gnu')
        if (type(package) is not bytes or re.fullmatch(rb'[A-Za-z0-9][A-Za-z0-9_-]{0,127}',package) is None
                or host_triple not in triples or type(argv) is not list or not 1<=len(argv)<=4096
                or type(environment) is not dict or len(environment)>4096
                or any(type(n) is not int or not 0<=n<2**64 for n in (ignored,blocked))):hold()
        strings=[file,cwd,*argv,*environment.keys(),*environment.values(),*([search] if search is not None else [])]
        if (any(type(s) is not bytes or len(s)>legacy.STRING_LIMIT or b'\0' in s for s in strings)
                or sum(len(s)+4 for s in strings)>legacy.LIMIT
                or any(not key or b'=' in key for key in environment)
                or file!=b'/toolchain/bin/rustc' or argv[0]!=file):hold()
        if cwd!=b'/workspace':path(cwd,b'/workspace')
        if cwd==b'/workspace/target' or cwd.startswith(b'/workspace/target/'):hold()
        if environment.get(b'CARGO_PKG_NAME',package)!=package:hold()
        args=argv[1:]
        if args in ([b'-vV'],[b'--version']):return {'kind':'information'}
        if args and args[0]==b'-':
            words={b'-',b'--crate-name',b'___',b'--crate-type',b'bin',b'rlib',b'dylib',b'cdylib',b'staticlib',
                b'proc-macro',b'-Wwarnings',b'--print=file-names',b'--print=sysroot',b'--print=split-debuginfo',b'--print=crate-name',b'--print=cfg'}
            if all(arg in words for arg in args) and any(arg.startswith(b'--print=') for arg in args):return {'kind':'information'}
            hold()
        fields={};codegen={};cfgs=[];checks=[];externs=[];searches=[];source=None;index=0
        single={b'--crate-name',b'--crate-type',b'--edition',b'--emit',b'--out-dir',b'--target',b'--error-format',b'--json',b'--cap-lints'}
        repeated={b'--cfg':cfgs,b'--check-cfg':checks,b'--extern':externs,b'-L':searches}
        while index<len(args):
            arg=args[index];index+=1
            if arg==b'--test':
                if arg in fields:hold()
                fields[arg]=True;continue
            key,separator,value=arg.partition(b'=')
            if key in single or key in repeated or arg==b'-C':
                if arg==b'-C':key=arg;separator=b''
                if not separator:
                    if index>=len(args):hold()
                    value=args[index];index+=1
                if not value:hold()
                if key==b'-C':
                    option,equals,setting=value.partition(b'=')
                    allowed={b'embed-bitcode',b'debuginfo',b'metadata',b'extra-filename',b'strip',b'opt-level',
                        b'debug-assertions',b'overflow-checks',b'panic',b'codegen-units'}
                    if not equals or option not in allowed or option in codegen:hold()
                    if (option==b'extra-filename' and re.fullmatch(rb'-[0-9a-f]{16}',setting) is None
                            or option==b'metadata' and re.fullmatch(rb'[0-9a-f]{16}',setting) is None
                            or option==b'embed-bitcode' and setting not in (b'yes',b'no')
                            or option==b'debuginfo' and setting not in (b'0',b'1',b'2')
                            or option==b'strip' and setting not in (b'none',b'debuginfo',b'symbols')
                            or option==b'opt-level' and setting not in (b'0',b'1',b'2',b'3',b's',b'z')
                            or option in (b'debug-assertions',b'overflow-checks') and setting not in (b'yes',b'no',b'on',b'off')
                            or option==b'panic' and setting not in (b'abort',b'unwind')
                            or option==b'codegen-units' and (not setting.isdigit() or not 1<=int(setting)<=256)):hold()
                    codegen[option]=setting
                elif key in repeated:repeated[key].append(value)
                else:
                    if key in fields:hold()
                    fields[key]=value
                continue
            if arg.startswith((b'-',b'@')) or source is not None:hold()
            source=arg
        if source is None or not {b'metadata',b'extra-filename'}<=codegen.keys():hold()
        if not source.startswith(b'/'):source=cwd+b'/'+source
        path(source,b'/workspace')
        if source.startswith(b'/workspace/target/') or b'.git' in source.split(b'/'):hold()
        name=fields.get(b'--crate-name',b'');kind=fields.get(b'--crate-type')
        if (re.fullmatch(rb'[A-Za-z_][A-Za-z0-9_]{0,127}',name) is None
                or fields.get(b'--edition') not in (b'2015',b'2018',b'2021',b'2024')
                or fields.get(b'--error-format',b'json')!=b'json'):hold()
        test=fields.get(b'--test',False)
        if kind not in ((None,b'lib',b'rlib',b'bin') if test else (b'lib',b'rlib',b'bin')):hold()
        emit=fields.get(b'--emit',b'').split(b',')
        if len(set(emit))!=len(emit) or set(emit) not in ({b'dep-info',b'link'},{b'dep-info',b'metadata'},{b'dep-info',b'metadata',b'link'}):hold()
        library=kind in (b'lib',b'rlib') and not test
        if not library and set(emit)!={b'dep-info',b'link'}:hold()
        target=fields.get(b'--target',host_triple)
        if target not in triples:hold()
        out=fields.get(b'--out-dir',b'');relative=path(out,b'/workspace/target');parts=relative.split(b'/')
        target_prefix=[]
        if b'--target' in fields:
            if not parts or parts[0]!=target:hold()
            target_prefix=[parts.pop(0)]
        if not parts or re.fullmatch(rb'[A-Za-z0-9][A-Za-z0-9_-]{0,63}',parts[0]) is None:hold()
        profile=parts[0];suffix=codegen[b'extra-filename']
        build=name==b'build_script_build' and not test and kind==b'bin'
        if parts!=([profile,b'build',package+suffix] if build else [profile,b'deps']):hold()
        action='build-script' if build else 'test' if test else 'library' if library and b'link' in emit else 'check' if library else 'binary'
        prefix=os.fsdecode(relative)+ '/';base=os.fsdecode(name+suffix);outputs=[]
        if b'link' in emit:outputs.append(prefix+('lib'+base+'.rlib' if library else base))
        if b'metadata' in emit:outputs.append(prefix+'lib'+base+'.rmeta')
        outputs.append(prefix+base+'.d')
        dependencies=[];seen=set()
        for item in externs:
            alias,equals,original=item.partition(b'=')
            if not equals or re.fullmatch(rb'[A-Za-z_][A-Za-z0-9_]*',alias) is None or alias in seen:hold()
            seen.add(alias);dep=path(original,b'/workspace/target')
            if not original.endswith((b'.rlib',b'.rmeta')) or b'deps' not in dep.split(b'/'):hold()
            dependencies.append(os.fsdecode(dep))
        for item in searches:
            if not item.startswith(b'dependency='):hold()
            directory=path(item[len(b'dependency='):],b'/workspace/target')
            if directory.split(b'/')[-1]!=b'deps':hold()
        generated=environment.get(b'OUT_DIR')
        if generated is not None:
            generated=path(generated,b'/workspace/target');tail=generated.split(b'/')
            if (len(tail)<4 or tail[-3]!=b'build' or tail[-1]!=b'out'
                    or re.fullmatch(re.escape(package)+rb'-[0-9a-f]{16}',tail[-2]) is None):hold()
            generated=os.fsdecode(generated)
        context=lookup_inputs._wire([file,argv,sorted(environment.items()),cwd,search,ignored,blocked])
        return {'kind':'compile','action':action,'source':source,'crate_name':name,'crate_type':kind,
            'target':target,'profile':profile,'out_dir':out,'outputs':outputs,'dependencies':dependencies,
            'generated_root':generated,'fingerprint':os.fsdecode(b'/'.join([*target_prefix,profile,b'.fingerprint',package+suffix])),
            'cfgs':cfgs,'check_cfgs':checks,'externs':externs,'searches':searches,'fields':fields,'codegen':codegen,
            'request_sha256':hashlib.sha256(json.dumps(context,sort_keys=True,separators=(',',':')).encode()).hexdigest(),
            'input_closure_complete':False,'shipping_authority':False}
    except (ValueError,TypeError,KeyError,AttributeError,IndexError,UnicodeError):hold()


class RestartGate:
    """Durable four-pass counter for one authenticated bootstrap grant.

    The caller supplies original execution authority. This gate cannot renew a
    deadline, clear invalidations, issue passing CI or authenticate a new grant.
    """
    def __init__(self,path,context):
        self.path=unit_views.literal(path)
        cache.validate_binding(context['binding'])
        deadline=context.get('deadline');command=context.get('command')
        if (type(deadline) not in (int,float) or not math.isfinite(deadline)
                or type(command) is not str or not 0<len(command)<=256):raise FreshnessHold('metadata_torn')
        encoded=json.dumps(context,sort_keys=True,separators=(',',':')).encode()
        if len(encoded)>65536:raise FreshnessHold('metadata_torn')
        self.grant=hashlib.sha256(encoded).hexdigest();self.deadline=deadline
        if self.path.exists() or self.path.is_symlink():self._state=cache._read_record(self.path)
        else:
            self._state={'version':1,'binding':copy.deepcopy(context['binding']),'command':command,'grant_sha256':self.grant,
                'deadline':deadline,'passes':0,'active':False,'shipping_authority':False}
            cache._write_record(self.path,self._state)
        value=self._state
        if (set(value)!={'version','binding','command','grant_sha256','deadline','passes','active','shipping_authority'}
                or type(value['version']) is not int or value['version']!=1 or value['binding']!=context['binding']
                or value['command']!=command or value['grant_sha256']!=self.grant or value['deadline']!=deadline
                or type(value['passes']) is not int or not 0<=value['passes']<=4
                or type(value['active']) is not bool or value['shipping_authority'] is not False):
            raise FreshnessHold('metadata_torn')

    def _check(self,context):
        if (hashlib.sha256(json.dumps(context,sort_keys=True,separators=(',',':')).encode()).hexdigest()!=self.grant
                or cache._read_record(self.path)!=self._state):raise FreshnessHold('metadata_torn')

    def begin(self,context):
        self._check(context)
        if self._state['active'] or self._state['passes']>=4 or time.monotonic()>=self.deadline:
            raise FreshnessHold('input_census_unknown')
        value=self._state|{'passes':self._state['passes']+1,'active':True}
        cache._write_record(self.path,value);self._state=value

    def finish(self,context,cleanup):
        self._check(context);cleanup=unit_views.literal(cleanup)
        if not self._state['active'] or cleanup.parent.parent!=self.path.parent:raise FreshnessHold('cleanup_uncertain')
        original_cargo_cleanup(cleanup,owner=os.geteuid());control=cache._read_record(cleanup.parent/'control.json')
        if (control.get('binding')!=self._state['binding'] or control.get('command')!=self._state['command']
                or control.get('operation')!='run'):raise FreshnessHold('cleanup_uncertain')
        value=self._state|{'active':False};cache._write_record(self.path,value);self._state=value

    def state(self):return copy.deepcopy(self._state)


def planner_snapshot(root,*,owner):
    """Inventory protected mutable Cargo state without changing its metadata.

    Cargo's internal hardlink aliases are retained. An alias outside the owned
    tree, unsupported entry, xattr or inconsistent read refuses the checkpoint.
    This inventory alone grants no artifact/input compatibility authority.
    """
    root=unit_views.literal(root);rows=[];links={};total=0
    def walk(directory,prefix):
        nonlocal total
        info=directory.lstat()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=owner or info.st_mode&0o7000
                or os.listxattr(directory,follow_symlinks=False)):raise FreshnessHold('artifact_provenance_unknown')
        key=unit_views._key(info);rows.append([os.fsencode(prefix).hex(),'directory',list(key)])
        fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NOATIME)
        try:names=sorted(os.listdir(fd),key=os.fsencode)
        finally:os.close(fd)
        for name in names:
            path=directory/name;relative=prefix/name;entry=path.lstat()
            if stat.S_ISDIR(entry.st_mode):walk(path,relative)
            elif stat.S_ISREG(entry.st_mode):
                descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NOATIME|os.O_NONBLOCK)
                try:
                    before=os.fstat(descriptor)
                    if (before.st_uid!=owner or before.st_mode&0o7000 or before.st_size>unit_views.MAX_BYTES
                            or os.listxattr(descriptor)):raise FreshnessHold('artifact_provenance_unknown')
                    digest=hashlib.sha256();size=0
                    while True:
                        block=os.read(descriptor,1024*1024)
                        if not block:break
                        size+=len(block);digest.update(block)
                        if size>before.st_size:raise FreshnessHold('input_census_unknown')
                    if (size!=before.st_size or unit_views._key(os.fstat(descriptor))!=unit_views._key(before)
                            or unit_views._key(path.lstat())!=unit_views._key(before)):
                        raise FreshnessHold('input_census_unknown')
                    rows.append([os.fsencode(relative).hex(),'file',list(unit_views._key(before)),before.st_nlink,digest.hexdigest()])
                    identity=(before.st_dev,before.st_ino);count,expected=links.get(identity,(0,before.st_nlink))
                    if expected!=before.st_nlink:raise FreshnessHold('input_census_unknown')
                    links[identity]=(count+1,expected);total+=size
                finally:os.close(descriptor)
            else:raise FreshnessHold('artifact_provenance_unknown')
            if len(rows)>unit_views.MAX_ENTRIES or total>unit_views.MAX_BYTES:raise FreshnessHold('input_census_unknown')
        if unit_views._key(directory.lstat())!=key:raise FreshnessHold('input_census_unknown')
    walk(root,Path('.'))
    if any(count!=expected for count,expected in links.values()):raise FreshnessHold('artifact_provenance_unknown')
    encoded=json.dumps(rows,separators=(',',':')).encode()
    return {'sha256':hashlib.sha256(encoded).hexdigest(),'entries':len(rows),'bytes':total}


class PublicationStore:
    """Controller-owned producer history and active publication indexes.

    The caller establishes admission and producer identity. Diagnostic snapshots
    are explicit and never claim execution. Recovery checks retained actual
    producers and immutable history; it cannot restore selector closures or
    establish complete input/artifact compatibility authority.
    """
    LIMIT=16*1024**2

    def __init__(self,root,*,binding,command,unit_root,allow_diagnostic=False):
        cache.validate_binding(binding)
        if not isinstance(command,str) or not command or len(command)>256 or type(allow_diagnostic) is not bool:
            raise FreshnessHold('metadata_torn')
        self.root=unit_views.literal(root);self.unit_root=unit_views.literal(unit_root)
        self.owner=os.geteuid();self.controller=os.getpid();self.binding=copy.deepcopy(binding);self.command=command
        self.allow_diagnostic=allow_diagnostic;self._lock=threading.RLock();self._records={};self._rows={};self._reserved=set();self._watches={}
        if self.root.exists() or self.root.is_symlink():raise FreshnessHold('metadata_torn')
        self.root.mkdir(mode=0o700)
        for name in ('objects','records','allocations'):(self.root/name).mkdir(mode=0o700)
        self.objects=self.root/'objects';self._state={'version':1,'binding':self.binding,'command':command,'epoch':0,
            'history':[],'files':{},'namespaces':{},'transition':None,'last_affected':[],'selector_namespaces':[],'shipping_authority':False}
        self._write(self.root/'state.json',self._state)

    @classmethod
    def _retained(cls,root,*,binding,command):
        cache.validate_binding(binding);root=unit_views.literal(root)
        for directory in (root,root/'objects',root/'records',root/'allocations'):
            info=directory.lstat()
            if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid()
                    or stat.S_IMODE(info.st_mode)!=0o700):raise FreshnessHold('artifact_provenance_unknown')
        state=cache._read_record(root/'state.json',max_bytes=cls.LIMIT)
        keys={'version','binding','command','epoch','history','files','namespaces','transition','last_affected','selector_namespaces','shipping_authority'}
        if (set(state)!=keys or type(state['version']) is not int or state['version']!=1 or state['binding']!=binding
                or state['command']!=command or not cache._uuid(command) or state['transition'] is not None
                or state['shipping_authority'] is not False or type(state['history']) is not list
                or len(state['history'])>4096 or any(not cache._uuid(item) for item in state['history'])
                or len(set(state['history']))!=len(state['history']) or type(state['epoch']) is not int
                or state['epoch']!=len(state['history']) or type(state['files']) is not dict or type(state['namespaces']) is not dict
                or type(state['last_affected']) is not list or any(not lookup_inputs.UnitInvalidations._name(item) for item in state['last_affected'])
                or type(state['selector_namespaces']) is not list or len(set(state['selector_namespaces']))!=len(state['selector_namespaces'])
                or any(item not in state['namespaces'] for item in state['selector_namespaces'])):raise FreshnessHold('metadata_torn')
        history=set(state['history'])
        if (set(path.name for path in (root/'objects').iterdir())!=history
                or set(path.name for path in (root/'records').iterdir())!={item+'.json' for item in history}
                or set(path.name for path in (root/'allocations').iterdir())!={item+'.json' for item in history}):
            raise FreshnessHold('cleanup_uncertain')
        records={};rows={};documents=[];active_files={};namespaces={}
        for identity in state['history']:
            record=cache._read_record(root/'records'/(identity+'.json'),max_bytes=cls.LIMIT)
            if (set(record)!={'version','binding','command','producer','publication','evidence','input_closure_complete','shipping_authority'}
                    or type(record['version']) is not int or record['version']!=1 or record['binding']!=binding
                    or not cache._uuid(record['command']) or record['producer']!=identity
                    or record['input_closure_complete'] is not False or record['shipping_authority'] is not False):
                raise FreshnessHold('artifact_provenance_unknown')
            allocation=cache._read_record(root/'allocations'/(identity+'.json'))
            if type(allocation.get('version')) is not int or allocation!={'version':1,'binding':binding,'command':record['command'],'producer':identity,'state':'allocated','shipping_authority':False}:
                raise FreshnessHold('artifact_provenance_unknown')
            evidence=record['evidence'];status=evidence.get('status',{})
            if (set(evidence)!={'kind','status','original_cleanup','cleanup_sha256'} or evidence['kind']!='unit'
                    or evidence['original_cleanup'] is not True or evidence['cleanup_sha256']!=hashlib.sha256(b'all-descendants-reaped\n').hexdigest()
                    or set(status)!={'version','binding','command','ordinal','returncode','exec_observed'}
                    or type(status['version']) is not int or status['version']!=3 or status['binding']!=binding
                    or status['command']!=record['command'] or type(status['ordinal']) is not int or not 0<=status['ordinal']<4096
                    or type(status['returncode']) is not int or status['returncode']!=0 or status['exec_observed'] is not True):
                raise FreshnessHold('artifact_provenance_unknown')
            publication=record['publication']
            if (type(publication) is not dict or set(publication)!={'manifest','files','roots','action','cargo_target','generated_root'}
                    or publication['action'] not in ('library','check','binary','test','build-script','generated')
                    or type(publication['files']) is not list or len(set(publication['files']))!=len(publication['files'])
                    or type(publication['roots']) is not list or len(set(publication['roots']))!=len(publication['roots'])
                    or bool(publication['roots'])!=(publication['action']=='generated')):raise FreshnessHold('artifact_provenance_unknown')
            publication=dict(publication,root=root/'objects'/identity);family=cls._family(publication)
            manifest=unit_views.inventory(publication['root'],owner=os.geteuid(),sealed=True)
            names={os.fsdecode(bytes.fromhex(row[0])):row for row in manifest if row[1]=='file'}
            if manifest!=publication['manifest'] or set(names)!=set(publication['files']):raise FreshnessHold('artifact_provenance_unknown')
            for namespace in publication['roots']:
                unit_views.relative(namespace)
                for previous,producer in list(namespaces.items()):
                    if previous==namespace:
                        if cls._family(records[producer])!=family:raise FreshnessHold('artifact_provenance_unknown')
                        active_files={name:item for name,item in active_files.items() if not name.startswith(namespace+'/')}
                    elif previous.startswith(namespace+'/') or namespace.startswith(previous+'/'):raise FreshnessHold('artifact_provenance_unknown')
                namespaces[namespace]=identity
            for name in names:
                previous=active_files.get(name)
                if previous is not None and cls._family(records[previous])!=family:raise FreshnessHold('artifact_provenance_unknown')
                for namespace,producer in namespaces.items():
                    if name.startswith(namespace+'/') and producer!=identity and cls._family(records[producer])!=family:
                        raise FreshnessHold('artifact_provenance_unknown')
                active_files[name]=identity
            records[identity]=publication;rows[identity]=names;documents.append(record)
        if state['files']!=active_files or state['namespaces']!=namespaces:raise FreshnessHold('artifact_provenance_unknown')
        digest=hashlib.sha256(json.dumps({'state':state,'records':documents,
            'objects':planner_snapshot(root/'objects',owner=os.geteuid())},sort_keys=True,separators=(',',':')).encode()).hexdigest()
        return state,records,rows,digest

    @classmethod
    def checkpoint(cls,root,*,binding,command):
        return cls._retained(root,binding=binding,command=command)[3]

    @classmethod
    def recover(cls,root,*,binding,command,predecessor,unit_root,expected_sha256):
        state,records,rows,digest=cls._retained(root,binding=binding,command=predecessor)
        if digest!=expected_sha256 or not cache._uuid(command) or command==predecessor:
            raise FreshnessHold('artifact_provenance_unknown')
        if state['selector_namespaces'] or state['namespaces']:
            # Restoring callback closures from an old process is not recovery.
            # Generated namespace reuse needs durable selector revalidation.
            raise FreshnessHold('input_census_unknown')
        store=object.__new__(cls);store.root=unit_views.literal(root);store.objects=store.root/'objects'
        store.unit_root=unit_views.literal(unit_root);store.owner=os.geteuid();store.controller=os.getpid()
        store.binding=copy.deepcopy(binding);store.command=command;store.allow_diagnostic=False
        store._lock=threading.RLock();store._records=records;store._rows=rows;store._reserved=set();store._watches={}
        store._state=copy.deepcopy(state);store._state['command']=command
        store._write(store.root/'state.json',store._state)
        return store

    def history_commands(self):
        with self._lock:
            self._check()
            return sorted({cache._read_record(self.root/'records'/(identity+'.json'),max_bytes=self.LIMIT)['command']
                for identity in self._state['history']})

    def _write(self,path,value):
        if len(json.dumps(value,separators=(',',':')).encode())>self.LIMIT:raise FreshnessHold('metadata_torn')
        cache._write_record(path,value)

    def _check(self,*,active=True):
        if (os.getpid()!=self.controller or os.geteuid()!=self.owner
                or cache._read_record(self.root/'state.json',max_bytes=self.LIMIT)!=self._state
                or active and self._state['transition'] is not None):raise FreshnessHold('metadata_torn')

    def state(self):
        with self._lock:self._check(active=False);return copy.deepcopy(self._state)

    def allocate(self):
        with self._lock:
            self._check()
            if len(self._reserved)+len(self._records)>=4096:raise FreshnessHold('invalidation_unsafe')
            identity=str(uuid.uuid4());path=self.objects/identity
            if path.exists() or path.is_symlink():raise FreshnessHold('invalidation_unsafe')
            self._write(self.root/'allocations'/(identity+'.json'),{'version':1,'binding':self.binding,
                'command':self.command,'producer':identity,'state':'allocated','shipping_authority':False})
            self._reserved.add(identity);return identity,path

    def watch(self,namespace,callback):
        with self._lock:
            self._check();unit_views.relative(namespace)
            if namespace not in self._state['namespaces'] or not callable(callback):raise FreshnessHold('input_census_unknown')
            following=copy.deepcopy(self._state);following['selector_namespaces']=sorted(set(following['selector_namespaces'])|{namespace})
            self._write(self.root/'state.json',following);self._state=following
            self._watches.setdefault(namespace,[]).append(callback)

    @staticmethod
    def _family(publication):
        target=publication['cargo_target']
        return (publication['action'],target['package_id'],target['target_name'],tuple(target['target_kind']),
            target['manifest_path'],target['source_path'],tuple(publication['roots']))

    def _evidence(self,scope,ordinal):
        if scope is None:
            if not self.allow_diagnostic or ordinal is not None:raise FreshnessHold('artifact_provenance_unknown')
            return {'kind':'controller-diagnostic','original_cleanup':False}
        scope=unit_views.literal(scope)
        if scope.parent!=self.unit_root or type(ordinal) is not int or not 0<=ordinal<4096:
            raise FreshnessHold('artifact_provenance_unknown')
        original_cargo_cleanup(scope/'cleanup',owner=self.owner)
        status=cache._read_record(scope/'exit-status.json')
        if (type(status.get('version')) is not int or status['version']!=3 or status.get('binding')!=self.binding
                or status.get('command')!=self.command or status.get('ordinal')!=ordinal
                or type(status.get('returncode')) is not int or status['returncode']!=0
                or status.get('exec_observed') is not True):raise FreshnessHold('artifact_provenance_unknown')
        return {'kind':'unit','status':status,'original_cleanup':True,
            'cleanup_sha256':hashlib.sha256(b'all-descendants-reaped\n').hexdigest()}

    def publish(self,identity,publication,*,scope=None,ordinal=None,materialize=None):
        with self._lock:
            self._check()
            if identity not in self._reserved or publication['root']!=self.objects/identity:
                raise FreshnessHold('artifact_provenance_unknown')
            publication=copy.deepcopy(publication);family=self._family(publication)
            rows=unit_views.inventory(publication['root'],owner=self.owner,sealed=True)
            names={os.fsdecode(bytes.fromhex(row[0])):row for row in rows if row[1]=='file'}
            if (rows!=publication['manifest'] or set(names)!=set(publication['files'])
                    or len(set(publication['files']))!=len(publication['files'])
                    or publication['action'] not in ('library','check','binary','test','build-script','generated')
                    or bool(publication['roots'])!=(publication['action']=='generated')):
                raise FreshnessHold('artifact_provenance_unknown')
            roots=publication['roots']
            if (type(roots) is not list or len(roots)>64 or len(set(roots))!=len(roots)
                    or any(a.startswith(b+'/') or b.startswith(a+'/') for i,a in enumerate(roots) for b in roots[i+1:])):
                raise FreshnessHold('artifact_provenance_unknown')
            for root in roots:unit_views.relative(root)
            following=copy.deepcopy(self._state);files=following['files'];namespaces=following['namespaces']
            for root in publication['roots']:
                for other,previous in list(namespaces.items()):
                    if root==other:
                        if self._family(self._records[previous])!=family:raise FreshnessHold('artifact_provenance_unknown')
                        files={name:producer for name,producer in files.items() if not name.startswith(root+'/')}
                    elif root.startswith(other+'/') or other.startswith(root+'/'):raise FreshnessHold('artifact_provenance_unknown')
                namespaces[root]=identity
            for name in names:
                previous=files.get(name)
                if previous is not None and self._family(self._records[previous])!=family:
                    raise FreshnessHold('artifact_provenance_unknown')
                for root,producer in namespaces.items():
                    if name.startswith(root+'/') and producer!=identity and self._family(self._records[producer])!=family:
                        raise FreshnessHold('artifact_provenance_unknown')
                files[name]=identity
            evidence=self._evidence(scope,ordinal)
            document={key:value for key,value in publication.items() if key!='root'}
            record={'version':1,'binding':self.binding,'command':self.command,'producer':identity,
                'publication':document,'evidence':evidence,'input_closure_complete':False,'shipping_authority':False}
            self._write(self.root/'records'/(identity+'.json'),record)
            pending=copy.deepcopy(self._state);pending['transition']={'producer':identity,'phase':'prepared'}
            self._write(self.root/'state.json',pending);self._state=pending
            self._records[identity]=publication;self._rows[identity]=names;affected=set()
            for root in publication['roots']:
                for callback in self._watches.get(root,[]):
                    changed=callback(copy.deepcopy(publication))
                    if (type(changed) is not list or len(changed)>4096
                            or any(not lookup_inputs.UnitInvalidations._name(name) for name in changed)):
                        raise FreshnessHold('input_census_unknown')
                    affected.update(changed)
            if materialize is not None:materialize(copy.deepcopy(publication))
            if unit_views.inventory(publication['root'],owner=self.owner,sealed=True)!=rows:
                raise FreshnessHold('artifact_provenance_unknown')
            following.update(files=files,epoch=following['epoch']+1,history=[*following['history'],identity],
                transition=None,last_affected=sorted(affected))
            self._write(self.root/'state.json',following);self._state=following;self._reserved.remove(identity)
            return identity

    def record(self,identity):
        with self._lock:
            self._check()
            if identity not in self._state['history']:raise FreshnessHold('artifact_provenance_unknown')
            return copy.deepcopy(self._records[identity])

    def snapshot(self,*,historical=False):
        with self._lock:
            self._check();selected=(set(self._state['history']) if historical
                else set(self._state['files'].values())|set(self._state['namespaces'].values()))
            publications={identity:copy.deepcopy(self._records[identity]) for identity in selected};files={}
            for name,identity in self._state['files'].items():
                files[name]=(identity,copy.deepcopy(self._rows[identity][name]))
            return publications,files


class GeneratedForests:
    """Immutable declared generated roots from an original completed step.

    These records establish snapshot provenance, not complete compiler input or
    artifact compatibility. Active producer changes remain refused at consume.
    """
    WORKSPACE=Path('/workspace')
    _WORKER_UID=1000

    def __init__(self,lease,outputs):
        cache.validate_outputs(outputs);self.lease=lease;self.outputs=dict(outputs)
        self.root=lease.target.parent/'generated-forests'
        if self.root.exists() or self.root.is_symlink():
            info=self.root.lstat()
            if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid() or stat.S_IMODE(info.st_mode)!=0o700:
                raise FreshnessHold('artifact_provenance_unknown')
        else:self.root.mkdir(mode=0o700)
        for name in ('objects','records'):
            path=self.root/name
            if not path.exists():path.mkdir(mode=0o700)
            info=path.lstat()
            if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid() or stat.S_IMODE(info.st_mode)!=0o700:
                raise FreshnessHold('artifact_provenance_unknown')

    def public(self):
        result={}
        for name,kind in sorted(self.outputs.items()):
            path=unit_views.literal(self.WORKSPACE/name)
            if kind=='directory':result[name]=planner_snapshot(path,owner=self._WORKER_UID)
            else:
                mode,size,digest=unit_views._read(path,self._WORKER_UID,False)
                result[name]={'sha256':hashlib.sha256(json.dumps([list(unit_views._key(path.stat())),digest],separators=(',',':')).encode()).hexdigest(),
                    'entries':1,'bytes':size}
        return result

    def load(self,refs,*,require_public=True):
        if type(refs) is not list or len(refs)>1:raise FreshnessHold('metadata_torn')
        if not refs:
            # Initial controller-created empty placeholders have no generated
            # producer authority and are already present in the source view.
            for name,kind in self.outputs.items():
                path=self.WORKSPACE/name
                if kind=='directory':
                    fd=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NOATIME)
                    try:nonempty=bool(os.listdir(fd))
                    finally:os.close(fd)
                else:nonempty=path.stat().st_size!=0
                if nonempty:raise FreshnessHold('artifact_provenance_unknown')
            return [],None
        reference=refs[0]
        if (type(reference) is not dict or set(reference)!={'producer','sha256'}
                or not cache._uuid(reference['producer']) or not cache._digest(reference['sha256'])):
            raise FreshnessHold('metadata_torn')
        record=cache._read_record(self.root/'records'/(reference['producer']+'.json'),max_bytes=PublicationStore.LIMIT)
        encoded=json.dumps(record,sort_keys=True,separators=(',',':')).encode()
        if (hashlib.sha256(encoded).hexdigest()!=reference['sha256']
                or set(record)!={'version','binding','producer','command','claim','scope_id','outputs','public','manifest','sealed','input_closure_complete','shipping_authority'}
                or type(record['version']) is not int or record['version']!=1 or record['binding']!=self.lease.binding
                or record['producer']!=reference['producer'] or record['outputs']!=self.outputs
                or not cache._uuid(record['command']) or not cache._uuid(record['claim']) or not cache._uuid(record['scope_id'])
                or record['input_closure_complete'] is not False or record['shipping_authority'] is not False):
            raise FreshnessHold('artifact_provenance_unknown')
        completed=self.lease.completed(record['command'])
        if (reference not in completed['forests'] or completed['claim']!=record['claim']
                or completed['scope']['id']!=record['scope_id']):raise FreshnessHold('artifact_provenance_unknown')
        data=unit_views.literal(self.root/'objects'/record['producer'])
        if (unit_views.inventory(data,owner=os.geteuid(),sealed=True)!=record['manifest']
                or planner_snapshot(data,owner=os.geteuid())!=record['sealed']
                or require_public and self.public()!=record['public']):raise FreshnessHold('input_census_unknown')
        bindings=[]
        for name,kind in sorted(self.outputs.items()):
            raw=os.fsencode(name).hex();original=data/name
            expected=[raw,'tree',unit_views.inventory(original,owner=os.geteuid(),sealed=True)] if kind=='directory' else [raw,'file',*unit_views._read(original,os.geteuid(),True)]
            bindings.append([raw,str(original),expected])
        return bindings,record

    def publish(self,scope):
        if not self.outputs:return []
        before=self.public();prior=self.lease._active(require_scope=True)['forests']
        if prior:
            _bindings,record=self.load(prior,require_public=False)
            if record['public']==before:return prior
        identity=str(uuid.uuid4());data=self.root/'objects'/identity;files=[];directories=[];times={}
        for name,kind in sorted(self.outputs.items()):
            original=self.WORKSPACE/name
            if kind=='directory':
                for row in unit_views.inventory(original,owner=self._WORKER_UID,sealed=False):
                    path=Path(name)/os.fsdecode(bytes.fromhex(row[0]));key=os.fsdecode(path)
                    (directories if row[1]=='directory' else files).append(key);times[key]=(self.WORKSPACE/path).stat()
            else:files.append(name);times[name]=original.stat()
        prefixes=sorted({str(parent) for name in self.outputs for parent in Path(name).parents})
        manifest=unit_views.copy_outputs(self.WORKSPACE,data,files,owner=self._WORKER_UID,controller=os.geteuid(),
            directories=directories,controller_prefixes=prefixes)
        for name,info in sorted(times.items(),key=lambda item:len(Path(item[0]).parts),reverse=True):
            os.utime(data/name,ns=(info.st_atime_ns,info.st_mtime_ns),follow_symlinks=False)
            descriptor=os.open(data/name,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
            try:os.fsync(descriptor)
            finally:os.close(descriptor)
        if self.public()!=before:raise FreshnessHold('input_census_unknown')
        record={'version':1,'binding':self.lease.binding,'producer':identity,'command':self.lease.command,
            'claim':self.lease.claim,'scope_id':scope.directory.name.removeprefix('worker-'),'outputs':self.outputs,
            'public':before,'manifest':manifest,'sealed':planner_snapshot(data,owner=os.geteuid()),
            'input_closure_complete':False,'shipping_authority':False}
        cache._write_record(self.root/'records'/(identity+'.json'),record)
        descriptor=os.open(self.root/'objects',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
        return [{'producer':identity,'sha256':hashlib.sha256(json.dumps(record,sort_keys=True,separators=(',',':')).encode()).hexdigest()}]


class ExecutionLease:
    """Durable ownership handoff across host-acknowledged CI commands.

    Only original scope cleanup closes a claim. Reopening validates retained
    protected state; this is not a complete-input/artifact compatibility proof.
    """
    LIMIT=16*1024**2

    def __init__(self,target,binding,command,predecessor):
        self.target=_source(target,binding,command);self.binding=copy.deepcopy(binding);self.command=command
        self.path=self.target.parent/'cargo-execution-state.json';self.owner=os.getpid();self.scope=None
        if not cache._uuid(command) or predecessor is not None and not cache._uuid(predecessor):raise FreshnessHold('metadata_torn')
        if self.path.exists() or self.path.is_symlink():
            prior=self._read(self.path,binding)
            if (prior['phase']!='closed' or predecessor!=prior['command'] or command==predecessor
                    or prior['scope'] is None or prior['scope']['cleanup_sha256']!=self.cleanup_digest()):
                raise FreshnessHold('cleanup_uncertain')
            self._completed_check(prior)
            root=prior['root'];checkpoint=prior['checkpoint'];forests=prior['forests']
            if root is not None:self._verify_executor(self.target,binding,root,checkpoint)
        else:
            if predecessor is not None:raise FreshnessHold('cleanup_uncertain')
            root=None;checkpoint=None;forests=[]
        self.claim=str(uuid.uuid4())
        self._state={'version':2,'binding':self.binding,'command':command,'predecessor':predecessor,
            'claim':self.claim,'phase':'active','scope':None,'root':root,'checkpoint':checkpoint,
            'forests':forests,'completion_sha256':None,'input_closure_complete':False,'shipping_authority':False}
        cache._write_record(self.path,self._state)

    @staticmethod
    def cleanup_digest():return cache.execution_cleanup_digest()

    @classmethod
    def _read(cls,path,binding):
        return cache.validate_execution_state(cache._read_record(path,max_bytes=cls.LIMIT),binding)

    def handle(self):
        return {'version':2,'target':str(self.target),'binding':self.binding,'command':self.command,'claim':self.claim}

    @classmethod
    def attach(cls,handle):
        if (type(handle) is not dict or set(handle)!={'version','target','binding','command','claim'}
                or type(handle['version']) is not int or handle['version']!=2 or not cache._uuid(handle['claim'])):
            raise FreshnessHold('metadata_torn')
        lease=object.__new__(cls);lease.target=_source(handle['target'],handle['binding'],handle['command'])
        lease.path=lease.target.parent/'cargo-execution-state.json';lease.binding=copy.deepcopy(handle['binding'])
        lease.command=handle['command'];lease.claim=handle['claim'];lease.owner=os.getpid();lease.scope=None
        lease._active(require_scope=True);return lease

    def completed(self,command):
        if not cache._uuid(command):raise FreshnessHold('metadata_torn')
        record=cache._read_record(self.target.parent/'completed-ci-commands'/(command+'.json'),max_bytes=self.LIMIT)
        return cache.validate_completed_execution(record,self.binding,command)

    def _completed_check(self,state):
        cache.validate_execution_completion(state,self.completed(state['command']))

    def generated_inputs(self,outputs):
        value=self._active(require_scope=True);bindings,_record=GeneratedForests(self,outputs).load(value['forests'])
        return bindings,value['forests']

    def _active(self,*,require_scope=False):
        _source(self.target,self.binding,self.command)
        value=self._read(self.path,self.binding)
        if (os.getpid()!=self.owner or value['phase']!='active' or value['command']!=self.command or value['claim']!=self.claim
                or require_scope and value['scope'] is None
                or (self.target.parent/'execution-dispositions').exists()
                or (self.target.parent/'execution-dispositions').is_symlink()):raise FreshnessHold('cleanup_uncertain')
        return value

    def bind_scope(self,scope):
        value=self._active()
        if value['scope'] is not None or scope.owner!=os.getpid() or scope.closed or scope._process is not None:
            raise FreshnessHold('cleanup_uncertain')
        identity=scope.directory.name.removeprefix('worker-')
        if not cache._uuid(identity):raise FreshnessHold('cleanup_uncertain')
        value['scope']={'id':identity,'receipt':str(scope.receipt),'cleanup_sha256':None}
        cache._write_record(self.path,value);self.scope=scope

    @staticmethod
    def _identity(root):
        info=root.lstat()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid() or info.st_gid!=1001
                or stat.S_IMODE(info.st_mode)!=0o710):raise FreshnessHold('artifact_provenance_unknown')
        return [info.st_dev,info.st_ino,info.st_uid,info.st_gid,info.st_mode]

    @classmethod
    def _root(cls,target,record):
        if (type(record) is not dict or set(record)!={'name','identity'} or type(record['name']) is not str
                or re.fullmatch(r'\.cargo-execution-[0-9a-f-]{36}',record['name']) is None
                or not cache._uuid(record['name'][len('.cargo-execution-'):])):raise FreshnessHold('metadata_torn')
        root=unit_views.literal(target/record['name'])
        if cls._identity(root)!=record['identity']:raise FreshnessHold('artifact_provenance_unknown')
        return root

    @classmethod
    def _snapshot(cls,root,binding,command):
        if (root/'invalidations').exists() or (root/'invalidations').is_symlink():raise FreshnessHold('input_census_unknown')
        commands=sorted((path for path in root.iterdir() if re.fullmatch(r'command-[0-9]+',path.name)),key=lambda p:int(p.name[8:]))
        if len(commands)>1024 or [path.name for path in commands]!=['command-'+str(i) for i in range(len(commands))]:
            raise FreshnessHold('metadata_torn')
        return {'command':command,'store_sha256':PublicationStore.checkpoint(root/'publications',binding=binding,command=command),
            'planner':planner_snapshot(root/'planner-target',owner=1001),'home':planner_snapshot(root/'home',owner=1001),
            'cargo_home':planner_snapshot(root/'cargo-home',owner=1001),
            'input_observations':planner_snapshot(root/'input-observations',owner=os.geteuid()),
            'commands':[planner_snapshot(path,owner=os.geteuid()) for path in commands],
            'forests':cache._read_record(root/'input-forests.json')['forests'],
            'outputs':cache._read_record(root/'input-forests.json')['outputs']}

    @classmethod
    def _verify_executor(cls,target,binding,record,checkpoint):
        root=cls._root(target,record)
        if (type(checkpoint) is not dict or not cache._uuid(checkpoint.get('command'))
                or cache._read_record(root/'control.json')!={'version':1,'kind':'cargo-execution','binding':binding,
                    'command':checkpoint['command'],'shipping_authority':False,'state':'closed'}
                or cls._snapshot(root,binding,checkpoint['command'])!=checkpoint):raise FreshnessHold('artifact_provenance_unknown')
        return root

    def retained(self):
        value=self._active(require_scope=True)
        if value['root'] is None:return None,None
        return self._verify_executor(self.target,self.binding,value['root'],value['checkpoint']),value['checkpoint']

    def created(self,root):
        value=self._active(require_scope=True)
        if value['root'] is not None:raise FreshnessHold('cleanup_uncertain')
        value['root']={'name':root.name,'identity':self._identity(root)}
        # A root without a completed checkpoint is valid only in this active
        # claim; another command cannot adopt it after a partial execution.
        value['checkpoint']={'command':self.command,'creating':True}
        cache._write_record(self.path,value)

    def _original_stopped_scope(self,scope,stopped,*,allow_recorded=False):
        _source(self.target,self.binding,self.command)
        value=self._read(self.path,self.binding)
        expected={'id':scope.directory.name.removeprefix('worker-'),'receipt':str(scope.receipt),
            'cleanup_sha256':self.cleanup_digest() if value['phase'] in ('closed','abandoned') else None}
        if (self.owner!=os.getpid() or value['command']!=self.command or value['claim']!=self.claim
                or self.scope is not scope or stopped is None or not stopped.matches(scope) or not scope.closed
                or value['scope']!=expected or not allow_recorded and value['phase']!='active'):
            raise FreshnessHold('cleanup_uncertain')
        if not allow_recorded and ((self.target.parent/'execution-dispositions').exists()
                or (self.target.parent/'execution-dispositions').is_symlink()):raise FreshnessHold('cleanup_uncertain')
        original_cargo_cleanup(scope.receipt,owner=os.geteuid())
        return value

    def abandon_nonreusable(self,scope,stopped):
        """Persist only original verified cleanup, without producer completion."""
        value=self._original_stopped_scope(scope,stopped,allow_recorded=True)
        if value['phase']=='closed':
            self._completed_check(value);return self.completed(self.command)
        directory=self.target.parent/'execution-dispositions'
        if not directory.exists():directory.mkdir(mode=0o700)
        cache._execution_directory(directory,self.target.parent.stat().st_dev)
        path=directory/(self.command+'.json')
        if value['phase']=='abandoned':
            return cache.validate_abandoned_execution(cache._read_record(path),value)
        record={'version':1,'kind':'ci-execution-abandoned','binding':self.binding,'command':self.command,
            'claim':self.claim,'predecessor':value['predecessor'],
            'scope':{**value['scope'],'cleanup_sha256':self.cleanup_digest()},'state_sha256':cache._record_digest(value),
            'input_closure_complete':False,'shipping_authority':False}
        cache.validate_abandoned_execution(record,value)
        if path.exists() or path.is_symlink():
            if cache._read_record(path)!=record:raise FreshnessHold('cleanup_uncertain')
        else:cache._write_record(path,record)
        following={**value,'version':3,'phase':'abandoned','scope':record['scope'],
            'abandonment_sha256':cache._record_digest(record)}
        cache.validate_execution_state(following,self.binding);cache._write_record(self.path,following)
        return record

    def finish(self,scope,stopped,*,outputs=None,invoked_cargo=False,producer_succeeded=True):
        value=self._original_stopped_scope(scope,stopped)
        forests=GeneratedForests(self,{} if outputs is None else outputs)
        if type(producer_succeeded) is not bool:raise FreshnessHold('artifact_provenance_unknown')
        if invoked_cargo or not producer_succeeded:forests.load(value['forests']);following_forests=value['forests']
        else:following_forests=forests.publish(scope)
        if value['root'] is not None:
            root=self._root(self.target,value['root']);control=cache._read_record(root/'control.json')
            expected={'version':1,'kind':'cargo-execution','binding':self.binding,'command':self.command,'shipping_authority':False,'state':'active'}
            if control==expected:
                checkpoint=self._snapshot(root,self.binding,self.command)
                cache._write_record(root/'control.json',expected|{'state':'closed'});value['checkpoint']=checkpoint
            else:self._verify_executor(self.target,self.binding,value['root'],value['checkpoint'])
        value['scope']['cleanup_sha256']=self.cleanup_digest();value['phase']='closed';value['forests']=following_forests
        record={'version':1,'binding':self.binding,'command':self.command,'claim':self.claim,'predecessor':value['predecessor'],
            'scope':value['scope'],'forests':following_forests,'input_closure_complete':False,'shipping_authority':False}
        directory=self.target.parent/'completed-ci-commands'
        if not directory.exists():directory.mkdir(mode=0o700)
        info=directory.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid() or stat.S_IMODE(info.st_mode)!=0o700:
            raise FreshnessHold('artifact_provenance_unknown')
        path=directory/(self.command+'.json')
        if path.exists() or path.is_symlink():raise FreshnessHold('cleanup_uncertain')
        cache._write_record(path,record)
        value['completion_sha256']=hashlib.sha256(json.dumps(record,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        cache._write_record(self.path,value)
        return {'version':2,'kind':'ci-cargo-execution-checkpoint','binding':self.binding,'command':self.command,
            'claim':self.claim,'predecessor':value['predecessor'],'scope_id':value['scope']['id'],
            'retained_executor':value['root'] is not None,'completion_sha256':value['completion_sha256'],
            'generated_forests':following_forests,'input_closure_complete':False,'shipping_authority':False}


class CargoGraph:
    """Bind controller-captured Cargo metadata to frozen workspace targets.

    The caller must establish the pinned Cargo invocation and original cleanup.
    This object does not authenticate arbitrary metadata, resolve dependencies,
    admit opaque tools, or issue input/reuse/CI authority.
    """
    def __init__(self,source,metadata,*,owner):
        self.root=unit_views.literal(source);self.owner=owner;self._files={};self._directories={};self.packages={}
        try:
            if (type(owner) is not int or not 0<=owner<2**32
                    or type(metadata) is not bytes or not 0<len(metadata)<=2*1024**2):raise ValueError()
            data=json.loads(metadata,object_pairs_hook=cache._unique_object)
            if (type(data) is not dict or type(data.get('version')) is not int or data['version']!=1
                    or data.get('workspace_root')!='/workspace' or data.get('target_directory')!='/workspace/target'
                    or type(data.get('packages')) is not list or not 0<len(data['packages'])<=1024
                    or type(data.get('workspace_members')) is not list):raise ValueError()
            self.metadata_sha256=hashlib.sha256(metadata).hexdigest();self._remember(self.root/'Cargo.lock')
            ids=set();targets=0
            for package in data['packages']:
                identity=package['id'];name=package['name'];version=package['version'];manifest=package['manifest_path']
                if (type(identity) is not str or not 0<len(identity)<=4096 or identity in ids
                        or type(name) is not str or re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,127}',name) is None
                        or type(version) is not str or not 0<len(version)<=128 or package.get('source') is not None
                        or type(package.get('targets')) is not list or not 0<len(package['targets'])<=128):raise ValueError()
                physical=self._virtual(manifest)
                if physical.name!='Cargo.toml':raise ValueError()
                manifest_digest=self._remember(physical);directory=manifest.rsplit('/',1)[0]
                if directory in self.packages:raise ValueError()
                admitted=[];seen=set()
                for target in package['targets']:
                    path=target['src_path'];target_name=target['name'];kinds=target['kind'];types=target['crate_types'];edition=target['edition']
                    if (type(target_name) is not str or re.fullmatch(r'[A-Za-z_][A-Za-z0-9_-]{0,127}',target_name) is None
                            or type(kinds) is not list or not 0<len(kinds)<=8 or any(type(v) is not str for v in kinds)
                            or type(types) is not list or not 0<len(types)<=8 or any(type(v) is not str for v in types)
                            or edition not in ('2015','2018','2021','2024')):raise ValueError()
                    key=(path,target_name,tuple(kinds),tuple(types))
                    if key in seen:raise ValueError()
                    seen.add(key);digest=self._remember(self._virtual(path));targets+=1
                    if targets>4096:raise ValueError()
                    admitted.append({'source':path,'name':target_name,'kinds':tuple(kinds),'types':tuple(types),
                        'edition':edition,'source_sha256':digest})
                self.packages[directory]={'id':identity,'name':name,'version':version,'manifest':manifest,
                    'manifest_sha256':manifest_digest,'targets':admitted};ids.add(identity)
            members=data['workspace_members']
            if any(type(value) is not str for value in members) or len(members)!=len(set(members)) or set(members)!=ids:raise ValueError()
            self.target_count=targets;self._verify()
        except (OSError,ValueError,KeyError,TypeError,AttributeError,UnicodeError):
            raise FreshnessHold('input_census_unknown') from None

    def _virtual(self,value):
        if (type(value) is not str or not value.startswith('/workspace/') or len(value)>4096
                or any(part in ('','..','.','.git') for part in value.split('/')[1:])
                or value.startswith('/workspace/target/')):raise FreshnessHold('input_census_unknown')
        return unit_views.literal(self.root/value[len('/workspace/'):])

    def _remember(self,path):
        for parent in [path.parent,*path.parent.parents]:
            if parent==self.root.parent:break
            info=parent.lstat()
            if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=self.owner
                    or stat.S_IMODE(info.st_mode)!=0o555):raise FreshnessHold('input_census_unknown')
            identity=unit_views._key(info)
            if parent in self._directories and self._directories[parent]!=identity:raise FreshnessHold('input_census_unknown')
            self._directories[parent]=identity
        mode,size,digest=unit_views._read(path,self.owner,True)
        identity=unit_views._key(path.lstat())
        if path in self._files and self._files[path]!=(identity,(mode,size,digest)):raise FreshnessHold('input_census_unknown')
        self._files[path]=(identity,(mode,size,digest));return digest

    def _verify(self):
        for path,identity in self._directories.items():
            if unit_views._key(path.lstat())!=identity:raise FreshnessHold('input_census_unknown')
        for path,(identity,content) in list(self._files.items()):
            if unit_views._key(path.lstat())!=identity or unit_views._read(path,self.owner,True)!=content:
                raise FreshnessHold('input_census_unknown')

    def admit(self,request,*,host_triple):
        try:
            self._verify();environment=request[2]
            directory=environment[b'CARGO_MANIFEST_DIR'].decode();package=self.packages.get(directory)
            if (package is None or environment.get(b'CARGO_PKG_NAME')!=package['name'].encode()
                    or environment.get(b'CARGO_PKG_VERSION')!=package['version'].encode()
                    or environment.get(b'CARGO_MANIFEST_PATH',package['manifest'].encode())!=package['manifest'].encode()):raise ValueError()
            mapped=rustc_unit(request,package=package['name'].encode(),host_triple=host_triple)
            if mapped['kind']!='compile' or environment.get(b'CARGO_CRATE_NAME',mapped['crate_name'])!=mapped['crate_name']:raise ValueError()
            matches=[]
            for target in package['targets']:
                if (mapped['source']!=target['source'].encode() or mapped['crate_name']!=target['name'].replace('-','_').encode()
                        or mapped['fields'][b'--edition']!=target['edition'].encode()):continue
                action=mapped['action'];kinds=set(target['kinds'])
                if (action in ('library','check') and kinds.intersection(('lib','rlib')) and mapped['crate_type'].decode() in target['types']
                        or action=='build-script' and kinds=={'custom-build'} and target['types']==('bin',)
                        or action=='binary' and kinds.intersection(('bin','example','test','bench')) and 'bin' in target['types']
                        or action=='test' and kinds.intersection(('lib','rlib','bin','example','test','bench'))):matches.append(target)
            if len(matches)!=1:raise ValueError()
            target=matches[0]
            return mapped|{'cargo_target':{'package_id':package['id'],'package_name':package['name'],'version':package['version'],
                'manifest_path':package['manifest'],'manifest_sha256':package['manifest_sha256'],
                'target_name':target['name'],'target_kind':list(target['kinds']),'source_path':target['source'],
                'source_sha256':target['source_sha256'],'metadata_sha256':self.metadata_sha256}}
        except (OSError,ValueError,KeyError,TypeError,AttributeError,UnicodeError):
            raise FreshnessHold('input_census_unknown') from None


def generated_publication(publications,name,package_id):
    try:
        matches=[publication for publication in publications.values() if publication.get('action')=='generated'
            and publication.get('roots')==[name] and publication['cargo_target']['package_id']==package_id]
        if len(matches)!=1:raise ValueError()
        return matches[0]
    except (ValueError,TypeError,KeyError,AttributeError):raise cargo_hold('planner_cargo_artifact') from None


def _declared_publication(graph,publication):
    target=publication['cargo_target'];matches=[]
    action=publication['action'];kinds=set(target['target_kind'])
    if not (action in ('library','check') and kinds.intersection(('lib','rlib'))
            or action in ('build-script','generated') and kinds=={'custom-build'}
            or action=='binary' and kinds.intersection(('bin','example','test','bench'))
            or action=='test' and kinds.intersection(('lib','rlib','bin','example','test','bench'))):
        raise cargo_hold('planner_cargo_artifact')
    for package in graph.packages.values():
        if (target['package_id']!=package['id'] or target['package_name']!=package['name']
                or target['version']!=package['version'] or target['manifest_path']!=package['manifest']
                or target['manifest_sha256']!=package['manifest_sha256']):continue
        matches.extend(candidate for candidate in package['targets']
            if candidate['name']==target['target_name'] and list(candidate['kinds'])==target['target_kind']
            and candidate['source']==target['source_path'] and candidate['source_sha256']==target['source_sha256'])
    if len(matches)!=1:raise cargo_hold('planner_cargo_artifact')
    return target


def dispatch_request(request,*,graph,planner_target,publications,files,host_triple=b'x86_64-unknown-linux-gnu'):
    """Map workspace execution through admitted targets and owned publications.

    The caller owns admission, original producer cleanup and sealed inventories;
    it must validate every selected artifact before mounting it. This dispatcher
    grants neither compatibility/reuse nor complete input or CI authority.
    """
    try:
        original=request
        file,argv,environment,cwd,search,ignored,blocked=request
        if not argv or argv[0]!=file:raise ValueError()
        if file==b'rustc':
            # The admitted runtime's immutable first PATH entry contains the
            # pinned compiler. Normalize only for decoding/graph admission;
            # the sealed original request still executes unchanged in its view.
            if (search not in (b'/toolchain/bin',b'/toolchain/bin:/usr/bin:/bin')
                    or environment.get(b'PATH')!=search):raise ValueError()
            file=b'/toolchain/bin/rustc'
            request=(file,[file,*argv[1:]],environment,cwd,search,ignored,blocked)
        if file==b'/toolchain/bin/rustc':
            candidate=rustc_unit(request,package=environment.get(b'CARGO_PKG_NAME',b'cargo-information'),host_triple=host_triple)
            if candidate['kind']=='information':return candidate
            if type(graph) is not CargoGraph:raise ValueError()
            admitted=graph.admit(request,host_triple=host_triple)
            if request is not original:
                context=lookup_inputs._wire([original[0],original[1],sorted(environment.items()),cwd,search,ignored,blocked])
                admitted['request_sha256']=hashlib.sha256(json.dumps(context,sort_keys=True,separators=(',',':')).encode()).hexdigest()
            for name in admitted['dependencies']:
                key,row=files[name];publication=publications[key]
                if publication['action'] not in ('library','check') or row[1]!='file':raise ValueError()
                _declared_publication(graph,publication)
            if admitted['generated_root'] is not None:
                publication=generated_publication(publications,admitted['generated_root'],admitted['cargo_target']['package_id'])
                _declared_publication(graph,publication)
            return {**admitted,'role':admitted['action']}
        if type(graph) is not CargoGraph:raise ValueError()
        graph._verify()
        if (cwd!=b'/workspace' and (not cwd.startswith(b'/workspace/')
                or any(part in (b'',b'.',b'..',b'.git') for part in cwd.split(b'/')[1:]))):raise ValueError()
        if file.startswith(b'/workspace/target/'):alias=os.fsdecode(file[len(b'/workspace/target/'):])
        elif cwd==b'/workspace' and file.startswith(b'target/'):alias=os.fsdecode(file[len(b'target/'):])
        else:raise ValueError()
        unit_views.relative(alias);planner_target=unit_views.literal(planner_target)
        entry=unit_views.literal(planner_target/alias);matches=[]
        for name,(key,row) in files.items():
            publication=publications[key]
            if (publication.get('action') not in ('binary','test','build-script') or row[1]!='file' or not row[2]&0o111):continue
            original=unit_views.literal(planner_target/name)
            if entry.samefile(original):matches.append((name,publication))
        if len(matches)!=1:raise ValueError()
        artifact,publication=matches[0];target=_declared_publication(graph,publication)
        if publication['action']=='build-script':
            raw=environment.get(b'OUT_DIR',b'')
            package=re.escape(target['package_name'].encode())
            if re.fullmatch(rb'/workspace/target/(?:[a-z0-9_-]+/)?[a-z0-9_-]+/build/'+package+rb'-[0-9a-f]{16}/out',raw) is None:raise ValueError()
            return {'kind':'build_script','artifact':artifact,'alias':alias,
                'generated_root':os.fsdecode(raw[len(b'/workspace/target/'):]),'cargo_target':target}
        generated=publication.get('generated_root')
        if generated is not None:_declared_publication(graph,generated_publication(publications,generated,target['package_id']))
        return {'kind':'application','artifact':artifact,'alias':alias,'generated_root':generated,'cargo_target':target}
    except (OSError,ValueError,TypeError,KeyError,AttributeError,IndexError):
        raise cargo_hold('planner_cargo_request') from None


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
    mapped=rustc_unit(request,package=b'private-cargo-probe',host_triple=b'x86_64-unknown-linux-gnu')
    if mapped['kind']=='information':return mapped
    fields=mapped['fields'];codegen=mapped['codegen']
    role={b'/workspace/src/main.rs':'binary',b'/workspace/src/lib.rs':'library',b'/workspace/build.rs':'build-script'}.get(mapped['source'])
    if role is None:raise FreshnessHold('input_census_unknown')
    suffix=codegen[b'extra-filename']
    out=(b'/workspace/target/debug/build/private-cargo-probe'+suffix
        if role=='build-script' else b'/workspace/target/debug/deps')
    required={b'--crate-name':b'build_script_build' if role=='build-script' else b'private_cargo_probe',
        b'--crate-type':b'lib' if role=='library' else b'bin',b'--out-dir':out,b'--edition':b'2021',
        b'--emit':b'dep-info,metadata,link' if role=='library' else b'dep-info,link'}
    if (any(fields.get(key)!=value for key,value in required.items())
            or set(fields)-set(required)-{b'--error-format',b'--json'}
            or fields.get(b'--error-format',b'json')!=b'json'
            or fields.get(b'--json',b'diagnostic-rendered-ansi,artifacts,future-incompat')!=b'diagnostic-rendered-ansi,artifacts,future-incompat'
            or set(codegen)-{b'embed-bitcode',b'debuginfo',b'metadata',b'extra-filename',b'strip'}
            or codegen.get(b'embed-bitcode',b'no')!=b'no' or codegen.get(b'debuginfo',b'0')!=b'0'
            or codegen.get(b'strip',b'debuginfo')!=b'debuginfo' or mapped['cfgs']
            or any(value not in (b'cfg(docsrs,test)',b'cfg(feature, values())') for value in mapped['check_cfgs'])
            or mapped['searches']!=[b'dependency=/workspace/target/debug/deps']):
        raise FreshnessHold('input_census_unknown')
    if mapped['externs']:
        if (role!='binary' or len(mapped['externs'])!=1
                or re.fullmatch(rb'private_cargo_probe=/workspace/target/debug/deps/libprivate_cargo_probe-[0-9a-f]{16}\.rlib',mapped['externs'][0]) is None):
            raise FreshnessHold('input_census_unknown')
    generated=fixture_out_dir(environment)
    if role=='library' and generated is None or role=='build-script' and generated is not None:
        raise FreshnessHold('input_census_unknown')
    return {'kind':'compile','role':role,'outputs':mapped['outputs'],'dependencies':mapped['dependencies'],
        'generated_root':generated,'fingerprint':mapped['fingerprint']}



def worker_cargo_request(request):
    file,argv,environment,cwd,search,*_=request
    if not argv:raise cargo_hold('planner_cargo_request')
    if file==b'/toolchain/bin/cargo':return True
    if file==b'cargo':
        if (search not in (b'/toolchain/bin',b'/toolchain/bin:/usr/bin:/bin')
                or environment.get(b'PATH')!=search):raise cargo_hold('planner_cargo_request')
        return True
    return False


def worker_runtime_request(request,library):
    """Only pinned dynamic runtimes may install the worker guard at startup.

    Static/opaque programs and loader overrides require a separate guarded
    execution implementation; they cannot silently run without mediation.
    """
    file,argv,environment,cwd,search,*_=request
    clean=strip_transport(environment,library)
    if any(key.startswith(b'LD_') for key in clean):raise cargo_hold('planner_cargo_runtime')
    aliases=('/bin/bash','/usr/bin/python3')
    allowed={os.path.realpath(path) for path in aliases}
    if b'/' not in file:
        if (search not in (b'/toolchain/bin',b'/toolchain/bin:/usr/bin:/bin')
                or environment.get(b'PATH')!=search):raise cargo_hold('planner_cargo_runtime')
        candidates=[Path(os.fsdecode(directory))/os.fsdecode(file) for directory in search.split(b':')]
        executable=next((path for path in candidates if path.exists() or path.is_symlink()),None)
    else:executable=Path(os.fsdecode(file))
    if executable is None or not executable.is_absolute():raise cargo_hold('planner_cargo_runtime')
    path=Path(os.path.realpath(executable))
    if str(path) not in allowed:raise cargo_hold('planner_cargo_runtime')
    descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
    try:
        info=os.fstat(descriptor);header=os.pread(descriptor,64,0)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid not in (0,1000) or info.st_mode&0o022 or not info.st_mode&0o111
                or not os.statvfs(path).f_flag&os.ST_RDONLY or len(header)!=64 or header[:7]!=b'\x7fELF\x02\x01\x01'):raise cargo_hold('planner_cargo_runtime')
        offset=struct.unpack_from('<Q',header,32)[0];size,count=struct.unpack_from('<HH',header,54)
        if size!=56 or not 1<=count<=128 or not 64<=offset<=16384:raise cargo_hold('planner_cargo_runtime')
        entries=os.pread(descriptor,size*count,offset);interpreters=[]
        if len(entries)!=size*count:raise cargo_hold('planner_cargo_runtime')
        for index in range(count):
            entry=entries[index*size:(index+1)*size]
            if struct.unpack_from('<I',entry)[0]!=3:continue
            start=struct.unpack_from('<Q',entry,8)[0];length=struct.unpack_from('<Q',entry,32)[0]
            if not 1<=length<=4096 or start+length>info.st_size:raise cargo_hold('planner_cargo_runtime')
            interpreters.append(os.pread(descriptor,length,start))
        if (len(interpreters)!=1 or interpreters[0] not in (b'/lib64/ld-linux-x86-64.so.2\0',b'/lib/ld-linux-aarch64.so.1\0')
                or unit_views._key(os.fstat(descriptor))!=unit_views._key(info)):raise cargo_hold('planner_cargo_runtime')
    finally:os.close(descriptor)
    return True


def execution_identity(value):
    ordinal=value['ordinal'];role=value.get('execution_role','unit')
    if type(ordinal) is not int or not 0<=ordinal<4096:raise FreshnessHold('invalidation_unsafe')
    expected={'unit':2000+ordinal,'worker':1000,'cargo':1001}.get(role)
    if expected is None or value.get('execution_uid',expected)!=expected:raise FreshnessHold('invalidation_unsafe')
    if role!='unit' and ('view' in value or type(value.get('relay_fd')) not in (int,type(None))):
        raise FreshnessHold('invalidation_unsafe')
    return expected


def strip_transport(environment,library):
    result=dict(environment);result.pop(b'JARVIS_CARGO_RELAY_FD',None)
    if b'LD_PRELOAD' in result:
        libraries=[part for part in result[b'LD_PRELOAD'].replace(b':',b' ').split() if part!=os.fsencode(library)]
        if libraries:result[b'LD_PRELOAD']=b' '.join(libraries)
        else:result.pop(b'LD_PRELOAD')
    return result


class NestedJobserverCredits:
    """Record the original unit's implicit job slot; never operate on its pipe.

    This is lineage bookkeeping under original supervisor ownership, not a
    second permit allocator. Callers prove the exec stop, actual pause and
    original descendant cleanup. No record authorizes reuse or parent resume.
    """
    def __init__(self,path,*,binding,command,lock):
        cache.validate_binding(binding)
        if not cache._uuid(command):raise FreshnessHold('generation_mismatch')
        self.path=Path(path);self.owner=os.getpid();self.lock=lock;self.committed=None;self.dirty=False
        parent=unit_views.literal(self.path.parent);info=parent.stat()
        if (self.path.name!='credits.json' or info.st_uid!=os.geteuid() or info.st_mode&0o022
                or self.path.exists() or self.path.is_symlink()):raise FreshnessHold('invalidation_unsafe')
        self.control=[info.st_dev,info.st_ino]
        self.state={'version':1,'kind':'ci-nested-jobserver-lineage','binding':copy.deepcopy(binding),'command':command,
            'owner_pid':self.owner,'owner_birth':Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19],
            'holder_nonce':os.urandom(32).hex(),'units':{},'loans':{},
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}

    @staticmethod
    def _ordinal(value):
        if type(value) is not int or not 0<=value<4096:raise FreshnessHold('input_census_unknown')
        return str(value)

    @staticmethod
    def _job(value,*,required=True):
        if value is None and not required:return None
        if (type(value) is not dict or set(value)!={'version','kind','kernel_pipe_sha256','worker_numbers','input_closure_complete','shipping_authority'}
                or value['version']!=3 or value['kind']!='ci-unit-jobserver' or value['worker_numbers']!=[3,4]
                or type(value['kernel_pipe_sha256']) is not str or re.fullmatch('[0-9a-f]{64}',value['kernel_pipe_sha256']) is None
                or value['input_closure_complete'] is not False or value['shipping_authority'] is not False):
            raise FreshnessHold('input_census_unknown')
        return copy.deepcopy(value)

    def _persist(self):
        self.dirty=True
        if len(json.dumps(self.state,separators=(',',':')).encode())>16*1024**2:raise FreshnessHold('input_census_unknown')
        cache._write_record(self.path,self.state);self.committed=copy.deepcopy(self.state);self.dirty=False

    def _check(self):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        info=self.path.parent.lstat()
        if (list(unit_views._key(info))[:2]!=self.control or info.st_uid!=os.geteuid() or info.st_mode&0o022):
            raise FreshnessHold('invalidation_unsafe')
        if self.path.exists():
            actual=cache._read_record(self.path,max_bytes=16*1024**2)
            if actual!=self.committed and not (self.dirty and actual==self.state):raise FreshnessHold('invalidation_unsafe')
        elif self.committed is not None:raise FreshnessHold('invalidation_unsafe')
        if self.dirty:self._persist()

    def enter(self,ordinal,scope,jobserver,*,parent=None):
        self._ordinal(ordinal)
        if not cache._uuid(scope):raise FreshnessHold('generation_mismatch')
        job=self._job(jobserver,required=False)
        if parent is not None:self._ordinal(parent)
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            self._check();key=str(ordinal)
            if key in self.state['units'] or len(self.state['units'])>=4096:raise FreshnessHold('invalidation_unsafe')
            if parent is None:
                if key in self.state['loans']:raise FreshnessHold('invalidation_unsafe')
                credit=hashlib.sha256((self.state['command']+':'+scope+':'+key).encode()).hexdigest()
            else:
                loan=self.state['loans'].get(key);source=self.state['units'].get(str(parent))
                if (loan is None or source is None or loan['parent']!=parent or loan['state']!='lent'
                        or source['state']!='lent' or source['child']!=ordinal or loan['jobserver']!=job):
                    raise FreshnessHold('cleanup_uncertain')
                credit=loan['credit_id']
            row={'ordinal':ordinal,'scope':scope,'parent':parent,'state':'owned','child':None,
                'credit_id':credit,'jobserver':job}
            self.state['units'][key]=row;self._persist();return copy.deepcopy(row)

    def prepare(self,parent,child,jobserver):
        self._ordinal(parent);self._ordinal(child);job=self._job(jobserver)
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            self._check();source=self.state['units'].get(str(parent));existing=self.state['loans'].get(str(child))
            if (source is None or parent==child or source['state']!='owned' or source['jobserver']!=job
                    or str(child) in self.state['units']):raise FreshnessHold('cleanup_uncertain')
            if existing is not None:
                if existing['parent']==parent and existing['state']=='prepared' and source['child']==child and existing['jobserver']==job:
                    return copy.deepcopy(existing)
                raise FreshnessHold('invalidation_unsafe')
            if source['child'] is not None or len(self.state['loans'])>=4096:raise FreshnessHold('input_census_unknown')
            source['child']=child
            row={'parent':parent,'child':child,'credit_id':source['credit_id'],'jobserver':job,'state':'prepared'}
            self.state['loans'][str(child)]=row;self._persist();return copy.deepcopy(row)

    def lend(self,parent,child,pause):
        self._ordinal(parent);self._ordinal(child)
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            self._check();source=self.state['units'].get(str(parent));loan=self.state['loans'].get(str(child))
            if (source is None or loan is None or loan['parent']!=parent or source['child']!=child
                    or type(pause) is not dict or pause.get('kind')!='ci-unit-freeze-observation'
                    or pause.get('version')!=1 or pause.get('scope')!=source['scope'] or pause.get('frozen') is not True
                    or pause.get('input_closure_complete') is not False or pause.get('shipping_authority') is not False):
                raise FreshnessHold('cleanup_uncertain')
            digest=hashlib.sha256(json.dumps(pause,sort_keys=True,separators=(',',':')).encode()).hexdigest()
            if loan['state']=='lent' and source['state']=='lent' and loan.get('pause_sha256')==digest:return copy.deepcopy(loan)
            if loan['state']!='prepared' or source['state']!='owned':raise FreshnessHold('cleanup_uncertain')
            source['state']='lent';loan.update(state='lent',pause_sha256=digest);self._persist();return copy.deepcopy(loan)

    def return_child(self,parent,child):
        self._ordinal(parent);self._ordinal(child)
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            self._check();source=self.state['units'].get(str(parent));target=self.state['units'].get(str(child));loan=self.state['loans'].get(str(child))
            if (source is None or target is None or loan is None or source['child']!=child or source['state']!='lent'
                    or target['parent']!=parent or target['state']!='owned' or target['child'] is not None
                    or loan['parent']!=parent or loan['state']!='lent' or target['credit_id']!=source['credit_id']):
                raise FreshnessHold('cleanup_uncertain')
            source.update(state='owned',child=None);target['state']='returned';loan['state']='returned'
            self._persist();return copy.deepcopy(loan)

    def abort_prepared(self,parent,child):
        self._ordinal(parent);self._ordinal(child)
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            self._check();source=self.state['units'].get(str(parent));loan=self.state['loans'].get(str(child))
            if (source is None or loan is None or loan['parent']!=parent or source['child']!=child
                    or source['state']!='owned' or loan['state']!='prepared' or str(child) in self.state['units']):
                raise FreshnessHold('cleanup_uncertain')
            source['child']=None;loan['state']='aborted';self._persist()

    def require_owned(self,ordinal):
        self._ordinal(ordinal)
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            self._check();row=self.state['units'].get(str(ordinal))
            if row is None or row['state']!='owned' or row['child'] is not None:raise FreshnessHold('cleanup_uncertain')
            return copy.deepcopy(row)

    def finish_root(self,ordinal):
        self._ordinal(ordinal)
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            self._check();row=self.state['units'].get(str(ordinal))
            if row is None or row['parent'] is not None or row['state']!='owned' or row['child'] is not None:
                raise FreshnessHold('cleanup_uncertain')
            row['state']='closed';self._persist()

    def observation(self):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self.lock:self._check();return copy.deepcopy(self.state)


class DispatchOrdinals:
    """One original command's process-owned request/UID namespace.

    Shared by relays in the controller, never copied to bootstrap processes.
    Allocation does not grant a runnable permit, output or CI authority.
    """
    def __init__(self,binding,command):
        cache.validate_binding(binding)
        if not cache._uuid(command):raise FreshnessHold('generation_mismatch')
        self.owner=os.getpid();self._binding=copy.deepcopy(binding);self.command=command
        self._next=0;self._lock=threading.Lock();self._fenced=False;self._stops=[]
        self.signal_lock=threading.RLock();self.credits=None

    def require(self,binding,command):
        if self.owner!=os.getpid() or self._binding!=binding or self.command!=command:
            raise FreshnessHold('generation_mismatch')

    def allocate(self):
        # Check before acquiring a possibly inherited locked mutex after fork.
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self._lock:
            if self._fenced:raise FreshnessHold('cleanup_uncertain')
            if self._next>=4096:raise FreshnessHold('input_census_unknown')
            ordinal=self._next;self._next+=1;return ordinal

    def register_stop(self,stop):
        if self.owner!=os.getpid() or not isinstance(stop,threading.Event):raise FreshnessHold('invalidation_unsafe')
        with self._lock:
            if self._fenced:stop.set()
            else:self._stops.append(stop)

    def fenced(self):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self._lock:return self._fenced

    def fence(self):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        with self._lock:self._fenced=True;stops=tuple(self._stops)
        for stop in stops:stop.set()


def bind_controller_socket(listener,path):
    """Bind an owned pathname inode without the AF_UNIX address-length limit.

    Filesystem mechanics only; RootRelay authenticates the root admission.
    Clients resolve the real pathname or its private bind mount, not this fd name.
    """
    path=Path(path)
    if (not path.is_absolute() or path.parent.resolve()!=path.parent or path.name in ('.','..')
            or re.fullmatch(r'[A-Za-z0-9._-]{1,64}',path.name) is None):raise FreshnessHold('invalidation_unsafe')
    descriptor=os.open(path.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW)
    try:
        info=os.fstat(descriptor);named=path.parent.lstat()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid() or info.st_mode&0o022
                or (info.st_dev,info.st_ino)!=(named.st_dev,named.st_ino)):raise FreshnessHold('invalidation_unsafe')
        listener.bind(f'/proc/self/fd/{descriptor}/{path.name}')
    finally:os.close(descriptor)


class RootRelay:
    def __init__(self,admission,*,library,timeout=30,deadline=None,ordinals=None,client_path=None):
        if (os.geteuid()!=0 or type(admission) is not _Admission or admission.token is not _TOKEN
                or admission.owner!=os.getpid() or type(timeout) not in (int,float) or not 1<=timeout<=900):
            raise FreshnessHold('invalidation_unsafe')
        if deadline is not None and (type(deadline) not in (int,float) or not math.isfinite(deadline)
                or not time.monotonic()<deadline<=time.monotonic()+901):raise FreshnessHold('invalidation_unsafe')
        self.deadline=deadline
        _source(admission.target,admission.binding,admission.command)
        self.admission=admission;self.root=admission.root;self.library=Path(library)
        info=self.library.lstat()
        if (self.library.resolve()!=self.library or not stat.S_ISREG(info.st_mode) or info.st_uid!=0
                or info.st_mode&0o022 or info.st_nlink!=1 or not 1<=info.st_size<=2*1024**2):raise FreshnessHold('authority_changed')
        self.ordinals=DispatchOrdinals(admission.binding,admission.command) if ordinals is None else ordinals
        if type(self.ordinals) is not DispatchOrdinals:raise FreshnessHold('invalidation_unsafe')
        self.ordinals.require(admission.binding,admission.command)
        with self.ordinals._lock:
            if self.ordinals.credits is None:
                # The probe/controller transport can live on guest tmpfs. Keep
                # unresolved credit lineage inside the original admitted
                # generation, under its existing managed storage/R ownership.
                credit_root=admission.target/('.jobserver-credit-'+admission.command)
                credit_root.mkdir(mode=0o700)
                self.ordinals.credits=NestedJobserverCredits(credit_root/'credits.json',
                    binding=admission.binding,command=admission.command,lock=self.ordinals.signal_lock)
        self.timeout=timeout;self.peer_uid=1001;self.config_magic=b'JARVCFG3';self.require_message_credentials=False
        self.path=self.root/'relay.sock';self.nonce=os.urandom(32);self.config_uid=0
        self.client_path=self.path if client_path is None else Path(client_path)
        if not self.client_path.is_absolute() or len(os.fsencode(self.client_path))>=108:raise FreshnessHold('invalidation_unsafe')
        self.views=None;self.completed=[];self.errors=[];self._stop=threading.Event();self._slots=threading.BoundedSemaphore(4)
        self._threads=[];self._children=set();self._lock=threading.Lock()
        self.ordinals.register_stop(self._stop)

    def __enter__(self):
        os.chown(self.root,0,self.peer_uid);self.root.chmod(0o710)
        (self.root/'units').mkdir(mode=0o700)
        self.listener=socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
        if self.require_message_credentials:self.listener.setsockopt(socket.SOL_SOCKET,socket.SO_PASSCRED,1)
        bind_controller_socket(self.listener,self.path);os.chown(self.path,0,self.peer_uid);self.path.chmod(0o660)
        self.listener.listen(8);self.listener.settimeout(.05)
        self.thread=threading.Thread(target=self._accept,daemon=True);self.thread.start();return self

    def __exit__(self,*unused):
        self._stop.set();self.thread.join(timeout=2);self.listener.close()
        for thread in self._threads:thread.join(timeout=6)
        if self.thread.is_alive() or any(thread.is_alive() for thread in self._threads) or self._children:
            raise FreshnessHold('cleanup_uncertain')
        self.path.unlink()

    def drain(self):
        # Call only after the original planner supervisor has reaped its tree.
        # A result can reach Cargo just before its handler releases descriptors.
        deadline=time.monotonic()+6
        threads=list(self._threads)
        for thread in threads:thread.join(timeout=max(0,deadline-time.monotonic()))
        with self._lock:
            if (threads!=self._threads or any(thread.is_alive() for thread in threads)
                    or self._children or self.errors):raise FreshnessHold('cleanup_uncertain')

    def config_fd(self):
        fd=os.memfd_create('ci-planner-client',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            body=legacy.CONFIG.pack(self.config_magic,self.config_uid,os.getpid(),int(self.timeout*1000),self.nonce,os.fsencode(self.client_path))
            if os.write(fd,body)!=len(body):raise OSError(errno.EIO,'client configuration write failed')
            fcntl.fcntl(fd,fcntl.F_ADD_SEALS,legacy.SEALS);return fd
        except BaseException:os.close(fd);raise

    def _accept(self):
        while not self._stop.is_set():
            try:connection,_=self.listener.accept()
            except socket.timeout:continue
            try:ordinal=self.ordinals.allocate()
            except FreshnessHold:
                self.rejected_request()
                self.errors.append(diagnostic({'stage':'planner_request_receive','category':'FreshnessHold','errno':errno.EBUSY}))
                try:connection.sendall(event('error',errno.EBUSY))
                finally:connection.close()
                continue
            if not self._slots.acquire(blocking=False):
                self.rejected_request()
                try:connection.sendall(event('error',errno.EBUSY))
                finally:connection.close()
                continue
            thread=threading.Thread(target=self._handle,args=(connection,ordinal),daemon=True)
            self._threads.append(thread);thread.start()

    def rejected_request(self):pass

    def _receive(self,connection,descriptors):
        pid,uid,gid=struct.unpack('3i',connection.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
        if pid<=0 or uid!=self.peer_uid or gid!=self.peer_uid:raise FreshnessHold('invalidation_unsafe')
        extra=socket.CMSG_SPACE(12) if self.require_message_credentials else 0
        data,ancillary,flags,_=connection.recvmsg(legacy.REQUEST.size,socket.CMSG_SPACE(65*4)+extra,socket.MSG_CMSG_CLOEXEC)
        credentials=[]
        for level,kind,body in ancillary:
            if level==socket.SOL_SOCKET and kind==socket.SCM_CREDENTIALS and self.require_message_credentials:
                if len(body)!=12:raise FreshnessHold('metadata_torn')
                credentials.append(struct.unpack('3i',body));continue
            if level!=socket.SOL_SOCKET or kind!=socket.SCM_RIGHTS:raise FreshnessHold('metadata_torn')
            values=array.array('i');values.frombytes(body[:len(body)-len(body)%4]);descriptors.extend(values)
        if self.require_message_credentials and credentials!=[(pid,uid,gid)]:raise FreshnessHold('invalidation_unsafe')
        if len(data)!=legacy.REQUEST.size or flags&(socket.MSG_TRUNC|socket.MSG_CTRUNC):raise FreshnessHold('metadata_torn')
        magic,nonce,count,*numbers=legacy.REQUEST.unpack(data)
        if (magic!=b'JARVREQ3' or not hmac.compare_digest(nonce,self.nonce) or not 3<=count<=64
                or len(descriptors)!=count+1 or numbers[:3]!=[0,1,2] or any(numbers[count:])
                or len(set(numbers[:count]))!=count or any(not 3<=number<=65535 for number in numbers[3:count])):
            raise FreshnessHold('metadata_torn')
        unit_views.validate_stdio(descriptors[1:4])
        _,_,environment,*_=payload(descriptors[0])
        _,mapping,_=staging.normalize_jobserver(environment,dict(zip(numbers[3:count],descriptors[4:])))
        if mapping:
            pipe=os.fstat(mapping[0][1]);identity=(pipe.st_dev,pipe.st_ino)
            # Staging stdin or writing output must never consume/fabricate
            # jobserver bytes through an unmodeled stdio alias. Refuse before
            # the first read, parent pause or credit-loan preparation.
            for descriptor in descriptors[1:4]:
                info=os.fstat(descriptor)
                if stat.S_ISFIFO(info.st_mode) and (info.st_dev,info.st_ino)==identity:
                    raise FreshnessHold('input_census_unknown')
        return numbers[:count]

    def _handle(self,connection,ordinal):
        descriptors=[];phase=['planner_request_receive'];release_slot=True
        try:
            connection.settimeout(self.timeout)
            numbers=self._receive(connection,descriptors);phase[0]='planner_unit'
            self._run(connection,ordinal,descriptors,numbers,phase)
        except BaseException as error:
            # An incomplete contained subtree must never release its ancestor's
            # original runnable allowance. Fence this command, cancel every
            # registered controller and retain this slot through original R
            # teardown instead of admitting another unit beside unknown work.
            release_slot=False;self.ordinals.fence()
            self.errors.append(diagnostic({'stage':getattr(error,'unit_view_stage',phase[0]),
                'category':type(error).__name__,'errno':getattr(error,'errno',None)}))
            try:connection.sendall(event('error',errno.EINVAL))
            except OSError:pass
        finally:
            legacy._close(descriptors);connection.close()
            if release_slot:self._slots.release()

    def request_context(self,ordinal,request):return {},None
    def execution_cancelled(self):return self._stop.is_set() or self.ordinals.fenced()
    def runnable_observation(self):
        # Observe the original semaphore; do not allocate a second pool or
        # acquire/release a permit just to determine its availability.
        with self._slots._cond:available=self._slots._value
        with self._lock:children=len(self._children)
        if not 0<=available<=4 or not 0<=children<=4:raise FreshnessHold('input_census_unknown')
        return {'version':1,'kind':'ci-original-dispatch-observation','capacity':4,'available':available,
            'original_supervisors':children,'shipping_authority':False}

    def unit_signal_grant(self,value,transport,grant):grant()
    def unit_launch(self,value):return None
    def unit_before_grant(self,value,pid,pidfd,supervisor,directory):pass

    def unit_credit_drained(self,value,transport,started):
        if started and transport is not None:self.ordinals.credits.finish_root(value['ordinal'])

    def unit_signal_drained(self,value,transport,started):pass

    def unit_started(self,value,directory):pass
    def unit_finished(self,value,record):pass

    def _run(self,connection,ordinal,descriptors,numbers,phase,*,staged=False,defer_terminal=False):
        directory=self.root/'units'/str(uuid.uuid4());directory.mkdir(mode=0o700)
        control,child_control=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
        cancel,stop=os.pipe2(os.O_CLOEXEC);release,grant=os.pipe2(os.O_CLOEXEC);exec_release,exec_grant=os.pipe2(os.O_CLOEXEC)
        deadline=min(time.monotonic()+self.timeout,self.deadline) if self.deadline is not None else time.monotonic()+self.timeout
        value={'version':3,'kind':'unit','parent':os.getpid(),'target':str(self.admission.target),
            'binding':self.admission.binding,'command':self.admission.command,'directory':str(directory),
            'ordinal':ordinal,'deadline':deadline,'library':str(self.library),'numbers':numbers,
            'extras':descriptors[4:]}
        view=None;bootstrap=None;child=None;pidfd=None;relay_fd=None;transport=None;launch=None;cancelled=False;ready=False;sealed=False;authorized=False;started=False
        signal_count=0;cancelled_at=None;control_eof=False;original_pid=None
        try:
            request=payload(descriptors[0]);context,relay_fd=self.request_context(ordinal,request)
            if relay_fd is not None:
                # Avoid all original descriptor target numbers before their
                # jobserver mapping is restored in the stage helper.
                minimum=max(256,max(numbers)+1)
                private=fcntl.fcntl(relay_fd,fcntl.F_DUPFD_CLOEXEC,minimum);os.close(relay_fd);relay_fd=private
                if relay_fd>65535:raise FreshnessHold('input_census_unknown')
                context=dict(context,relay_fd=relay_fd)
            value.update(context)
            launch=self.unit_launch(value)
            if self.views is not None:
                phase[0]='planner_view_prepare';view=self.views.prepare(ordinal,request)
                if view is not None:
                    def check_stdio():
                        if self.execution_cancelled() or time.monotonic()>=deadline:
                            raise FreshnessHold('cleanup_uncertain')
                    mounts=cache._read_record(Path(view['mounts']),max_bytes=16*1024**2)
                    if (mounts.get('binding')!=self.admission.binding or mounts.get('command')!=self.admission.command
                            or mounts.get('ordinal')!=ordinal):raise FreshnessHold('generation_mismatch')
                    observed_stdio=unit_views.reject_projection_stdio(descriptors[2:4],GeneratedForests.WORKSPACE,
                        mounts['declared_outputs'],owner=GeneratedForests._WORKER_UID,check=check_stdio)
                    cache._write_record(Path(view['root']).parent/'worker-output-stdio.json',{
                        'version':1,'binding':self.admission.binding,'command':self.admission.command,
                        'ordinal':ordinal,'scope':directory.name,'observation':observed_stdio,
                        'input_closure_complete':False,'full_ci':False,'shipping_authority':False})
                    transport_root=Path(view['root']).parent/'transport';transport_root.mkdir(mode=0o700)
                    admission=admit(self.admission.target,self.admission.binding,self.admission.command,transport_root)
                    transport=UnitPeerRelay(admission,library=self.library,ordinal=ordinal,scope=directory,
                        ordinals=self.ordinals,deadline=deadline,view=view,
                        credit_parent=self.parent_ordinal if isinstance(self,UnitPeerRelay) else None,
                        original_dispatcher=self.original_dispatcher if isinstance(self,UnitPeerRelay) else self)
                    transport.__enter__()
                    if relay_fd is not None:raise FreshnessHold('invalidation_unsafe')
                    descriptor=transport.config_fd()
                    try:relay_fd=fcntl.fcntl(descriptor,fcntl.F_DUPFD_CLOEXEC,max(256,max(numbers)+1))
                    finally:os.close(descriptor)
                    if relay_fd>65535:raise FreshnessHold('input_census_unknown')
                    value['relay_fd']=relay_fd
                    value['view']=dict(view,transport={'socket':str(transport.path),'library':str(self.library)})
            bootstrap=_sealed(value)
            argv=[sys.executable,'-I',str(Path(__file__).resolve()),'--unit',
                *map(str,(bootstrap,descriptors[0],cancel,release,exec_release,child_control.fileno()))]
            child=subprocess.Popen(argv,stdin=descriptors[1],stdout=descriptors[2],stderr=descriptors[3],
                pass_fds=tuple([bootstrap,*descriptors,cancel,release,exec_release,child_control.fileno(),*(() if relay_fd is None else (relay_fd,))]),
                env={'PATH':os.defpath,'JARVIS_SUPERVISOR_PARENT_PID':str(os.getpid())})
            with self._lock:self._children.add(child)
            os.close(bootstrap);bootstrap=None;child_control.close();legacy._close(descriptors);descriptors.clear()
            phase[0]='planner_capture'
            while True:
                if (self.execution_cancelled() or time.monotonic()>=deadline
                        or transport is not None and transport._stop.is_set()):cancelled=True
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
                            try:
                                if transport is None:signal.pidfd_send_signal(pidfd,number)
                                else:transport.forward_signal(number)
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
                        if not cancelled and not staged:connection.sendall(event('staging_ready',0))
                        ready=True
                    elif stage==2 and not sealed and not received:
                        observed=cache._read_record(directory/'stdin.json')
                        if (observed.get('command')!=self.admission.command or observed.get('binding')!=self.admission.binding
                                or observed.get('ordinal')!=ordinal or observed.get('shipping_authority') is not False):
                            raise FreshnessHold('input_census_unknown')
                        sealed=True;phase[0]='planner_authorize'
                    elif stage==3 and authorized and not started and not received and child_pid==original_pid:
                        if not cancelled:
                            self.unit_before_grant(value,original_pid,pidfd,child.pid,directory)
                            if transport is not None:transport.register_parent(original_pid,directory/'view-namespace.json',supervisor_pid=child.pid)
                            def grant_started():
                                connection.sendall(event('tool_started',0));os.write(exec_grant,b'go\n')
                            self.unit_signal_grant(value,transport,grant_started)
                            started=True;phase[0]='planner_unit_exec';self.unit_started(value,directory)
                            if launch is not None:launch.close();launch=None
                    else:legacy._close(received);raise ValueError('out-of-order original control frame')
                    legacy._close(received)
                if ready and sealed and not authorized and not cancelled:
                    # Do not grant an exec while an already queued client
                    # cancellation is waiting to be read at this boundary.
                    if not select.select([connection],[],[],0)[0]:os.write(grant,b'go\n');authorized=True
                if cancelled and child.poll() is None and cancelled_at is None:
                    if transport is not None:transport.request_cancel()
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
            # The original cleanup marker and raw status must be validated
            # before removing a signal target or closing its owned pidfd.
            self.unit_credit_drained(value,transport,started)
            self.unit_signal_drained(value,transport,started)
            if transport is not None:transport.check()
            if view is not None:
                phase[0]='planner_view_publish';self.views.finish(view,directory,raw)
            self.completed.append({'ordinal':ordinal,'uid':execution_identity(value),'started':started,
                'cancelled':cancelled,'returncode':raw,'original_cleanup':True,
                'input_closure_complete':False,'shipping_authority':False})
            self.unit_finished(value,self.completed[-1])
            if not cancelled and not defer_terminal:
                if not started or record.get('exec_observed') is not True:connection.sendall(event('error',errno.EIO))
                else:connection.sendall(event('signaled' if raw<0 else 'finished',abs(raw)))
            return {'ordinal':ordinal,'directory':directory,'view':view,'returncode':raw,
                'original_cleanup':True,'exec_observed':record.get('exec_observed') is True,'cancelled':cancelled}
        finally:
            if launch is not None:self._stop.set()
            if child is not None:
                if child.poll() is None:child.terminate();child.wait(timeout=6)
                with self._lock:self._children.discard(child)
            legacy._close([cancel,stop,release,grant,exec_release,exec_grant]+([] if bootstrap is None else [bootstrap])+([] if relay_fd is None else [relay_fd]))
            control.close();child_control.close()
            if pidfd is not None:os.close(pidfd)
            if transport is not None:transport.__exit__(None,None,None)
            if launch is not None:launch.close()


def stage_request_stdin(connection,source,*,deadline,stop,owner_pidfd=None):
    """Stage original bytes before a parent pause, without a child exec grant.

    Before tool_started, a queued signal or transport loss cancels staging,
    matching the original supervisor protocol. The watcher never consumes a
    signal that a running child would need: this function cannot start one.
    """
    cancelled,notify=os.pipe2(os.O_CLOEXEC)
    finished=threading.Event();errors=[];captured=None
    readers=[connection]+([] if owner_pidfd is None else [owner_pidfd])
    def unavailable():
        return stop.is_set() or bool(select.select(readers,[],[],0)[0])
    def watch():
        try:
            while not finished.is_set():
                if unavailable() or time.monotonic()>=deadline:
                    os.write(notify,b'cancel');return
                finished.wait(.02)
        except BaseException:
            errors.append('stdin_watch_unavailable')
            try:os.write(notify,b'cancel')
            except OSError:pass
    watcher=threading.Thread(target=watch,daemon=True);watcher.start()
    try:
        connection.sendall(event('staging_ready',0))
        captured=staging.capture(source,cancelled,command_deadline=deadline)
        finished.set();watcher.join(timeout=2)
        if watcher.is_alive():raise FreshnessHold('cleanup_uncertain')
        if errors or unavailable():raise staging.StagingUnavailable('stdin_cancelled')
        result=captured;captured=None;return result
    finally:
        finished.set();watcher.join(timeout=2)
        if captured is not None:captured.close()
        if watcher.is_alive():raise FreshnessHold('cleanup_uncertain')
        os.close(cancelled);os.close(notify)


class StagedParentSignals:
    """Unfrozen ingress during input staging, never a child-execution grant.

    Frames are ordered at this barrier. Kernel standard-signal coalescing is
    unchanged; this does not promise one userspace handler call per frame.
    """
    def __init__(self,pidfd,*,lock=None):
        if type(pidfd) is not int or pidfd<0 or os.readlink(f'/proc/self/fd/{pidfd}')!='anon_inode:[pidfd]':
            raise FreshnessHold('invalidation_unsafe')
        self.owner=os.getpid();self.pidfd=os.dup(pidfd);self.lock=threading.RLock() if lock is None else lock
        self.active_child=None;self.controller_parent=None;self.routed=0
        self.cancelled=threading.Event();self.paused=None;self.used=set();self.queue=[]
        self.received=0;self.delivered=0;self.late=0;self.revoked=False;self.closed=False;self.delivery_uncertain=False

    def _check(self):
        if self.owner!=os.getpid() or self.closed:raise FreshnessHold('invalidation_unsafe')

    def _alive(self):return not self.closed and not select.select([self.pidfd],[],[],0)[0]

    def _deliver(self,number):
        if not self._alive():self.late+=1;return 'late'
        try:signal.pidfd_send_signal(self.pidfd,number)
        except ProcessLookupError:self.late+=1;return 'late'
        self.delivered+=1;return 'forwarded'

    def _route(self,*,allow_closed=False):
        route=[];seen=set();node=self
        while node is not None:
            if (id(node) in seen or len(route)>=4096 or node.owner!=self.owner
                    or node.closed and not allow_closed or node.lock is not self.lock):raise FreshnessHold('cleanup_uncertain')
            seen.add(id(node));route.append(node);node=node.active_child
        return route

    @staticmethod
    def _revoke(route,*,uncertain=False):
        for node in route:
            node.revoked=True;node.cancelled.set()
            if uncertain:node.delivery_uncertain=True

    def activate_child(self,request,child,grant):
        """Bind before the original exec grant; serialize signal ingress.

        No new process, permit or jobserver token is allocated here. The caller
        owns authenticated lineage and the original supervisor's exec stop.
        """
        self._check()
        with self.lock:
            if (type(request) is not int or request!=self.paused or not callable(grant)
                    or type(child) is not StagedParentSignals or child is self or child.owner!=self.owner
                    or child.closed or child.lock is not self.lock or child.controller_parent is not None
                    or self.active_child is not None or self.revoked or self.cancelled.is_set()
                    or not self._alive() or not child._alive()):raise FreshnessHold('cleanup_uncertain')
            route=child._route()
            if any(node.revoked or node.cancelled.is_set() or node.delivery_uncertain for node in route):
                raise FreshnessHold('cleanup_uncertain')
            ancestor=self;seen=set()
            while ancestor is not None:
                if id(ancestor) in seen or len(seen)>=4096 or ancestor in route:
                    raise FreshnessHold('cleanup_uncertain')
                if ancestor.revoked or ancestor.cancelled.is_set() or ancestor.delivery_uncertain or not ancestor._alive():
                    raise FreshnessHold('cleanup_uncertain')
                seen.add(id(ancestor));ancestor=ancestor.controller_parent
            if len(seen)+len(route)>4096:raise FreshnessHold('input_census_unknown')
            self.active_child=child;child.controller_parent=self
            try:grant()
            except BaseException:
                self._revoke([self,*route],uncertain=True);raise

    def child_drained(self,request,child):
        """Release routing only after caller-verified original child cleanup.

        A dead pidfd alone is not a descendant-cleanup receipt. The original
        root supervisor must validate that receipt before calling this method.
        """
        self._check()
        with self.lock:
            if (type(request) is not int or request!=self.paused or child is not self.active_child
                    or child.controller_parent is not self or child._alive() or child.active_child is not None):
                raise FreshnessHold('cleanup_uncertain')
            self.active_child=None;child.controller_parent=None

    def forward(self,number):
        self._check()
        if isinstance(number,bool) or not isinstance(number,int) or number not in legacy.FORWARDED_SIGNALS:
            raise FreshnessHold('invalidation_unsafe')
        with self.lock:
            route=self._route()
            if any(node.delivery_uncertain for node in route):raise FreshnessHold('cleanup_uncertain')
            if self.received>=4096:
                self._revoke(route);raise FreshnessHold('input_census_unknown')
            self.received+=1;leaf=route[-1]
            if leaf.paused is not None:
                leaf.queue.append(number)
                for node in route:node.cancelled.set()
                return 'queued'
            try:disposition=leaf._deliver(number)
            except BaseException:
                self._revoke(route,uncertain=True);raise
            if leaf is not self:
                self.routed+=1
                if disposition=='late':self.late+=1
                else:self.delivered+=1
            return disposition

    def pause(self,request):
        self._check()
        with self.lock:
            if (type(request) is not int or not 0<=request<4096 or request in self.used
                    or self.paused is not None or self.revoked or not self._alive()):
                self.cancelled.set();raise FreshnessHold('invalidation_unsafe')
            self.used.add(request);self.paused=request;self.cancelled.clear()

    def resume(self,request,thaw):
        self._check()
        with self.lock:
            if (type(request) is not int or request!=self.paused or not callable(thaw)
                    or self.delivery_uncertain or self.active_child is not None):raise FreshnessHold('cleanup_uncertain')
            # Serialize ingress across checked thaw and every queued send. A
            # failed thaw leaves the queue intact; uncertain delivery is never
            # retried as though the previous signal had not happened.
            try:thaw()
            except BaseException:self.cancelled.set();raise
            while self.queue:
                try:self._deliver(self.queue[0])
                except BaseException:
                    self.delivery_uncertain=True;self.revoked=True;self.cancelled.set();raise
                self.queue.pop(0)
            self.paused=None

    def cancel(self):
        self._check()
        with self.lock:
            # Revoke the current route synchronously. Waiting for each relay's
            # polling loop would leave a window for a descendant exec grant.
            self._revoke([self]);self._revoke(self._route(allow_closed=True))

    def observation(self):
        self._check()
        with self.lock:return {'received':self.received,'delivered':self.delivered,'late':self.late,
            'queued':len(self.queue),'paused_request':self.paused,'revoked':self.revoked,
            'routed':self.routed,'active_child':self.active_child is not None,'shipping_authority':False}

    def close(self):
        if self.closed:return
        self._check()
        with self.lock:
            if self.paused is not None and self._alive():raise FreshnessHold('cleanup_uncertain')
            if self.active_child is not None:
                if self.active_child._alive():raise FreshnessHold('cleanup_uncertain')
                self.active_child.controller_parent=None;self.active_child=None
            os.close(self.pidfd);self.closed=True


def unit_mutable_roots(mounts):
    declared=mounts.get('declared_outputs');cache.validate_outputs(declared)
    return ['workspace/target','tmp','home/worker','cargo','var/tmp','dev/shm',
        *['workspace/'+name for name in declared]]


def unit_mount_rules(view,mounts,*,library,endpoint):
    """Original admitted view bindings; never accept requested tool mount rules."""
    rules={}
    def add(name,source,kind,access='ro'):
        if name in rules:raise FreshnessHold('invalidation_unsafe')
        rules[name]={'source':str(source) if source is not None else None,'kind':kind,'access':access}
    add(b'/usr','/usr','directory')
    if Path('/toolchain').is_dir():
        for name in ('bin','etc','lib','libexec','share'):
            add(os.fsencode('/toolchain/'+name),'/toolchain/'+name,'directory')
    add(b'/workspace',view['source'],'directory')
    add(b'/workspace/target',view['artifacts'],'directory','rw')
    for name in ('tmp','home/worker','cargo','var/tmp','dev/shm'):
        if 'working' in view:add(os.fsencode('/'+name),Path(view['working'])/name,'directory','rw')
    add(b'/proc',None,'proc')
    for name in ('null','zero','random','urandom'):
        add(os.fsencode('/dev/'+name),'/dev/'+name,'device','rw')
    add(b'/run/ci-unit.sock',endpoint,'socket')
    add(b'/run/ci-cargo-relay.so',library,'file')
    source_entries=mounts.get('source_entries')
    if source_entries is None:source_entries=[]
    for prefix,entries in ((b'/workspace/target/',mounts['entries']),
            (b'/workspace/',source_entries)):
        for raw,source,expected in entries:
            name=os.fsencode(unit_views.relative(bytes.fromhex(raw)))
            if expected[1] not in ('file','tree'):raise FreshnessHold('invalidation_unsafe')
            add(prefix+name,source,'directory' if expected[1]=='tree' else 'file')
    return rules


class WorkerProjectionLock:
    """Serialize worker launch admission with a bounded projection pause.

    A lease coordinates Root threads only; it never proves a kernel freeze,
    input closure, or original cleanup.
    """
    class Lease:
        def __init__(self,lock,kind):
            self.lock=lock;self.kind=kind;self.owner=(os.getpid(),threading.get_ident());self.closed=False

        def close(self):
            if self.owner!=(os.getpid(),threading.get_ident()):raise FreshnessHold('cleanup_uncertain')
            if self.closed:return
            if self.lock.active is not self:raise FreshnessHold('cleanup_uncertain')
            self.lock.active=None;self.closed=True;self.lock.mutex.release()

        def __enter__(self):return self
        def __exit__(self,*unused):self.close()

    def __init__(self):
        self.owner=os.getpid();self.mutex=threading.Lock();self.active=None

    def _acquire(self,kind,deadline,check):
        if (self.owner!=os.getpid() or type(deadline) not in (int,float)
                or not math.isfinite(deadline) or not callable(check)):
            raise FreshnessHold('cleanup_uncertain')
        owner=(os.getpid(),threading.get_ident())
        if self.active is not None and self.active.owner==owner:raise FreshnessHold('cleanup_uncertain')
        while True:
            check();remaining=deadline-time.monotonic()
            if remaining<=0:raise FreshnessHold('cleanup_uncertain')
            if self.mutex.acquire(timeout=min(.05,remaining)):break
        try:
            check()
            if time.monotonic()>=deadline:raise FreshnessHold('cleanup_uncertain')
            lease=self.Lease(self,kind);self.active=lease;return lease
        except BaseException:self.mutex.release();raise

    def launch(self,deadline,check):return self._acquire('launch',deadline,check)
    def pause(self,deadline,check):return self._acquire('pause',deadline,check)


class UnitFreezer:
    """Original unit's guest-only cgroup; a pause is never original cleanup.

    Enrollment happens at the original single-threaded exec stop, before any
    untrusted instruction. No controller or supervisor enters this cgroup.
    """
    _setup=threading.Lock()

    def __init__(self,admission,scope,uid,*,_worker_token=None):
        worker=_worker_token is _WORKER_GROUP_TOKEN
        if (os.getresuid()!=(0,0,0) or type(admission) is not _Admission
                or admission.token is not _TOKEN or admission.owner!=os.getpid()
                or not cache._uuid(Path(scope).name) or type(uid) is not int
                or not (uid==1000 if worker else 2000<=uid<6096)
                or _worker_token is not None and not worker):
            raise FreshnessHold('invalidation_unsafe')
        _source(admission.target,admission.binding,admission.command)
        self.owner=os.getpid();self.uid=uid;self.original=None;self.pidfd=None;self.entry=None;self.mount_layout=None
        self.paused_cohort=None
        self.pid_namespace=os.readlink('/proc/self/ns/pid')
        if os.readlink('/proc/1/ns/pid')!=self.pid_namespace:raise FreshnessHold('invalidation_unsafe')
        self.setup_path=Path(scope)/'freeze-setup.json'
        self.setup_record={'version':1,'kind':'ci-worker-freezer-setup' if worker else 'ci-unit-freezer-setup','binding':admission.binding,
            'command':admission.command,'uid':uid,'pid_namespace':self.pid_namespace,'shipping_authority':False}
        with self._setup:
            mount=Path('/sys/fs/cgroup')
            # This controller runs in the isolated guest's initial cgroup.
            # Refuse a host service/delegation rather than modifying its tree.
            membership=Path('/proc/self/cgroup').read_bytes()
            self._note('before_mount',membership_root=membership==b'0::/\n',membership_empty=membership==b'',
                mountpoint_exists=mount.is_dir(),v2_controls_present=(mount/'cgroup.controllers').exists())
            # The unified hierarchy is omitted from proc until its first
            # mount. Verify actual root membership after making it visible.
            if membership not in (b'',b'0::/\n'):raise FreshnessHold('invalidation_unsafe')
            if not (mount/'cgroup.controllers').exists():
                unit_views._mount('-t','cgroup2','-o','nosuid,nodev,noexec','none',mount)
            self._note('verify_mount')
            if Path('/proc/self/cgroup').read_bytes()!=b'0::/\n':raise FreshnessHold('invalidation_unsafe')
            rows=Path('/proc/self/mountinfo').read_text().splitlines()
            if not any(line.split()[4]=='/sys/fs/cgroup' and ' - cgroup2 ' in line for line in rows):
                raise FreshnessHold('invalidation_unsafe')
            root=mount/('ci-workers' if worker else 'ci-units')
            if not root.exists():root.mkdir(mode=0o700)
            info=root.lstat()
            if (root.resolve()!=root or not stat.S_ISDIR(info.st_mode) or info.st_uid!=0
                    or stat.S_IMODE(info.st_mode)!=0o700):raise FreshnessHold('invalidation_unsafe')
            self.path=root/Path(scope).name;self.path.mkdir(mode=0o700)
        self.identity=self.path.stat();self.closed=False
        self._note('configure_scope')
        self._write('cgroup.max.depth',b'0\n');self._write('cgroup.max.descendants',b'0\n')
        if self._read('cgroup.type')!=b'domain\n' or self._read('cgroup.procs'):
            raise FreshnessHold('invalidation_unsafe')
        self._note('scope_ready')

    def _note(self,phase,**values):
        self.setup_record.update(phase=phase,**values);cache._write_record(self.setup_path,self.setup_record)

    def _check(self):
        current=self.path.lstat()
        if (self.owner!=os.getpid() or self.closed or current.st_uid!=0 or current.st_mode!=self.identity.st_mode
                or (current.st_dev,current.st_ino)!=(self.identity.st_dev,self.identity.st_ino)):
            raise FreshnessHold('cleanup_uncertain')

    def _read(self,name):
        self._check()
        with (self.path/name).open('rb') as stream:body=stream.read(16385)
        if len(body)>16384:raise FreshnessHold('input_census_unknown')
        return body

    def _write(self,name,body):
        self._check();descriptor=os.open(self.path/name,os.O_WRONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
        try:
            if os.write(descriptor,body)!=len(body):raise FreshnessHold('cleanup_uncertain')
        finally:os.close(descriptor)

    def attach(self,pid,pidfd,*,entry,mount_layout,private_base,mutable_roots,deadline):
        if self.original is not None:raise FreshnessHold('invalidation_unsafe')
        process=Path('/proc')/str(pid);status=process.joinpath('stat').read_text().rsplit(')',1)[1].split()
        if status[0]!='t' or {p.name for p in process.joinpath('task').iterdir()}!={str(pid)}:
            raise FreshnessHold('invalidation_unsafe')
        if entry['pid']!=pid or entry['birth']!=int(status[19]):raise FreshnessHold('invalidation_unsafe')
        if os.readlink(f'/proc/{pid}/ns/pid')!=self.pid_namespace:raise FreshnessHold('invalidation_unsafe')
        if self._read('cgroup.procs'):raise FreshnessHold('invalidation_unsafe')
        signal.pidfd_send_signal(pidfd,0)
        self._write('cgroup.procs',str(pid).encode()+b'\n')
        if (process.joinpath('stat').read_text().rsplit(')',1)[1].split()[19]!=status[19]
                or process.joinpath('cgroup').read_text()!='0::/ci-units/'+self.path.name+'\n'
                or set(self._read('cgroup.procs').split())!={str(pid).encode()}):
            raise FreshnessHold('invalidation_unsafe')
        self.original=pid;self.pidfd=os.dup(pidfd);self.entry=copy.deepcopy(entry)
        self.mount_layout=copy.deepcopy(mount_layout)
        self.private_base=copy.deepcopy(private_base);self.mutable_roots=list(mutable_roots)
        def check_entry():
            if time.monotonic()>=deadline:raise FreshnessHold('cleanup_uncertain')
            signal.pidfd_send_signal(self.pidfd,0)
        self.entry_uid_cohort=staging.observe_uid_cohort(self.uid,
            [{'pid':pid,'tid':pid,'birth':entry['birth']}],
            check=check_entry)
        if self.entry_uid_cohort['pid_namespace']!=self.pid_namespace:raise FreshnessHold('invalidation_unsafe')
        self._note('original_enrolled',uid_cohort=self.entry_uid_cohort)

    def _wait(self,frozen,deadline):
        while True:
            fields=dict(line.split() for line in self._read('cgroup.events').splitlines())
            if fields.get(b'frozen')==str(frozen).encode():return
            if time.monotonic()>=deadline:raise FreshnessHold('cleanup_uncertain')
            time.sleep(.005)

    def freeze(self,deadline):
        if self.original is None or self.entry is None:raise FreshnessHold('invalidation_unsafe')
        signal.pidfd_send_signal(self.pidfd,0)
        self._write('cgroup.freeze',b'1\n');self._wait(1,deadline)
        if unit_views.observe_mount_layout(self.original)!=self.mount_layout:
            raise FreshnessHold('invalidation_unsafe')
        rows=[]
        pids=set(map(int,self._read('cgroup.procs').split()))
        if self.original not in pids or not 1<=len(pids)<=512:raise FreshnessHold('input_census_unknown')
        for pid in sorted(pids):
            process=Path('/proc')/str(pid)
            status=process.joinpath('status').read_text().splitlines()
            for label in ('Uid:','Gid:'):
                if list(map(int,next(line for line in status if line.startswith(label)).split()[1:]))!=[self.uid]*4:
                    raise FreshnessHold('invalidation_unsafe')
            if next(line for line in status if line.startswith('Groups:')).split()[1:]:raise FreshnessHold('invalidation_unsafe')
            if process.joinpath('cgroup').read_text()!='0::/ci-units/'+self.path.name+'\n':
                raise FreshnessHold('invalidation_unsafe')
            tasks=[]
            for task in process.joinpath('task').iterdir():
                fields=task.joinpath('stat').read_text().rsplit(')',1)[1].split()
                observed=dict(line.split(':',1) for line in task.joinpath('status').read_text().splitlines())
                if (any(list(map(int,observed[label].split()))!=[self.uid]*4 for label in ('Uid','Gid'))
                        or observed['Groups'].split() or int(observed['NoNewPrivs'])!=1
                        or int(observed['Seccomp'])!=2 or int(observed['Seccomp_filters'])<self.entry['seccomp_filters']
                        or any(int(observed[label],16) for label in ('CapEff','CapPrm','CapAmb'))):
                    raise FreshnessHold('invalidation_unsafe')
                if int(task.name)==self.original and int(fields[19])!=self.entry['birth']:
                    raise FreshnessHold('invalidation_unsafe')
                tasks.append({'tid':int(task.name),'birth':fields[19],
                    'no_new_privs':1,'seccomp':2,'seccomp_filters':int(observed['Seccomp_filters'])})
                if len(tasks)+sum(len(row['tasks']) for row in rows)>512:raise FreshnessHold('input_census_unknown')
            rows.append({'pid':pid,'tasks':sorted(tasks,key=lambda row:row['tid'])})
        self._wait(1,deadline)
        if set(map(int,self._read('cgroup.procs').split()))!=pids:raise FreshnessHold('input_census_unknown')
        def check_private():
            if time.monotonic()>=deadline:raise FreshnessHold('cleanup_uncertain')
            signal.pidfd_send_signal(self.pidfd,0);self._wait(1,deadline)
        descriptor=os.open(f'/proc/{self.original}/root',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:
            private_base=unit_views.validate_private_base(descriptor,self.mount_layout,self.mutable_roots,
                unit_uid=self.uid,check=check_private)
            if private_base!=self.private_base:raise FreshnessHold('invalidation_unsafe')
        finally:os.close(descriptor)
        expected=[{'pid':row['pid'],'tid':task['tid'],'birth':int(task['birth'])}
            for row in rows for task in row['tasks']]
        self.paused_cohort=staging.observe_uid_cohort(self.uid,expected,check=check_private)
        if self.paused_cohort['pid_namespace']!=self.pid_namespace:raise FreshnessHold('invalidation_unsafe')
        self.check_frozen_cohort()
        return {'version':1,'kind':'ci-unit-freeze-observation','scope':self.path.name,
            'cgroup_device':self.identity.st_dev,'cgroup_inode':self.identity.st_ino,'original_pid':self.original,
            'entry_sha256':hashlib.sha256(json.dumps(self.entry,sort_keys=True,separators=(',',':')).encode()).hexdigest(),
            'kernel_policy_sha256':self.entry['policy_activation']['policy']['sha256'],
            'mount_layout_sha256':self.mount_layout['sha256'],
            'private_base_sha256':self.private_base['base_sha256'],
            'uid_cohort':self.paused_cohort,
            'frozen':True,'processes':rows,'input_closure_complete':False,'shipping_authority':False}

    def check_frozen_cohort(self):
        # Only this controller can change the private cgroup, and the unit
        # policy prevents migration/credential namespace changes. Retain the
        # original census barrier while the same kernel scope stays frozen.
        if self.paused_cohort is None:raise FreshnessHold('cleanup_uncertain')
        fields=dict(line.split() for line in self._read('cgroup.events').splitlines())
        expected={row['pid'] for row in self.paused_cohort['tasks']}
        if fields.get(b'frozen')!=b'1' or set(map(int,self._read('cgroup.procs').split()))!=expected:
            raise FreshnessHold('cleanup_uncertain')

    def thaw(self,deadline):
        self._write('cgroup.freeze',b'0\n');self._wait(0,deadline)
        self.paused_cohort=None

    def close(self):
        if self.closed:return
        if self._read('cgroup.procs'):raise FreshnessHold('cleanup_uncertain')
        self.path.rmdir();self.closed=True
        if self.pidfd is not None:os.close(self.pidfd);self.pidfd=None


class RepositoryWorkerFreezer(UnitFreezer):
    """Original UID1000 population; launch admission precedes every exec grant.

    This uses the guest's shared worker mount namespace, not a unit's private
    base. Freezing establishes a bounded writer census, never input authority.
    """
    def __init__(self,admission,scope,lock,namespaces):
        if type(lock) is not WorkerProjectionLock:raise FreshnessHold('invalidation_unsafe')
        self.progress_path=admission.target/('.jobserver-credit-'+admission.command)/'worker-scope-progress.json'
        super().__init__(admission,scope,1000,_worker_token=_WORKER_GROUP_TOKEN)
        self.lock=lock;self.namespaces=dict(namespaces);self.members={}
        self.membership='0::/ci-workers/'+self.path.name+'\n'
        if (set(self.namespaces)!={'mnt','ipc','net','user','pid'} or any(
                value!=os.readlink('/proc/self/ns/'+name) for name,value in self.namespaces.items())):
            raise FreshnessHold('invalidation_unsafe')

    def _note(self,phase,**values):
        super()._note(phase,**values)
        cache._write_record(self.progress_path,{'version':1,'kind':'ci-worker-scope-progress',
            'binding':self.setup_record['binding'],'command':self.setup_record['command'],
            'phase':phase,'scope_id':self.setup_path.parent.name,
            'members':[{'pid':pid,'birth':member['birth']} for pid,member in sorted(getattr(self,'members',{}).items())],
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False})

    def _lease(self,kind):
        lease=self.lock.active
        if (self.owner!=os.getpid() or lease is None or lease.closed or lease.kind!=kind
                or lease.owner!=(os.getpid(),threading.get_ident())):raise FreshnessHold('cleanup_uncertain')

    def enroll(self,pid,pidfd,entry,*,supervisor,original=False):
        self._lease('launch');self._check()
        if pid in self.members or original!=(self.original is None):raise FreshnessHold('invalidation_unsafe')
        signal.pidfd_send_signal(pidfd,0)
        info=Path(f'/proc/self/fdinfo/{pidfd}').read_text().splitlines()
        if next((line.split()[1] for line in info if line.startswith('Pid:')),None)!=str(pid):
            raise FreshnessHold('cleanup_uncertain')
        actual=staging.observe_exec_entry(pid,tracer=supervisor)
        staging.require_exec_entry(actual,uid=1000,gid=1000,groups=[],descriptors=entry['descriptors'])
        if {key:entry[key] for key in actual}!=actual:raise FreshnessHold('invalidation_unsafe')
        process=Path('/proc')/str(pid)
        status=process.joinpath('stat').read_text().rsplit(')',1)[1].split()
        if (status[0]!='t' or int(status[1])!=supervisor
                or {task.name for task in process.joinpath('task').iterdir()}!={str(pid)}
                or any(os.readlink(f'/proc/{pid}/ns/'+name)!=namespace for name,namespace in self.namespaces.items())
                or process.joinpath('cgroup').read_text()!='0::/\n'):
            raise FreshnessHold('invalidation_unsafe')
        self._write('cgroup.procs',str(pid).encode()+b'\n')
        if (process.joinpath('cgroup').read_text()!=self.membership
                or process.joinpath('stat').read_text().rsplit(')',1)[1].split()[19]!=str(entry['birth'])
                or str(pid).encode() not in self._read('cgroup.procs').split()):
            raise FreshnessHold('invalidation_unsafe')
        self.members[pid]={'pidfd':os.dup(pidfd),'birth':entry['birth']}
        if original:
            self.original=pid;self.entry=copy.deepcopy(entry);self.pidfd=os.dup(pidfd)
        self._note('worker_enrolled',original_pid=self.original,enrolled_pid=pid,enrolled_birth=entry['birth'])
        return {'pid':pid,'birth':entry['birth'],'scope_id':self.path.name,
            'cgroup_device':self.identity.st_dev,'cgroup_inode':self.identity.st_ino,'shipping_authority':False}

    def descriptor(self):
        self._check()
        if self.original is None:raise FreshnessHold('cleanup_uncertain')
        return {'version':1,'kind':'ci-repository-worker-scope','scope_id':self.path.name,'uid':1000,
            'cgroup_device':self.identity.st_dev,'cgroup_inode':self.identity.st_ino,
            'original_pid':self.original,'original_birth':self.entry['birth'],'pid_namespace':self.pid_namespace,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}

    def drained(self,pid):
        self._check()
        member=self.members.get(pid)
        if member is None or not select.select([member['pidfd']],[],[],0)[0]:
            raise FreshnessHold('cleanup_uncertain')
        # Called only after the original supervisor's full descendant cleanup
        # and exit record. pidfd readiness by itself never authorizes this.
        os.close(member['pidfd']);del self.members[pid]

    def freeze(self,deadline):
        self._lease('pause')
        if self.original is None or self.entry is None:raise FreshnessHold('cleanup_uncertain')
        signal.pidfd_send_signal(self.pidfd,0)
        self._note('freezing_workers')
        self._write('cgroup.freeze',b'1\n');self._wait(1,deadline)
        self._note('frozen_workers')
        rows=[];expected=[];pids=set(map(int,self._read('cgroup.procs').split()))
        if self.original not in pids or not 1<=len(pids)<=512:raise FreshnessHold('input_census_unknown')
        for pid in sorted(pids):
            process=Path('/proc')/str(pid)
            if (process.joinpath('cgroup').read_text()!=self.membership or any(
                    os.readlink(f'/proc/{pid}/ns/'+name)!=namespace for name,namespace in self.namespaces.items())):
                raise FreshnessHold('invalidation_unsafe')
            tasks=[]
            for task in process.joinpath('task').iterdir():
                fields=task.joinpath('stat').read_text().rsplit(')',1)[1].split()
                status=dict(line.split(':',1) for line in task.joinpath('status').read_text().splitlines())
                if (any(list(map(int,status[label].split()))!=[1000]*4 for label in ('Uid','Gid'))
                        or status['Groups'].split() or int(status['NoNewPrivs'])!=1
                        or int(status['Seccomp'])!=2 or int(status['Seccomp_filters'])<self.entry['seccomp_filters']
                        or any(int(status[label],16) for label in ('CapEff','CapPrm','CapAmb'))):
                    raise FreshnessHold('invalidation_unsafe')
                if int(task.name)==self.original and int(fields[19])!=self.entry['birth']:
                    raise FreshnessHold('invalidation_unsafe')
                expected.append({'pid':pid,'tid':int(task.name),'birth':int(fields[19])})
                if len(expected)>512:raise FreshnessHold('input_census_unknown')
                tasks.append({'tid':int(task.name),'birth':int(fields[19])})
            rows.append({'pid':pid,'tasks':sorted(tasks,key=lambda row:row['tid'])})
        def check():
            if time.monotonic()>=deadline:raise FreshnessHold('cleanup_uncertain')
            signal.pidfd_send_signal(self.pidfd,0);self._wait(1,deadline)
        self._note('worker_uid_census')
        self.paused_cohort=staging.observe_uid_cohort(1000,expected,check=check)
        if self.paused_cohort['pid_namespace']!=self.pid_namespace:raise FreshnessHold('invalidation_unsafe')
        self.check_frozen_cohort()
        self._note('worker_pause_verified')
        return {'version':1,'kind':'ci-worker-freeze-observation','scope':self.descriptor(),
            'uid_cohort':self.paused_cohort,'processes':rows,'frozen':True,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}

    def thaw(self,deadline):
        self._lease('pause');super().thaw(deadline)
        self._note('workers_thawed')

    def close(self):
        if self.members:raise FreshnessHold('cleanup_uncertain')
        super().close()


class UnitPeerRelay(RootRelay):
    """Private contained execution; effect transfer and CI authority stay fenced."""
    def __init__(self,admission,*,library,ordinal,scope,ordinals,deadline,view,credit_parent=None,original_dispatcher=None):
        if type(ordinal) is not int or not 0<=ordinal<4096 or not cache._uuid(Path(scope).name):
            raise FreshnessHold('invalidation_unsafe')
        if (type(view) is not dict or set(view) not in ({'root','artifacts','source','mounts','ordinal'},
                {'root','artifacts','source','mounts','ordinal','working'})
                or view['ordinal']!=ordinal):raise FreshnessHold('invalidation_unsafe')
        super().__init__(admission,library=library,timeout=min(900,max(1,deadline-time.monotonic())),
            deadline=deadline,ordinals=ordinals,client_path='/run/ci-unit.sock')
        self.peer_uid=2000+ordinal;self.config_uid=self.peer_uid;self.config_magic=b'JARVUNT3'
        if credit_parent is not None:NestedJobserverCredits._ordinal(credit_parent)
        self.credit_parent=credit_parent
        if not isinstance(original_dispatcher,RootRelay) or original_dispatcher.ordinals is not ordinals:
            raise FreshnessHold('generation_mismatch')
        self.original_dispatcher=original_dispatcher
        self.require_message_credentials=True;self.parent_ordinal=ordinal;self.scope=Path(scope)
        self.scope_pidfd=None;self.scope_namespaces=None;self.scope_root=None;self.received_requests=0;self.closed=False
        self.staged_signals=None
        self.active_record=None
        self._slots=threading.BoundedSemaphore(1)
        self.view=dict(view);self.mutation_lock=threading.RLock()
        self.captures=self.root/'captures';self.captures.mkdir(mode=0o700)
        self.freezer=UnitFreezer(admission,scope,self.peer_uid)

    def register_parent(self,pid,path,*,supervisor_pid):
        if self.scope_pidfd is not None:raise FreshnessHold('invalidation_unsafe')
        record=cache._read_record(path)
        if (record.get('binding')!=self.admission.binding or record.get('command')!=self.admission.command
                or record.get('ordinal')!=self.parent_ordinal or record.get('pid')!=pid):
            raise FreshnessHold('generation_mismatch')
        entered=cache._read_record(self.scope/'exec-entry.json')
        if (entered.get('binding')!=self.admission.binding or entered.get('command')!=self.admission.command
                or entered.get('ordinal')!=self.parent_ordinal):raise FreshnessHold('generation_mismatch')
        entry=entered['entry'];actual=staging.observe_exec_entry(pid,tracer=supervisor_pid)
        if {key:entry[key] for key in actual}!=actual:raise FreshnessHold('invalidation_unsafe')
        measured=UnitPolicy()
        expected={'version':1,'kind':'ci-unit-policy-receipt','binding':self.admission.binding,
            'command':self.admission.command,'ordinal':self.parent_ordinal,'pid':pid,
            'policy':{'version':1,'kind':'ci-unit-policy-activation','sha256':measured.sha256,
                'size_bytes':measured.size_bytes,'activation_returncode':0,
                'input_closure_complete':False,'full_ci':False,'shipping_authority':False}}
        if entry['policy_activation']!=expected:raise FreshnessHold('authority_changed')
        layout=cache._read_record(self.scope/'mount-layout.json',max_bytes=16*1024**2)
        if (layout.get('binding')!=self.admission.binding or layout.get('command')!=self.admission.command
                or layout.get('ordinal')!=self.parent_ordinal
                or layout.get('entry_sha256')!=hashlib.sha256(json.dumps(entry,sort_keys=True,separators=(',',':')).encode()).hexdigest()
                or layout.get('layout')!=unit_views.observe_mount_layout(pid)
                or layout['layout']['sha256']!=entry['mount_layout_sha256']):
            raise FreshnessHold('invalidation_unsafe')
        mounts=cache._read_record(Path(self.view['mounts']),max_bytes=16*1024**2)
        if (mounts.get('binding')!=self.admission.binding or mounts.get('command')!=self.admission.command
                or mounts.get('ordinal')!=self.parent_ordinal):raise FreshnessHold('generation_mismatch')
        root_descriptor=os.open(f'/proc/{pid}/root',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:
            verified=unit_views.validate_mount_bindings(root_descriptor,layout['layout'],
                unit_mount_rules(self.view,mounts,library=self.library,endpoint=self.path))
            mutable_roots=unit_mutable_roots(mounts)
            private_base=unit_views.validate_private_base(root_descriptor,layout['layout'],mutable_roots,
                unit_uid=self.peer_uid,check=lambda:None)
        finally:os.close(root_descriptor)
        binding_record={'version':1,
            'binding':self.admission.binding,'command':self.admission.command,'ordinal':self.parent_ordinal,
            'entry_sha256':layout['entry_sha256'],'mount_bindings':verified,'private_base':private_base,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        cache._write_record(self.scope/'mount-bindings.json',binding_record)
        namespaces={name:os.readlink(f'/proc/{pid}/ns/'+name) for name in ('mnt','ipc','net','user','pid')}
        if any(record['namespaces'][name]['unit']!=namespaces[name] for name in ('mnt','ipc','net')):
            raise FreshnessHold('invalidation_unsafe')
        self.scope_root=record['root_identity'];self.scope_namespaces=namespaces;self.scope_pidfd=os.pidfd_open(pid)
        self.freezer.attach(pid,self.scope_pidfd,entry=entry,mount_layout=layout['layout'],
            private_base=private_base,mutable_roots=mutable_roots,deadline=self.deadline)
        # Original root compiler scopes otherwise disappear with the guest
        # controller tree. Preserve the same entry and global UID census.
        cache._write_record(Path(self.view['root']).parent/'entry-boundary.json',{
            'version':1,'kind':'ci-unit-entry-boundary','binding':self.admission.binding,
            'command':self.admission.command,'ordinal':self.parent_ordinal,'scope':self.scope.name,
            'entry':entered,'mount_layout':layout,'mount_bindings':binding_record,
            'uid_cohort':self.freezer.entry_uid_cohort,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False})
        self.staged_signals=StagedParentSignals(self.scope_pidfd,lock=self.ordinals.signal_lock)
        staged=cache._read_record(self.scope/'stdin.json')
        if (staged.get('binding')!=self.admission.binding or staged.get('command')!=self.admission.command
                or staged.get('ordinal')!=self.parent_ordinal or staged.get('shipping_authority') is not False):
            raise FreshnessHold('generation_mismatch')
        self.ordinals.credits.enter(self.parent_ordinal,self.scope.name,staged.get('jobserver'),parent=self.credit_parent)

    def forward_signal(self,number):
        if self.staged_signals is None:raise FreshnessHold('invalidation_unsafe')
        disposition=self.staged_signals.forward(number)
        if disposition=='queued':self.request_cancel()
        return disposition

    def execution_cancelled(self):
        return (super().execution_cancelled() or self.scope_pidfd is None
            or bool(select.select([self.scope_pidfd],[],[],0)[0])
            or self.staged_signals is not None and self.staged_signals.cancelled.is_set())

    def unit_signal_grant(self,value,transport,grant):
        record=self.active_record
        if (record is None or record['last_request_ordinal']!=value['ordinal']
                or type(transport) is not UnitPeerRelay or transport.staged_signals is None):
            raise FreshnessHold('generation_mismatch')
        self.staged_signals.activate_child(value['ordinal'],transport.staged_signals,grant)

    def unit_credit_drained(self,value,transport,started):
        if not started:return
        loan=self.ordinals.credits.return_child(self.parent_ordinal,value['ordinal'])
        record=self.active_record
        if record is None or record['last_request_ordinal']!=value['ordinal']:
            raise FreshnessHold('generation_mismatch')
        record['jobserver_loan']['state']=loan['state'];cache._write_record(self.root/'admission.json',record)

    def unit_signal_drained(self,value,transport,started):
        if not started:return
        if type(transport) is not UnitPeerRelay or transport.staged_signals is None:
            raise FreshnessHold('cleanup_uncertain')
        self.staged_signals.child_drained(value['ordinal'],transport.staged_signals)

    def unit_started(self,value,directory):
        record=self.active_record
        if record is None or record['last_request_ordinal']!=value['ordinal']:raise FreshnessHold('generation_mismatch')
        record['dispatch_tested']=True
        record['child_dispatch']={'ordinal':value['ordinal'],'uid':execution_identity(value),'scope':directory.name,
            'exec_observed':True,'exec_grant_sent':True,'shipping_authority':False}
        cache._write_record(self.root/'admission.json',record)

    def request_cancel(self):
        self._stop.set()
        if self.staged_signals is not None:self.staged_signals.cancel()

    def rejected_request(self):
        self.request_cancel()
        self.errors.append(diagnostic({'stage':'planner_request_receive','category':'FreshnessHold','errno':errno.EBUSY}))

    def _receive(self,connection,descriptors):
        numbers=super()._receive(connection,descriptors)
        if self.scope_pidfd is None:raise FreshnessHold('invalidation_unsafe')
        signal.pidfd_send_signal(self.scope_pidfd,0)
        pid,uid,gid=struct.unpack('3i',connection.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
        status=Path(f'/proc/{pid}/status').read_text()
        for label in ('Uid:','Gid:'):
            row=next(line for line in status.splitlines() if line.startswith(label))
            if list(map(int,row.split()[1:]))!=[self.peer_uid]*4:raise FreshnessHold('invalidation_unsafe')
        if next(line for line in status.splitlines() if line.startswith('Groups:')).split()[1:]:
            raise FreshnessHold('invalidation_unsafe')
        if any(os.readlink(f'/proc/{pid}/ns/'+name)!=value for name,value in self.scope_namespaces.items()):
            raise FreshnessHold('invalidation_unsafe')
        root=os.stat(f'/proc/{pid}/root')
        if self.scope_root!={'device':(os.major(root.st_dev)<<20)|os.minor(root.st_dev),'inode':root.st_ino,'mode':root.st_mode}:
            raise FreshnessHold('invalidation_unsafe')
        self.validate_jobserver_request(descriptors,numbers)
        return numbers

    def validate_jobserver_request(self,descriptors,numbers):
        # This runs after authenticated receive and before stage_request_stdin.
        # A missing model must not get as far as draining an aliased stdin.
        _,_,environment,*_=payload(descriptors[0])
        _,_,jobserver=staging.normalize_jobserver(environment,dict(zip(numbers[3:],descriptors[4:])))
        if jobserver is None or self.ordinals.credits.require_owned(self.parent_ordinal)['jobserver']!=jobserver:
            raise FreshnessHold('input_census_unknown')

    def _run(self,connection,ordinal,descriptors,numbers,phase):
        # A child runs only in detached private working state under the original
        # supervision/grant. Its result cannot reach the paused parent before
        # verified effect transfer, and it grants no CI/reuse authority.
        body=payload(descriptors[0]);info=os.fstat(descriptors[0])
        with self._lock:
            self.received_requests+=1
            record={'version':1,'kind':'ci-unit-peer-admission',
                'binding':self.admission.binding,'command':self.admission.command,
                'parent_ordinal':self.parent_ordinal,'parent_scope':self.scope.name,'client_uid':self.peer_uid,
                'last_request_ordinal':ordinal,'received_requests':self.received_requests,
                'request_sha256':hashlib.sha256(os.pread(descriptors[0],info.st_size,0)).hexdigest(),
                'peer_and_message_credentials_bound':True,'original_namespaces_and_root_bound':True,
                'payload_validated':body is not None,'dispatch_tested':False,'shipping_authority':False}
            cache._write_record(self.root/'admission.json',record)
        with stage_request_stdin(connection,descriptors[1],deadline=self.deadline,
                stop=self._stop,owner_pidfd=self.scope_pidfd) as captured:
            record['stdin']=captured.observation()
            cache._write_record(self.root/'admission.json',record)
            phase[0]='planner_capture'
            with self.mutation_lock:
                environment=body[2]
                _,_,jobserver=staging.normalize_jobserver(environment,dict(zip(numbers[3:],descriptors[4:])))
                loan=self.ordinals.credits.prepare(self.parent_ordinal,ordinal,jobserver)
                record['jobserver_loan']={'credit_id':loan['credit_id'],'parent':self.parent_ordinal,
                    'child':ordinal,'state':'prepared','shipping_authority':False}
                cache._write_record(self.root/'admission.json',record)
                self.staged_signals.pause(ordinal)
                child_attempted=False;contained_views=None
                try:
                    record['pause']=self.freezer.freeze(self.deadline)
                    cache._write_record(self.root/'admission.json',record)
                    record['capture']=self._capture(ordinal,record,descriptors[2:4])
                    cache._write_record(self.root/'admission.json',record)
                    if self._stop.is_set() or self.staged_signals.cancelled.is_set():raise FreshnessHold('cleanup_uncertain')
                    contained_views=_ContainedViews(self,ordinal);self.views=contained_views;self.active_record=record
                    replacement=os.dup(captured.fileno());os.close(descriptors[1]);descriptors[1]=replacement
                    child_attempted=True
                    loan=self.ordinals.credits.lend(self.parent_ordinal,ordinal,record['pause'])
                    pipe=staging.observe_jobserver(environment,dict(zip(numbers[3:],descriptors[4:])))
                    ledger=self.ordinals.credits.observation()
                    lent_roots=sorted(row['ordinal'] for row in ledger['units'].values()
                        if row['parent'] is None and row['state']=='lent' and row['jobserver'] is not None
                        and row['jobserver']['kernel_pipe_sha256']==pipe['kernel_pipe_sha256'])
                    record['jobserver_loan'].update(state=loan['state'],pipe=pipe,lent_root_units=lent_roots,
                        dispatcher=self.original_dispatcher.runnable_observation(),observed_monotonic_ns=time.monotonic_ns())
                    cache._write_record(self.root/'admission.json',record)
                    result=super()._run(connection,ordinal,descriptors,numbers,phase,staged=True,defer_terminal=True)
                    record['child_result']={'returncode':result['returncode'],'original_cleanup':result['original_cleanup'],
                        'exec_observed':result['exec_observed'],'scope':result['directory'].name,
                        'effects_materialized':False,'shipping_authority':False}
                    # Terminal results and parent resume remain fenced until
                    # identity-preserving effect transfer is implemented.
                    raise FreshnessHold('input_census_unknown')
                finally:
                    self.active_record=None
                    if contained_views is not None:contained_views.close()
                    if child_attempted:
                        record['parent_resume_held']=True;self.request_cancel()
                    else:
                        self.ordinals.credits.abort_prepared(self.parent_ordinal,ordinal)
                        self.ordinals.credits.require_owned(self.parent_ordinal)
                        self.staged_signals.resume(ordinal,lambda:self.freezer.thaw(self.deadline))
                        if 'pause' in record:record['thaw_verified']=True
                    record['staged_signals']=self.staged_signals.observation()
                    cache._write_record(self.root/'admission.json',record)
        raise FreshnessHold('input_census_unknown')

    def _capture(self,ordinal,admission,outputs):
        mounts=cache._read_record(Path(self.view['mounts']),max_bytes=16*1024**2)
        if (mounts.get('binding')!=self.admission.binding or mounts.get('command')!=self.admission.command
                or mounts.get('ordinal')!=self.parent_ordinal):raise FreshnessHold('generation_mismatch')
        roots=unit_mutable_roots(mounts)
        state={'version':1,'kind':'ci-unit-input-capture','binding':self.admission.binding,
            'command':self.admission.command,'parent_ordinal':self.parent_ordinal,'parent_scope':self.scope.name,
            'request_ordinal':ordinal,'request_sha256':admission['request_sha256'],
            'pause_sha256':hashlib.sha256(json.dumps(admission['pause'],sort_keys=True,separators=(',',':')).encode()).hexdigest(),
            'state':'preparing','alias_closure_complete':False,'input_closure_complete':False,
            'full_ci':False,'shipping_authority':False}
        path=self.captures/(str(ordinal)+'.json');cache._write_record(path,state)
        def check():
            if (self._stop.is_set() or self.staged_signals.cancelled.is_set() or time.monotonic()>=self.deadline
                    or select.select([self.scope_pidfd],[],[],0)[0]):raise FreshnessHold('cleanup_uncertain')
            self.freezer.check_frozen_cohort()
        check()
        descriptor=os.open(f'/proc/{self.freezer.original}/root',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:
            root=os.fstat(descriptor)
            if self.scope_root!={'device':(os.major(root.st_dev)<<20)|os.minor(root.st_dev),'inode':root.st_ino,'mode':root.st_mode}:
                raise FreshnessHold('invalidation_unsafe')
            capture=unit_views.capture_roots(descriptor,self.captures/str(ordinal),roots,
                owners=(0,self.peer_uid),check=check)
            unit_views.validate_capture_stdio(capture,outputs,parent_uid=self.peer_uid);check()
            projection_stdio=unit_views.reject_projection_stdio(outputs,GeneratedForests.WORKSPACE,
                mounts['declared_outputs'],owner=GeneratedForests._WORKER_UID,check=check)
            state.update(state='captured',filesystem=capture,worker_output_stdio=projection_stdio)
            encoded=json.dumps(state,sort_keys=True,separators=(',',':')).encode()
            if len(encoded)>16*1024**2:raise FreshnessHold('input_census_unknown')
            cache._write_record(path,state);check()
            return {'record_sha256':hashlib.sha256(encoded).hexdigest(),'payload_sha256':capture['payload_sha256'],
                'copied_entries':capture['copied_entries'],'copied_bytes':capture['copied_bytes'],
                'alias_closure_complete':False,'original_metadata_emulated':False,
                'input_closure_complete':False,'shipping_authority':False}
        finally:os.close(descriptor)

    def check(self):
        self.__exit__(None,None,None)
        if self.received_requests or self.errors:raise FreshnessHold('input_census_unknown')

    def __exit__(self,*arguments):
        if self.closed:return
        try:super().__exit__(*arguments);self.freezer.close();self.closed=True
        finally:
            if self.staged_signals is not None:self.staged_signals.close()
            if self.scope_pidfd is not None:os.close(self.scope_pidfd);self.scope_pidfd=None


class _ContainedViews:
    """Private working bytes for a contained call; no publication capability."""
    def __init__(self,relay,request):
        self.relay=relay;self.request=request;self.units={};self.retained={}
        self.capture=cache._read_record(relay.captures/(str(request)+'.json'),max_bytes=16*1024**2)
        if (self.capture.get('binding')!=relay.admission.binding or self.capture.get('command')!=relay.admission.command
                or self.capture.get('state')!='captured' or self.capture.get('parent_scope')!=relay.scope.name
                or self.capture.get('request_ordinal')!=request or self.capture.get('shipping_authority') is not False):
            raise FreshnessHold('generation_mismatch')
        self.root=relay.root/'contained';self.root.mkdir(mode=0o700,exist_ok=True)

    def prepare(self,ordinal,request):
        # A nested Cargo request requires the separate protected Cargo planner;
        # it never receives an ordinary writable-target execution fallback.
        if worker_cargo_request(request):raise FreshnessHold('input_census_unknown')
        relay=self.relay
        if ordinal!=self.request or relay._stop.is_set() or relay.staged_signals.cancelled.is_set():
            raise FreshnessHold('invalidation_unsafe')
        slot=self.root/str(ordinal);slot.mkdir(mode=0o700);working=slot/'working'
        self.check()
        copied=unit_views.materialize_capture(relay.captures/str(ordinal),self.capture['filesystem'],working,
            parent_uid=relay.peer_uid,unit_uid=2000+ordinal,check=self.check)
        descriptor=os.open(f'/proc/{relay.freezer.original}/root',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:
            root=os.fstat(descriptor)
            if relay.scope_root!={'device':(os.major(root.st_dev)<<20)|os.minor(root.st_dev),'inode':root.st_ino,'mode':root.st_mode}:
                raise FreshnessHold('invalidation_unsafe')
            self.retained[ordinal]=unit_views.RetainedInodes(descriptor,self.capture['filesystem'],working,copied,
                parent_uid=relay.peer_uid,unit_uid=2000+ordinal,check=self.check)
        finally:os.close(descriptor)
        mounts=cache._read_record(Path(relay.view['mounts']),max_bytes=16*1024**2)
        if (mounts.get('binding')!=relay.admission.binding or mounts.get('command')!=relay.admission.command
                or mounts.get('ordinal')!=relay.parent_ordinal):raise FreshnessHold('generation_mismatch')
        mounts=dict(mounts,ordinal=ordinal);path=slot/'mounts.json';cache._write_record(path,mounts)
        cache._write_record(slot/'working-copy.json',{'version':1,'binding':relay.admission.binding,
            'command':relay.admission.command,'parent_scope':relay.scope.name,'request_ordinal':ordinal,
            'working_copy':copied,'input_closure_complete':False,'shipping_authority':False})
        root=slot/'root';root.mkdir(mode=0o700)
        view={'root':str(root),'artifacts':str(working/'workspace/target'),'source':relay.view['source'],
            'mounts':str(path),'ordinal':ordinal,'working':str(working)}
        self.units[ordinal]=view;return view

    def check(self):
        relay=self.relay
        if relay.execution_cancelled() or time.monotonic()>=relay.deadline:
            raise FreshnessHold('cleanup_uncertain')
        relay.freezer.check_frozen_cohort()

    def close(self):
        for retained in self.retained.values():retained.close()
        self.retained.clear()

    def running(self,view):pass

    def finish(self,view,directory,raw):
        relay=self.relay
        record=cache._read_record(directory/'exit-status.json')
        if (record.get('binding')!=relay.admission.binding or record.get('command')!=relay.admission.command
                or record.get('ordinal')!=view['ordinal'] or record.get('returncode')!=raw
                or record.get('exec_observed') is not True):raise FreshnessHold('cleanup_uncertain')
        self.check();slot=Path(view['root']).parent
        retained=self.retained[view['ordinal']]
        plan=retained.prepare_transfer(slot/'transfer',relay.captures/str(view['ordinal']),check=self.check)
        if cache._read_record(slot/'transfer/plan.json',max_bytes=16*1024**2)!=plan:
            raise FreshnessHold('input_census_unknown')
        digest=hashlib.sha256(json.dumps(plan,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        cache._write_record(slot/'transfer-intent.json',{'version':1,'kind':'ci-contained-transfer-intent',
            'binding':relay.admission.binding,'command':relay.admission.command,'parent_scope':relay.scope.name,
            'child_scope':directory.name,'ordinal':view['ordinal'],'state':'prepared','plan_sha256':digest,
            'origin':{'returncode':raw,'failed':raw!=0,'reusable':False},'effects_materialized':False,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False})
        cache._write_record(slot/'old-inode-effects.json',{'version':1,'binding':relay.admission.binding,
            'command':relay.admission.command,'parent_scope':relay.scope.name,'child_scope':directory.name,
            'ordinal':view['ordinal'],'old_effects':plan['old_effects'],'payload_root':'transfer/old',
            'effects_materialized':False,'input_closure_complete':False,'shipping_authority':False})
        cache._write_record(slot/'contained-result.json',{
            'version':1,'kind':'ci-contained-result-observation','binding':relay.admission.binding,
            'command':relay.admission.command,'parent_scope':relay.scope.name,'child_scope':directory.name,
            'ordinal':view['ordinal'],'uid':2000+view['ordinal'],'returncode':raw,
            'original_exec_observed':True,'original_child_cleanup':True,'effects_materialized':False,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False})


class WorkerRelay(RootRelay):
    """Repository-worker transport, with a distinct sealed UID-1000 role.

    Ordinary worker children keep the worker credential and execution guard.
    Only a pinned Cargo request enters the isolated UID-1001 planner view.
    This routing does not establish generated-input closure or CI authority.
    """
    def __init__(self,admission,*,planner_relay,value):
        super().__init__(admission,library=value['library'],timeout=min(900,max(1,value['deadline']-time.monotonic())),deadline=value['deadline'],ordinals=planner_relay.ordinals)
        self.peer_uid=1000;self.config_magic=b'JARVWRK3';self.require_message_credentials=True;self.planner_relay=planner_relay
        self.value=value;self.executor=None;self.cargo_lock=threading.Lock();self.cargo_owner=None;self.cargo_commands=[];self.metadata_units=[]
        self.original_worker_entry=None
        self.projection_lock=WorkerProjectionLock();self.worker_freezer=None;self.worker_members={}

    def __enter__(self):
        super().__enter__()
        # Both credentials traverse this root, but neither can list or write
        # it. Their child listener directories/sockets enforce their own GID.
        self.root.chmod(0o711)
        return self

    def _receive(self,connection,descriptors):
        numbers=super()._receive(connection,descriptors)
        pid,uid,gid=struct.unpack('3i',connection.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
        if any(os.readlink(f'/proc/{pid}/ns/'+name)!=os.readlink('/proc/self/ns/'+name)
                for name in ('mnt','ipc','net','user','pid')):raise FreshnessHold('input_census_unknown')
        return numbers

    def _run(self,connection,ordinal,descriptors,numbers,phase):
        is_cargo=worker_cargo_request(payload(descriptors[0]));locked=False
        try:
            if is_cargo:
                locked=self.cargo_lock.acquire(blocking=False)
                if not locked:raise FreshnessHold('input_census_unknown')
                self.cargo_owner=ordinal
            super()._run(connection,ordinal,descriptors,numbers,phase)
        finally:
            if locked and self.cargo_owner==ordinal:
                self.cargo_owner=None;self.cargo_lock.release()

    def check_worker_scope(self):
        if self.execution_cancelled() or time.monotonic()>=self.value['deadline']:
            raise FreshnessHold('cleanup_uncertain')

    def unit_launch(self,value):
        if value.get('execution_role')!='worker':return None
        lease=self.projection_lock.launch(value['deadline'],self.check_worker_scope)
        if self.worker_freezer is None:
            lease.close();raise FreshnessHold('cleanup_uncertain')
        return lease

    def unit_before_grant(self,value,pid,pidfd,supervisor,directory):
        if value.get('execution_role')!='worker':return
        entry=cache._read_record(directory/'exec-entry.json')
        if (entry.get('binding')!=self.admission.binding or entry.get('command')!=self.admission.command
                or entry.get('ordinal')!=value['ordinal']):raise FreshnessHold('generation_mismatch')
        observed=entry['entry'];policy=UnitPolicy()
        expected={'version':1,'kind':'ci-unit-policy-receipt','binding':self.admission.binding,
            'command':self.admission.command,'ordinal':value['ordinal'],'pid':pid,
            'policy':{'version':1,'kind':'ci-unit-policy-activation','sha256':policy.sha256,
                'size_bytes':policy.size_bytes,'activation_returncode':0,
                'input_closure_complete':False,'full_ci':False,'shipping_authority':False}}
        if observed['policy_activation']!=expected:raise FreshnessHold('authority_changed')
        member=self.worker_freezer.enroll(pid,pidfd,observed,supervisor=supervisor)
        self.worker_members[value['ordinal']]=member
        cache._write_record(directory/'worker-scope.json',member)

    def unit_credit_drained(self,value,transport,started):
        super().unit_credit_drained(value,transport,started)
        if value.get('execution_role')=='worker' and started:
            member=self.worker_members[value['ordinal']]
            self.worker_freezer.drained(member['pid'])

    def unit_finished(self,value,record):
        if value.get('execution_role')=='worker':
            if record['started']:record['worker_scope']=self.worker_members.pop(record['ordinal'])
            return
        if value.get('execution_role')!='cargo':return
        self.planner_relay.drain()
        if record['uid']!=1001 or self.cargo_owner!=record['ordinal']:raise FreshnessHold('cleanup_uncertain')
        units=self.planner_relay.completed[value['planner_first_unit']:]
        self.cargo_commands.append({'ordinal':record['ordinal'],'cargo_exit':record['returncode'],
            'original_cargo_cleanup':True,'compiler_calls':sum(
                self.executor.units[unit['ordinal']]['mapping']['kind']=='compile' for unit in units),'units':units})
        # Finish evidence and release the original Cargo slot before sending
        # its terminal response, so a sequential caller cannot race the slot.
        self.cargo_owner=None;self.cargo_lock.release()

    def request_context(self,ordinal,request):
        try:return self._request_context(ordinal,request)
        except (FreshnessHold,OSError) as error:
            file,argv,environment,cwd,search,*_=request
            cache._write_record(self.admission.target/('.jobserver-credit-'+self.admission.command)/'worker-held-request.json',{
                'version':1,'binding':self.admission.binding,'command':self.admission.command,
                'file_hex':file[:4096].hex(),'argv_hex':[arg[:2048].hex() for arg in argv[:32]],
                'cwd_hex':cwd[:4096].hex(),'search_hex':None if search is None else search[:4096].hex(),
                'stage':getattr(error,'unit_view_stage','planner_cargo_request'),
                'worker_scope_phase':None if self.worker_freezer is None else self.worker_freezer.setup_record['phase'],
                'shipping_authority':False})
            raise

    def _request_context(self,ordinal,request):
        if not worker_cargo_request(request):
            worker_runtime_request(request,self.library)
            return {'execution_role':'worker','execution_uid':1000},self.config_fd()
        file,argv,environment,cwd,search,*_=request
        environment=strip_transport(environment,self.library)
        # The canonical executable is used only for this additional controller
        # metadata query. _stage executes the sealed original file/argv/search.
        invocation=validate_invocation(['/toolchain/bin/cargo',*[os.fsdecode(arg) for arg in argv[1:]]],
            {os.fsdecode(key):os.fsdecode(value) for key,value in environment.items()},os.fsdecode(cwd))
        if self.executor is None:
            with self.projection_lock.pause(self.value['deadline'],self.check_worker_scope):
                if self.worker_freezer is None or self.original_worker_entry is None:
                    raise FreshnessHold('cleanup_uncertain')
                paused=self.worker_freezer.freeze(self.value['deadline'])
                try:
                    self.executor=_CargoExecutor(self.planner_relay.admission,execution_lease=self.value.get('execution_lease'),generated_outputs=self.value.get('generated_outputs'));self.planner_relay.views=self.executor
                    self.worker_freezer.check_frozen_cohort()
                    self.original_worker_entry['worker_pauses'].append(paused)
                    cache._write_record(self.executor.root/'original-worker-entry.json',self.original_worker_entry)
                finally:self.worker_freezer.thaw(self.value['deadline'])
            self.executor.preparing_graph=True
            metadata_start=len(self.planner_relay.completed)
            try:status,metadata,error=self.executor._command(dict(self.value,original_invocation=invocation),
                self.planner_relay,operation='metadata')
            finally:self.executor.preparing_graph=False
            self.metadata_units=list(self.planner_relay.completed[metadata_start:])
            if status!=0:raise cargo_hold('planner_cargo_request')
            self.executor.graph=CargoGraph(self.executor.source,metadata,owner=0)
        self.executor.graph._verify()
        return {'execution_role':'cargo','execution_uid':1001,'planner_first_unit':len(self.planner_relay.completed),'cargo_root':str(self.executor.root),
            'source':str(self.executor.source),'source_mounts':str(self.executor.source_mounts_path)},self.planner_relay.config_fd()


def _unit(arguments):
    global _PHASE
    _PHASE='planner_unit'
    bootstrap,data,cancel,release,exec_release,control=map(int,arguments)
    value=_bootstrap(bootstrap,'unit');_parent_guard(value['parent'])
    descriptors=[bootstrap,data,cancel,release,exec_release,control,*value['extras'],*(() if value.get('relay_fd') is None else (value['relay_fd'],))]
    for fd in descriptors:os.set_inheritable(fd,False)
    mediation=ExecMediation(value['deadline'],execution_identity(value),cancel=cancel,minimum=max(256,max(value['numbers'])+1))
    raw_receipt=os.memfd_create('ci-unit-policy-receipt',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
    try:policy_receipt=fcntl.fcntl(raw_receipt,fcntl.F_DUPFD_CLOEXEC,max(256,max(value['numbers'])+1))
    finally:os.close(raw_receipt)
    stage=dict(value,kind='stage',parent=os.getpid(),exec_mediation_fd=mediation.child_fd,
        policy_receipt_fd=policy_receipt);stage_bootstrap=_sealed(stage)
    stage_identity=os.fstat(stage_bootstrap)
    endpoint=socket.socket(fileno=control);observed=False
    supervisor=_load('planner_original_supervisor','provider-supervisor.py')
    def spawned(child):
        pidfd=os.pidfd_open(child.pid)
        try:_control(endpoint,1,child.pid,pidfd)
        finally:os.close(pidfd)
        mediation.spawned(child)
    def executed(child):
        nonlocal observed
        expected=execution_identity(value)
        stdin=cache._read_record(Path(value['directory'])/'stdin.json')
        if (stdin.get('binding')!=value['binding'] or stdin.get('command')!=value['command']
                or stdin.get('ordinal')!=value['ordinal']):raise FreshnessHold('generation_mismatch')
        allowed=[0,1,2]
        if stdin['jobserver'] is not None:allowed.extend([3,4])
        relay_enabled=(value.get('execution_role') in ('worker','cargo')
            or 'transport' in value.get('view',{}))
        if relay_enabled:allowed.append(value['relay_fd'])
        entry=staging.observe_exec_entry(child.pid)
        staging.require_exec_entry(entry,uid=expected,gid=expected,groups=[],descriptors=allowed)
        entry['policy_activation']=staging.read_policy_receipt(policy_receipt,{
            'version':1,'kind':'ci-unit-policy-receipt','binding':value['binding'],
            'command':value['command'],'ordinal':value['ordinal'],'pid':child.pid,
            'policy':{'version':1,'kind':'ci-unit-policy-activation','sha256':mediation.policy.sha256,
                'size_bytes':mediation.policy.size_bytes,'activation_returncode':0,
                'input_closure_complete':False,'full_ci':False,'shipping_authority':False}})
        table={row['number']:row for row in entry['fd_table']}
        sealed=staging.observe_sealed_descriptor(child.pid,0)
        if (sealed['uid']!=0 or sealed['gid']!=0
                or sealed['sha256']!=stdin['stdin']['sha256']
                or sealed['size_bytes']!=stdin['stdin']['size_bytes']):raise FreshnessHold('invalidation_unsafe')
        entry['sealed_stdin']=sealed
        if relay_enabled:
            descriptor=value['relay_fd']
            original=staging.observe_sealed_descriptor(os.getpid(),descriptor)
            actual=staging.observe_sealed_descriptor(child.pid,descriptor)
            if original!=actual or actual['uid']!=0 or actual['gid']!=0:
                raise FreshnessHold('invalidation_unsafe')
            entry['sealed_relay']=actual
        for number in (1,2):
            info=os.fstat(number);actual=table[number]
            if ((actual['device'],actual['inode'],actual['type'],actual['flags']&os.O_ACCMODE)
                    !=(info.st_dev,info.st_ino,stat.S_IFMT(info.st_mode),
                        fcntl.fcntl(number,fcntl.F_GETFL)&os.O_ACCMODE)):
                raise FreshnessHold('invalidation_unsafe')
        if stdin['jobserver'] is not None:
            reader,writer=table[3],table[4]
            if (reader['type']!=stat.S_IFIFO or writer['type']!=stat.S_IFIFO
                    or (reader['device'],reader['inode'])!=(writer['device'],writer['inode'])
                    or reader['flags']&os.O_ACCMODE!=os.O_RDONLY
                    or writer['flags']&os.O_ACCMODE!=os.O_WRONLY
                    or hashlib.sha256(f"{reader['device']}:{reader['inode']}".encode('ascii')).hexdigest()
                        !=stdin['jobserver']['kernel_pipe_sha256']):raise FreshnessHold('invalidation_unsafe')
        layout=None
        if 'view' in value:
            layout=unit_views.observe_mount_layout(child.pid);entry['mount_layout_sha256']=layout['sha256']
        cache._write_record(Path(value['directory'])/'exec-entry.json',{'version':1,
            'binding':value['binding'],'command':value['command'],'ordinal':value['ordinal'],'entry':entry,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False})
        if 'view' in value:
            cache._write_record(Path(value['directory'])/'mount-layout.json',{'version':1,
                'binding':value['binding'],'command':value['command'],'ordinal':value['ordinal'],
                'entry_sha256':hashlib.sha256(json.dumps(entry,sort_keys=True,separators=(',',':')).encode()).hexdigest(),
                'layout':layout,'input_closure_complete':False,'full_ci':False,'shipping_authority':False})
            namespaces={name:{'controller':os.readlink('/proc/self/ns/'+name),
                'unit':os.readlink(f'/proc/{child.pid}/ns/'+name)} for name in ('mnt','ipc','net')}
            if any(row['controller']==row['unit'] for row in namespaces.values()):raise FreshnessHold('invalidation_unsafe')
            root=os.stat(f'/proc/{child.pid}/root')
            if not stat.S_ISDIR(root.st_mode):raise FreshnessHold('invalidation_unsafe')
            cache._write_record(Path(value['directory'])/'view-namespace.json',{'version':1,
                'binding':value['binding'],'command':value['command'],'ordinal':value['ordinal'],
                'pid':child.pid,'namespaces':namespaces,
                'root_identity':{'device':(os.major(root.st_dev)<<20)|os.minor(root.st_dev),
                    'inode':root.st_ino,'mode':root.st_mode}})
        mediation.executed(child)
        _control(endpoint,3,child.pid);_release(exec_release,cancel,value['deadline']);observed=True
    try:
        forwarded=tuple([stage_bootstrap,data,cancel,release,control,mediation.child_fd,policy_receipt,*value['extras'],*(() if value.get('relay_fd') is None else (value['relay_fd'],))])
        argv=[sys.executable,'-I',str(Path(__file__).resolve()),'--stage',*map(str,(stage_bootstrap,data,cancel,release,control))]
        raw=supervisor.supervise(Path(value['directory'])/'cleanup',argv,forward_fds=forwarded,
            close_after_spawn=(stage_bootstrap,),return_raw_status=True,on_spawn=spawned,on_exec=executed)
        stage_bootstrap=None
        if 'view' in value:
            mediation.settle()
            cache._write_record(Path(value['view']['root']).parent/'exec-mediation.json',{
                'version':1,'kind':'ci-exec-mediation-observation','binding':value['binding'],
                'command':value['command'],'ordinal':value['ordinal'],'returncode':raw,
                'exec_observed':observed,'initial_attempts':sum(row['allowed'] for row in mediation.records),
                'denied_execs':sum(not row['allowed'] for row in mediation.records),
                'notification_errors':len(mediation.errors),'shipping_authority':False})
        mediation.check()
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
        os.close(policy_receipt);endpoint.close();mediation.close()


class UnitPolicy:
    """Constructor-free measured helper, loaded before a unit changes root."""
    def __init__(self):
        path=Path('/ci-tools/ci-cargo-unit-policy.so');descriptor=None
        self._activated=False
        try:
            if path.resolve()!=path:raise FreshnessHold('authority_changed')
            descriptor=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
            info=os.fstat(descriptor)
            if (not stat.S_ISREG(info.st_mode) or info.st_uid!=0 or info.st_mode&0o022
                    or info.st_nlink!=1 or not 1<=info.st_size<=2*1024**2):raise FreshnessHold('authority_changed')
            def measured():
                digest=hashlib.sha256();offset=0
                while offset<info.st_size:
                    block=os.pread(descriptor,min(65536,info.st_size-offset),offset)
                    if not block:raise FreshnessHold('authority_changed')
                    digest.update(block);offset+=len(block)
                current=os.fstat(descriptor)
                if (os.pread(descriptor,1,info.st_size)
                        or (current.st_mode,current.st_uid,current.st_nlink,current.st_size)
                            !=(info.st_mode,info.st_uid,info.st_nlink,info.st_size)):
                    raise FreshnessHold('authority_changed')
                return digest.hexdigest()
            self.sha256=measured();self.size_bytes=info.st_size
            # Load the checked inode, rather than looking its name up again.
            # This DSO has no relay constructor and is never LD_PRELOAD input.
            self.library=ctypes.CDLL(f'/proc/self/fd/{descriptor}',use_errno=True)
            if measured()!=self.sha256:raise FreshnessHold('authority_changed')
            self.restrict=self.library.jarvis_ci_restrict_unit
            self.restrict.argtypes=[];self.restrict.restype=ctypes.c_int
        except (OSError,AttributeError):raise FreshnessHold('authority_changed') from None
        finally:
            if descriptor is not None:os.close(descriptor)

    def activate(self):
        self._activated=False
        if self.restrict()!=0:raise FreshnessHold('invalidation_unsafe')
        self._activated=True

    def activation_observation(self):
        if not self._activated:raise FreshnessHold('invalidation_unsafe')
        return {'version':1,'kind':'ci-unit-policy-activation','sha256':self.sha256,
            'size_bytes':self.size_bytes,'activation_returncode':0,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}


class ExecMediation:
    """Root-local listener ownership under the original exec supervisor.

    The only CONTINUE recipient is its still-trusted, original bootstrap.
    Original on_exec must close that permission before acknowledging/releasing
    the kernel exec stop. No untrusted pathname/pointer is authorized here.
    """
    def __init__(self,deadline,uid,*,cancel=None,minimum=256):
        if (type(deadline) not in (int,float) or not math.isfinite(deadline)
                or not time.monotonic()<deadline<=time.monotonic()+901
                or type(uid) is not int or uid not in (1000,1001) and not 2000<=uid<6096
                or type(minimum) is not int or not 3<=minimum<=65535):raise FreshnessHold('invalidation_unsafe')
        self.deadline=deadline;self.uid=uid;self.cancel=cancel;self.setup_uid=os.geteuid()
        self.policy=UnitPolicy();self.native=self.policy.library
        self.native.jarvis_ci_exec_receive.argtypes=[ctypes.c_int,ctypes.POINTER(ctypes.c_uint64),
            ctypes.POINTER(ctypes.c_uint32),ctypes.POINTER(ctypes.c_int32)]
        self.native.jarvis_ci_exec_respond.argtypes=[ctypes.c_int,ctypes.c_uint64,ctypes.c_int]
        self.endpoint,child=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
        try:self.child_fd=fcntl.fcntl(child.fileno(),fcntl.F_DUPFD_CLOEXEC,minimum)
        finally:child.close()
        self.endpoint.setsockopt(socket.SOL_SOCKET,socket.SO_PASSCRED,1)
        self.listener=None;self.pid=None;self.pidfd=None;self.ack=False;self.initial=True
        self.errors=[];self.records=[];self._lock=threading.Lock();self._stop=threading.Event();self.thread=None

    def spawned(self,child):
        os.close(self.child_fd);self.child_fd=None
        self.pid=child.pid;self.pidfd=os.pidfd_open(child.pid)
        while True:
            remaining=self.deadline-time.monotonic()
            if remaining<=0:raise FreshnessHold('cleanup_uncertain')
            readers=select.select([self.endpoint]+([] if self.cancel is None else [self.cancel]),[],[],min(.05,remaining))[0]
            if self.cancel is not None and self.cancel in readers:return
            if self.endpoint in readers:break
        data,control,flags,_=self.endpoint.recvmsg(16,socket.CMSG_SPACE(4)+socket.CMSG_SPACE(12),socket.MSG_CMSG_CLOEXEC)
        received=[];credentials=[]
        try:
            for level,kind,body in control:
                if level==socket.SOL_SOCKET and kind==socket.SCM_RIGHTS and len(body)%4==0:
                    values=array.array('i');values.frombytes(body);received.extend(values)
                elif level==socket.SOL_SOCKET and kind==socket.SCM_CREDENTIALS and len(body)==12:
                    credentials.append(struct.unpack('3i',body))
                else:raise FreshnessHold('invalidation_unsafe')
            if (data!=b'listener' or flags&~socket.MSG_CMSG_CLOEXEC or len(received)!=1
                    or credentials!=[(self.pid,self.setup_uid,os.getegid())]):raise FreshnessHold('invalidation_unsafe')
            descriptor=received[0]
            if os.readlink(f'/proc/self/fd/{descriptor}')!='anon_inode:seccomp notify':raise FreshnessHold('invalidation_unsafe')
            self.listener=received.pop();os.set_inheritable(self.listener,False)
        finally:legacy._close(received)
        self.thread=threading.Thread(target=self._notifications);self.thread.start()

    def _notifications(self):
        try:
            poller=select.poll();poller.register(self.listener,select.POLLIN)
            while not self._stop.is_set():
                if time.monotonic()>=self.deadline:raise FreshnessHold('cleanup_uncertain')
                events=poller.poll(20)
                if not events:continue
                flags=events[0][1]
                if flags&(select.POLLERR|select.POLLNVAL):raise FreshnessHold('cleanup_uncertain')
                if flags&select.POLLHUP and not flags&select.POLLIN:return
                identity,pid,number=ctypes.c_uint64(),ctypes.c_uint32(),ctypes.c_int32()
                if self.native.jarvis_ci_exec_receive(self.listener,ctypes.byref(identity),ctypes.byref(pid),ctypes.byref(number))!=0:
                    if ctypes.get_errno() in (errno.ENOENT,errno.EINTR):continue
                    raise FreshnessHold('cleanup_uncertain')
                with self._lock:
                    if len(self.records)>=4096:raise FreshnessHold('input_census_unknown')
                    cancelled=self.cancel is not None and bool(select.select([self.cancel],[],[],0)[0])
                    allow=self.initial and pid.value==self.pid and not cancelled
                    if number.value not in (59,322):raise FreshnessHold('invalidation_unsafe')
                    if self.native.jarvis_ci_exec_respond(self.listener,identity.value,int(allow))!=0:
                        if ctypes.get_errno()==errno.ENOENT:continue  # Withdrawn; no decision committed.
                        raise FreshnessHold('cleanup_uncertain')
                    self.records.append({'pid':pid.value,'nr':number.value,'allowed':allow,'after_ack':self.ack})
        except BaseException:
            self.errors.append('exec_mediation_unavailable')
            try:signal.pidfd_send_signal(self.pidfd,signal.SIGKILL)
            except (OSError,TypeError):pass

    def executed(self,child):
        with self._lock:
            if self.ack or self.listener is None or child.pid!=self.pid or self.errors:raise FreshnessHold('invalidation_unsafe')
            signal.pidfd_send_signal(self.pidfd,0)
            status=Path(f'/proc/{child.pid}/status').read_text()
            for label in ('Uid:','Gid:'):
                row=next(line for line in status.splitlines() if line.startswith(label))
                if list(map(int,row.split()[1:]))!=[self.uid]*4:raise FreshnessHold('invalidation_unsafe')
            self.initial=False;self.ack=True

    def settle(self):
        self._stop.set()
        if self.thread is not None:
            self.thread.join(timeout=2)
            if self.thread.is_alive():raise FreshnessHold('cleanup_uncertain')

    def check(self):
        self.settle()
        if self.errors:raise FreshnessHold('cleanup_uncertain')
        if any(not record['allowed'] for record in self.records):raise FreshnessHold('input_census_unknown')

    def close(self):
        self._stop.set()
        if self.thread is not None:
            self.thread.join(timeout=2)
            if self.thread.is_alive():raise FreshnessHold('cleanup_uncertain')
        legacy._close(([] if self.child_fd is None else [self.child_fd])+([] if self.listener is None else [self.listener])+([] if self.pidfd is None else [self.pidfd]))
        self.endpoint.close()


def stage_exec_mediation(value,policy):
    descriptor=value.get('exec_mediation_fd')
    if type(descriptor) is not int or not 3<=descriptor<=65535:raise FreshnessHold('invalidation_unsafe')
    function=policy.library.jarvis_ci_exec_listener;function.argtypes=[];function.restype=ctypes.c_int
    listener=function()
    if listener<0:raise FreshnessHold('invalidation_unsafe')
    try:
        with socket.socket(fileno=descriptor) as channel:
            peer=struct.unpack('3i',channel.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
            if peer!=(value['parent'],os.geteuid(),os.getegid()):raise FreshnessHold('invalidation_unsafe')
            channel.sendmsg([b'listener'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[listener]))])
    finally:os.close(listener)


def arm_original_exec():
    if ctypes.CDLL(None,use_errno=True).ptrace(0,0,None,None)!=0:raise FreshnessHold('invalidation_unsafe')
    os.kill(os.getpid(),signal.SIGSTOP)


def _stage(arguments):
    global _PHASE
    bootstrap,data,cancel,release,control=map(int,arguments)
    value=_bootstrap(bootstrap,'stage');_parent_guard(value['parent'])
    if type(value['ordinal']) is not int or not 0<=value['ordinal']<4096:raise FreshnessHold('invalidation_unsafe')
    receipt=value['policy_receipt_fd']
    if type(receipt) is not int or not 3<=receipt<=65535:raise FreshnessHold('invalidation_unsafe')
    for fd in (bootstrap,data,cancel,release,control,value['exec_mediation_fd'],receipt,*value['extras'],*(() if value.get('relay_fd') is None else (value['relay_fd'],))):os.set_inheritable(fd,False)
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
    environment=strip_transport(environment,value['library'])
    role=value.get('execution_role','unit');uid=execution_identity(value)
    policy=UnitPolicy()
    if role in ('worker','cargo'):
        descriptor=value.get('relay_fd')
        if type(descriptor) is not int or not 3<=descriptor<=65535:raise FreshnessHold('invalidation_unsafe')
        os.set_inheritable(descriptor,True)
        environment[b'JARVIS_CARGO_RELAY_FD']=str(descriptor).encode()
        environment[b'LD_PRELOAD']=os.fsencode(value['library'])+(b' '+environment[b'LD_PRELOAD'] if b'LD_PRELOAD' in environment else b'')
        if role=='cargo':cargo_namespace(value['cargo_root'],value['source'],value.get('source_mounts'))
    if 'view' in value:
        _PHASE='planner_view_enter';view=value['view']
        mounts=cache._read_record(Path(view['mounts']),max_bytes=16*1024**2)
        if (mounts.get('binding')!=value['binding'] or mounts.get('command')!=value['command']
                or mounts.get('ordinal')!=value['ordinal']):raise FreshnessHold('generation_mismatch')
        unit_views.enter(view['root'],2000+value['ordinal'],view['source'],view['artifacts'],mounts['entries'],
            mounts.get('source_entries'),mounts.get('declared_outputs'),transport=view.get('transport'),working=view.get('working'))
        if 'transport' in view:
            descriptor=value.get('relay_fd')
            if type(descriptor) is not int or not 3<=descriptor<=65535:raise FreshnessHold('invalidation_unsafe')
            os.set_inheritable(descriptor,True)
            environment[b'JARVIS_CARGO_RELAY_FD']=str(descriptor).encode()
            environment[b'LD_PRELOAD']=b'/run/ci-cargo-relay.so'+(b' '+environment[b'LD_PRELOAD'] if b'LD_PRELOAD' in environment else b'')
    stage_exec_mediation(value,policy);policy.activate()
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    staging.publish_policy_receipt(receipt,{'version':1,'kind':'ci-unit-policy-receipt',
        'binding':value['binding'],'command':value['command'],'ordinal':value['ordinal'],
        'pid':os.getpid(),'policy':policy.activation_observation()})
    _PHASE='planner_drop'
    os.setgroups([]);os.setgid(uid);os.setuid(uid);os.umask(0o077)
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
        self.input_index=self.lookup_index(self.inputs)
        self.before=unit_views.inventory(self.inputs,owner=0,sealed=True)
        self.input_stat=(self.inputs/'deps/input').stat()
        self.directory_stats={p:_CargoProbe._stat(p) for p in (self.inputs/'deps',self.inputs/'deps/empty',self.inputs/'deps/cursor',self.inputs/'deps/src')}
        self.directory_members={os.fsencode(name):((self.inputs/'deps'/name).stat().st_ino,
            4 if name in ('empty','cursor','src') else 8) for name in ('input','one.rs','empty','cursor','src',os.fsdecode(b'raw-\xff'),'l'*255)}
        self.abstract=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM|socket.SOCK_CLOEXEC)
        self.abstract.bind(b'\0jarvis-unit-view-probe');self.abstract.listen(1)
        if Path('/tmp/ci-unit-probe-private-marker').exists():raise FreshnessHold('invalidation_unsafe')

    @staticmethod
    def _inputs(root,data,*,unread=b'entry\n',new_module=False):
        root.mkdir(mode=0o700);(root/'deps').mkdir(mode=0o700)
        (root/'deps/input').write_bytes(data);(root/'deps/input').chmod(0o444)
        for name in ('one.rs',os.fsdecode(b'raw-\xff'),'l'*255):
            (root/'deps'/name).write_bytes(unread if name=='one.rs' else b'entry\n');(root/'deps'/name).chmod(0o444)
        (root/'deps/empty').mkdir(mode=0o555)
        (root/'deps/cursor').mkdir(mode=0o555)
        (root/'deps/src').mkdir(mode=0o700);(root/'deps/src/lib.rs').write_bytes(b'entry\n')
        if new_module:
            (root/'deps/src/main.rs').write_bytes(b'introduced module\n')
            (root/'deps/src/main.rs').chmod(0o444);os.utime(root/'deps/src/main.rs',ns=(1,1))
        (root/'deps/src/lib.rs').chmod(0o444);(root/'deps/src').chmod(0o555)
        (root/'deps').chmod(0o555);root.chmod(0o555)

    def lookup_index(self,inputs):
        source=self.source/'src';generated=inputs/'deps'
        return lookup_inputs.FrozenLookupIndex({
            'source':('/workspace/src',source,unit_views.inventory(source,owner=0,sealed=True)),
            'generated':('/workspace/target/deps',generated,unit_views.inventory(generated,owner=0,sealed=True))},
            anchors={'/workspace':self.source},owner=0)

    def observe_content(self,events,metadata,pid,user_namespace,mount_namespace):
        if self.owner!=os.getpid() or self.receipt is None:raise FreshnessHold('cleanup_uncertain')
        selected=[e for e in events if e['uid']==2004 and e['pid']==e['tid']==pid
            and e['syscall']==262 and e['path']==b'src/main.rs' and e['result'] in (0,-errno.ENOENT)]
        if len(selected)!=2:raise FreshnessHold('input_census_unknown')
        observation=self.input_index.select(selected,metadata,pid=pid,uid=2004,
            user_namespace=user_namespace,mount_namespace=mount_namespace)
        unrelated=self.root/'selector-unrelated';self._inputs(unrelated,b'OLD',unread=b'other\n')
        introduced=self.root/'selector-introduced';self._inputs(introduced,b'OLD',new_module=True)
        same=observation.refresh(self.lookup_index(unrelated));changed=observation.refresh(self.lookup_index(introduced))
        original=observation.observation
        if original['sha256']!=same['sha256'] or original['sha256']==changed['sha256']:
            raise FreshnessHold('input_census_unknown')
        return {'queries':len(selected),'binding_sha256':observation.binding_sha256,
            'content_sha256':original['sha256'],'unrelated_sha256':same['sha256'],'introduced_sha256':changed['sha256'],
            'kernel_bases_bound':True,'unrelated_content_unchanged':True,'negative_lookup_changed_identity':True,
            'input_closure_complete':False,'shipping_authority':False}

    def observe_directories(self,events,pid,user_namespace,mount_namespace):
        """Apply the real selector to this unchanged native unit's scans."""
        if self.owner!=os.getpid() or self.receipt is None:raise FreshnessHold('cleanup_uncertain')
        selected,unbound=self.input_index.select_directory_subset([event for event in events
            if event['uid']==2004 and event['pid']==pid],pid=pid,uid=2004,
            user_namespace=user_namespace,mount_namespace=mount_namespace)
        queries=lookup_inputs._unwire(selected.record()['queries'])
        if {query['directory'] for query in queries}!={b'/workspace/target/deps',b'/workspace/target/deps/empty'}:
            raise FreshnessHold('input_census_unknown')
        cursor=(self.inputs/'deps/cursor').stat();cursor_calls=[event for event in events
            if event['uid']==2004 and event['pid']==pid and event['inode']==cursor.st_ino
                and event['device']==((os.major(cursor.st_dev)<<20)|os.minor(cursor.st_dev))]
        if len(cursor_calls)!=3 or not all({key:event[key] for key in ('pid','tid','uid','syscall','sequence')}
                in unbound for event in cursor_calls):raise FreshnessHold('input_census_unknown')
        unrelated=self.root/'selector-unrelated'
        following=self.root/'selector-directory-added';self._inputs(following,b'OLD')
        empty=following/'deps/empty';empty.chmod(0o755);(empty/'member').write_bytes(b'new membership')
        (empty/'member').chmod(0o444);empty.chmod(0o555)
        before=selected.observation['sha256'];same=selected.refresh(self.lookup_index(unrelated))['sha256']
        changed=selected.refresh(self.lookup_index(following))['sha256']
        if before!=same or before==changed:raise FreshnessHold('input_census_unknown')
        restored=lookup_inputs._ContentSelection.restore(selected.record(),self.input_index)
        if restored.observation!=selected.observation:raise FreshnessHold('input_census_unknown')
        return {'queries':len(queries),'unbound_calls':len(unbound),'binding_sha256':selected.binding_sha256,
            'content_sha256':before,'unrelated_sha256':same,'introduced_sha256':changed,
            'kernel_namespaces_bound':True,'rewound_scan_unbound':True,'membership_changed_identity':True,
            'input_closure_complete':False,'shipping_authority':False}

    def observe_descriptors(self,events,pid,user_namespace,mount_namespace):
        if self.owner!=os.getpid() or self.receipt is None:raise FreshnessHold('cleanup_uncertain')
        original=[event for event in events if event['syscall']==5 and event['uid']==2004 and event['pid']==pid]
        selected,unbound=self.input_index.select_descriptor_subset(original,pid=pid,uid=2004,
            user_namespace=user_namespace,mount_namespace=mount_namespace)
        paths={query['path'] for query in lookup_inputs._unwire(selected.record()['queries'])}
        if not {b'/workspace/target/deps',b'/workspace/target/deps/raw-\xff'}<=paths:
            raise FreshnessHold('input_census_unknown')
        failed=[event for event in original if event['result'] in (-errno.EFAULT,-errno.EBADF)]
        if (not {-errno.EFAULT,-errno.EBADF}<={event['result'] for event in failed}
                or not all({key:event[key] for key in ('pid','tid','uid','syscall','sequence')} in unbound for event in failed)):
            raise FreshnessHold('input_census_unknown')
        restored=lookup_inputs._ContentSelection.restore(selected.record(),self.input_index)
        if restored.observation!=selected.observation:raise FreshnessHold('input_census_unknown')
        replacement=self.root/'selector-descriptor-replacement';self._inputs(replacement,b'OLD')
        refreshed=selected.refresh(self.lookup_index(replacement))['sha256']
        if refreshed==selected.observation['sha256']:raise FreshnessHold('input_census_unknown')
        return {'queries':len(selected.record()['queries']),'unbound_calls':len(unbound),
            'binding_sha256':selected.binding_sha256,'metadata_sha256':selected.observation['sha256'],
            'replacement_metadata_sha256':refreshed,'kernel_namespaces_bound':True,
            'failed_calls_unbound':True,'replacement_metadata_changed':True,
            'input_closure_complete':False,'shipping_authority':False}

    def prepare(self,ordinal,request=None):
        if self.owner!=os.getpid() or ordinal!=4:raise FreshnessHold('invalidation_unsafe')
        self.slot=self.root/'unit';self.slot.mkdir(mode=0o700)
        artifacts=self.slot/'artifacts'
        entries=unit_views.clone_inputs(self.inputs,artifacts,self.before,owner=0,unit=2004)
        # One tree mount preserves directory identity and kernel membership.
        # clone_inputs creates the detached placeholders; do not overlap its
        # individual file bindings with the tree binding.
        entries=[e for e in entries if not bytes.fromhex(e[0]).startswith(b'deps/')]
        tree=unit_views.inventory(self.inputs/'deps',owner=0,sealed=True)
        raw=b'deps'.hex();entries.append([raw,str(self.inputs/'deps'),[raw,'tree',tree]])
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
                or any(_CargoProbe._stat(path)!=before for path,before in self.directory_stats.items())
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
 let expected=format!("#[path={:?}] mod generated_mod;\\npub const GENERATED:u8=generated_mod::VALUE;\\n",out.join("mod.rs"));
 assert_eq!(std::fs::read(out.join("generated.rs")).unwrap(),expected.as_bytes());
 assert!(out.join("empty").is_dir());
 assert_eq!(std::fs::read_dir(out.join("empty")).unwrap().count(),0);
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
 std::fs::create_dir_all(out.join("chosen")).unwrap();
 std::fs::write(out.join("chosen/mod.rs"),b"pub const VALUE:u8=7;\\n").unwrap();
 std::fs::write(out.join("mod.rs"),b"mod chosen;pub const VALUE:u8=chosen::VALUE;\\n").unwrap();
 let generated=format!("#[path={:?}] mod generated_mod;\\npub const GENERATED:u8=generated_mod::VALUE;\\n",out.join("mod.rs"));
 std::fs::write(out.join("generated.rs"),generated.as_bytes()).unwrap();
 let metadata=std::path::Path::new("target/debug/.fingerprint");
 std::fs::create_dir_all(metadata).unwrap();
 std::fs::write(metadata.join("build-script-forged"),b"private unit only").unwrap();
 println!("cargo:rerun-if-changed=build.rs");
}
'''


def validate_invocation(argv,environment,cwd):
    """Root-captured worker context; mediation only adds its private transport."""
    if (type(argv) is not list or not 2<=len(argv)<=4096 or argv[0]!='/toolchain/bin/cargo'
            or any(type(arg) is not str or not arg or '\0' in arg or len(arg)>65536 for arg in argv)
            or type(environment) is not dict or len(environment)>4096 or cwd!='/workspace'
            or any(type(key) is not str or not key or '=' in key or '\0' in key
                or type(value) is not str or '\0' in value for key,value in environment.items())
            or any(key.startswith('LD_') or key=='JARVIS_CARGO_RELAY_FD' for key in environment)
            or environment.get('CARGO_TARGET_DIR')!='/workspace/target'
            or environment.get('CARGO_HOME')!='/ci-cargo-home'):
        raise cargo_hold('planner_cargo_request')
    return {'argv':list(argv),'environment':dict(environment),'cwd':cwd}


def capture_command(child,deadline,*,limit=8*1024**2):
    """Bound original Cargo streams while they are produced, not after EOF."""
    output={'stdout':bytearray(),'stderr':bytearray()};size=0
    with selectors.DefaultSelector() as streams:
        streams.register(child.stdout,selectors.EVENT_READ,'stdout')
        streams.register(child.stderr,selectors.EVENT_READ,'stderr')
        while streams.get_map():
            remaining=deadline-time.monotonic()
            if remaining<=0:raise subprocess.TimeoutExpired(child.args,0)
            for key,_ in streams.select(min(remaining,.1)):
                block=os.read(key.fileobj.fileno(),65536)
                if not block:
                    streams.unregister(key.fileobj);key.fileobj.close();continue
                size+=len(block)
                if size>limit:raise FreshnessHold('input_census_unknown')
                output[key.data].extend(block)
    child.wait(timeout=max(.001,deadline-time.monotonic()))
    return bytes(output['stdout']),bytes(output['stderr'])


def artifact_input_bindings(store_root,entries,context,*,current=False):
    """Authenticate mounted inputs against original successful retained units.

    `context` must come from PublicationStore._retained for this command/binding.
    No pathname, root ownership or matching byte hash alone is producer proof.
    """
    try:
        if type(entries) is not list or len(entries)>4096 or type(current) is not bool:raise ValueError()
        state,publications,rows,_digest=context;objects=unit_views.literal(store_root)/'objects';result=[];seen=set()
        for entry in entries:
            if type(entry) is not list or len(entry)!=3:raise ValueError()
            raw,original,expected=entry;name=unit_views.relative(bytes.fromhex(raw));literal=str(name)
            if bytes.fromhex(raw).hex()!=raw or raw in seen:raise ValueError()
            seen.add(raw);original=unit_views.literal(original);parts=original.relative_to(objects).parts
            if len(parts)<2 or parts[0] not in publications:raise ValueError()
            producer=parts[0];publication=publications[producer]
            if original!=publication['root']/name or type(expected) is not list or len(expected)<2 or expected[0]!=raw:raise ValueError()
            if expected[1]=='file':
                if (publication['action'] not in ('library','check') or literal not in rows[producer]
                        or expected!=[raw,*rows[producer][literal][1:]]):raise ValueError()
                if current:
                    producer=state['files'].get(literal)
                    if producer not in publications or publications[producer]['action'] not in ('library','check'):raise ValueError()
                    original=publications[producer]['root']/name;expected=[raw,*rows[producer][literal][1:]]
            elif expected[1]=='tree':
                if (publication['action']!='generated' or literal not in publication['roots'] or len(expected)!=3
                        or expected[2]!=unit_views.inventory(original,owner=os.geteuid(),sealed=True)):raise ValueError()
                if current:
                    producer=state['namespaces'].get(literal)
                    if producer not in publications or publications[producer]['action']!='generated':raise ValueError()
                    original=publications[producer]['root']/name
                    expected=[raw,'tree',unit_views.inventory(original,owner=os.geteuid(),sealed=True)]
            else:raise ValueError()
            result.append([raw,str(original),copy.deepcopy(expected)])
        return result
    except (OSError,ValueError,KeyError,TypeError,IndexError,AttributeError):
        raise FreshnessHold('artifact_provenance_unknown') from None


def compiler_artifact_bindings(executor,binding,command):
    """Read the exact root-private mount manifests used by original compilers."""
    result={};context=None
    try:
        for ordinal,item in executor.units.items():
            if item['mapping']['kind']!='compile':continue
            view=item['view'];record=cache._read_record(Path(view['mounts']),max_bytes=16*1024**2)
            if (set(record)!={'version','binding','command','ordinal','entries','source_entries','declared_outputs'}
                    or type(record['version']) is not int or record['version']!=1
                    or record['binding']!=binding or record['command']!=command
                    or type(record['ordinal']) is not int or record['ordinal']!=ordinal
                    or type(view['ordinal']) is not int or view['ordinal']!=ordinal
                    or record['source_entries']!=executor.source_bindings or record['declared_outputs']!=executor.declared_outputs
                    or type(record['entries']) is not list or len(record['entries'])>4096):raise ValueError()
            entries=record['entries']
            if entries:
                if context is None:context=PublicationStore._retained(executor.store.root,binding=binding,command=command)
                entries=artifact_input_bindings(executor.store.root,entries,context)
            result[ordinal]=entries
        return result
    except (OSError,ValueError,KeyError,TypeError,AttributeError):raise FreshnessHold('input_census_unknown') from None


def frozen_input_index(source,bindings,scope):
    """Index original immutable mounts; caller authenticates their producers."""
    namespaces={'source':('/workspace',source,unit_views.inventory(source,owner=os.geteuid(),sealed=True))}
    for ordinal,(raw,original,expected) in enumerate(bindings):
        name=os.fsdecode(bytes.fromhex(raw));root=unit_views.literal(original)
        rows=expected[2] if expected[1]=='tree' else [['2e','file',*expected[2:]]]
        namespaces['generated_'+str(ordinal)]=('/workspace/'+name,root,rows)
    artifacts=scope.get('artifact_bindings',[])
    if type(artifacts) is not list or len(artifacts)>4096:raise FreshnessHold('input_census_unknown')
    seen=set()
    for ordinal,entry in enumerate(artifacts):
        try:
            if type(entry) is not list or len(entry)!=3:raise ValueError()
            raw,original,expected=entry;name=unit_views.relative(bytes.fromhex(raw))
            if bytes.fromhex(raw).hex()!=raw or raw in seen or type(original) is not str:raise ValueError()
            if (type(expected) is not list or len(expected)<2 or expected[0]!=raw
                    or expected[1] not in ('tree','file') or len(expected)!=(3 if expected[1]=='tree' else 5)):raise ValueError()
            root=unit_views.literal(original);seen.add(raw)
            rows=expected[2] if expected[1]=='tree' else [['2e','file',*expected[2:]]]
            namespaces['artifact_'+str(ordinal)]=('/workspace/target/'+str(name),root,rows)
        except (ValueError,TypeError,KeyError,IndexError):raise FreshnessHold('input_census_unknown') from None
    return lookup_inputs.FrozenLookupIndex(namespaces,kernel_roots={'/':scope['root_identity']},owner=os.geteuid(),
        overlays=True,opaque=('/workspace/target',))


def record_selected_revalidation(root,source,lease,checkpoint,bindings,outputs,refs):
    """Persist changed selected facts before holding incomplete revalidation.

    The retained executor checkpoint has already been verified by the caller.
    These partial selections can prove relevance changed, never complete reuse.
    No fingerprints, producer history or old input inodes are modified here.
    """
    previous,_record=GeneratedForests(lease,outputs).load(checkpoint['forests'],require_public=False)
    context=PublicationStore._retained(root/'publications',binding=lease.binding,command=checkpoint['command'])
    state,_records,_rows,_digest=context
    active=set()
    for producer in set(state['files'].values())|set(state['namespaces'].values()):
        publication=cache._read_record(root/'publications/records'/(producer+'.json'),max_bytes=PublicationStore.LIMIT)
        status=publication['evidence']['status'];active.add((status['command'],status['ordinal']))
    paths=sorted((root/'input-observations').iterdir())
    if len(paths)>1024:raise FreshnessHold('input_census_unknown')
    observations=[];affected=set()
    for path in paths:
        if not path.name.endswith('.json') or not cache._uuid(path.stem):raise FreshnessHold('artifact_provenance_unknown')
        record=cache._read_record(path,max_bytes=16*1024**2)
        if (record.get('version')!=3 or record.get('kind')!='ci-command-kernel-record'
                or record.get('binding')!=lease.binding or record.get('command')!=path.stem
                or record.get('input_closure_complete') is not False or record.get('shipping_authority') is not False
                or record.get('original_observer_cleanup') is not True):raise FreshnessHold('artifact_provenance_unknown')
        kernel_observer.validate_census(record['probe_census'],path_scope='all')
        record_sha=hashlib.sha256(json.dumps(record,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        for scope in record['scopes']:
            if scope['mapping']['kind']!='compile' or (record['command'],scope['ordinal']) not in active:continue
            original_artifacts=artifact_input_bindings(root/'publications',scope.get('artifact_bindings',[]),context)
            current_artifacts=artifact_input_bindings(root/'publications',original_artifacts,context,current=True)
            before=frozen_input_index(source,previous,dict(scope,artifact_bindings=original_artifacts))
            following=frozen_input_index(source,bindings,dict(scope,artifact_bindings=current_artifacts))
            selected=lookup_inputs._ContentSelection.restore(scope['selected_reads'],before)
            metadata=lookup_inputs._ContentSelection.restore(scope['selected_metadata'],before)
            directory_record=scope.get('selected_directories')
            if directory_record is None:
                directories=lookup_inputs._ContentSelection([],before.census.observe([]),
                    hashlib.sha256(lookup_inputs._encoded([])).hexdigest(),before._layout)
            else:directories=lookup_inputs._ContentSelection.restore(directory_record,before)
            descriptor_record=scope.get('selected_descriptors')
            if descriptor_record is None:
                descriptors=lookup_inputs._ContentSelection([],before.census.observe([]),
                    hashlib.sha256(lookup_inputs._encoded([])).hexdigest(),before._layout)
            else:descriptors=lookup_inputs._ContentSelection.restore(descriptor_record,before)
            refreshed=selected.refresh(following);refreshed_metadata=metadata.refresh(following)
            refreshed_directories=directories.refresh(following)
            refreshed_descriptors=descriptors.refresh(following)
            read_changed=refreshed['sha256']!=selected.observation['sha256']
            metadata_changed=refreshed_metadata['sha256']!=metadata.observation['sha256']
            directory_changed=refreshed_directories['sha256']!=directories.observation['sha256']
            descriptor_changed=refreshed_descriptors['sha256']!=descriptors.observation['sha256']
            changed=read_changed or metadata_changed or directory_changed or descriptor_changed
            fingerprint=scope['mapping']['fingerprint']
            if changed:affected.add(fingerprint)
            observations.append({'source_command':record['command'],'ordinal':scope['ordinal'],
                'capture_sha256':record_sha,'binding_sha256':selected.binding_sha256,'fingerprint':fingerprint,
                'previous_sha256':hashlib.sha256(lookup_inputs._encoded([selected.observation['sha256'],metadata.observation['sha256'],directories.observation['sha256'],descriptors.observation['sha256']])).hexdigest(),
                'current_sha256':hashlib.sha256(lookup_inputs._encoded([refreshed['sha256'],refreshed_metadata['sha256'],refreshed_directories['sha256'],refreshed_descriptors['sha256']])).hexdigest(),
                'read_relevance_changed':read_changed,'metadata_relevance_changed':metadata_changed,
                'directory_relevance_changed':directory_changed,
                'descriptor_relevance_changed':descriptor_changed,'descriptor_binding_sha256':descriptors.binding_sha256,
                'previous_descriptor_sha256':descriptors.observation['sha256'],'current_descriptor_sha256':refreshed_descriptors['sha256'],
                'directory_binding_sha256':directories.binding_sha256,
                'previous_directory_sha256':directories.observation['sha256'],'current_directory_sha256':refreshed_directories['sha256'],
                'metadata_binding_sha256':metadata.binding_sha256,
                'previous_metadata_sha256':metadata.observation['sha256'],'current_metadata_sha256':refreshed_metadata['sha256'],
                'relevance_changed':changed,'unbound_read_opens':len(scope['unbound_read_opens']),
                'unbound_metadata_queries':len(scope['unbound_metadata_queries']),
                'unbound_directory_reads':None if directory_record is None else len(scope['unbound_directory_reads']),
                'unbound_descriptor_queries':None if descriptor_record is None else len(scope['unbound_descriptor_queries']),
                'input_closure_complete':False,'shipping_authority':False})
    report={'version':2,'kind':'ci-selected-input-revalidation','binding':lease.binding,'command':lease.command,
        'checkpoint_command':checkpoint['command'],'previous_forests':checkpoint['forests'],'current_forests':refs,
        'observations':observations,'affected_fingerprints':sorted(affected),
        'action':'hold_for_complete_input_revalidation','input_closure_complete':False,'full_ci':False,'shipping_authority':False}
    directory=root/'selected-revalidation'
    if not directory.exists():directory.mkdir(mode=0o700)
    info=directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid() or stat.S_IMODE(info.st_mode)!=0o700:
        raise FreshnessHold('artifact_provenance_unknown')
    path=directory/(lease.command+'.json')
    if path.exists() or path.is_symlink():raise FreshnessHold('cleanup_uncertain')
    cache._write_record(path,report)
    return report


class CommandObservation:
    """Original-unit kernel facts for later selectors, never complete inputs."""
    def __init__(self,admission,deadline):
        self.admission=admission;self.observer=kernel_observer.Observer(admission,deadline=deadline,path_scope='all')

    def __enter__(self):
        try:self.observer.__enter__();return self
        except (kernel_observer.ObservationUnavailable,OSError) as error:
            raise FreshnessHold('input_census_unknown',b'original kernel collector startup failed') from error

    def __exit__(self,*arguments):
        try:self.observer.__exit__(*arguments)
        except (kernel_observer.ObservationUnavailable,OSError) as error:
            raise FreshnessHold('cleanup_uncertain',b'original kernel collector cleanup failed') from error

    def finish(self,executor):
        try:events,directories,metadata,census=self.observer.capture()
        except (kernel_observer.ObservationUnavailable,OSError) as error:
            raise FreshnessHold('input_census_unknown',b'original kernel capture incomplete') from error
        units=[];scopes=[];known=set()
        mounted={} if executor is None else compiler_artifact_bindings(executor,self.admission.binding,self.admission.command)
        for ordinal,item in sorted(({} if executor is None else executor.units).items()):
            if not item['finished']:raise FreshnessHold('cleanup_uncertain')
            pid=item['pid'];uid=2000+ordinal;known.add(uid)
            queries=[event for event in events if event['uid']==uid]
            reads=[event for event in directories if event['uid']==uid]
            stats=[event for event in metadata if event['uid']==uid]
            mapping=item['mapping'];source=mapping.get('source');observed=False
            if mapping['kind']=='compile':
                names={os.fsencode(source),os.fsencode(source).removeprefix(b'/workspace/')}
                original=unit_views.literal(executor.source/os.fsdecode(os.fsencode(source).removeprefix(b'/workspace/'))).stat()
                identity=((os.major(original.st_dev)<<20)|os.minor(original.st_dev),original.st_ino,original.st_mode)
                observed=any(event['pid']==pid and event['path'] in names and kernel_observer.readable_open(event)
                    and tuple(event['open']['installed'][key] for key in ('device','inode','mode'))==identity for event in queries)
                if not observed:raise FreshnessHold('input_census_unknown',b'original compiler source open absent')
            units.append({'ordinal':ordinal,'pid':pid,'uid':uid,'kind':mapping['kind'],
                'queries':len(queries),'directory_reads':len(reads),'metadata_queries':len(stats),
                'source_open_observed':observed})
            selected=None;unbound=[];selected_metadata=None;unbound_metadata=[];selected_directories=None;unbound_directories=[];selected_descriptors=None;unbound_descriptors=[]
            if mapping['kind']=='compile':
                index=frozen_input_index(executor.source,executor.source_bindings,dict(item,artifact_bindings=mounted[ordinal]))
                selected,unbound=index.select_read_subset([event for event in queries if event['syscall'] in (2,257,437)
                    and event['pid']==pid],pid=pid,uid=uid,user_namespace=self.observer.user_namespace,
                    mount_namespace=int(item['namespaces']['mnt']['unit'].split('[',1)[1][:-1]))
                if not selected.record()['queries']:raise FreshnessHold('input_census_unknown',b'compiler read selection absent')
                selected_metadata,unbound_metadata=index.select_metadata_subset([event for event in queries
                    if event['pid']==pid and event['syscall'] in (262,332)],stats,
                    pid=pid,uid=uid,user_namespace=self.observer.user_namespace,
                    mount_namespace=int(item['namespaces']['mnt']['unit'].split('[',1)[1][:-1]),threaded=True)
                selected_directories,unbound_directories=index.select_directory_subset([event for event in reads
                    if event['pid']==pid],pid=pid,uid=uid,user_namespace=self.observer.user_namespace,
                    mount_namespace=int(item['namespaces']['mnt']['unit'].split('[',1)[1][:-1]))
                selected_descriptors,unbound_descriptors=index.select_descriptor_subset([event for event in stats
                    if event['pid']==pid and event['syscall']==5],pid=pid,uid=uid,user_namespace=self.observer.user_namespace,
                    mount_namespace=int(item['namespaces']['mnt']['unit'].split('[',1)[1][:-1]))
            scopes.append({'ordinal':ordinal,'pid':pid,'uid':uid,'mapping':{key:mapping[key] for key in ('kind','role','source','fingerprint','request_sha256','dependencies','generated_root') if key in mapping},
                'artifact_bindings':mounted.get(ordinal,[]),
                'selected_reads':None if selected is None else selected.record(),'unbound_read_opens':unbound,
                'selected_metadata':None if selected_metadata is None else selected_metadata.record(),
                'unbound_metadata_queries':unbound_metadata,
                'selected_directories':None if selected_directories is None else selected_directories.record(),
                'unbound_directory_reads':unbound_directories,
                'selected_descriptors':None if selected_descriptors is None else selected_descriptors.record(),
                'unbound_descriptor_queries':unbound_descriptors,
                'scope_evidence':item['scope_evidence'],'namespaces':item['namespaces'],'root_identity':item['root_identity'],'returncode':item['returncode']})
        if any(event['uid']!=1999 and event['uid'] not in known
                for event in [*events,*directories,*metadata]):
            raise FreshnessHold('input_census_unknown',b'kernel event outside original unit scopes')
        directory=self.observer.root if executor is None else executor.root/'input-observations'
        record={'version':3,'kind':'ci-command-kernel-record','binding':self.admission.binding,
            'command':self.admission.command,'path_scope':'all','scopes':lookup_inputs._wire(scopes),
            'events':lookup_inputs._wire(events),'directories':lookup_inputs._wire(directories),
            'metadata':lookup_inputs._wire(metadata),'probe_census':census,
            'program_sha256':hashlib.sha256(self.observer.program.encode()).hexdigest(),
            'binary_sha256':self.observer.binary_sha256,'original_observer_cleanup':True,
            'input_closure_complete':False,'shipping_authority':False}
        encoded=json.dumps(record,sort_keys=True,separators=(',',':')).encode()
        if len(encoded)>16*1024**2:raise FreshnessHold('input_census_unknown')
        path=directory/(self.admission.command+'.json')
        if path.exists() or path.is_symlink():raise FreshnessHold('artifact_provenance_unknown')
        cache._write_record(path,record)
        return {'version':2,'kind':'ci-command-kernel-observation','binding':self.admission.binding,
            'command':self.admission.command,'record_sha256':hashlib.sha256(encoded).hexdigest(),
            'program_sha256':record['program_sha256'],'binary_sha256':record['binary_sha256'],
            'probe_census':census,'units':units,'original_observer_cleanup':True,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}


def original_unit_ordinals(units):
    if type(units) is not list or len(units)>4096:raise FreshnessHold('input_census_unknown')
    seen=set()
    for unit in units:
        if (type(unit) is not dict or type(unit.get('ordinal')) is not int or not 0<=unit['ordinal']<4096
                or unit['ordinal'] in seen or unit.get('uid')!=2000+unit['ordinal']
                or unit.get('started') is not True or unit.get('cancelled') is not False
                or unit.get('original_cleanup') is not True or unit.get('input_closure_complete') is not False
                or unit.get('shipping_authority') is not False):raise FreshnessHold('input_census_unknown')
        seen.add(unit['ordinal'])
    return seen


def validate_command_observation(value,binding,command,ordinals):
    try:
        if (type(value) is not dict or set(value)!={'version','kind','binding','command','record_sha256',
                'program_sha256','binary_sha256','probe_census','units','original_observer_cleanup',
                'input_closure_complete','full_ci','shipping_authority'}
                or type(value['version']) is not int or value['version']!=2
                or value['kind']!='ci-command-kernel-observation' or value['binding']!=binding or value['command']!=command
                or any(not cache._digest(value[key]) for key in ('record_sha256','program_sha256','binary_sha256'))
                or value['original_observer_cleanup'] is not True or type(value['units']) is not list or len(value['units'])>4096
                or any(value[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))):raise ValueError()
        kernel_observer.validate_census(value['probe_census'],path_scope='all');seen=set()
        for unit in value['units']:
            if (type(unit) is not dict or set(unit)!={'ordinal','pid','uid','kind','queries','directory_reads','metadata_queries','source_open_observed'}
                    or type(unit['ordinal']) is not int or unit['ordinal'] not in ordinals or unit['ordinal'] in seen
                    or type(unit['pid']) is not int or not 0<unit['pid']<2**31 or unit['uid']!=2000+unit['ordinal']
                    or unit['kind'] not in ('compile','build_script','application','information')
                    or any(type(unit[key]) is not int or not 0<=unit[key]<=40000 for key in ('queries','directory_reads','metadata_queries'))
                    or type(unit['source_open_observed']) is not bool
                    or unit['kind']=='compile' and not unit['source_open_observed']):raise ValueError()
            seen.add(unit['ordinal'])
        if seen!=ordinals:raise ValueError()
        return value
    except (ValueError,TypeError,KeyError,kernel_observer.ObservationUnavailable):
        raise FreshnessHold('input_census_unknown') from None


class _CargoExecutor:
    """Graph-backed execution; incomplete input closure grants no reuse/CI proof."""
    def __init__(self,admission,*,diagnostic=False,execution_lease=None,generated_outputs=None):
        if (type(admission) is not _Admission or admission.token is not _TOKEN
                or admission.owner!=os.getpid() or os.getresuid()!=(0,0,0)):
            raise FreshnessHold('invalidation_unsafe')
        _source(admission.target,admission.binding,admission.command)
        self.admission=admission;self.owner=os.getpid();self.units={};self.commands=[]
        self.graph=None;self.preparing_graph=False;self.source=admission.target.parent/'source-projection'
        lease=ExecutionLease.attach(execution_lease) if execution_lease is not None else None
        self.declared_outputs={} if generated_outputs is None else dict(generated_outputs)
        self.source_bindings,self.forest_refs=lease.generated_inputs(self.declared_outputs) if lease is not None else ([],[])
        retained,checkpoint=lease.retained() if lease is not None else (None,None)
        if checkpoint is not None:
            if checkpoint['outputs']!=self.declared_outputs:raise FreshnessHold('input_census_unknown')
            if checkpoint['forests']!=self.forest_refs:
                report=record_selected_revalidation(retained,self.source,lease,checkpoint,self.source_bindings,self.declared_outputs,self.forest_refs)
                raise FreshnessHold('input_census_unknown',json.dumps(report,sort_keys=True,separators=(',',':')).encode())
        self.control={'version':1,'kind':'cargo-execution','binding':admission.binding,
            'command':admission.command,'shipping_authority':False}
        if retained is None:
            self.root=admission.target/('.cargo-execution-'+str(uuid.uuid4()));self.root.mkdir(mode=0o710)
            os.chown(self.root,0,1001)
            cache._write_record(self.root/'control.json',{**self.control,'state':'active'})
            self.store=PublicationStore(self.root/'publications',binding=admission.binding,command=admission.command,
                unit_root=admission.root/'units',allow_diagnostic=diagnostic)
            for name in ('planner-target','home','cargo-home'):
                path=self.root/name;path.mkdir(mode=0o700);os.chown(path,1001,1001)
            self.next_command=0
            (self.root/'input-observations').mkdir(mode=0o700)
            if lease is not None:lease.created(self.root)
        else:
            if diagnostic:raise FreshnessHold('invalidation_unsafe')
            self.root=retained
            self.store=PublicationStore.recover(self.root/'publications',binding=admission.binding,command=admission.command,
                predecessor=checkpoint['command'],unit_root=admission.root/'units',expected_sha256=checkpoint['store_sha256'])
            self.next_command=len(checkpoint['commands'])
            cache._write_record(self.root/'control.json',{**self.control,'state':'active'})
        if lease is not None:
            cache._write_record(self.root/'input-forests.json',{'version':1,'binding':admission.binding,'forests':self.forest_refs,'outputs':self.declared_outputs})
        self.source_mounts_path=None if diagnostic else self.root/'source-mounts.json'
        if not diagnostic:cache._write_record(self.source_mounts_path,{'version':1,'binding':admission.binding,'command':admission.command,
            'entries':self.source_bindings,'outputs':self.declared_outputs})
        self.target=self.root/'planner-target'
        self.slot_root=self.root if diagnostic else self.root/('views-'+admission.command)
        if not diagnostic:self.slot_root.mkdir(mode=0o700)


    @property
    def publications(self):return self.store.snapshot()[0]


    @property
    def files(self):return self.store.snapshot()[1]


    @property
    def history(self):return self.store.snapshot(historical=True)[0]


    @staticmethod
    def _directory(root,name,unit):
        current=root
        for part in unit_views.relative(name).parts:
            current=current/part
            if not current.exists():current.mkdir(mode=0o700);os.chown(current,unit,unit)
            unit_views._output_directory(current,unit)
        return current


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


    def _file_input(self,artifacts,unit,name,alias=None,*,publications,files):
        if name not in files:raise cargo_hold('planner_cargo_artifact')
        label,row=files[name];publication=publications[label]
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


    def _generated_input(self,artifacts,unit,name,package_id,*,publications):
        publication=generated_publication(publications,name,package_id)
        _declared_publication(self.graph,publication)
        if publication is None or publication['roots']!=[name]:raise cargo_hold('planner_cargo_artifact')
        if unit_views.inventory(publication['root'],owner=0,sealed=True)!=publication['manifest']:
            raise cargo_hold('planner_cargo_artifact')
        original=publication['root']/name;manifest=unit_views.inventory(original,owner=0,sealed=True)
        self._directory(artifacts,str(Path(name).parent),unit)
        (artifacts/name).mkdir(mode=0o555);raw=os.fsencode(name).hex()
        return [raw,str(original),[raw,'tree',manifest]]


    def _mapping(self,request,publications,files):
        try:
            return dispatch_request(request,graph=self.graph,planner_target=self.target,
                publications=publications,files=files)
        except FreshnessHold:
            # Keep one bounded root-owned diagnostic, without environment
            # values or granting any input/reuse capability.
            file,argv,environment,cwd,search,*_=request
            diagnostic={'version':1,'binding':self.admission.binding,'command':self.admission.command,
                'file_hex':file[:4096].hex(),'argv_hex':[arg[:2048].hex() for arg in argv[:32]],
                'cwd_hex':cwd[:4096].hex(),'search_hex':None if search is None else search[:4096].hex(),
                'path_hex':environment.get(b'PATH',b'')[:4096].hex(),'shipping_authority':False}
            cache._write_record(self.root/'held-request.json',diagnostic)
            raise


    def _library_input(self):pass
    def _failed(self,mapping,raw):pass
    def _generated_outputs(self,view,ordinal):pass
    def _published(self,label,identity,sealed,roots):pass
    def _application_outputs(self,view,ordinal):pass

    def prepare(self,ordinal,request):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        try:
            publications,files=self.store.snapshot()
            mapped=self._mapping(request,publications,files)
        except FreshnessHold as error:
            if not hasattr(error,'unit_view_stage'):error.unit_view_stage='planner_cargo_request'
            raise
        admission=mapped.get('cargo_target')
        if self.preparing_graph:
            if mapped['kind']!='information':raise cargo_hold('planner_cargo_request')
        slot=self.slot_root/('unit-'+str(ordinal));slot.mkdir(mode=0o700)
        unit=2000+ordinal;artifacts=slot/'artifacts';artifacts.mkdir(mode=0o700);os.chown(artifacts,unit,unit)
        for name in ('debug','debug/deps'):
            path=artifacts/name;path.mkdir(mode=0o700);os.chown(path,unit,unit)
        entries=[];mounted=set()
        if mapped['kind'] in ('application','build_script'):
            entries.append(self._file_input(artifacts,unit,mapped['artifact'],mapped['alias'],publications=publications,files=files))
        if mapped['kind']=='compile':
            for name in mapped['dependencies']:
                if name not in files:raise cargo_hold('planner_cargo_artifact')
                publication=publications[files[name][0]]
                if publication['action'] not in ('library','check'):raise cargo_hold('planner_cargo_artifact')
                for artifact in publication['files']:
                    if artifact.endswith(('.rlib','.rmeta')) and artifact not in mounted:
                        entries.append(self._file_input(artifacts,unit,artifact,publications=publications,files=files));mounted.add(artifact)
                self._library_input()
            self._directory(artifacts,str(Path(mapped['outputs'][0]).parent),unit)
        generated=mapped.get('generated_root')
        if generated is not None:
            if mapped['kind']=='build_script':self._directory(artifacts,generated,unit)
            else:entries.append(self._generated_input(artifacts,unit,generated,admission['package_id'],publications=publications))
        root=slot/'root';root.mkdir(mode=0o700);mounts=slot/'mounts.json'
        cache._write_record(mounts,{'version':1,'binding':self.admission.binding,
            'command':self.admission.command,'ordinal':ordinal,'entries':entries,
            'source_entries':self.source_bindings,'declared_outputs':self.declared_outputs})
        view={'root':str(root),'artifacts':str(artifacts),'source':str(self.source),
            'mounts':str(mounts),'ordinal':ordinal}
        self.units[ordinal]={'view':view,'mapping':mapped,'finished':False,'cargo_target':admission,'graph_dispatch':True}
        return view


    def running(self,view):pass


    def _materialize(self,publication,previous_files):
        for row in publication['manifest']:
            if row[0]!='2e' and row[1]=='directory':self._directory(self.target,os.fsdecode(bytes.fromhex(row[0])),1001)
        for name in publication['files']:
            destination=self.target/name;self._directory(self.target,str(Path(name).parent),1001)
            if destination.exists() or destination.is_symlink():
                if name not in previous_files:raise cargo_hold('planner_cargo_artifact')
                self._planner_file(name,previous_files[name][1])
            temporary=destination.with_name('.ci-publication-'+str(uuid.uuid4()))
            descriptor=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
            try:
                mode,_size,_digest=unit_views._read(publication['root']/name,0,True,descriptor)
                os.fchown(descriptor,1001,1001);os.fchmod(descriptor,0o755 if mode&0o111 else 0o644);os.fsync(descriptor)
            finally:os.close(descriptor)
            try:
                os.replace(temporary,destination)
                directory=os.open(destination.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW)
                try:os.fsync(directory)
                finally:os.close(directory)
            finally:
                try:temporary.unlink()
                except FileNotFoundError:pass


    def finish(self,view,directory,raw):
        ordinal=view['ordinal'];record=self.units[ordinal];mapping=record['mapping']
        observed=cache._read_record(directory/'view-namespace.json')
        if (observed.get('binding')!=self.admission.binding or observed.get('command')!=self.admission.command
                or observed.get('ordinal')!=ordinal
                or set(observed['namespaces'])!={'mnt','ipc','net'}
                or any(row['controller']==row['unit'] for row in observed['namespaces'].values())):
            raise FreshnessHold('invalidation_unsafe')
        pid=observed.get('pid')
        if type(pid) is not int or pid<=0:raise FreshnessHold('cleanup_uncertain')
        status=cache._read_record(directory/'exit-status.json')
        if status!={'version':3,'binding':self.admission.binding,'command':self.admission.command,
                'ordinal':ordinal,'returncode':raw,'exec_observed':True}:raise FreshnessHold('cleanup_uncertain')
        original_cargo_cleanup(directory/'cleanup')
        record.update(finished=True,namespaces=observed['namespaces'],pid=pid,
            root_identity=observed['root_identity'],returncode=raw,
            scope_evidence={'status':status,'cleanup_sha256':ExecutionLease.cleanup_digest()})
        if raw!=0:
            self._failed(mapping,raw)
            return
        if mapping['kind'] in ('compile','build_script'):
            label=mapping['role'] if mapping['kind']=='compile' else 'generated'
            identity,sealed=self.store.allocate()
            staged=self.store.objects/('.preparing-'+identity);roots=[]
            if mapping['kind']=='compile':
                manifest=unit_views.copy_outputs(Path(view['artifacts']),staged,mapping['outputs'],owner=2000+ordinal,controller=0)
            else:
                roots=[mapping['generated_root']]
                manifest=unit_views.copy_output_roots(Path(view['artifacts']),staged,roots,owner=2000+ordinal,controller=0)
                self._generated_outputs(view,ordinal)
            staged.rename(sealed)
            parent=os.open(self.store.objects,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
            try:os.fsync(parent)
            finally:os.close(parent)
            outputs=[os.fsdecode(bytes.fromhex(row[0])) for row in manifest if row[1]=='file']
            if mapping['kind']=='compile':outputs=mapping['outputs']
            action=mapping['role'] if mapping['kind']=='compile' else 'generated'
            publication={'root':sealed,'manifest':manifest,'files':outputs,'roots':roots,
                'action':action,'cargo_target':record['cargo_target'],'generated_root':mapping.get('generated_root')}
            _publications,previous_files=self.store.snapshot()
            self.store.publish(identity,publication,scope=directory,ordinal=ordinal,
                materialize=lambda item:self._materialize(item,previous_files))
            self._published(label,identity,sealed,roots)
        elif mapping['kind']=='application':self._application_outputs(view,ordinal)

    def _command(self,value,relay,*,operation='run'):
        cache.require_cache_capacity(self.target)
        global _PHASE,_CHILD_DIAGNOSTIC
        _PHASE='planner_cargo_supervisor'
        if self.owner!=os.getpid() or len(self.commands)>=4 or operation not in ('run','metadata','original'):raise FreshnessHold('invalidation_unsafe')
        invocation=self.next_command
        if invocation>=1000:raise FreshnessHold('input_census_unknown')
        self.next_command+=1;directory=self.root/('command-'+str(invocation));directory.mkdir(mode=0o700)
        control={'version':1,'binding':self.admission.binding,'command':self.admission.command,
            'invocation':invocation,'operation':operation}
        cache._write_record(directory/'control.json',control)
        client=relay.config_fd();bootstrap=None;child=None
        try:
            config=dict(value,kind='cargo-supervisor',parent=os.getpid(),invocation=invocation,
                command_directory=str(directory),cargo_operation=operation,cargo_root=str(self.root),source=str(self.source),
                source_mounts=None if self.source_mounts_path is None else str(self.source_mounts_path))
            bootstrap=_sealed(config)
            child=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--cargo-supervisor',str(bootstrap),str(client)],
                pass_fds=(bootstrap,client),env={'PATH':os.defpath},stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            output,error=capture_command(child,value['deadline'])
            cleanup=original_cargo_cleanup(directory/'cleanup')
            cache._write_record(directory/'result.json',{'version':1,'binding':self.admission.binding,
                'command':self.admission.command,'returncode':child.returncode,
                'diagnostics':relay.errors,'original_cleanup':True,'shipping_authority':False})
            if relay.errors:
                _CHILD_DIAGNOSTIC=failed_probe_diagnostic(error,relay.errors)
                raise FreshnessHold('cleanup_uncertain')
            relay.drain()
            if len(output)+len(error)>8*1024**2:raise FreshnessHold('input_census_unknown')
            self.commands.append({'directory':directory,'control':control,'cleanup':cleanup,'returncode':child.returncode})
            return child.returncode,output,error
        finally:
            try:
                if child is not None and child.poll() is None:
                    child.terminate()
                    try:child.wait(timeout=6)
                    except subprocess.TimeoutExpired:
                        child.kill();child.wait(timeout=2)
                        raise FreshnessHold('cleanup_uncertain') from None
            finally:
                os.close(client)
                if bootstrap is not None:os.close(bootstrap)


class _CargoProbe(_CargoExecutor):
    """Keep the exact private fixture guard separate from general CI execution."""
    def __init__(self,admission):
        source=admission.target.parent/'source-projection'
        for name,body in (('Cargo.toml',CARGO_FIXTURE_MANIFEST),('src/main.rs',CARGO_FIXTURE_SOURCE),
                ('src/lib.rs',CARGO_FIXTURE_LIB),('build.rs',CARGO_FIXTURE_BUILD)):
            if unit_views._read(source/name,0,True)[1:]!=(len(body.encode()),hashlib.sha256(body.encode()).hexdigest()):
                raise FreshnessHold('input_census_unknown')
        super().__init__(admission,diagnostic=True)
        self.fixture_labels={};self.report=None;self.generated_stats={};self.library_used=False
        self.retry_active=False;self.retry_publication=None;self.journal=None;self.fingerprints=None;self.restart_gate=None

    def _mapping(self,request,publications,files):
        guarded=fixture_request(request)
        mapped=super()._mapping(request,publications,files)
        if any(mapped.get(key)!=value for key,value in guarded.items()):raise cargo_hold('planner_cargo_request')
        return mapped

    def _library_input(self):self.library_used=True

    def _failed(self,mapping,raw):
        if self.retry_active and mapping['kind']=='compile' and mapping['role']=='library' and raw==1:return
        stage={'compile':'planner_cargo_compile','build_script':'planner_cargo_build_script',
            'application':'planner_cargo_application'}.get(mapping['kind'],'planner_cargo_compile')
        raise cargo_hold(stage)

    def _generated_outputs(self,view,ordinal):
        forged=Path(view['artifacts'])/'debug/.fingerprint/build-script-forged'
        if unit_views._read(forged,2000+ordinal,False)[1:]!=(17,hashlib.sha256(b'private unit only').hexdigest()):
            raise FreshnessHold('invalidation_unsafe')

    def _published(self,label,identity,sealed,roots):
        self.fixture_labels.setdefault(label,identity)
        if label=='generated':
            tree=sealed/roots[0]
            for row in unit_views.inventory(tree,owner=0,sealed=True):
                path=tree/Path(os.fsdecode(bytes.fromhex(row[0])));self.generated_stats[path]=self._stat(path)

    def _application_outputs(self,view,ordinal):
        forged=Path(view['artifacts'])/'debug/.fingerprint/unit-forged'
        if unit_views._read(forged,2000+ordinal,False)[1:]!=(17,hashlib.sha256(b'private unit only').hexdigest()):
            raise FreshnessHold('invalidation_unsafe')



    def publication(self,label):return self.store.record(self.fixture_labels[label])


    @staticmethod
    def _stat(path):
        info=path.stat()
        return (*unit_views._key(info),info.st_nlink,info.st_atime_ns)


    def record_invalidations(self,events,metadata,user_namespace):
        """Consume the actual library's missing-module query after unit drain.

        This fixed private replay only records invalidation intent. It cannot
        approve reuse, delete Cargo metadata, or publish a passing-CI proof.
        """
        if self.owner!=os.getpid() or self.report is None:raise FreshnessHold('cleanup_uncertain')
        libraries=[(ordinal,unit) for ordinal,unit in self.units.items()
            if unit['mapping']['kind']=='compile' and unit['mapping']['role']=='library']
        binaries=[unit for unit in self.units.values()
            if unit['mapping']['kind']=='compile' and unit['mapping']['role']=='binary']
        if len(libraries)!=1 or len(binaries)!=1:raise FreshnessHold('input_census_unknown')
        ordinal,unit=libraries[0];mapping=unit['mapping'];publication=self.publication('generated')
        original=publication['root']/mapping['generated_root']
        if unit_views.inventory(publication['root'],owner=0,sealed=True)!=publication['manifest']:
            raise FreshnessHold('input_census_unknown')
        prefix='/workspace/target/'+mapping['generated_root']
        def index(root):
            return lookup_inputs.FrozenLookupIndex({'generated':(prefix,root,
                unit_views.inventory(root,owner=0,sealed=True))},kernel_roots={'/':unit['root_identity']},owner=0)
        selected=[e for e in events if e['pid']==unit['pid'] and e['uid']==2000+ordinal
            and e['path']==(prefix+'/chosen.rs').encode() and e['syscall'] in (262,332) and e['result']==-errno.ENOENT]
        if not selected:raise FreshnessHold('input_census_unknown')
        scope=int(unit['namespaces']['mnt']['unit'].split('[',1)[1][:-1])
        observation=index(original).select(selected,metadata,pid=unit['pid'],uid=2000+ordinal,
            user_namespace=user_namespace,mount_namespace=scope,threaded=True)
        variant_id,variant=self.store.allocate()
        unit_views.copy_output_roots(publication['root'],variant,[mapping['generated_root']],owner=0,controller=0)
        changed=variant/mapping['generated_root'];changed.chmod(0o755)
        introduced=changed/'chosen.rs';introduced.write_bytes(b'pub const VALUE:u8=9;\n')
        os.utime(introduced,ns=(946684800000000000,946684800000000000));introduced.chmod(0o444);changed.chmod(0o555)
        before=observation.observation['sha256'];after=observation.refresh(index(changed))['sha256']
        if before==after:raise FreshnessHold('input_census_unknown')
        ledger=self.root/'invalidations';ledger.mkdir(mode=0o700)
        args={'binding':self.admission.binding,'command':self.admission.command}
        journal=lookup_inputs.UnitInvalidations(ledger/'units.json',**args)
        journal.register('library',dependencies=[],content_sha256=before,fingerprints=[mapping['fingerprint']])
        # The fixed graph's binary consumes this library. Its selected subset
        # includes that dependency; this is not a complete binary-input census.
        journal.register('binary',dependencies=['library'],content_sha256=before,
            fingerprints=[binaries[0]['mapping']['fingerprint']])
        def refresh(publication):
            digest=observation.refresh(index(publication['root']/mapping['generated_root']))['sha256']
            return journal.changed({'library':digest})
        self.store.watch(mapping['generated_root'],refresh)
        variant_manifest=unit_views.inventory(variant,owner=0,sealed=True)
        self.retry_publication={'root':variant,'manifest':variant_manifest,
            'files':[os.fsdecode(bytes.fromhex(row[0])) for row in variant_manifest if row[1]=='file'],
            'roots':[mapping['generated_root']],'action':'generated','cargo_target':publication['cargo_target'],
            'generated_root':mapping['generated_root']}
        # This is a root-created relevance snapshot, not another BRS execution.
        self.store.publish(variant_id,self.retry_publication)
        pending=journal.pending()
        restored=lookup_inputs.UnitInvalidations(ledger/'units.json',**args)
        if (pending!=['binary','library'] or restored.pending()!=pending
                or self.store.state()['last_affected']!=pending):raise FreshnessHold('metadata_torn')
        self.journal=restored
        if (any(self._stat(path)!=prior for path,prior in self.generated_stats.items())
                or unit_views.inventory(publication['root'],owner=0,sealed=True)!=publication['manifest']):
            raise FreshnessHold('input_census_unknown')
        self.report['invalidation']={'version':1,'unit_pid':unit['pid'],'unit_uid':2000+ordinal,
            'queries':len(selected),'binding_sha256':observation.binding_sha256,
            'original_sha256':before,'refreshed_sha256':after,'pending':pending,
            'durable':True,'original_generated_preserved':True,
            'input_closure_complete':False,'shipping_authority':False}


    def run(self,value,relay):
        first=len(relay.completed);counts=[];script_counts=[];fingerprint=None
        self.preparing_graph=True
        try:status,output,error=self._command(value,relay,operation='metadata')
        finally:self.preparing_graph=False
        if status!=0:raise cargo_hold('planner_cargo_request')
        self.graph=CargoGraph(self.source,output,owner=0)
        for iteration in range(2):
            if iteration==1:
                self.restart_gate=RestartGate(self.root/'restart.json',value);self.restart_gate.begin(value)
            before=len(relay.completed);status,output,error=self._command(value,relay)
            if iteration==1:self.restart_gate.finish(value,self.commands[-1]['directory']/'cleanup')
            if status!=0 or output!=b'ISOLATED\n':
                global _CHILD_DIAGNOSTIC
                _CHILD_DIAGNOSTIC=failed_probe_diagnostic(error,relay.errors)
                raise FreshnessHold('invalidation_unsafe')
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
        self.fingerprints=fingerprint
        if (set(self.fixture_labels)!={'build-script','generated','library','binary'} or not self.library_used
                or not self.generated_stats or any(not unit['finished'] for unit in self.units.values())
                or any(unit_views.inventory(pub['root'],owner=0,sealed=True)!=pub['manifest'] for pub in self.publications.values())
                or any(self._stat(path)!=before for path,before in self.generated_stats.items())):
            raise FreshnessHold('cleanup_uncertain')
        self.report={'version':1,'compiler_calls':counts,'build_script_calls':script_counts,'application_runs':2,
            'generated_tree_preserved':True,'library_input_mounted':True,
            'actual_metadata_preserved':True,'sealed_artifact_detached':True,
            'units':relay.completed[first:],'allocation_removed':False,
            'trusted_input_closure':False,'shipping_authority':False}


    def retry_invalidated(self,value,relay):
        global _PHASE
        _PHASE='planner_cargo_retire'
        if (self.owner!=os.getpid() or self.report is None or len(self.commands)!=3
                or self.journal is None or self.retry_publication is None
                or any(command['returncode']!=0 for command in self.commands)
                or any(not unit['finished'] for unit in self.units.values())):raise FreshnessHold('cleanup_uncertain')
        _source(self.admission.target,self.admission.binding,self.admission.command)
        for command in self.commands:
            if (cache._read_record(command['directory']/'control.json')!=command['control']
                    or original_cargo_cleanup(command['directory']/'cleanup')!=command['cleanup']):
                raise FreshnessHold('cleanup_uncertain')
        relay.drain()
        restored=lookup_inputs.UnitInvalidations(self.journal.path,
            binding=self.admission.binding,command=self.admission.command)
        if restored.state()!=self.journal.state() or restored.pending()!=['binary','library']:
            raise FreshnessHold('metadata_torn')
        if self.restart_gate is None or self.restart_gate.state()['passes']!=1:raise FreshnessHold('metadata_torn')
        self.restart_gate.begin(value)
        state=restored.state();paths=[path for name in restored.pending() for path in state['units'][name]['fingerprints']]
        intent={'version':1,'binding':self.admission.binding,'command':self.admission.command,
            'paths':paths,'fingerprints':self.fingerprints,'pending':restored.pending(),'state':'prepared'}
        cache._write_record(self.root/'invalidations/retirement.json',intent)
        retained=retire_unit_fingerprints(self.target,self.fingerprints,paths)
        cache._write_record(self.root/'invalidations/retirement.json',intent|{'state':'retired'})
        before=len(relay.completed);old_publications=copy_publications(self.history)
        self.retry_active=True;_PHASE='planner_cargo_retry'
        try:status,output,error=self._command(value,relay)
        finally:self.retry_active=False
        self.restart_gate.finish(value,self.commands[-1]['directory']/'cleanup')
        completed=relay.completed[before:]
        compilers=[unit for unit in completed if self.units[unit['ordinal']]['mapping']['kind']=='compile']
        if (status!=101 or output or b'E0761' not in error or len(compilers)!=1
                or self.units[compilers[0]['ordinal']]['mapping']['role']!='library'
                or compilers[0]['returncode']!=1
                or any(self.units[unit['ordinal']]['mapping']['kind'] in ('application','build_script') for unit in completed)
                or copy_publications(self.history)!=old_publications
                or any(self._stat(path)!=prior for path,prior in self.generated_stats.items())):
            raise cargo_hold('planner_cargo_compile')
        current=unit_views.inventory(self.target/'debug/.fingerprint',owner=1001,sealed=False)
        selected=[os.fsencode(Path(path).name) for path in paths]
        unaffected=[row for row in current if not any(bytes.fromhex(row[0])==prefix
            or bytes.fromhex(row[0]).startswith(prefix+b'/') for prefix in selected)]
        if unaffected!=retained:raise FreshnessHold('invalidation_unsafe')
        restored=lookup_inputs.UnitInvalidations(self.journal.path,
            binding=self.admission.binding,command=self.admission.command)
        if restored.state()!=self.journal.state():raise FreshnessHold('metadata_torn')
        self.report['retry']={'version':1,'cargo_exit':status,'library_exit':1,'compiler_calls':1,'diagnostic':'E0761',
            'rounds':1,'original_planner_cleanup':True,'original_unit_cleanup':True,
            'unaffected_fingerprints_preserved':True,'original_generated_preserved':True,
            'failed_outputs_unpublished':True,'pending':restored.pending(),'units':completed,
            'planner_passes':self.restart_gate.state()['passes'],'planner_pass_limit':4,'original_grant_unchanged':True,
            'input_closure_complete':False,'shipping_authority':False}
        admissions=[]
        for ordinal,unit in sorted(self.units.items()):
            if unit['mapping']['kind']=='compile':
                if not unit['finished'] or unit['cargo_target'] is None or unit['graph_dispatch'] is not True:raise cargo_hold('planner_cargo_request')
                admissions.append({'ordinal':ordinal,'target':unit['cargo_target']})
        self.report['cargo_graph']={'version':1,'metadata_sha256':self.graph.metadata_sha256,
            'packages':len(self.graph.packages),'targets':self.graph.target_count,
            'original_metadata_cleanup':True,'admissions':admissions,'graph_dispatch':True,
            'input_closure_complete':False,'shipping_authority':False}
        state=self.store.state();history=self.history;active,_files=self.store.snapshot()
        evidence=[cache._read_record(self.store.root/'records'/(identity+'.json'),max_bytes=self.store.LIMIT)['evidence']
            for identity in state['history']]
        actual=[item for item in evidence if item['kind']=='unit'];diagnostic=[item for item in evidence if item['kind']=='controller-diagnostic']
        if (len(history)!=5 or len(active)!=4 or len(actual)!=4 or len(diagnostic)!=1
                or any(item['original_cleanup'] is not True for item in actual)
                or diagnostic[0]['original_cleanup'] is not False or state['last_affected']!=['binary','library']):
            raise FreshnessHold('artifact_provenance_unknown')
        self.report['publication_store']={'version':1,'history_records':5,'active_producers':4,'unit_producers':4,
            'diagnostic_producers':1,'invalidation_before_activation':True,'last_affected':state['last_affected'],
            'original_unit_cleanup':True,'history_preserved':True,'input_closure_complete':False,'shipping_authority':False}


    def cleanup(self):
        if self.owner!=os.getpid() or self.report is None:raise FreshnessHold('cleanup_uncertain')
        _source(self.admission.target,self.admission.binding,self.admission.command)
        if cache._read_record(self.root/'control.json')!={**self.control,'state':'active'}:
            raise FreshnessHold('cleanup_uncertain')
        shutil.rmtree(self.root);self.report['allocation_removed']=True




def cargo_hold(stage):
    error=FreshnessHold('artifact_provenance_unknown');error.unit_view_stage=stage;return error


def copy_publications(publications):
    result={}
    for label,publication in publications.items():
        actual=unit_views.inventory(publication['root'],owner=0,sealed=True)
        if actual!=publication['manifest']:raise FreshnessHold('artifact_provenance_unknown')
        result[label]=(str(publication['root']),actual,tuple(publication['files']),tuple(publication['roots']),
            publication['action'],json.dumps(publication['cargo_target'],sort_keys=True),publication['generated_root'])
    return result


def original_cargo_cleanup(path,*,owner=0):
    try:
        path=unit_views.literal(path)
        expected=b'all-descendants-reaped\n'
        if unit_views._read(path,owner,False)!=(0o600,len(expected),hashlib.sha256(expected).hexdigest()):
            raise FreshnessHold('cleanup_uncertain')
        return _CargoProbe._stat(path)
    except (OSError,unit_views.FreshnessHold):raise FreshnessHold('cleanup_uncertain') from None


def retire_unit_fingerprints(target,expected,paths,*,owner=1001):
    """Remove exact inventoried directories after caller-proved scope drainage.

    This is not an ownership/quiescence capability. The admitted root planner
    must persist invalidation intent and authenticate these paths beforehand.
    """
    try:
        target=unit_views.literal(target);metadata=unit_views.literal(target/'debug/.fingerprint')
        if (target.stat().st_uid!=owner or type(paths) is not list or not 0<len(paths)<=64
                or any(type(path) is not str for path in paths) or len(set(paths))!=len(paths)
                or not shutil.rmtree.avoids_symlink_attacks):raise FreshnessHold('invalidation_unsafe')
        names=[]
        for path in paths:
            parts=unit_views.relative(path).parts
            if len(parts)!=3 or parts[:2]!=('debug','.fingerprint'):raise FreshnessHold('invalidation_unsafe')
            names.append(parts[2])
        current=unit_views.inventory(metadata,owner=owner,sealed=False)
        if current!=expected:raise FreshnessHold('invalidation_unsafe')
        roots={bytes.fromhex(row[0]) for row in current if row[1]=='directory'}
        prefixes=[os.fsencode(name) for name in names]
        if any(prefix not in roots for prefix in prefixes):raise FreshnessHold('invalidation_unsafe')
        retained=[row for row in current if not any(bytes.fromhex(row[0])==prefix
            or bytes.fromhex(row[0]).startswith(prefix+b'/') for prefix in prefixes)]
        descriptor=os.open(metadata,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW)
        try:
            for name in names:shutil.rmtree(name,dir_fd=descriptor)
            os.fsync(descriptor)
        finally:os.close(descriptor)
        if unit_views.inventory(metadata,owner=owner,sealed=False)!=retained:raise FreshnessHold('invalidation_unsafe')
        return retained
    except OSError:raise FreshnessHold('invalidation_unsafe') from None


def _cargo_supervisor(arguments):
    """Original root supervisor for exactly one sealed Cargo invocation."""
    global _PHASE
    _PHASE='planner_cargo_supervisor'
    bootstrap,configuration=map(int,arguments)
    value=_bootstrap(bootstrap,'cargo-supervisor');_parent_guard(value['parent']);os.close(bootstrap)
    root=unit_views.literal(value['cargo_root']);directory=unit_views.literal(value['command_directory'])
    if (directory.parent!=root or re.fullmatch(r'command-[0-9]{1,3}',directory.name) is None
            or stat.S_IMODE(directory.stat().st_mode)!=0o700 or directory.stat().st_uid!=0
            or cache._read_record(directory/'control.json')!={'version':1,'binding':value['binding'],
                'command':value['command'],'invocation':value['invocation'],'operation':value['cargo_operation']}):
        raise FreshnessHold('cleanup_uncertain')
    mediation=ExecMediation(value['deadline'],1001)
    child_bootstrap=_sealed(dict(value,kind='cargo',parent=os.getpid(),exec_mediation_fd=mediation.child_fd))
    supervisor=_load('original_cargo_supervisor','provider-supervisor.py')
    try:
        raw=supervisor.supervise(directory/'cleanup',
            [sys.executable,'-I',str(Path(__file__).resolve()),'--cargo',str(child_bootstrap),str(configuration)],
            forward_fds=(child_bootstrap,configuration,mediation.child_fd),close_after_spawn=(child_bootstrap,),
            on_spawn=mediation.spawned,on_exec=mediation.executed)
        mediation.check();return raw
    finally:mediation.close()


def cargo_namespace(root,source,source_mounts=None):
    root=unit_views.literal(root);source=unit_views.literal(source)
    libc=ctypes.CDLL(None,use_errno=True)
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
    if source_mounts is not None:
        path=unit_views.literal(source_mounts)
        if path!=root/'source-mounts.json':raise FreshnessHold('artifact_provenance_unknown')
        document=cache._read_record(path,max_bytes=16*1024**2);control=cache._read_record(root/'control.json')
        if (document.get('version')!=1 or document.get('binding')!=control.get('binding')
                or document.get('command')!=control.get('command')):raise FreshnessHold('generation_mismatch')
        unit_views.source_mounts(Path('/workspace'),document['entries'],document['outputs'])


def _cargo(arguments):
    """Cargo bootstrap under non-root planner credentials and the sealed grant."""
    global _PHASE
    _PHASE='planner_client_start';bootstrap,configuration=map(int,arguments)
    value=_bootstrap(bootstrap,'cargo');_parent_guard(value['parent']);os.close(bootstrap)
    root=unit_views.literal(value['cargo_root']);source=unit_views.literal(value['source'])
    if (root.parent!=Path(value['target']) or not root.name.startswith('.cargo-execution-')
            or cache._read_record(root/'control.json')!={'version':1,'kind':'cargo-execution',
                'binding':value['binding'],'command':value['command'],'shipping_authority':False,'state':'active'}):
        raise FreshnessHold('invalidation_unsafe')
    policy=UnitPolicy()
    _PHASE='planner_cargo_runtime';cargo_namespace(root,source,value.get('source_mounts'));libc=ctypes.CDLL(None,use_errno=True)
    invocation=value.get('original_invocation')
    if invocation is None:
        environment={'PATH':'/toolchain/bin:/usr/bin:/bin','HOME':'/home/worker','CARGO_HOME':'/ci-cargo-home',
            'CARGO_TARGET_DIR':'/workspace/target','CARGO_INCREMENTAL':'0','CARGO_BUILD_JOBS':'1',
            'CARGO_PROFILE_DEV_DEBUG':'0','RUSTC':'/toolchain/bin/rustc',
            'LD_PRELOAD':value['library'],'JARVIS_CARGO_RELAY_FD':str(configuration)}
    else:
        validate_invocation(**invocation)
        environment=dict(invocation['environment'],LD_PRELOAD=value['library'],JARVIS_CARGO_RELAY_FD=str(configuration))
    stage_exec_mediation(value,policy);policy.activate()
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    os.setgroups([]);os.setgid(1001);os.setuid(1001);os.umask(0o077);_parent_guard(value['parent'])
    os.set_inheritable(configuration,True);os.chdir('/workspace');_PHASE='planner_cargo_execute'
    operation=value.get('cargo_operation')
    if operation=='metadata':argv=['/toolchain/bin/cargo','metadata','--offline','--locked','--no-deps','--format-version','1']
    elif operation=='run' and invocation is None:argv=['/toolchain/bin/cargo','run','--offline','--locked','--quiet']
    elif operation=='original' and invocation is not None:argv=invocation['argv']
    else:raise FreshnessHold('invalidation_unsafe')
    arm_original_exec();os.execve('/toolchain/bin/cargo',argv,environment)



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
 binary=libc.syscall(ctypes.c_long(257),ctypes.c_long(-100),
  ctypes.c_char_p(b'/workspace/target/deps/raw-\xff'),ctypes.c_long(os.O_RDONLY|os.O_CLOEXEC),ctypes.c_long(0))
 assert binary>=0
 try:
  metadata=ctypes.create_string_buffer(144)
  assert libc.syscall(ctypes.c_long(5),ctypes.c_long(binary),metadata)==0
  assert libc.syscall(ctypes.c_long(5),ctypes.c_long(binary),ctypes.c_void_p(0))==-1
  assert ctypes.get_errno()==errno.EFAULT
  assert libc.syscall(ctypes.c_long(5),ctypes.c_long(-1),metadata)==-1
  assert ctypes.get_errno()==errno.EBADF
  assert os.read(binary,64)==b'entry\n'
 finally:os.close(binary)
 binary_missing=libc.syscall(ctypes.c_long(257),ctypes.c_long(-100),
  ctypes.c_char_p(b'/workspace/target/deps/missing-\xff\n '),ctypes.c_long(os.O_RDONLY|os.O_CLOEXEC),ctypes.c_long(0))
 assert binary_missing==-1 and ctypes.get_errno()==errno.ENOENT
 directory=os.open('/workspace/target/deps',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
 try:
  metadata=ctypes.create_string_buffer(144)
  assert libc.syscall(ctypes.c_long(5),ctypes.c_long(directory),metadata)==0
  assert libc.syscall(ctypes.c_long(262),ctypes.c_long(directory),ctypes.c_char_p(b'src/lib.rs'),metadata,ctypes.c_int(256))==0
  extended=ctypes.create_string_buffer(256)
  assert libc.syscall(ctypes.c_long(332),ctypes.c_long(directory),ctypes.c_char_p(b'src/lib.rs'),ctypes.c_uint(256),ctypes.c_uint(8191),extended)==0
  result['statx_sha256']=hashlib.sha256(extended.raw).hexdigest()
  assert libc.syscall(ctypes.c_long(332),ctypes.c_long(directory),ctypes.c_char_p(b'src/lib.rs'),ctypes.c_uint(256),ctypes.c_uint(8191),ctypes.c_void_p(0))==-1
  assert ctypes.get_errno()==errno.EFAULT
  assert libc.syscall(ctypes.c_long(332),ctypes.c_long(directory),ctypes.c_char_p(b'/workspace/target/deps/missing'),ctypes.c_uint(256),ctypes.c_uint(8191),extended)==-1
  assert ctypes.get_errno()==errno.ENOENT
 finally:os.close(directory)
 metadata=ctypes.create_string_buffer(144)
 assert libc.syscall(ctypes.c_long(262),ctypes.c_long(-100),ctypes.c_char_p(b'src/main.rs'),metadata,ctypes.c_int(256))==0
 os.chdir('/workspace/target/deps')
 try:
  assert libc.syscall(ctypes.c_long(262),ctypes.c_long(-100),ctypes.c_char_p(b'src/lib.rs'),metadata,ctypes.c_int(256))==0
  assert libc.syscall(ctypes.c_long(262),ctypes.c_long(-100),ctypes.c_char_p(b'src/main.rs'),metadata,ctypes.c_int(256))==-1
  assert ctypes.get_errno()==errno.ENOENT
  assert libc.syscall(ctypes.c_long(262),ctypes.c_long(-1),ctypes.c_char_p(b'src/main.rs'),metadata,ctypes.c_int(256))==-1
  assert ctypes.get_errno()==errno.EBADF
 finally:os.chdir('/workspace')
 _PROBE_PHASE='planner_tool_view'
 from pathlib import Path
 import time
 input=Path('/workspace/target/deps/input')
 selected=lambda info:(info.st_ino,info.st_nlink,info.st_mode,info.st_size,info.st_mtime_ns,info.st_ctime_ns,info.st_atime_ns)
 before=selected(input.stat());assert input.read_bytes()==b'OLD'
 import struct
 def directory_names(path):
  descriptor=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
  names=[];reads=0;buffer=ctypes.create_string_buffer(288)
  try:
   for _ in range(32):
    amount=libc.syscall(ctypes.c_long(217),ctypes.c_long(descriptor),buffer,ctypes.c_uint(288))
    assert amount>=0;reads+=1
    if amount==0:return names,reads
    offset=0
    while offset<amount:
     inode,next_offset,length,kind=struct.unpack_from('=QqHB',buffer.raw,offset)
     assert length>=24 and length%8==0 and offset+length<=amount
     name=buffer.raw[offset+19:offset+length].split(b'\0',1)[0]
     assert name;names.append(name);offset+=length
    assert offset==amount
   raise AssertionError('directory EOF missing')
  finally:os.close(descriptor)
 names,reads=directory_names('/workspace/target/deps')
 assert len(names)==9 and set(names)=={b'.',b'..',b'input',b'one.rs',b'empty',b'cursor',b'src',b'raw-\xff',b'l'*255}
 assert reads>=3
 names,reads=directory_names('/workspace/target/deps/empty')
 assert len(names)==2 and set(names)=={b'.',b'..'} and reads==2
 descriptor=os.open('/workspace/target/deps/cursor',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
 try:
  buffer=ctypes.create_string_buffer(288)
  amount=libc.syscall(ctypes.c_long(217),ctypes.c_long(descriptor),buffer,ctypes.c_uint(24))
  assert amount==24
  first_cookie=struct.unpack_from('=q',buffer.raw,8)[0]
  assert first_cookie!=0
  assert os.lseek(descriptor,0,os.SEEK_SET)==0
  amount=libc.syscall(ctypes.c_long(217),ctypes.c_long(descriptor),buffer,ctypes.c_uint(288))
  assert amount==48
  amount=libc.syscall(ctypes.c_long(217),ctypes.c_long(descriptor),buffer,ctypes.c_uint(288))
  assert amount==0
 finally:os.close(descriptor)
 for operation in (lambda:input.open('wb'),lambda:input.unlink(),
     lambda:Path('/workspace/target/deps/forbidden').mkdir()):
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


def _credential_client(arguments):
    bootstrap=int(arguments[0]);value=_bootstrap(bootstrap,'credential-client');_parent_guard(value['parent']);os.close(bootstrap)
    uid=value['uid'];configuration=int(arguments[1])
    if type(uid) is not int or uid not in (1000,1001):raise FreshnessHold('invalidation_unsafe')
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    os.setgroups([]);os.setgid(uid);os.setuid(uid);_parent_guard(value['parent'])
    os.set_inheritable(configuration,True)
    os.execve('/usr/bin/python3',['python3','-I','-c','print("CREDENTIAL_MAIN_REACHED",flush=True)'],
        {'PATH':os.defpath,'LD_PRELOAD':value['library'],'JARVIS_CARGO_RELAY_FD':str(configuration)})


def credential_role_probe(value,relay):
    results={}
    for name,uid,magic in (('worker_rejects_planner',1000,b'JARVCFG3'),('planner_rejects_worker',1001,b'JARVWRK3')):
        configuration=os.memfd_create('negative-credential-role',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        bootstrap=None;child=None
        try:
            body=legacy.CONFIG.pack(magic,0,os.getpid(),30000,relay.nonce,os.fsencode(relay.path))
            if os.write(configuration,body)!=len(body):raise FreshnessHold('metadata_torn')
            fcntl.fcntl(configuration,fcntl.F_ADD_SEALS,legacy.SEALS)
            bootstrap=_sealed(dict(value,kind='credential-client',parent=os.getpid(),uid=uid))
            child=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--credential-client',str(bootstrap),str(configuration)],
                pass_fds=(bootstrap,configuration),env={'PATH':os.defpath},stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            output,error=child.communicate(timeout=5)
            if child.returncode!=125 or output or error:raise FreshnessHold('invalidation_unsafe')
            results[name]=True
        finally:
            if child is not None and child.poll() is None:child.kill();child.wait(timeout=2)
            os.close(configuration)
            if bootstrap is not None:os.close(bootstrap)
    return results


def _unit_credential_client(arguments):
    bootstrap=int(arguments[0]);value=_bootstrap(bootstrap,'unit-credential-client');_parent_guard(value['parent']);os.close(bootstrap)
    uid=value['uid'];configuration=int(arguments[1])
    if type(uid) is not int or uid not in (1000,1001,2000,6095):raise FreshnessHold('invalidation_unsafe')
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    os.setgroups([]);os.setgid(uid);os.setuid(uid);_parent_guard(value['parent'])
    os.set_inheritable(configuration,True)
    os.execve('/usr/bin/python3',['python3','-I','-c','print("UNIT_ROLE_MAIN_REACHED",flush=True)'],
        {'PATH':os.defpath,'LD_PRELOAD':value['library'],'JARVIS_CARGO_RELAY_FD':str(configuration)})


def unit_credential_role_probe(value,relay):
    """Constructor credential feasibility only; does not dispatch a child unit."""
    results={}
    cases=(('unit_first',2000,b'JARVUNT3',2000,True),('unit_last',6095,b'JARVUNT3',6095,True),
        ('unit_wrong_client',2000,b'JARVUNT3',6095,False),('unit_zero',2000,b'JARVUNT3',0,False),
        ('unit_below_range',2000,b'JARVUNT3',1999,False),('unit_above_range',2000,b'JARVUNT3',6096,False),
        ('worker_rejects_unit',1000,b'JARVUNT3',2000,False),('planner_rejects_unit',1001,b'JARVUNT3',2000,False),
        ('unit_rejects_worker',2000,b'JARVWRK3',0,False),('unit_rejects_planner',2000,b'JARVCFG3',0,False))
    for name,uid,magic,client_uid,accepted in cases:
        configuration=os.memfd_create('unit-credential-role',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        bootstrap=None;child=None
        try:
            body=legacy.CONFIG.pack(magic,client_uid,os.getpid(),30000,relay.nonce,os.fsencode(relay.path))
            if os.write(configuration,body)!=len(body):raise FreshnessHold('metadata_torn')
            fcntl.fcntl(configuration,fcntl.F_ADD_SEALS,legacy.SEALS)
            bootstrap=_sealed(dict(value,kind='unit-credential-client',parent=os.getpid(),uid=uid))
            child=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--unit-credential-client',str(bootstrap),str(configuration)],
                pass_fds=(bootstrap,configuration),env={'PATH':os.defpath},stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            output,error=child.communicate(timeout=5)
            result={'uid':uid,'config_uid':client_uid,'exit_code':child.returncode,'main_reached':output==b'UNIT_ROLE_MAIN_REACHED\n','expected_acceptance':accepted}
            results[name]=result
            if child.returncode!=(0 if accepted else 125) or error or output!=(b'UNIT_ROLE_MAIN_REACHED\n' if accepted else b''):
                break
        finally:
            if child is not None and child.poll() is None:child.kill();child.wait(timeout=2)
            os.close(configuration)
            if bootstrap is not None:os.close(bootstrap)
    record={'version':1,'kind':'ci-unit-role-constructor-probe','binding':value['binding'],'command':value['command'],
        'cases':results,'dispatch_tested':False,'input_closure_complete':False,'shipping_authority':False}
    cache._write_record(Path(value['target'])/'unit-role-constructor-probe.json',record)
    if len(results)!=len(cases) or any(row['main_reached']!=row['expected_acceptance']
            or row['exit_code']!=(0 if row['expected_acceptance'] else 125) for row in results.values()):
        raise FreshnessHold('invalidation_unsafe')


def _probe(arguments):
    global _PHASE,_CHILD_DIAGNOSTIC
    value=_bootstrap(int(arguments[0]),'probe');_parent_guard(value['parent'])
    root=Path(value['root']);admission=admit(value['target'],value['binding'],value['command'],root)
    results=[];view_probe=None;cargo_probe=None
    _PHASE='planner_kernel_observer'
    relay=RootRelay(admission,library=value['library'],timeout=30)
    # Credential-only constructor cases are not planned compiler units. Keep
    # them outside the original unit observer's process/UID attribution window.
    unit_credential_role_probe(value,relay)
    with kernel_observer.Observer(admission,deadline=value['deadline']) as observer, relay:
        credential_roles=credential_role_probe(value,relay)
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
        relay.drain()
        _PHASE='planner_kernel_observer'
        compilers={item['mapping']['role']:(item['pid'],2000+ordinal)
            for ordinal,item in cargo_probe.units.items() if item['mapping']['kind']=='compile'}
        generated=cargo_probe.publication('generated')
        empty=generated['root']/generated['roots'][0]/'empty'
        directories=[(view_probe.original_pid,2004,view_probe.inputs/'deps',view_probe.directory_members),
            (view_probe.original_pid,2004,view_probe.inputs/'deps/empty',{})]
        directories.extend((item['pid'],2000+ordinal,empty,{}) for ordinal,item in cargo_probe.units.items()
            if item['mapping']['kind']=='application')
        kernel=observer.finish(view_probe.original_pid,compilers,directories,view_probe.inputs/'deps/cursor',
            view_probe.inputs/'deps'/os.fsdecode(b'raw-\xff'),results[4]['statx_sha256'],view_probe.source,
            int(view_probe.receipt['namespaces']['mnt']['unit'].split('[',1)[1][:-1]),view_probe,cargo_probe)
        cargo_probe.retry_invalidated(value,relay)
    view_probe.cleanup()
    cargo_probe.cleanup()
    print(json.dumps({'version':3,'kind':'ci-planner-protocol-probe','binding':value['binding'],
        'command':value['command'],'credential_roles':credential_roles,'cases':results,'units':original_units,'filesystem':view_probe.receipt,
        'cargo':cargo_probe.report,'kernel_observation':kernel,
        'input_closure_complete':False,'cargo_metadata_isolation_established':False,'shipping_authority':False}))
    return 0


def _execute(arguments):
    """Real structured CI commands through graph dispatch, without CI authority.

    The normal outer host supervisor/deadline/resource lease remains in force.
    This inner 900s ceiling cannot extend that grant. Persistent recovery and
    complete input closure are still activation prerequisites.
    """
    value=_bootstrap(int(arguments[0]),'execution');_parent_guard(value['parent'])
    commands=value['commands'];environment=value['environment'];cwd=value['cwd']
    if type(commands) is not list or not 1<=len(commands)<=2:raise cargo_hold('planner_cargo_request')
    for argv in commands:validate_invocation(argv,environment,cwd)
    admission=admit(value['target'],value['binding'],value['command'],value['root'])
    outputs=[];errors=[];receipts=[];exit_code=0
    with CommandObservation(admission,value['deadline']) as observation, RootRelay(admission,library=value['library'],timeout=min(900,max(1,value['deadline']-time.monotonic()))) as relay:
        executor=_CargoExecutor(admission,execution_lease=value.get('execution_lease'),generated_outputs=value.get('generated_outputs'));relay.views=executor
        first=validate_invocation(commands[0],environment,cwd)
        context=dict(value,original_invocation=first)
        executor.preparing_graph=True
        try:status,metadata,error=executor._command(context,relay,operation='metadata')
        finally:executor.preparing_graph=False
        if status!=0:raise cargo_hold('planner_cargo_request')
        executor.graph=CargoGraph(executor.source,metadata,owner=0)
        metadata_units=list(relay.completed)
        for argv in commands:
            invocation=validate_invocation(argv,environment,cwd);before=len(relay.completed)
            exit_code,output,error=executor._command(dict(value,original_invocation=invocation),relay,operation='original')
            outputs.append(output);errors.append(error)
            if sum(map(len,outputs))+sum(map(len,errors))>8*1024**2:raise FreshnessHold('input_census_unknown')
            units=relay.completed[before:]
            if any(not executor.units[unit['ordinal']]['finished'] for unit in units):raise FreshnessHold('cleanup_uncertain')
            receipts.append({'argv':argv,'context_sha256':hashlib.sha256(json.dumps(invocation,sort_keys=True,
                separators=(',',':')).encode()).hexdigest(),'exit_code':exit_code,'original_cargo_cleanup':True,
                'compiler_calls':sum(executor.units[unit['ordinal']]['mapping']['kind']=='compile' for unit in units),
                'units':units})
            if exit_code:break
        relay.drain()
        captured=observation.finish(executor)
        receipt={'version':1,'kind':'ci-cargo-execution','binding':value['binding'],'command':value['command'],
            'commands':receipts,'metadata_units':metadata_units,'metadata_sha256':executor.graph.metadata_sha256,'original_metadata_cleanup':True,'kernel_observation':captured,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        # Leave the protected ownership records intact. A following command must
        # recover/revalidate them; a fresh private target is not a reuse policy.
        cache._write_record(executor.root/'execution.json',receipt)
    print(json.dumps({'exit_code':exit_code,'stdout':b''.join(outputs).decode('utf-8','replace'),
        'stderr':b''.join(errors).decode('utf-8','replace'),'cargo_execution':receipt}))
    return 0


def repository_worker_policy_context(value,policy,pid):
    scope=Path(value['receipt']).parent.name.removeprefix('worker-')
    if not cache._uuid(scope):raise FreshnessHold('generation_mismatch')
    return {'version':1,'kind':'ci-repository-worker-policy-receipt','binding':value['binding'],
        'command':value['command'],'scope_id':scope,'pid':pid,
        'policy':{'version':1,'kind':'ci-unit-policy-activation','sha256':policy.sha256,
            'size_bytes':policy.size_bytes,'activation_returncode':0,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}}


def admit_original_repository_worker(child,endpoint,configuration,receipt,value):
    global _PHASE
    _PHASE='planner_original_worker_entry'
    pidfd=None
    try:
        while True:
            remaining=value['deadline']-time.monotonic()
            if remaining<=0 or child.poll() is not None:raise FreshnessHold('cleanup_uncertain')
            if select.select([endpoint],[],[],min(.05,remaining))[0]:break
        frame=receive_control(endpoint)
        if frame is None:raise FreshnessHold('cleanup_uncertain')
        stage,pid,received=frame
        if len(received)!=1:
            legacy._close(received);raise FreshnessHold('cleanup_uncertain')
        pidfd=received[0]
        if stage!=3 or pid<=0:raise FreshnessHold('generation_mismatch')
        signal.pidfd_send_signal(pidfd,0)
        info=Path(f'/proc/self/fdinfo/{pidfd}').read_text().splitlines()
        if next((line.split()[1] for line in info if line.startswith('Pid:')),None)!=str(pid):
            raise FreshnessHold('cleanup_uncertain')
        observed=staging.observe_exec_entry(pid,tracer=child.pid)
        staging.require_exec_entry(observed,uid=1000,gid=1000,groups=[],descriptors=[0,1,2,configuration])
        entry=cache._read_record(Path(value['root'])/'original-worker-entry.json')
        if entry.get('binding')!=value['binding'] or entry.get('command')!=value['command']:
            raise FreshnessHold('generation_mismatch')
        stored=entry['entry']
        if {key:stored[key] for key in observed}!=observed:raise FreshnessHold('invalidation_unsafe')
        expected=repository_worker_policy_context(value,UnitPolicy(),pid)
        if stored['policy_activation']!=staging.read_policy_receipt(receipt,expected):
            raise FreshnessHold('authority_changed')
        configuration_identity=staging.observe_sealed_descriptor(os.getpid(),configuration)
        if (stored['sealed_relay']!=configuration_identity
                or staging.observe_sealed_descriptor(pid,configuration)!=configuration_identity):
            raise FreshnessHold('invalidation_unsafe')
        table={row['number']:row for row in observed['fd_table']}
        for number,stream in ((1,child.stdout),(2,child.stderr)):
            actual=os.fstat(stream.fileno());target=table[number]
            if ((target['device'],target['inode'],target['type'])!=(actual.st_dev,actual.st_ino,stat.S_IFMT(actual.st_mode))
                    or target['flags']&os.O_ACCMODE!=os.O_WRONLY):raise FreshnessHold('invalidation_unsafe')
        namespaces={name:os.readlink(f'/proc/{pid}/ns/'+name) for name in ('mnt','ipc','net','user','pid')}
        if any(namespace!=os.readlink('/proc/self/ns/'+name) for name,namespace in namespaces.items()):
            raise FreshnessHold('invalidation_unsafe')
        admission={'version':1,'kind':'ci-repository-worker-admission','binding':value['binding'],
            'command':value['command'],'scope_id':expected['scope_id'],'supervisor_pid':child.pid,
            'entry':stored,'namespaces':namespaces,'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        cache._write_record(Path(value['root'])/'original-worker-admission.json',admission)
        return pidfd,admission
    except BaseException:
        if pidfd is not None:os.close(pidfd)
        raise


def _repository_worker(arguments):
    bootstrap=int(arguments[0]);value=_bootstrap(bootstrap,'repository-worker');_parent_guard(value['parent']);os.close(bootstrap)
    configuration=int(arguments[1]);environment=dict(value['environment'])
    receipt=value['policy_receipt_fd'];os.set_inheritable(receipt,False)
    if (value['cwd']!='/workspace' or type(value['argv']) is not list or not value['argv']
            or value['argv'][0]!='/bin/bash' or any(key in environment for key in ('LD_PRELOAD','JARVIS_CARGO_RELAY_FD'))):
        raise FreshnessHold('invalidation_unsafe')
    worker_runtime_request((os.fsencode(value['argv'][0]),list(map(os.fsencode,value['argv'])),
        {os.fsencode(key):os.fsencode(item) for key,item in environment.items()},b'/workspace',None,0,0),value['library'])
    environment.update(LD_PRELOAD=value['library'],JARVIS_CARGO_RELAY_FD=str(configuration))
    policy=UnitPolicy();stage_exec_mediation(value,policy);policy.activate()
    staging.publish_policy_receipt(receipt,repository_worker_policy_context(value,policy,os.getpid()))
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.prctl(38,1,0,0,0)!=0:raise FreshnessHold('invalidation_unsafe')
    os.setgroups([]);os.setgid(1000);os.setuid(1000);os.umask(0o022);_parent_guard(value['parent'])
    os.set_inheritable(configuration,True);os.chdir(value['cwd'])
    arm_original_exec();os.execve(value['argv'][0],value['argv'],environment)


def _repository_worker_supervisor(arguments):
    global _PHASE
    _PHASE='planner_original_worker_supervisor'
    value=_bootstrap(int(arguments[0]),'repository-worker-supervisor');_parent_guard(value['parent'])
    mediation=ExecMediation(value['deadline'],1000)
    bootstrap=_sealed(dict(value,kind='repository-worker',parent=os.getpid(),exec_mediation_fd=mediation.child_fd))
    supervisor=_load('original_repository_worker_supervisor','provider-supervisor.py')
    endpoint=socket.socket(fileno=int(arguments[2]));os.set_inheritable(endpoint.fileno(),False)
    def executed(child):
        observed=staging.observe_worker_entry(child.pid,int(arguments[1]),value['policy_receipt_fd'],
            repository_worker_policy_context(value,mediation.policy,child.pid),uid=1000,gid=1000,groups=[])
        cache._write_record(Path(value['root'])/'original-worker-entry.json',{
            'version':1,'binding':value['binding'],'command':value['command'],'entry':observed,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False})
        mediation.executed(child)
        descriptor=os.pidfd_open(child.pid)
        try:_control(endpoint,3,child.pid,descriptor)
        finally:os.close(descriptor)
        wait_original_worker_grant(endpoint,child.pid,value['deadline'])
    try:
        raw=supervisor.supervise(value['worker_receipt'],[sys.executable,'-I',str(Path(__file__).resolve()),
            '--repository-worker',str(bootstrap),arguments[1]],forward_fds=(bootstrap,int(arguments[1]),mediation.child_fd,value['policy_receipt_fd']),
            close_after_spawn=(bootstrap,),on_spawn=mediation.spawned,on_exec=executed)
        mediation.check();return raw
    finally:endpoint.close();mediation.close()


def _execute_repository(arguments):
    value=_bootstrap(int(arguments[0]),'repository-execution');_parent_guard(value['parent'])
    root=Path(value['root']);worker_admission=admit(value['target'],value['binding'],value['command'],root)
    planner_root=root/'planner';planner_root.mkdir(mode=0o700)
    planner_admission=admit(value['target'],value['binding'],value['command'],planner_root)
    supervisor_receipt=root/'original-worker-cleanup';child=None;bootstrap=None;configuration=None;worker_pidfd=None;original_launch=None
    controller,worker_control=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
    policy_receipt=os.memfd_create('ci-repository-worker-policy',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
    with CommandObservation(planner_admission,value['deadline']) as observation, RootRelay(planner_admission,library=value['library'],timeout=min(900,max(1,value['deadline']-time.monotonic())),deadline=value['deadline']) as planner:
      with WorkerRelay(worker_admission,planner_relay=planner,value=value) as worker:
        try:
            configuration=worker.config_fd()
            bootstrap=_sealed(dict(value,kind='repository-worker-supervisor',parent=os.getpid(),worker_receipt=str(supervisor_receipt),policy_receipt_fd=policy_receipt))
            original_launch=worker.projection_lock.launch(value['deadline'],worker.check_worker_scope)
            child=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--repository-worker-supervisor',str(bootstrap),str(configuration),str(worker_control.fileno())],
                pass_fds=(bootstrap,configuration,worker_control.fileno(),policy_receipt),env={'PATH':os.defpath},stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            worker_control.close()
            worker_pidfd,worker.original_worker_entry=admit_original_repository_worker(child,controller,configuration,policy_receipt,value)
            scopes=root/'worker-scopes';scopes.mkdir(mode=0o700)
            scope=scopes/worker.original_worker_entry['scope_id'];scope.mkdir(mode=0o700)
            worker.worker_freezer=RepositoryWorkerFreezer(worker_admission,scope,worker.projection_lock,
                worker.original_worker_entry['namespaces'])
            worker.worker_freezer.enroll(worker.original_worker_entry['entry']['pid'],worker_pidfd,
                worker.original_worker_entry['entry'],supervisor=child.pid,original=True)
            worker.original_worker_entry.update(version=2,worker_scope=worker.worker_freezer.descriptor(),worker_pauses=[])
            cache._write_record(root/'original-worker-admission.json',worker.original_worker_entry)
            _control(controller,3,worker.original_worker_entry['entry']['pid'])
            original_launch.close();original_launch=None
            output,error=capture_command(child,value['deadline']);original_cargo_cleanup(supervisor_receipt)
            worker.drain();planner.drain()
            if worker.errors or planner.errors:raise FreshnessHold('input_census_unknown')
            worker.worker_freezer.drained(worker.original_worker_entry['entry']['pid']);worker.worker_freezer.close()
            captured=observation.finish(worker.executor)
            receipt={'version':2,'kind':'ci-repository-execution','binding':value['binding'],'command':value['command'],
                'worker_entry':worker.original_worker_entry,
                'worker_units':worker.completed,'metadata_units':worker.metadata_units,'cargo_commands':worker.cargo_commands,'original_worker_cleanup':True,'kernel_observation':captured,
                'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
            print(json.dumps({'exit_code':child.returncode,'stdout':output.decode('utf-8','replace'),
                'stderr':error.decode('utf-8','replace'),'repository_execution':receipt}))
        finally:
            try:
                if original_launch is not None:worker._stop.set()
                controller.close()
                if child is not None and child.poll() is None:
                    child.terminate()
                    try:child.wait(timeout=6)
                    except subprocess.TimeoutExpired:child.kill();child.wait(timeout=2);raise FreshnessHold('cleanup_uncertain') from None
            finally:
                legacy._close([] if bootstrap is None else [bootstrap]);legacy._close([] if configuration is None else [configuration])
                legacy._close([policy_receipt]+([] if worker_pidfd is None else [worker_pidfd]));controller.close();worker_control.close()
                if original_launch is not None:original_launch.close()
    return 0


def _repository_execution_supervisor(arguments):
    value=_bootstrap(int(arguments[0]),'repository-execution-supervisor');_parent_guard(value['parent'])
    bootstrap=_sealed(dict(value,kind='repository-execution',parent=os.getpid()))
    supervisor=_load('original_repository_execution_supervisor','provider-supervisor.py')
    return supervisor.supervise(value['receipt'],[sys.executable,'-I',str(Path(__file__).resolve()),'--execute-repository',str(bootstrap)],
        forward_fds=(bootstrap,),close_after_spawn=(bootstrap,))


def _execution_supervisor(arguments):
    value=_bootstrap(int(arguments[0]),'execution-supervisor');_parent_guard(value['parent'])
    child=_sealed(dict(value,kind='execution',parent=os.getpid()))
    supervisor=_load('original_ci_cargo_supervisor','provider-supervisor.py')
    return supervisor.supervise(value['receipt'],[sys.executable,'-I',str(Path(__file__).resolve()),'--execute',str(child)],
        forward_fds=(child,),close_after_spawn=(child,))


def validate_checkpoint(value,binding,command,predecessor,scope):
    if (type(value) is not dict or set(value)!={'version','kind','binding','command','claim','predecessor','scope_id','retained_executor','completion_sha256','generated_forests','input_closure_complete','shipping_authority'}
            or type(value['version']) is not int or value['version']!=2 or value['kind']!='ci-cargo-execution-checkpoint'
            or value['binding']!=binding or value['command']!=command or value['predecessor']!=predecessor
            or not cache._uuid(value['claim']) or value['scope_id']!=scope['scope_id']
            or not cache._digest(value['completion_sha256']) or type(value['generated_forests']) is not list
            or len(value['generated_forests'])>1 or type(value['retained_executor']) is not bool or value['input_closure_complete'] is not False
            or value['shipping_authority'] is not False):raise FreshnessHold('cleanup_uncertain')
    return value


def validate_worker_scope(value,entry,scope_id,pid_namespace):
    keys={'version','kind','scope_id','uid','cgroup_device','cgroup_inode','original_pid','original_birth',
        'pid_namespace','input_closure_complete','full_ci','shipping_authority'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['kind']!='ci-repository-worker-scope' or value['scope_id']!=scope_id or value['uid']!=1000
            or any(type(value[key]) is not int or value[key]<minimum for key,minimum in (
                ('uid',1000),('cgroup_device',0),('cgroup_inode',1),('original_pid',1),('original_birth',0)))
            or value['original_pid']!=entry['pid'] or value['original_birth']!=entry['birth']
            or value['pid_namespace']!=pid_namespace
            or any(value[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))):
        raise ValueError()


def validate_worker_pauses(pauses,scope):
    if type(pauses) is not list or len(pauses)>4096:raise ValueError()
    for pause in pauses:
        if (type(pause) is not dict or set(pause)!={'version','kind','scope','uid_cohort','processes','frozen',
                'input_closure_complete','full_ci','shipping_authority'} or type(pause['version']) is not int
                or pause['version']!=1 or pause['kind']!='ci-worker-freeze-observation'
                or pause['scope']!=scope or pause['frozen'] is not True
                or any(pause[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))):
            raise ValueError()
        census=pause['uid_cohort'];processes=pause['processes']
        if (type(census) is not dict or set(census)!={'version','kind','uid','tasks','pid_namespace',
                'input_closure_complete','full_ci','shipping_authority'} or type(census['version']) is not int
                or census['version']!=1 or census['kind']!='ci-unit-uid-cohort' or census['uid']!=1000
                or type(census['uid']) is not int or census['pid_namespace']!=scope['pid_namespace']
                or any(census[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))
                or type(processes) is not list or not 1<=len(processes)<=512):raise ValueError()
        tasks=[];seen=set()
        for process in processes:
            if (type(process) is not dict or set(process)!={'pid','tasks'} or type(process['pid']) is not int
                    or process['pid']<=0 or process['pid'] in seen or type(process['tasks']) is not list
                    or not 1<=len(process['tasks'])<=512):raise ValueError()
            seen.add(process['pid'])
            for task in process['tasks']:
                if (type(task) is not dict or set(task)!={'tid','birth'} or type(task['tid']) is not int
                        or task['tid']<=0 or type(task['birth']) is not int or task['birth']<0):raise ValueError()
                tasks.append({'pid':process['pid'],**task})
        if (len(tasks)>512 or len({task['tid'] for task in tasks})!=len(tasks)
                or census['tasks']!=sorted(tasks,key=lambda row:(row['pid'],row['tid']))
                or {'pid':scope['original_pid'],'tid':scope['original_pid'],'birth':scope['original_birth']} not in tasks):
            raise ValueError()


def validate_worker_entry(value,binding,command,scope,progress=None):
    """Receipt grammar only; original live admission remains controller-owned."""
    stage='worker_entry_header'
    def at(name):
        nonlocal stage
        stage=name
        if progress is not None:progress['stage']=name
    try:
        at('worker_entry_header')
        keys={'version','kind','binding','command','scope_id','supervisor_pid',
            'entry','namespaces','input_closure_complete','full_ci','shipping_authority'}
        if (type(value) is not dict or type(value.get('version')) is not int or value['version'] not in (1,2)
                or set(value)!=(keys|{'worker_scope','worker_pauses'} if value['version']==2 else keys)
                or value['kind']!='ci-repository-worker-admission' or value['binding']!=binding or value['command']!=command
                or value['scope_id']!=scope['scope_id'] or not cache._uuid(value['scope_id'])
                or type(value['supervisor_pid']) is not int or value['supervisor_pid']<=0
                or any(value[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))):raise ValueError()
        at('worker_entry_credentials');entry=value['entry'];numbers=entry['descriptors']
        if (type(entry['version']) is not int or entry['version']!=1 or entry['kind']!='ci-unit-exec-entry' or type(entry['pid']) is not int or entry['pid']<=0
                or type(entry['birth']) is not int or entry['birth']<0 or entry['tracer']!=value['supervisor_pid']
                or entry['uids']!=[1000]*4 or entry['gids']!=[1000]*4 or entry['groups']!=[]
                or any(type(entry[key]) is not int for key in ('no_new_privs','seccomp','seccomp_filters'))
                or entry['no_new_privs']!=1 or entry['seccomp']!=2 or entry['seccomp_filters']<1
                or set(entry['capabilities'])!={'CapEff','CapPrm','CapAmb'}
                or any(type(number) is not int or number!=0 for number in entry['capabilities'].values())
                or type(numbers) is not list or len(numbers)!=4 or numbers[:3]!=[0,1,2]
                or type(numbers[3]) is not int or not 3<=numbers[3]<=65535
                or any(entry[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))):raise ValueError()
        at('worker_entry_policy');policy=entry['policy_activation'];native=policy['policy']
        if (set(policy)!={'version','kind','binding','command','scope_id','pid','policy'} or type(policy['version']) is not int or policy['version']!=1
                or policy['kind']!='ci-repository-worker-policy-receipt' or policy['binding']!=binding or policy['command']!=command
                or policy['scope_id']!=value['scope_id'] or policy['pid']!=entry['pid']
                or set(native)!={'version','kind','sha256','size_bytes','activation_returncode','input_closure_complete','full_ci','shipping_authority'}
                or type(native['version']) is not int or native['version']!=1 or native['kind']!='ci-unit-policy-activation' or not cache._digest(native['sha256'])
                or type(native['size_bytes']) is not int or not 1<=native['size_bytes']<=2*1024**2
                or type(native['activation_returncode']) is not int or native['activation_returncode']!=0
                or any(native[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))):raise ValueError()
        at('worker_entry_configuration');configuration=entry['sealed_relay']
        if (configuration['uid']!=0 or configuration['gid']!=0 or configuration['seals']&15!=15
                or not cache._digest(configuration['sha256']) or not 1<=configuration['size_bytes']<=staging.MAX_BYTES):raise ValueError()
        at('worker_entry_fd_shape');rows=entry['fd_table']
        if type(rows) is not list or len(rows)!=4 or [row['number'] for row in rows]!=numbers:raise ValueError()
        at('worker_entry_fd_keys')
        for row in rows:
            if set(row)!={'number','device','inode','type','uid','gid','flags','position'}:raise ValueError()
        for key in ('number','device','inode','type','uid','gid','flags','position'):
            at('worker_entry_fd_'+key)
            if any(type(row[key]) is not int or row[key]<0 for row in rows):raise ValueError()
        at('worker_entry_fd_stdin')
        if rows[0]['type']!=stat.S_IFCHR or rows[0]['flags']&os.O_ACCMODE not in (os.O_RDONLY,os.O_RDWR):raise ValueError()
        at('worker_entry_fd_stdout')
        if any(row['type']!=stat.S_IFIFO or row['flags']&os.O_ACCMODE!=os.O_WRONLY for row in rows[1:3]):raise ValueError()
        at('worker_entry_fd_configuration')
        if rows[3]['type']!=stat.S_IFREG or (rows[3]['device'],rows[3]['inode'])!=(configuration['device'],configuration['inode']):raise ValueError()
        at('worker_entry_namespaces')
        if (set(value['namespaces'])!={'mnt','ipc','net','user','pid'} or any(
                re.fullmatch(name+r':\[[0-9]+\]',namespace) is None for name,namespace in value['namespaces'].items())):raise ValueError()
        if value['version']==2:
            validate_worker_scope(value['worker_scope'],entry,value['scope_id'],value['namespaces']['pid'])
            validate_worker_pauses(value['worker_pauses'],value['worker_scope'])
        return value
    except (ValueError,TypeError,KeyError,IndexError):raise FreshnessHold('input_census_unknown',stage.encode()) from None


def validate_repository_execution(value,binding,command,workers,scope,predecessor=None,progress=None):
    try:
        if progress is not None:progress['stage']='repository_receipt_header'
        keys={'version','kind','binding','command','checkpoint','worker_units','cargo_commands','original_worker_cleanup',
            'input_closure_complete','full_ci','shipping_authority','guest_worker_quiescence','kernel_observation','metadata_units'}
        if (type(value) is not dict or type(value.get('version')) is not int or value['version'] not in (1,2)
                or set(value)!=(keys|{'worker_entry'} if value['version']==2 else keys)
                or value['kind']!='ci-repository-execution' or value['binding']!=binding or value['command']!=command
                or value['original_worker_cleanup'] is not True
                or any(value[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))
                or value['guest_worker_quiescence']!=scope or scope not in workers['scopes']
                or type(value['worker_units']) is not list or len(value['worker_units'])>4096
                or type(value['cargo_commands']) is not list or len(value['cargo_commands'])>4096):raise ValueError()
        if value['version']==2:validate_worker_entry(value['worker_entry'],binding,command,scope,progress)
        if progress is not None:progress['stage']='repository_checkpoint'
        validate_checkpoint(value['checkpoint'],binding,command,predecessor,scope)
        if progress is not None:progress['stage']='repository_units'
        units={};cargo=set()
        for unit in value['worker_units']:
            ordinal=unit['ordinal']
            if (type(ordinal) is not int or not 0<=ordinal<4096 or ordinal in units
                    or type(unit['uid']) is not int or unit['uid'] not in (1000,1001)
                    or unit['started'] is not True or unit['cancelled'] is not False or unit['original_cleanup'] is not True
                    or unit['shipping_authority'] is not False or unit['input_closure_complete'] is not False
                    or type(unit['returncode']) is not int or not -64<=unit['returncode']<=255):raise ValueError()
            units[ordinal]=unit
            if value['version']==2 and value['worker_entry']['version']==2 and unit['uid']==1000:
                member=unit['worker_scope'];group=value['worker_entry']['worker_scope']
                if (type(member) is not dict or set(member)!={'pid','birth','scope_id','cgroup_device','cgroup_inode','shipping_authority'}
                        or any(member[key]!=group[key] for key in ('scope_id','cgroup_device','cgroup_inode'))
                        or type(member['pid']) is not int or member['pid']<=0 or member['pid']==group['original_pid']
                        or type(member['birth']) is not int or member['birth']<0 or member['shipping_authority'] is not False):
                    raise ValueError()
        for cargo_command in value['cargo_commands']:
            ordinal=cargo_command['ordinal'];unit=units[ordinal]
            if (ordinal in cargo or unit['uid']!=1001 or cargo_command['cargo_exit']!=unit['returncode']
                    or cargo_command['original_cargo_cleanup'] is not True or type(cargo_command['compiler_calls']) is not int
                    or type(cargo_command['units']) is not list or not 0<=cargo_command['compiler_calls']<=len(cargo_command['units'])):raise ValueError()
            cargo.add(ordinal)
            for compiler in cargo_command['units']:
                if (type(compiler['ordinal']) is not int or compiler['uid']!=2000+compiler['ordinal']
                        or compiler['original_cleanup'] is not True or compiler['shipping_authority'] is not False
                        or compiler['input_closure_complete'] is not False):raise ValueError()
        if cargo!={ordinal for ordinal,unit in units.items() if unit['uid']==1001}:raise ValueError()
        if progress is not None:progress['stage']='repository_kernel_observation'
        validate_command_observation(value['kernel_observation'],binding,command,
            original_unit_ordinals([*value['metadata_units'],*[unit for item in value['cargo_commands'] for unit in item['units']]]))
        return value
    except (ValueError,TypeError,KeyError):raise FreshnessHold('input_census_unknown') from None


def validate_execution(value,binding,command,commands,workers,scope,exit_code,predecessor=None):
    try:
        keys={'version','kind','binding','command','checkpoint','commands','metadata_sha256','original_metadata_cleanup',
            'input_closure_complete','full_ci','shipping_authority','guest_worker_quiescence','kernel_observation','metadata_units'}
        if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
                or value['kind']!='ci-cargo-execution' or value['binding']!=binding or value['command']!=command
                or not cache._digest(value['metadata_sha256']) or value['original_metadata_cleanup'] is not True
                or any(value[key] is not False for key in ('input_closure_complete','full_ci','shipping_authority'))
                or value['guest_worker_quiescence']!=scope or scope not in workers['scopes']
                or type(value['commands']) is not list or not 1<=len(value['commands'])<=len(commands)):
            raise ValueError()
        validate_checkpoint(value['checkpoint'],binding,command,predecessor,scope)
        ordinals=original_unit_ordinals(value['metadata_units'])
        for index,item in enumerate(value['commands']):
            if (type(item) is not dict or set(item)!={'argv','context_sha256','exit_code','original_cargo_cleanup','compiler_calls','units'}
                    or item['argv']!=commands[index] or not cache._digest(item['context_sha256'])
                    or type(item['exit_code']) is not int or not 0<=item['exit_code']<=255
                    or item['original_cargo_cleanup'] is not True or type(item['compiler_calls']) is not int
                    or type(item['units']) is not list or not 0<=item['compiler_calls']<=len(item['units'])
                    or index<len(value['commands'])-1 and item['exit_code']!=0):raise ValueError()
            for unit in item['units']:
                if (type(unit) is not dict or type(unit.get('ordinal')) is not int or not 0<=unit['ordinal']<4096
                        or unit['ordinal'] in ordinals or unit.get('uid')!=2000+unit['ordinal']
                        or unit.get('started') is not True or unit.get('cancelled') is not False
                        or unit.get('original_cleanup') is not True):raise ValueError()
                ordinals.add(unit['ordinal'])
        validate_command_observation(value['kernel_observation'],binding,command,ordinals)
        if (len(value['commands'])<len(commands) and value['commands'][-1]['exit_code']==0
                or type(exit_code) is not int or value['commands'][-1]['exit_code']!=exit_code):raise ValueError()
        return value
    except (ValueError,TypeError,KeyError):raise FreshnessHold('input_census_unknown') from None


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


def validate_compiler_invalidation(value):
    keys={'version','unit_pid','unit_uid','queries','binding_sha256','original_sha256','refreshed_sha256',
        'pending','durable','original_generated_preserved','input_closure_complete','shipping_authority'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or type(value['unit_pid']) is not int or not 0<value['unit_pid']<2**31
            or type(value['unit_uid']) is not int or not 2005<=value['unit_uid']<6096
            or type(value['queries']) is not int or not 0<value['queries']<=4096
            or any(not cache._digest(value[key]) for key in ('binding_sha256','original_sha256','refreshed_sha256'))
            or value['original_sha256']==value['refreshed_sha256'] or value['pending']!=['binary','library']
            or value['durable'] is not True or value['original_generated_preserved'] is not True
            or value['input_closure_complete'] is not False or value['shipping_authority'] is not False):
        raise FreshnessHold('input_census_unknown')
    return value


def validate_cargo_retry(value,*,start):
    flags={'original_planner_cleanup','original_unit_cleanup','unaffected_fingerprints_preserved',
        'original_generated_preserved','failed_outputs_unpublished','original_grant_unchanged'}
    keys=flags|{'version','cargo_exit','library_exit','compiler_calls','diagnostic','rounds','pending','units',
        'input_closure_complete','shipping_authority','planner_passes','planner_pass_limit'}
    integers={'version':1,'cargo_exit':101,'library_exit':1,'compiler_calls':1,'rounds':1,'planner_passes':2,'planner_pass_limit':4}
    if (type(value) is not dict or set(value)!=keys or type(start) is not int or not 5<=start<4096
            or any(type(value[key]) is not int or value[key]!=expected for key,expected in integers.items())
            or value['diagnostic']!='E0761' or value['pending']!=['binary','library']
            or any(value[key] is not True for key in flags) or value['input_closure_complete'] is not False
            or value['shipping_authority'] is not False or type(value['units']) is not list or not 0<len(value['units'])<=32):
        raise FreshnessHold('input_census_unknown')
    for ordinal,unit in enumerate(value['units'],start=start):
        if (type(unit) is not dict or set(unit)!={'ordinal','uid','started','cancelled','returncode',
                'original_cleanup','input_closure_complete','shipping_authority'}
                or type(unit['ordinal']) is not int or unit['ordinal']!=ordinal or ordinal>=4096
                or type(unit['uid']) is not int or unit['uid']!=2000+ordinal
                or type(unit['returncode']) is not int or unit['returncode'] not in (0,1)
                or unit['started'] is not True or unit['cancelled'] is not False or unit['original_cleanup'] is not True
                or unit['input_closure_complete'] is not False or unit['shipping_authority'] is not False):
            raise FreshnessHold('cleanup_uncertain')
    if sum(unit['returncode']==1 for unit in value['units'])!=1:raise FreshnessHold('input_census_unknown')
    return value


def validate_cargo_graph(value,*,initial_units,retry_units,selected_uid):
    keys={'version','metadata_sha256','packages','targets','original_metadata_cleanup','admissions',
        'input_closure_complete','shipping_authority','graph_dispatch'}
    if (type(value) is not dict or set(value)!=keys
            or any(type(value[key]) is not int or value[key]!=count for key,count in (('version',1),('packages',1),('targets',3)))
            or not cache._digest(value['metadata_sha256']) or value['original_metadata_cleanup'] is not True
            or value['graph_dispatch'] is not True
            or value['input_closure_complete'] is not False or value['shipping_authority'] is not False
            or type(value['admissions']) is not list or len(value['admissions'])!=4):raise FreshnessHold('input_census_unknown')
    target_keys={'package_id','package_name','version','manifest_path','manifest_sha256','target_name','target_kind',
        'source_path','source_sha256','metadata_sha256'}
    expected={'/workspace/build.rs':('build-script-build',['custom-build']),
        '/workspace/src/main.rs':('private-cargo-probe',['bin']),'/workspace/src/lib.rs':('private_cargo_probe',['lib'])}
    initial={unit['ordinal'] for unit in initial_units};failed={unit['ordinal'] for unit in retry_units if unit['returncode']==1}
    ordinals=[];sources=[];library=[]
    for admission in value['admissions']:
        if type(admission) is not dict or set(admission)!={'ordinal','target'} or type(admission['ordinal']) is not int:
            raise FreshnessHold('input_census_unknown')
        target=admission['target'];ordinal=admission['ordinal']
        if (type(target) is not dict or set(target)!=target_keys
                or any(type(target[key]) is not str for key in target_keys-{'target_kind'})
                or not 0<len(target['package_id'])<=4096 or target['package_name']!='private-cargo-probe'
                or target['version']!='0.0.0' or target['manifest_path']!='/workspace/Cargo.toml'
                or any(not cache._digest(target[key]) for key in ('manifest_sha256','source_sha256','metadata_sha256'))
                or target['metadata_sha256']!=value['metadata_sha256']
                or (target['target_name'],target['target_kind'])!=expected.get(target['source_path'])
                or ordinal not in initial|failed):raise FreshnessHold('input_census_unknown')
        ordinals.append(ordinal);sources.append(target['source_path'])
        if target['source_path']=='/workspace/src/lib.rs':library.append(ordinal)
    if (ordinals!=sorted(set(ordinals)) or sorted(sources)!=sorted([*expected,'/workspace/src/lib.rs'])
            or len(library)!=2 or library[0]+2000!=selected_uid or {library[1]}!=failed
            or any(ordinal not in initial for ordinal in ordinals[:-1])):raise FreshnessHold('input_census_unknown')
    return value


def validate_publication_store(value):
    integers={'version':1,'history_records':5,'active_producers':4,'unit_producers':4,'diagnostic_producers':1}
    flags={'invalidation_before_activation','original_unit_cleanup','history_preserved'}
    keys=set(integers)|flags|{'last_affected','input_closure_complete','shipping_authority'}
    if (type(value) is not dict or set(value)!=keys
            or any(type(value[key]) is not int or value[key]!=expected for key,expected in integers.items())
            or any(value[key] is not True for key in flags) or value['last_affected']!=['binary','library']
            or value['input_closure_complete'] is not False or value['shipping_authority'] is not False):
        raise FreshnessHold('artifact_provenance_unknown')
    return value


def validate_cargo_probe(value):
    keys={'version','compiler_calls','application_runs','actual_metadata_preserved','sealed_artifact_detached',
        'units','allocation_removed','trusted_input_closure','shipping_authority','build_script_calls',
        'generated_tree_preserved','library_input_mounted','invalidation','retry','cargo_graph','publication_store'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['compiler_calls']!=[3,0] or any(type(count) is not int for count in value['compiler_calls'])
            or value['build_script_calls']!=[1,0] or any(type(count) is not int for count in value['build_script_calls'])
            or type(value['application_runs']) is not int or value['application_runs']!=2
            or any(value[key] is not True for key in ('actual_metadata_preserved','sealed_artifact_detached',
                'allocation_removed','generated_tree_preserved','library_input_mounted'))
            or value['trusted_input_closure'] is not False or value['shipping_authority'] is not False
            or type(value['units']) is not list or not 6<=len(value['units'])<=32):
        raise FreshnessHold('input_census_unknown')
    validate_compiler_invalidation(value['invalidation'])
    if value['invalidation']['unit_uid'] not in [unit.get('uid') for unit in value['units'] if type(unit) is dict]:
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
    validate_cargo_retry(value['retry'],start=value['units'][-1]['ordinal']+1)
    validate_cargo_graph(value['cargo_graph'],initial_units=value['units'],retry_units=value['retry']['units'],
        selected_uid=value['invalidation']['unit_uid'])
    validate_publication_store(value['publication_store'])
    return value


def validate_probe(value,binding,command,workers,writer,source_projection):
    keys={'version','kind','binding','command','credential_roles','cases','units','guest_worker_quiescence','filesystem',
        'input_closure_complete','cargo_metadata_isolation_established','shipping_authority','kernel_observation'}
    if (type(value) is not dict or set(value) not in (keys,keys|{'cargo'}) or type(value['version']) is not int or value['version']!=3
            or value['kind']!='ci-planner-protocol-probe' or value['binding']!=binding or value['command']!=command
            or type(value['credential_roles']) is not dict
            or set(value['credential_roles'])!={'worker_rejects_planner','planner_rejects_worker'}
            or any(result is not True for result in value['credential_roles'].values())
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
            if ordinal==4:case_keys|={'filesystem','statx_sha256'}
            if (type(case) is not dict or set(case)!=case_keys or type(case['uid']) is not int or case['uid']!=2000+ordinal
                    or case['stdin_sha256']!=expected or case['metadata_denied'] is not True
                    or case['root_socket_denied'] is not True or case['control_fds_closed'] is not True
                    or case['jobserver'] is not (ordinal==1)
                    or (ordinal==3 and (case['signal_forwarded']!=10 or case['proxy_signal']!=10))):
                raise FreshnessHold('input_census_unknown')
            if ordinal==4:
                if not cache._digest(case['statx_sha256']):raise FreshnessHold('input_census_unknown')
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
        elif sys.argv[1:2]==['--cargo-supervisor']:result=_cargo_supervisor(sys.argv[2:])
        elif sys.argv[1:2]==['--execute']:result=_execute(sys.argv[2:])
        elif sys.argv[1:2]==['--execution-supervisor']:result=_execution_supervisor(sys.argv[2:])
        elif sys.argv[1:2]==['--repository-worker']:result=_repository_worker(sys.argv[2:])
        elif sys.argv[1:2]==['--repository-worker-supervisor']:result=_repository_worker_supervisor(sys.argv[2:])
        elif sys.argv[1:2]==['--execute-repository']:result=_execute_repository(sys.argv[2:])
        elif sys.argv[1:2]==['--repository-execution-supervisor']:result=_repository_execution_supervisor(sys.argv[2:])
        elif sys.argv[1:2]==['--credential-client']:result=_credential_client(sys.argv[2:])
        elif sys.argv[1:2]==['--unit-credential-client']:result=_unit_credential_client(sys.argv[2:])
        elif sys.argv[1:2]==['--probe']:result=_probe(sys.argv[2:])
        elif sys.argv[1:2]==['--probe-supervisor']:result=_probe_supervisor(sys.argv[2:])
        else:raise FreshnessHold('invalidation_unsafe')
    except BaseException as error:
        print(json.dumps({'planner_diagnostic':_CHILD_DIAGNOSTIC or diagnostic({
            'stage':getattr(error,'unit_view_stage',_PHASE),'category':type(error).__name__,'errno':getattr(error,'errno',None)})}),file=sys.stderr)
        result=125
    raise SystemExit(result)
