#!/usr/bin/env python3
"""Bounded stdin capture for the private protected-planner protocol.

This module never executes a tool or supplies a complete input/CI proof. The
original supervisor must acknowledge staging_ready before calling capture, keep
its cancellation channel live, and gate tool_started after capture returns.
Requested tool environments never configure this controller interpreter.
"""
import errno
import array
import fcntl
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import select
import stat
import termios
import time


_spec=importlib.util.spec_from_file_location('ci_staging_cache',Path(__file__).with_name('ci-cache-generation.py'))
_cache=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(_cache)
MAX_BYTES=8*1024**2
MAX_SECONDS=60
SEALS=fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL
_TOKEN=object()


class StagingUnavailable(_cache.FreshnessHold):
    def __init__(self,reason):
        self.stage_reason=reason
        super().__init__('input_census_unknown',reason.encode('ascii'))


def observe_exec_entry(pid,*,tracer=None):
    """Inspect the original supervisor's still-stopped, single-threaded exec.

    No descriptor is opened in the tracee and no pipe bytes, file contents or
    shared offsets are read. These facts do not identify the installed filter
    program, establish input closure, or authorize a transfer or resume.
    """
    if tracer is None:tracer=os.getpid()
    if type(pid) is not int or pid<=0 or type(tracer) is not int or tracer<=0:
        raise StagingUnavailable('exec_entry')
    root=Path('/proc')/str(pid)
    def read(path):
        with path.open('rb') as stream:data=stream.read(65537)
        if len(data)>65536:raise StagingUnavailable('exec_entry')
        return data.decode('ascii')
    def status():
        rows=dict(line.split(':',1) for line in read(root/'status').splitlines())
        fields=read(root/'stat').rsplit(')',1)[1].split()
        tasks=sorted(path.name for path in (root/'task').iterdir())
        if (fields[0]!='t' or int(rows['TracerPid'])!=tracer or int(rows['PPid'])!=tracer
                or tasks!=[str(pid)]):raise StagingUnavailable('exec_entry')
        return {'pid':pid,'birth':int(fields[19]),'tracer':int(rows['TracerPid']),
            'uids':[int(n) for n in rows['Uid'].split()],
            'gids':[int(n) for n in rows['Gid'].split()],
            'groups':sorted(int(n) for n in rows['Groups'].split()),
            'no_new_privs':int(rows['NoNewPrivs']),'seccomp':int(rows['Seccomp']),
            'seccomp_filters':int(rows['Seccomp_filters']),
            'capabilities':{key:int(rows[key],16) for key in ('CapEff','CapPrm','CapAmb')}}
    try:
        before=status();names=sorted(int(path.name) for path in (root/'fd').iterdir())
        if len(names)>64 or any(not 0<=number<=65535 for number in names):
            raise StagingUnavailable('exec_entry')
        rows=[]
        for number in names:
            info=os.stat(root/'fd'/str(number))
            detail=dict(line.split(':',1) for line in read(root/'fdinfo'/str(number)).splitlines())
            rows.append({'number':number,'device':info.st_dev,'inode':info.st_ino,
                'type':stat.S_IFMT(info.st_mode),'uid':info.st_uid,'gid':info.st_gid,
                'flags':int(detail['flags'].strip(),8),'position':int(detail['pos'])})
        if before!=status() or names!=sorted(int(path.name) for path in (root/'fd').iterdir()):
            raise StagingUnavailable('exec_entry')
        return {'version':1,'kind':'ci-unit-exec-entry',**before,'descriptors':names,'fd_table':rows,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
    except (OSError,ValueError,KeyError,IndexError,UnicodeError):
        raise StagingUnavailable('exec_entry') from None


def require_exec_entry(entry,*,uid,gid,groups,descriptors):
    """Check observed entry against trusted caller expectations, never grant it."""
    if (entry['uids']!=[uid]*4 or entry['gids']!=[gid]*4 or entry['groups']!=groups
            or entry['descriptors']!=sorted(descriptors) or len(set(descriptors))!=len(descriptors)
            or entry['no_new_privs']!=1 or entry['seccomp']!=2 or entry['seccomp_filters']<1
            or any(entry['capabilities'].values())):
        raise StagingUnavailable('exec_entry')


def observe_sealed_descriptor(pid,number):
    """Hash an actual sealed regular descriptor without touching its OFD.

    The caller must already own the original exec stop. O_NOATIME and positioned
    reads preserve the inherited stream's offset, flags and access timestamp.
    This is content/identity evidence, not provenance or policy authorization.
    """
    if (type(pid) is not int or pid<=0 or type(number) is not int or not 0<=number<=65535):
        raise StagingUnavailable('exec_entry_descriptor')
    descriptor=None
    def key(info):
        return (info.st_dev,info.st_ino,info.st_size,info.st_mode,info.st_uid,info.st_gid,
                info.st_nlink,info.st_mtime_ns,info.st_ctime_ns)
    try:
        path=Path('/proc')/str(pid)/'fd'/str(number);before=os.stat(path)
        if not stat.S_ISREG(before.st_mode) or not 0<=before.st_size<=MAX_BYTES:
            raise StagingUnavailable('exec_entry_descriptor')
        descriptor=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOATIME|os.O_NONBLOCK)
        if key(os.fstat(descriptor))!=key(before):raise StagingUnavailable('exec_entry_descriptor')
        seals=fcntl.fcntl(descriptor,fcntl.F_GET_SEALS)
        if seals&SEALS!=SEALS:raise StagingUnavailable('exec_entry_descriptor')
        digest=hashlib.sha256();offset=0
        while offset<before.st_size:
            block=os.pread(descriptor,min(65536,before.st_size-offset),offset)
            if not block:raise StagingUnavailable('exec_entry_descriptor')
            digest.update(block);offset+=len(block)
        if (key(os.fstat(descriptor))!=key(before) or key(os.stat(path))!=key(before)
                or fcntl.fcntl(descriptor,fcntl.F_GET_SEALS)!=seals):
            raise StagingUnavailable('exec_entry_descriptor')
        return {'device':before.st_dev,'inode':before.st_ino,'uid':before.st_uid,'gid':before.st_gid,
                'size_bytes':before.st_size,'sha256':digest.hexdigest(),'seals':seals}
    except OSError:raise StagingUnavailable('exec_entry_descriptor') from None
    finally:
        if descriptor is not None:os.close(descriptor)


