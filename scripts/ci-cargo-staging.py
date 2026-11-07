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
import struct
import sys
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


def observe_uid_cohort(uid,expected,*,check):
    """Census actual task identities, not a freeze or exclusive-writer grant.

    The caller owns the original stopped cohort, non-reused UID allocation,
    namespace visibility and mutation lock. Both procfs passes must find
    exactly those task births, including threads with a different leader UID.
    """
    if (type(uid) is not int or not 0<=uid<2**32-1 or type(expected) is not list
            or len(expected)>512 or not callable(check)):
        raise StagingUnavailable('uid_cohort_identity')
    tasks={}
    for row in expected:
        if (type(row) is not dict or set(row)!={'pid','tid','birth'}
                or any(type(row[key]) is not int or row[key]<(0 if key=='birth' else 1) for key in row)
                or row['tid'] in tasks):raise StagingUnavailable('uid_cohort_identity')
        tasks[row['tid']]=dict(row)
    def read(path):
        with path.open('rb') as stream:body=stream.read(65537)
        if len(body)>65536:raise StagingUnavailable('uid_cohort_unobservable')
        return body
    def identity(pid,tid):
        root=Path('/proc')/str(pid)/'task'/str(tid)
        def observe():
            fields=read(root/'stat').rsplit(b')',1)[1].split()
            status=dict(line.split(b':',1) for line in read(root/'status').splitlines())
            ids=[int(n) for n in status[b'Uid'].split()]
            if len(ids)!=4 or int(status[b'Pid'])!=tid or int(status[b'Tgid'])!=pid:
                raise StagingUnavailable('uid_cohort_identity')
            return {'pid':pid,'tid':tid,'birth':int(fields[19])},ids
        before=observe();after=observe()
        if before!=after and (tid in tasks or uid in before[1] or uid in after[1]):
            raise StagingUnavailable('uid_cohort_identity')
        return after
    try:
        namespace=os.readlink('/proc/self/ns/pid')
        for row in tasks.values():
            check();observed,ids=identity(row['pid'],row['tid'])
            if observed!=row or ids!=[uid]*4:raise StagingUnavailable('uid_cohort_identity')
        def scan():
            seen={};count=0
            with os.scandir('/proc') as processes:
                for process in processes:
                    if not process.name.isdecimal():continue
                    check();count+=1
                    if count>100000:raise StagingUnavailable('uid_cohort_bound')
                    try:
                        with os.scandir(Path(process.path)/'task') as members:
                            for task in members:
                                check();count+=1
                                if count>100000:raise StagingUnavailable('uid_cohort_bound')
                                try:row,ids=identity(int(process.name),int(task.name))
                                except FileNotFoundError:continue
                                if uid not in ids:continue
                                if ids!=[uid]*4 or tasks.get(row['tid'])!=row:
                                    raise StagingUnavailable('uid_cohort_outside')
                                seen[row['tid']]=row
                    except FileNotFoundError:continue
            if seen!=tasks:raise StagingUnavailable('uid_cohort_identity')
            return sorted(seen.values(),key=lambda row:(row['pid'],row['tid']))
        observed=scan()
        if scan()!=observed or namespace!=os.readlink('/proc/self/ns/pid'):
            raise StagingUnavailable('uid_cohort_identity')
        return {'version':1,'kind':'ci-unit-uid-cohort','uid':uid,'tasks':observed,
            'pid_namespace':namespace,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
    except (OSError,ValueError,KeyError,IndexError):
        raise StagingUnavailable('uid_cohort_unobservable') from None


def observe_task_inode_aliases(task,inodes,*,check):
    """Observe one actual task's references to caller-held inode identities.

    Includes readonly/directory capabilities and mappings after FD close. This
    never proves a stopped population, global alias absence or writer closure.
    The caller must bind task identity, visibility, allocation and quiescence.
    No target FD or mapped byte is read, and no shared offset is changed.
    """
    if (type(task) is not dict or set(task)!={'pid','tid','birth'}
            or any(type(task[key]) is not int or task[key]<(0 if key=='birth' else 1) for key in task)
            or type(inodes) not in (set,frozenset) or len(inodes)>100000 or not callable(check)
            or any(type(key) is not tuple or len(key)!=2 or any(type(n) is not int or n<0 for n in key)
                or key[1]==0 for key in inodes)):
        raise StagingUnavailable('inode_alias_identity')
    root=Path('/proc')/str(task['pid'])/'task'/str(task['tid'])
    def read(path):
        check()
        with path.open('rb') as stream:body=stream.read(MAX_BYTES+1)
        if len(body)>MAX_BYTES:raise StagingUnavailable('inode_alias_bound')
        return body
    def identity():
        fields=read(root/'stat').rsplit(b')',1)[1].split()
        status=dict(line.split(b':',1) for line in read(root/'status').splitlines())
        actual={'pid':int(status[b'Tgid']),'tid':int(status[b'Pid']),'birth':int(fields[19])}
        ids={key:[int(n) for n in status[key.encode()].split()] for key in ('Uid','Gid')}
        if actual!=task or any(len(values)!=4 for values in ids.values()):
            raise StagingUnavailable('inode_alias_identity')
        return ids
    def scan():
        aliases=[]
        with os.scandir(root/'fd') as entries:names=sorted(int(entry.name) for entry in entries)
        if len(names)>65536 or any(not 0<=n<=65535 for n in names):raise StagingUnavailable('inode_alias_bound')
        for number in names:
            check();path=root/'fd'/str(number)
            try:info=path.stat()
            except FileNotFoundError:continue
            key=(info.st_dev,info.st_ino)
            if key not in inodes:continue
            if not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)):
                raise StagingUnavailable('inode_alias_identity')
            lines=read(root/'fdinfo'/str(number)).splitlines()
            details=dict(line.split(b':',1) for line in lines);locks=[]
            for line in lines:
                if not line.startswith(b'lock:'):continue
                fields=line.split(b':',1)[1].split();blocked=len(fields)>1 and fields[1]==b'->'
                if blocked:fields.pop(1)
                if (len(fields)!=8 or fields[1] not in (b'FLOCK',b'POSIX',b'OFDLCK',b'LEASE')
                        or fields[2] not in (b'ADVISORY',b'MANDATORY',b'ACTIVE',b'BREAKING')
                        or fields[3] not in (b'READ',b'WRITE')):
                    raise StagingUnavailable('inode_alias_kernel_lock')
                major,minor,inode=fields[5].split(b':')
                if (os.makedev(int(major,16),int(minor,16)),int(inode))!=key:
                    raise StagingUnavailable('inode_alias_kernel_lock')
                locks.append({'kind':fields[1].decode('ascii'),'scope':fields[2].decode('ascii'),
                    'access':fields[3].decode('ascii'),'owner':int(fields[4]),'start':int(fields[6]),
                    'end':None if fields[7]==b'EOF' else int(fields[7]),'blocked':blocked})
            after=path.stat()
            if (after.st_dev,after.st_ino,after.st_mode)!=(info.st_dev,info.st_ino,info.st_mode):
                raise StagingUnavailable('inode_alias_changed')
            aliases.append({'kind':'fd','number':number,'device':key[0],'inode':key[1],
                'flags':int(details[b'flags'],8),'position':int(details[b'pos']),
                'type':stat.S_IFMT(info.st_mode),'locks':locks})
        for line in read(root/'maps').splitlines():
            fields=line.split(None,5)
            if len(fields)<5:raise StagingUnavailable('inode_alias_unobservable')
            major,minor=fields[3].split(b':');key=(os.makedev(int(major,16),int(minor,16)),int(fields[4]))
            if key not in inodes:continue
            bounds=fields[0].split(b'-');access=fields[1]
            if (len(bounds)!=2 or len(access)!=4 or access[0:1] not in (b'r',b'-')
                    or access[1:2] not in (b'w',b'-') or access[2:3] not in (b'x',b'-')
                    or access[3:4] not in (b's',b'p')):raise StagingUnavailable('inode_alias_unobservable')
            start,end=(int(value,16) for value in bounds)
            if not 0<=start<end:raise StagingUnavailable('inode_alias_identity')
            aliases.append({'kind':'mapping','start':start,'end':end,'device':key[0],'inode':key[1],
                'writable':access[1:2]==b'w','shared':access[3:4]==b's'})
        for name in ('cwd','root','exe'):
            check()
            try:info=root.joinpath(name).stat()
            except FileNotFoundError:continue
            key=(info.st_dev,info.st_ino)
            if key in inodes:aliases.append({'kind':name,'device':key[0],'inode':key[1]})
        if len(aliases)>100000:raise StagingUnavailable('inode_alias_bound')
        return aliases
    try:
        before=identity();aliases=scan()
        if identity()!=before or scan()!=aliases or identity()!=before:
            raise StagingUnavailable('inode_alias_changed')
        check()
        return {'version':1,'kind':'ci-task-inode-aliases','task':dict(task),
            'uids':before['Uid'],'gids':before['Gid'],'aliases':aliases,
            'alias_closure_complete':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
    except (OSError,ValueError,KeyError,IndexError,OverflowError):
        raise StagingUnavailable('inode_alias_unobservable') from None


def observe_private_inode_aliases(inodes,cohort,controller_fds,*,check):
    """Root's bounded negative check over the guest's actual task population.

    Original private allocation, initial FD closure, namespaces, policy, pause
    and controller serialization remain caller-owned prerequisites. A procfs
    observation does not establish complete kernel-resource/writer authority.
    """
    if (os.getresuid()!=(0,0,0) or os.getresgid()!=(0,0,0) or not callable(check)
            or type(inodes) not in (set,frozenset) or len(inodes)>100000
            or any(type(key) is not tuple or len(key)!=2 or any(type(n) is not int or n<0 for n in key)
                or key[1]==0 for key in inodes)
            or type(cohort) is not dict or cohort.get('kind')!='ci-unit-uid-cohort'
            or type(cohort.get('version')) is not int or cohort['version']!=1 or type(cohort.get('uid')) is not int
            or type(cohort.get('tasks')) is not list or len(cohort['tasks'])>512
            or any(type(row) is not dict or set(row)!={'pid','tid','birth'}
                or any(type(row[key]) is not int or row[key]<(0 if key=='birth' else 1) for key in row)
                for row in cohort['tasks'])
            or any(cohort.get(key) is not False for key in ('input_closure_complete','full_ci','shipping_authority'))
            or type(controller_fds) is not list or len(controller_fds)>100000
            or any(type(fd) is not int or not 0<=fd<=65535 for fd in controller_fds)
            or len(set(controller_fds))!=len(controller_fds)):
        raise StagingUnavailable('inode_alias_identity')
    namespace=os.readlink('/proc/self/ns/pid')
    if os.readlink('/proc/1/ns/pid')!=namespace or cohort.get('pid_namespace')!=namespace:
        raise StagingUnavailable('inode_alias_visibility')
    expected={(row['pid'],row['tid'],row['birth']) for row in cohort['tasks']}
    if len(expected)!=len(cohort['tasks']):raise StagingUnavailable('inode_alias_identity')
    controlled={}
    for fd in controller_fds:
        check();info=os.fstat(fd);key=(info.st_dev,info.st_ino)
        if key not in inodes:raise StagingUnavailable('inode_alias_identity')
        controlled[fd]=key
    # This also reobserves the original UID allocation, not merely a supplied
    # list of task IDs. It never turns the census into a freeze capability.
    if observe_uid_cohort(cohort['uid'],cohort['tasks'],check=check)!=cohort:
        raise StagingUnavailable('inode_alias_identity')
    observations=[];seen=set();visited=0;encoded_bytes=0
    try:
        with os.scandir('/proc') as processes:
            for process in processes:
                if not process.name.isdecimal():continue
                check();visited+=1
                if visited>100000:raise StagingUnavailable('inode_alias_bound')
                try:
                    with os.scandir(Path(process.path)/'task') as tasks:
                        for task in tasks:
                            if not task.name.isdecimal():continue
                            check();visited+=1
                            if visited>100000:raise StagingUnavailable('inode_alias_bound')
                            try:
                                with (Path(task.path)/'stat').open('rb') as stream:body=stream.read(65537)
                                if len(body)>65536:raise StagingUnavailable('inode_alias_bound')
                                birth=int(body.rsplit(b')',1)[1].split()[19])
                                identity={'pid':int(process.name),'tid':int(task.name),'birth':birth}
                                observation=observe_task_inode_aliases(identity,inodes,check=check)
                            except FileNotFoundError:continue
                            token=(identity['pid'],identity['tid'],identity['birth'])
                            parent=token in expected
                            if parent:
                                seen.add(token)
                                if observation['uids']!=[cohort['uid']]*4:
                                    raise StagingUnavailable('inode_alias_identity')
                            for alias in observation['aliases']:
                                if alias['kind']=='fd' and alias['locks']:
                                    error=StagingUnavailable('inode_alias_kernel_lock')
                                    error.alias_reference={'task':identity,'uids':observation['uids'],'alias':alias}
                                    raise error
                                if parent:continue
                                if (identity['pid']==os.getpid() and alias['kind']=='fd'
                                        and controlled.get(alias['number'])==(alias['device'],alias['inode'])):continue
                                error=StagingUnavailable('inode_alias_outside')
                                error.alias_reference={'task':identity,'uids':observation['uids'],'alias':alias}
                                raise error
                            if observation['aliases']:
                                encoded_bytes+=len(json.dumps(observation,separators=(',',':')).encode())
                                if encoded_bytes>16*1024**2:raise StagingUnavailable('inode_alias_bound')
                                observations.append(observation)
                except FileNotFoundError:continue
        if seen!=expected or observe_uid_cohort(cohort['uid'],cohort['tasks'],check=check)!=cohort:
            raise StagingUnavailable('inode_alias_identity')
        for fd,key in controlled.items():
            check();info=os.fstat(fd)
            if (info.st_dev,info.st_ino)!=key:raise StagingUnavailable('inode_alias_identity')
        if namespace!=os.readlink('/proc/self/ns/pid'):raise StagingUnavailable('inode_alias_visibility')
        return {'version':1,'kind':'ci-private-inode-alias-census','pid_namespace':namespace,
            'cohort_sha256':hashlib.sha256(json.dumps(cohort,sort_keys=True,separators=(',',':')).encode()).hexdigest(),
            'inodes_checked':len(inodes),'controller_descriptions':len(controlled),
            'observations':observations,'alias_closure_complete':False,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
    except (OSError,ValueError,KeyError,TypeError,IndexError):
        raise StagingUnavailable('inode_alias_unobservable') from None


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


def listener_relay_body(body):
    """Data-only canonical form for an already listener-protected bootstrap.

    This tag does not establish policy installation or authorize any exec.
    Ordinary V3/V2 callers keep the constructor's duplicate exec denial.
    """
    tags={b'JARVWRK3':b'JARVWRL3',b'JARVCFG3':b'JARVCFL3',b'JARVUNT3':b'JARVUNL3'}
    if type(body) is not bytes or len(body)!=160 or body[:8] not in tags:
        raise StagingUnavailable('worker_entry_configuration')
    return tags[body[:8]]+body[8:]


def observe_relay_configuration(pid,number,*,listener_owned=False):
    """Verify exact original or canonical bootstrap bytes at original exec-stop.

    Only the original supervisor may select listener_owned after its own kernel
    listener transfer/exec observation. Matching bytes alone are not that proof.
    """
    original=observe_sealed_descriptor(os.getpid(),number)
    actual=observe_sealed_descriptor(pid,number)
    if not listener_owned:
        if actual!=original or actual['uid']!=os.geteuid() or actual['gid']!=os.getegid():
            raise StagingUnavailable('worker_entry_configuration')
    else:
        if os.getresuid()!=(0,0,0) or os.getresgid()!=(0,0,0):
            raise StagingUnavailable('worker_entry_configuration')
        body=os.pread(number,161,0)
        expected=listener_relay_body(body)
        if (original['uid']!=0 or original['gid']!=0 or original['size_bytes']!=160
                or original['sha256']!=hashlib.sha256(body).hexdigest()
                or actual['uid']!=0 or actual['gid']!=0 or actual['size_bytes']!=160
                or actual['sha256']!=hashlib.sha256(expected).hexdigest()):
            raise StagingUnavailable('worker_entry_configuration')
    return actual


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


def observe_worker_entry(pid,configuration,receipt,expected_policy,*,uid,gid,groups,tracer=None,listener_owned=False):
    """Original supervisor observation, without a runnable or cleanup grant."""
    entry=observe_exec_entry(pid,tracer=tracer)
    require_exec_entry(entry,uid=uid,gid=gid,groups=groups,descriptors=[0,1,2,configuration])
    entry['policy_activation']=read_policy_receipt(receipt,expected_policy)
    entry['sealed_relay']=observe_relay_configuration(pid,configuration,listener_owned=listener_owned)
    table={row['number']:row for row in entry['fd_table']}
    for number in (0,1,2):
        info=os.fstat(number);row=table[number]
        if (row['device'],row['inode'],row['type'],row['flags']&os.O_ACCMODE)!=(
                info.st_dev,info.st_ino,stat.S_IFMT(info.st_mode),fcntl.fcntl(number,fcntl.F_GETFL)&os.O_ACCMODE):
            raise StagingUnavailable('worker_entry_stream')
        if (number==0 and (not stat.S_ISCHR(info.st_mode) or info.st_rdev!=os.makedev(1,3))
                or number in (1,2) and not stat.S_ISFIFO(info.st_mode)):
            raise StagingUnavailable('worker_entry_stream')
    return entry


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


EXECUTABLE_MAX_BYTES=2*1024**2
EXECUTABLE_SEALS=SEALS|0x20 # Linux F_SEAL_EXEC; unsupported kernels must refuse.


def _executable_key(info):
    return (info.st_dev,info.st_ino,info.st_mode,info.st_uid,info.st_gid,
        info.st_nlink,info.st_size,info.st_mtime_ns,info.st_ctime_ns)


def _static_executable(descriptor,size):
    header=os.pread(descriptor,64,0)
    if (len(header)!=64 or header[:7]!=b'\x7fELF\x02\x01\x01'
            or header[7] not in (0,3) or header[8]!=0):raise StagingUnavailable('executable_format')
    kind,machine,version,entry,offset,_,_,header_size,step,count,_,_,_=struct.unpack_from('<HHIQQQIHHHHHH',header,16)
    if (kind!=2 or machine!=62 or version!=1 or header_size!=64 or step!=56
            or not 0<count<=128 or offset<64 or offset+count*step>size):
        raise StagingUnavailable('executable_format')
    table=os.pread(descriptor,count*step,offset)
    if len(table)!=count*step:raise StagingUnavailable('executable_format')
    executable_entry=False
    for at in range(0,len(table),step):
        program,flags,start,address,_,file_size,memory_size,alignment=struct.unpack_from('<IIQQQQQQ',table,at)
        if program in (2,3) or program==0x6474e551 and flags&1:
            raise StagingUnavailable('executable_format') # No dynamic loader or executable stack.
        if program!=1:continue
        if (file_size>memory_size or start+file_size>size or address+memory_size>=2**64
                or flags&3==3 or alignment>1 and (alignment&(alignment-1) or address%alignment!=start%alignment)):
            raise StagingUnavailable('executable_format')
        if flags&1 and address<=entry<address+memory_size:executable_entry=True
    if not executable_entry:raise StagingUnavailable('executable_format')


class CapturedExecutable:
    """Locally owned immutable bytes/execute bits, not trusted-source admission.

    The future runtime consumer must separately bind the original trusted asset,
    request, root peer, notification, writer closure and live resource ownership.
    Neither this object nor its descriptive observation authorizes an exec.
    """
    def __init__(self,token,descriptor,size,digest):
        if token is not _TOKEN:raise StagingUnavailable('executable_descriptor')
        self._fd=descriptor;self._pid=os.getpid();self._pin=os.dup(descriptor)
        try:self._identity=os.fstat(self._pin)
        except BaseException:os.close(self._pin);raise
        self._size=size;self._digest=digest

    def fileno(self):
        if self._pid!=os.getpid() or self._fd is None:raise StagingUnavailable('executable_descriptor')
        try:
            info=os.fstat(self._fd);pin=os.fstat(self._pin)
            if (not stat.S_ISREG(info.st_mode) or info.st_nlink!=0 or stat.S_IMODE(info.st_mode)!=0o555
                    or (pin.st_dev,pin.st_ino)!=(self._identity.st_dev,self._identity.st_ino)
                    or not fcntl.fcntl(self._pin,fcntl.F_GETFD)&fcntl.FD_CLOEXEC
                    or (info.st_dev,info.st_ino,info.st_uid,info.st_gid,info.st_size)
                        !=(self._identity.st_dev,self._identity.st_ino,self._identity.st_uid,self._identity.st_gid,self._size)
                    or fcntl.fcntl(self._fd,fcntl.F_GET_SEALS)&EXECUTABLE_SEALS!=EXECUTABLE_SEALS):
                raise StagingUnavailable('executable_descriptor')
        except OSError:raise StagingUnavailable('executable_descriptor') from None
        return self._fd

    def observation(self):
        descriptor=self.fileno();info=os.fstat(descriptor)
        return {'version':1,'kind':'ci-captured-static-executable','device':info.st_dev,'inode':info.st_ino,
            'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),'size_bytes':self._size,
            'sha256':self._digest,'seals':fcntl.fcntl(descriptor,fcntl.F_GET_SEALS),'source_authority':False,
            'execution_authority':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False}

    def close(self):
        if self._fd is not None:
            if self._pid!=os.getpid():raise StagingUnavailable('executable_descriptor')
            try:info=os.fstat(self._fd);pin=os.fstat(self._pin)
            except OSError:raise StagingUnavailable('executable_descriptor') from None
            # Pin the actual inode until cleanup. Validation failure must not
            # strand an owned FD, and FD-slot substitution must not close a
            # foreign object. This close is not descendant/resource teardown.
            if ((info.st_dev,info.st_ino)!=(self._identity.st_dev,self._identity.st_ino)
                    or (pin.st_dev,pin.st_ino)!=(self._identity.st_dev,self._identity.st_ino)):
                raise StagingUnavailable('executable_descriptor')
            descriptor=self._fd;pin_descriptor=self._pin;self._fd=self._pin=None
            try:os.close(descriptor)
            finally:os.close(pin_descriptor)

    def __enter__(self):self.fileno();return self
    def __exit__(self,*unused):self.close()
    def __copy__(self):raise StagingUnavailable('executable_descriptor')
    def __deepcopy__(self,memo):raise StagingUnavailable('executable_descriptor')
    def __reduce_ex__(self,protocol):raise StagingUnavailable('executable_descriptor')


def capture_executable(source):
    """Copy a borrowed readonly static ELF FD into a sealed executable memfd.

    Size and observed drift are bounded here. Original source provenance,
    controller deadline/cancellation and resource lifetime remain caller-owned.
    There is no production caller or environment-selected executable route.
    """
    if (sys.platform!='linux' or os.uname().machine!='x86_64' or type(source) is not int
            or not 0<=source<=65535):raise StagingUnavailable('executable_descriptor')
    descriptor=None
    try:
        before=os.fstat(source);flags=fcntl.fcntl(source,fcntl.F_GETFL)
        if (not stat.S_ISREG(before.st_mode) or not 64<=before.st_size<=EXECUTABLE_MAX_BYTES
                or flags&os.O_ACCMODE!=os.O_RDONLY):raise StagingUnavailable('executable_descriptor')
        descriptor=os.memfd_create('ci-static-executable',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING|0x10) # MFD_EXEC
        offset=0;digest=hashlib.sha256()
        while offset<before.st_size:
            block=os.pread(source,min(65536,before.st_size-offset),offset)
            if not block:raise StagingUnavailable('executable_changed')
            digest.update(block);offset+=len(block);written=0
            while written<len(block):
                count=os.write(descriptor,block[written:])
                if count<=0:raise StagingUnavailable('executable_descriptor')
                written+=count
        if (os.pread(source,1,before.st_size) or _executable_key(os.fstat(source))!=_executable_key(before)
                or fcntl.fcntl(source,fcntl.F_GETFL)!=flags):raise StagingUnavailable('executable_changed')
        # The eventual root owner must be able to hand this anonymous image to
        # a non-root worker. Execute bits do not grant runtime authorization.
        os.fchmod(descriptor,0o555)
        fcntl.fcntl(descriptor,fcntl.F_ADD_SEALS,EXECUTABLE_SEALS)
        _static_executable(descriptor,before.st_size)
        result=CapturedExecutable(_TOKEN,descriptor,before.st_size,digest.hexdigest())
        descriptor=None
        try:result.fileno()
        except BaseException:result.close();raise
        return result
    except (OSError,AttributeError,struct.error):raise StagingUnavailable('executable_descriptor') from None
    finally:
        if descriptor is not None:os.close(descriptor)


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