def publish_policy_receipt(descriptor,value):
    """Seal a bootstrap observation in the original supervisor's empty memfd.

    Only the trusted bootstrap calls this, after successful native activation.
    The descriptor remains CLOEXEC and never reaches the untrusted tool.
    """
    try:
        info=os.fstat(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid()
                or info.st_nlink!=0 or info.st_size!=0
                or fcntl.fcntl(descriptor,fcntl.F_GET_SEALS)!=0):
            raise StagingUnavailable('policy_receipt')
        encoded=json.dumps(value,sort_keys=True,separators=(',',':')).encode()
        if not 1<=len(encoded)<=8192:raise StagingUnavailable('policy_receipt')
        if os.pwrite(descriptor,encoded,0)!=len(encoded):raise StagingUnavailable('policy_receipt')
        _seal(descriptor)
    except OSError:raise StagingUnavailable('policy_receipt') from None


def read_policy_receipt(descriptor,expected):
    """Read only a sealed receipt matching the original caller's full context."""
    try:
        info=os.fstat(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid() or info.st_nlink!=0
                or not 1<=info.st_size<=8192
                or fcntl.fcntl(descriptor,fcntl.F_GET_SEALS)&SEALS!=SEALS):
            raise StagingUnavailable('policy_receipt')
        value=json.loads(os.pread(descriptor,info.st_size+1,0),object_pairs_hook=_cache._unique_object)
        if type(value) is not dict or value!=expected:raise StagingUnavailable('policy_receipt')
        return value
    except (OSError,ValueError,UnicodeError):raise StagingUnavailable('policy_receipt') from None


def _seal(descriptor):
    fcntl.fcntl(descriptor,fcntl.F_ADD_SEALS,SEALS)


class CapturedStdin:
    """Local ownership of captured bytes, not an execution authorization."""
    def __init__(self,token,descriptor,size,digest):
        if token is not _TOKEN:raise StagingUnavailable('stdin_descriptor')
        self._fd=descriptor;self._pid=os.getpid();self._identity=os.fstat(descriptor)
        self._size=size;self._digest=digest

    def fileno(self):
        if self._pid!=os.getpid() or self._fd is None:raise StagingUnavailable('stdin_descriptor')
        try:
            current=os.fstat(self._fd)
            if ((current.st_dev,current.st_ino,current.st_size)!=(self._identity.st_dev,self._identity.st_ino,self._size)
                    or fcntl.fcntl(self._fd,fcntl.F_GET_SEALS)&SEALS!=SEALS):
                raise StagingUnavailable('stdin_descriptor')
        except OSError:raise StagingUnavailable('stdin_descriptor') from None
        return self._fd

    def observation(self):
        self.fileno()
        return {'version':3,'kind':'ci-unit-stdin','size_bytes':self._size,'sha256':self._digest,
                'input_closure_complete':False,'shipping_authority':False}

    def close(self):
        if self._fd is not None:
            descriptor=self.fileno();self._fd=None;os.close(descriptor)

    def __enter__(self):
        self.fileno();return self

    def __exit__(self,*unused):
        self.close()


def capture(source,cancellation,*,command_deadline):
    """Capture actual EOF to a sealed memfd under the original command deadline.

    source is the request's stdin descriptor, not a pathname. cancellation is a
    private read pipe owned by the original supervisor. Data, EOF, a hangup or an
    error on that pipe all prevent publication. Neither descriptor's file flags
    are changed. Cancellation remains the caller's obligation until tool start.
    OSError, including resource exhaustion, is preserved for the existing owner.
    """
    if (any(type(fd) is not int or fd<0 for fd in (source,cancellation))
            or source==cancellation or type(command_deadline) not in (int,float)
            or not math.isfinite(command_deadline)):
        raise StagingUnavailable('stdin_context')
    source_info=os.fstat(source);cancel_info=os.fstat(cancellation)
    if ((source_info.st_dev,source_info.st_ino)==(cancel_info.st_dev,cancel_info.st_ino)
            or not stat.S_ISFIFO(cancel_info.st_mode)
            or any(fcntl.fcntl(fd,fcntl.F_GETFL)&os.O_ACCMODE==os.O_WRONLY for fd in (source,cancellation))
            or not (stat.S_ISREG(source_info.st_mode) or stat.S_ISFIFO(source_info.st_mode)
                or stat.S_ISCHR(source_info.st_mode) and source_info.st_rdev==os.makedev(1,3))):
        raise StagingUnavailable('stdin_descriptor')
    deadline=min(command_deadline,time.monotonic()+MAX_SECONDS)
    events=select.POLLIN|select.POLLHUP|select.POLLERR|select.POLLNVAL
    cancel_poll=select.poll();cancel_poll.register(cancellation,events)
    poll=select.poll();poll.register(source,events);poll.register(cancellation,events)
    def check():
        if cancel_poll.poll(0):raise StagingUnavailable('stdin_cancelled')
        if time.monotonic()>=deadline:raise StagingUnavailable('stdin_deadline')
    check()
    descriptor=os.memfd_create('jarvis-unit-stdin',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
    try:
        size=0;digest=hashlib.sha256()
        while True:
            check()
            ready=dict(poll.poll(max(1,min(50,math.ceil((deadline-time.monotonic())*1000)))))
            check()  # Cancellation wins even when source EOF is ready too.
            if source not in ready:continue
            if ready[source]&(select.POLLERR|select.POLLNVAL):raise StagingUnavailable('stdin_descriptor')
            try:block=os.read(source,min(65536,MAX_BYTES-size+1))
            except (BlockingIOError,InterruptedError):continue
            if not block:break
            size+=len(block)
            if size>MAX_BYTES:raise StagingUnavailable('stdin_limit')
            digest.update(block);remaining=memoryview(block)
            while remaining:
                check()
                try:written=os.write(descriptor,remaining)
                except InterruptedError:continue
                if written<=0:raise OSError(errno.EIO,'stdin capture write failed')
                remaining=remaining[written:]
        check();os.lseek(descriptor,0,os.SEEK_SET);_seal(descriptor);check()
        result=CapturedStdin(_TOKEN,descriptor,size,digest.hexdigest());descriptor=None
        return result
    finally:
        if descriptor is not None:os.close(descriptor)


def normalize_jobserver(environment,inherited):
    """Plan stable worker FDs for the exact received Cargo jobserver pipes.

    inherited maps the original request's descriptor numbers to their received
    CLOEXEC copies. Only the two bound ends are accepted. The caller must save
    control descriptors before applying this mapping after credential drop.
    Kernel pipe identity is transport evidence, not a compiler input digest.
    """
    if (type(environment) is not dict or type(inherited) is not dict
            or len(environment)>4096 or len(inherited)>2
            or any(type(key) is not bytes or type(value) is not bytes or not key
                or b'=' in key or b'\0' in key or b'\0' in value
                or len(key)+len(value)>1024**2 for key,value in environment.items())
            or sum(len(key)+len(value)+2 for key,value in environment.items())>MAX_BYTES
            or any(type(number) is not int or not 3<=number<=65535
                or type(descriptor) is not int or descriptor<3 for number,descriptor in inherited.items())):
        raise StagingUnavailable('jobserver_binding')
    normalized=dict(environment);pair=None;cargo_binding=False
    for key in (b'CARGO_MAKEFLAGS',b'MAKEFLAGS'):
        if key not in normalized:continue
        pieces=re.split(rb'(\s+)',normalized[key]);seen=set()
        for index,piece in enumerate(pieces):
            if b'--jobserver-' not in piece:continue
            match=re.fullmatch(rb'--jobserver-(auth|fds)=([1-9][0-9]{0,4}),([1-9][0-9]{0,4})',piece)
            if match is None or match[1] in seen:raise StagingUnavailable('jobserver_binding')
            seen.add(match[1]);current=(int(match[2]),int(match[3]))
            if (current[0]==current[1] or any(not 3<=number<=65535 for number in current)
                    or pair is not None and pair!=current):raise StagingUnavailable('jobserver_binding')
            pair=current;cargo_binding|=key==b'CARGO_MAKEFLAGS'
            pieces[index]=b'--jobserver-'+match[1]+b'=3,4'
        normalized[key]=b''.join(pieces)
    if pair is None:
        if inherited:raise StagingUnavailable('jobserver_binding')
        return normalized,(),None
    if not cargo_binding or set(inherited)!=set(pair):raise StagingUnavailable('jobserver_binding')
    reader,writer=(inherited[number] for number in pair)
    read_info,write_info=os.fstat(reader),os.fstat(writer)
    if (not stat.S_ISFIFO(read_info.st_mode) or not stat.S_ISFIFO(write_info.st_mode)
            or (read_info.st_dev,read_info.st_ino)!=(write_info.st_dev,write_info.st_ino)
            or fcntl.fcntl(reader,fcntl.F_GETFL)&os.O_ACCMODE!=os.O_RDONLY
            or fcntl.fcntl(writer,fcntl.F_GETFL)&os.O_ACCMODE!=os.O_WRONLY
            or any(not fcntl.fcntl(fd,fcntl.F_GETFD)&fcntl.FD_CLOEXEC for fd in (reader,writer))):
        raise StagingUnavailable('jobserver_binding')
    pipe=hashlib.sha256(f'{read_info.st_dev}:{read_info.st_ino}'.encode('ascii')).hexdigest()
    return normalized,((3,reader),(4,writer)),{'version':3,'kind':'ci-unit-jobserver',
        'kernel_pipe_sha256':pipe,'worker_numbers':[3,4],
        'input_closure_complete':False,'shipping_authority':False}


def observe_jobserver(environment,inherited):
    """Sample pending bytes without reading tokens or changing pipe flags.

    This is an instant observation, not exclusive-writer or credit ownership
    proof. The original caller must bind it to its paused unit and grant.
    """
    _,mapping,job=normalize_jobserver(environment,inherited)
    if job is None or len(mapping)!=2:raise StagingUnavailable('jobserver_binding')
    reader,writer=(row[1] for row in mapping)
    before=[os.fstat(fd) for fd in (reader,writer)]
    flags=[fcntl.fcntl(fd,fcntl.F_GETFL) for fd in (reader,writer)]
    count=array.array('i',[0]);fcntl.ioctl(reader,termios.FIONREAD,count,True)
    if not 0<=count[0]<=MAX_BYTES:raise StagingUnavailable('jobserver_binding')
    after=[os.fstat(fd) for fd in (reader,writer)]
    if (any((a.st_dev,a.st_ino,a.st_mode)!=(b.st_dev,b.st_ino,b.st_mode) for a,b in zip(before,after))
            or [fcntl.fcntl(fd,fcntl.F_GETFL) for fd in (reader,writer)]!=flags):
        raise StagingUnavailable('jobserver_binding')
    return {'version':1,'kind':'ci-jobserver-observation','kernel_pipe_sha256':job['kernel_pipe_sha256'],
        'available_bytes':count[0],'reader_flags':flags[0],'writer_flags':flags[1],
        'input_closure_complete':False,'shipping_authority':False}


if __name__=='__main__':raise SystemExit(125)  # No public or environment-selected launcher.
