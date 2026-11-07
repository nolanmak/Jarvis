#!/usr/bin/env python3
"""Private kernel-copied pathname feasibility probe, not an input-proof API.

Only selected lookup spellings and getdents64 actor results are observed. The
sixteen original kernel attachments require a bounded miss census. This does not establish full
syscall/metadata/device coverage, compatibility or reuse authority. It does not
emulate or change the target's system calls.
"""
import ctypes
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import stat
import struct
import subprocess
import sys
import time
import uuid

spec=importlib.util.spec_from_file_location('observer_cache',Path(__file__).with_name('ci-cache-generation.py'))
cache=importlib.util.module_from_spec(spec);spec.loader.exec_module(cache)
MAX_BYTES=4*1024**2
# Name and result records use separate probes to avoid copying map strings on
# the BPF exit probe stack. The terminator slot never proves a complete name.
PATH_BYTES=4096
CHUNK_BYTES=63
SYSCALLS={2,4,6,21,89,257,262,267,269,332,439}
PROBE_ROLES={('getname_flags',1):2,('iterate_dir',0):1,('iterate_dir',1):1,
    ('filldir64',0):1,('filldir64',1):1,('vfs_fstat',0):1,('vfs_fstat',1):1,
    ('cp_new_stat',0):1,('cp_new_stat',1):1,('cp_statx',0):1,('cp_statx',1):1,
    ('path_init',0):1,('path_init',1):1}
PROBE_COUNT=2+sum(PROBE_ROLES.values())


class ObservationUnavailable(RuntimeError):
    pass


def startup_errno(error):
    errors=b'\n'.join(line for line in error.lower().splitlines()
        if not line.startswith(b'stdin:') and b'warning:' not in line)
    if b'bpf stack limit is exceeded' in errors:return errno.E2BIG
    if b'bpf program is too large' in errors:return errno.E2BIG
    if b'operation not permitted' in errors:return errno.EPERM
    if b'not found' in errors or b'no such file or directory' in errors:return errno.ENOENT
    return errno.EINVAL


def probe_roles(path_scope):
    if path_scope not in ('fixture','all'):raise ObservationUnavailable('unknown pathname scope')
    return PROBE_ROLES if path_scope=='fixture' else {**PROBE_ROLES,('do_file_open',0):1,('fd_install',0):1}


def validate_census(value,*,path_scope='fixture'):
    expected_roles=probe_roles(path_scope);count=2+sum(expected_roles.values())
    if type(value) is not dict or set(value)!={'before','after'}:
        raise ObservationUnavailable('kernel census unavailable')
    for sample in value.values():
        if (type(sample) is not dict or set(sample)!={'pid','birth','programs','links'}
                or type(sample['pid']) is not int or not 0<sample['pid']<2**31
                or type(sample['birth']) is not int or not 0<sample['birth']<2**64
                or type(sample['programs']) is not list or len(sample['programs'])!=count
                or type(sample['links']) is not list or len(sample['links'])!=count):
            raise ObservationUnavailable('original kernel objects unavailable')
        programs={};links=set();raw=set();linked=set();roles={}
        for program in sample['programs']:
            if (type(program) is not dict or set(program)!={'id','type','tag','recursion_misses'}
                    or type(program['id']) is not int or not 0<program['id']<2**31 or program['id'] in programs
                    or type(program['type']) is not int or program['type'] not in (2,17)
                    or type(program['tag']) is not str or re.fullmatch(r'[a-f0-9]{16}',program['tag']) is None
                    or type(program['recursion_misses']) is not int or program['recursion_misses']!=0):
                raise ObservationUnavailable('kernel program recursion census failed')
            programs[program['id']]=program
        for link in sample['links']:
            if (type(link) is not dict or not {'id','type','prog_id','tag'}<=set(link)
                    or type(link['id']) is not int or not 0<link['id']<2**31 or link['id'] in links
                    or type(link['prog_id']) is not int or link['prog_id'] not in programs or link['prog_id'] in linked
                    or link['tag']!=programs[link['prog_id']]['tag']):
                raise ObservationUnavailable('original kernel attachment changed')
            links.add(link['id']);linked.add(link['prog_id'])
            if link['type']=='raw_tracepoint':
                if (set(link)!={'id','type','prog_id','tag','tracepoint'}
                        or link['tracepoint'] not in ('sys_enter','sys_exit') or link['tracepoint'] in raw
                        or programs[link['prog_id']]['type']!=17):
                    raise ObservationUnavailable('original syscall attachment unavailable')
                raw.add(link['tracepoint'])
            elif link['type'] in ('kprobe_multi','kretprobe_multi'):
                if (set(link)!={'id','type','prog_id','tag','function','count','flags','missed'}
                        or type(link['function']) is not str or programs[link['prog_id']]['type']!=2
                        or any(type(link[key]) is not int for key in ('count','flags','missed'))
                        or (link['count'],link['missed'])!=(1,0)
                        or link['flags']!=(1 if link['type']=='kretprobe_multi' else 0)
                        or (link['function'],link['flags']) not in expected_roles):
                    raise ObservationUnavailable('kernel return probe census failed')
                role=(link['function'],link['flags']);roles[role]=roles.get(role,0)+1
            else:raise ObservationUnavailable('unknown kernel attachment')
        if raw!={'sys_enter','sys_exit'} or roles!=expected_roles or linked!=set(programs):
            raise ObservationUnavailable('kernel attachment census incomplete')
    if value['before']!=value['after']:
        raise ObservationUnavailable('kernel census identity or counters changed')
    return value


def _fdinfo(data):
    if len(data)>16384:raise ObservationUnavailable('unbounded kernel descriptor metadata')
    try:text=data.decode('ascii')
    except UnicodeError:raise ObservationUnavailable('unknown kernel descriptor encoding') from None
    result={};lines=text.splitlines();table=False
    for ordinal,line in enumerate(lines):
        if re.fullmatch(r'cookie[ \t]+func',line):
            rows=lines[ordinal+1:]
            if (result.get('link_type') not in ('kprobe_multi','kretprobe_multi') or result.get('kprobe_cnt')!='1'
                    or len(rows)!=1 or re.fullmatch(r'0[ \t]+(getname_flags|iterate_dir|filldir64|vfs_fstat|cp_new_stat|cp_statx|path_init|do_file_open|fd_install)\+0x0/0x[1-9a-f][0-9a-f]{0,15}',rows[0]) is None):
                raise ObservationUnavailable('unknown kernel function table')
            result['function']=rows[0].split()[1].split('+',1)[0]
            table=True;break
        match=re.fullmatch(r'([a-z0-9_]+):[ \t]*(.*)',line)
        if match is None or match[1] in result:raise ObservationUnavailable('ambiguous kernel descriptor metadata')
        result[match[1]]=match[2]
    if result.get('link_type') in ('kprobe_multi','kretprobe_multi') and not table:
        raise ObservationUnavailable('kernel function table missing')
    return result


def _number(value):
    if not isinstance(value,str) or re.fullmatch(r'[0-9]{1,20}',value) is None or int(value)>=2**64:
        raise ObservationUnavailable('unknown kernel counter')
    return int(value)


def _birth(pid,owner):
    fields=Path('/proc',str(pid),'stat').read_text().rsplit(')',1)[1].split()
    if int(fields[1])!=owner:raise ObservationUnavailable('foreign observer parent')
    return int(fields[19])


def _function_address(path_scope='fixture'):
    found={name:[] for name,flags in probe_roles(path_scope)};names={name.encode():name for name in found};size=0
    with Path('/proc/kallsyms').open('rb') as stream:
        for line in stream:
            size+=len(line)
            if size>64*1024**2:raise ObservationUnavailable('unbounded kernel symbol census')
            parts=line.split()
            if len(parts)==3 and parts[2] in names and parts[1] in (b't',b'T'):
                found[parts[2].decode()].append(int(parts[0],16))
    if any(len(rows)!=1 or not 0<rows[0]<2**64 for rows in found.values()):
        raise ObservationUnavailable('kernel function identity unavailable')
    return {name:rows[0] for name,rows in found.items()}


def _return_link(identity,program,address,function,expected_flags):
    # Linux UAPI prefix: bpf_link_info header, alignment, kprobe_multi fields.
    # The kernel reports the copied prefix length; absent fields never mean zero.
    if os.uname().machine!='x86_64' or sys.byteorder!='little':
        raise ObservationUnavailable('unsupported kernel census ABI')
    libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
    def call(command,argument):
        result=libc.syscall(ctypes.c_long(321),ctypes.c_int(command),ctypes.byref(argument),ctypes.c_uint(ctypes.sizeof(argument)))
        if result<0:raise OSError(ctypes.get_errno(),'kernel census query failed')
        return result
    argument=ctypes.create_string_buffer(struct.pack('<III',identity,0,0),12)
    descriptor=call(30,argument)
    try:
        addresses=(ctypes.c_uint64*1)();info=ctypes.create_string_buffer(40)
        struct.pack_into('<QII',info,16,ctypes.addressof(addresses),1,0)
        query=ctypes.create_string_buffer(struct.pack('<IIQ',descriptor,40,ctypes.addressof(info)),16)
        call(15,query)
        if struct.unpack_from('<I',query,4)[0]<40:raise OSError(errno.EPROTONOSUPPORT,'kernel counter ABI unavailable')
        kind,actual,prog=struct.unpack_from('<III',info)
        count,flags,missed=struct.unpack_from('<IIQ',info,24)
        if kind!=8 or actual!=identity or prog!=program or count!=1 or flags!=expected_flags or addresses[0]!=address:
            raise OSError(errno.ERANGE,'original return attachment unavailable')
        return {'function':function,'count':count,'flags':flags,'missed':missed}
    finally:os.close(descriptor)  # Never keep the observer's attachments alive.


def kernel_census(child,owner,birth,address,diagnostic,*,probe_count=PROBE_COUNT):
    if child.poll() is not None or _birth(child.pid,owner)!=birth:
        raise ObservationUnavailable('original observer is not live')
    root=Path('/proc',str(child.pid));programs=[];links=[];rows=[]
    descriptors=list((root/'fd').iterdir())
    if len(descriptors)>512:raise ObservationUnavailable('unbounded original descriptor census')
    for path in descriptors:
        target=os.readlink(path)
        if target not in ('anon_inode:bpf-prog','anon_inode:bpf_link','anon_inode:bpf-link'):
            continue
        data=(root/'fdinfo'/path.name).read_bytes()
        if len(data)>16384 or len(rows)>=2*probe_count:raise ObservationUnavailable('unbounded original BPF descriptor census')
        rows.append({'fd':path.name,'target':target,'fdinfo':data.hex()})
    cache._write_record(diagnostic,{'descriptors':rows})
    for row in rows:
        target=row['target'];info=_fdinfo(bytes.fromhex(row['fdinfo']))
        if target=='anon_inode:bpf-prog':
            programs.append({'id':_number(info.get('prog_id')),'type':_number(info.get('prog_type')),
                'tag':info.get('prog_tag'),'recursion_misses':_number(info.get('recursion_misses'))})
        else:
            identity=_number(info.get('link_id'));program=_number(info.get('prog_id'));kind=info.get('link_type')
            link={'id':identity,'type':kind,'prog_id':program,'tag':info.get('prog_tag')}
            if kind=='raw_tracepoint':link['tracepoint']=info.get('tp_name')
            elif kind in ('kprobe_multi','kretprobe_multi'):
                function=info['function']
                link.update(_return_link(identity,program,address[function],function,1 if kind=='kretprobe_multi' else 0))
                if link['missed']!=_number(info.get('missed')):
                    raise ObservationUnavailable('kernel counters changed during census')
            else:raise ObservationUnavailable('unaccounted observer attachment')
            links.append(link)
    if child.poll() is not None or _birth(child.pid,owner)!=birth:
        raise ObservationUnavailable('original observer changed during census')
    return {'pid':child.pid,'birth':birth,'programs':sorted(programs,key=lambda row:row['id']),
        'links':sorted(links,key=lambda row:row['id'])}


def directory_queries(records,*,complete=True):
    """Reconcile kernel actor attempts with the original getdents64 result.

    This decoder alone provides no collection or reuse authority. An accepted
    actor fills the previous record's offset; iterate_dir supplies the last
    offset. Keep rejected attempts distinct from bytes returned to the caller.
    """
    pairs={}
    if type(records) is not list or len(records)>40000:
        raise ObservationUnavailable('unbounded directory observation')
    layouts={'B':(2,False),'I':(4,True),'A':(2,False),'T':(2,True),'G':(1,True)}
    for line in records:
        if type(line) is not str or len(line)>1024:
            raise ObservationUnavailable('unbounded directory record')
        match=re.fullmatch(r'([BIDATG]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,19}) (.*)',line)
        if match is None:raise ObservationUnavailable('unknown directory record')
        kind=match[1];uid,pid,tid,sequence=map(int,match.groups()[1:5])
        if (not (uid==1999 or 2000<=uid<6096) or pid>=2**31 or tid>=2**31 or sequence>=2**64):
            raise ObservationUnavailable('foreign directory observation')
        pair=pairs.setdefault((uid,pid,tid,sequence),{'attempts':{}})
        if kind=='D':
            chunk=re.fullmatch(r'([1-9][0-9]{0,4}) ([0-9]{1,20}) ([0-9]{1,10}) (-?[0-9]{1,20}) ([0-9]{1,3}) ([1-9][0-9]{0,2}) ([0-9a-f]{2}(?: [0-9a-f]{2})*)',match[6])
            if chunk is None:raise ObservationUnavailable('unknown directory name chunk')
            ordinal,inode,raw_type,offset,start,total=map(int,chunk.groups()[:6])
            data=bytes.fromhex(chunk[7])
            if (ordinal>40000 or inode>=2**64 or raw_type>=2**32 or not -2**63<=offset<2**63
                    or not 0<=start<total<=255 or start%CHUNK_BYTES
                    or len(data)!=min(CHUNK_BYTES,total-start)):
                raise ObservationUnavailable('invalid directory name chunk')
            attempt=pair['attempts'].setdefault(ordinal,{'chunks':{}})
            metadata=(inode,raw_type,offset,total)
            if start in attempt['chunks'] or attempt.get('metadata',metadata)!=metadata:
                raise ObservationUnavailable('ambiguous directory attempt')
            attempt['metadata']=metadata;attempt['chunks'][start]=data
        else:
            count,signed=layouts[kind]
            # Old captures lack namespace identity. Preserve that absence;
            # only the extended original record can bind a directory selector.
            if kind=='I' and len(match[6].split())==6:count=6
            if re.fullmatch(('(-?[0-9]{1,20})' if signed else '([0-9]{1,20})')
                    +(' (-?[0-9]{1,20})' if signed else ' ([0-9]{1,20})')*(count-1),match[6]) is None:
                raise ObservationUnavailable('invalid directory result')
            values=tuple(map(int,match[6].split()))
            if any(not -2**63<=v<2**64 for v in values):raise ObservationUnavailable('directory value overflow')
            if kind=='A':
                ordinal,accepted=values
                if not 1<=ordinal<=40000 or accepted not in (0,1):raise ObservationUnavailable('invalid actor result')
                attempt=pair['attempts'].setdefault(ordinal,{'chunks':{}})
                if 'accepted' in attempt:raise ObservationUnavailable('duplicate actor result')
                attempt['accepted']=bool(accepted)
            else:
                if kind in pair:raise ObservationUnavailable('duplicate directory observation')
                pair[kind]=values
    queries=[]
    for (uid,pid,tid,sequence),pair in pairs.items():
        attempts=pair['attempts']
        ready=(set(pair)=={'attempts','B','I','T','G'}
            and all(set(a)=={'chunks','metadata','accepted'}
                and set(a['chunks'])==set(range(0,a['metadata'][3],CHUNK_BYTES)) for a in attempts.values()))
        if not ready:
            if complete:raise ObservationUnavailable('incomplete directory observation')
            continue
        descriptor,capacity=pair['B'];device,inode,mode,start_position=pair['I'][:4]
        mount_namespace,user_namespace=pair['I'][4:] if len(pair['I'])==6 else (None,None)
        status,position=pair['T'];result,=pair['G']
        if (descriptor>=2**31 or capacity>=2**32 or not 0<=device<2**32 or not 0<=inode<2**64
                or not 0<=mode<=65535 or not stat.S_ISDIR(mode) or not -2**31<=status<2**31
                or not -2**63<=start_position<2**63
                or not -2**63<=position<2**63 or not -4095<=result<=capacity
                or set(attempts)!=set(range(1,len(attempts)+1))):
            raise ObservationUnavailable('inconsistent directory call')
        if mount_namespace is not None and not (0<mount_namespace<2**32 and 0<user_namespace<2**32):
            raise ObservationUnavailable('invalid directory namespace')
        entries=[];ordered=[];rejected=False
        for ordinal in sorted(attempts):
            a=attempts[ordinal];ino,raw_type,offset,total=a['metadata']
            name=b''.join(a['chunks'][i] for i in range(0,total,CHUNK_BYTES))
            if b'\0' in name or b'/' in name or rejected:
                raise ObservationUnavailable('invalid directory actor sequence')
            entry={'inode':ino,'raw_type':raw_type,'type':raw_type&15,'offset':offset,
                'name':name,'record_length':(20+total+7)&~7,'accepted':a['accepted']}
            ordered.append(entry)
            if a['accepted']:entries.append(dict(entry))
            else:rejected=True
        copied=sum(e['record_length'] for e in entries)
        if copied!=max(0,result):raise ObservationUnavailable('directory byte count mismatch')
        for index,entry in enumerate(entries):
            entry['returned_offset']=entries[index+1]['offset'] if index+1<len(entries) else position
        queries.append({'uid':uid,'pid':pid,'tid':tid,'sequence':sequence,'syscall':217,
            'fd':descriptor,'capacity':capacity,'device':device,'inode':inode,'mode':mode,
            'mount_namespace':mount_namespace,'user_namespace':user_namespace,
            'start_position':start_position,
            'iterator_result':status,'position':position,'result':result,'attempts':ordered,'entries':entries})
    return queries


def _directory_calls(queries,pid,uid,path):
    info=path.stat()
    # Linux internal dev_t uses 12 major/20 minor bits (kdev_t.h). stat's
    # userspace dev_t has a different encoding. Retain raw captured values and
    # translate the expected identity explicitly rather than dropping it.
    major,minor=os.major(info.st_dev),os.minor(info.st_dev)
    if not 0<=major<2**12 or not 0<=minor<2**20:
        raise ObservationUnavailable('unsupported directory device identity')
    kernel_device=(major<<20)|minor
    selected=[q for q in queries if (q['pid'],q['tid'],q['uid'],q['device'],q['inode'])
        ==(pid,pid,uid,kernel_device,info.st_ino)]
    selected.sort(key=lambda q:q['sequence'])
    return selected


def directory_witness(queries,pid,uid,path,members):
    """Check one controlled, immutable tree against original kernel records."""
    selected=_directory_calls(queries,pid,uid,path);info=path.stat()
    if (not selected or any(q['iterator_result']!=0 or q['result']<0 for q in selected)
            or selected[0]['start_position']!=0
            or any(b['start_position']!=a['position'] for a,b in zip(selected,selected[1:]))
            or selected[-1]['result']!=0 or any(q['result']==0 for q in selected[:-1])
            or len({q['fd'] for q in selected})!=1):
        raise ObservationUnavailable('original directory EOF witness missing')
    entries=[e for q in selected for e in q['entries']]
    expected={b'.':(info.st_ino,4),b'..':(path.parent.stat().st_ino,4),**members}
    if len(entries)!=len(expected) or {e['name']:(e['inode'],e['type']) for e in entries}!=expected:
        raise ObservationUnavailable('original directory membership witness differs')
    return {'reads':len(selected),'entries':len(entries),
        'rejected_attempts':sum(not a['accepted'] for q in selected for a in q['attempts'])}


def directory_cursor_witness(queries,pid,uid,path):
    """Require the fixed original read/rewind/full-read/EOF experiment."""
    selected=_directory_calls(queries,pid,uid,path)
    if (len(selected)!=3 or len({q['fd'] for q in selected})!=1
            or any(q['iterator_result']!=0 for q in selected)
            or selected[0]['start_position']!=0 or selected[0]['position']==0
            or selected[0]['capacity']!=24 or selected[0]['result']!=24
            or len(selected[0]['entries'])!=1 or selected[1]['start_position']!=0):
        raise ObservationUnavailable('original directory rewind witness missing')
    directory_witness(selected[1:],pid,uid,path,{})
    first=selected[0]['entries'][0]
    if not any(all(first[k]==entry[k] for k in ('name','inode','type')) for entry in selected[1]['entries']):
        raise ObservationUnavailable('rewound directory membership differs')
    return {'reads':3,'rewind_from':selected[0]['position'],'rewind_to':selected[1]['start_position']}


STAT_FIELDS=('device','inode','mode','nlink','owner_uid','owner_gid','rdev','size','blksize','blocks','result_mask')
STAT_TIMES=('atime_sec','atime_nsec','mtime_sec','mtime_nsec','ctime_sec','ctime_nsec')
STATX_FIELDS=('attributes','attributes_mask','btime_sec','btime_nsec','mnt_id','change_cookie','subvol',
    'dio_mem_align','dio_offset_align','dio_read_offset_align','atomic_write_unit_min','atomic_write_unit_max',
    'atomic_write_unit_max_opt','atomic_write_segments_max')
BASE_FIELDS=('device','inode','mode','dentry','mount','inode_generation','path_seq','dentry_seq','flags','state')


def lookup_bases(records,*,complete=True):
    """Original path_init attempts; a chosen base is not a completed path walk."""
    if type(records) is not list or len(records)>40000:raise ObservationUnavailable('unbounded lookup bases')
    pairs={};counts={'Q':5,'R':2,'C':11}
    for line in records:
        if type(line) is not str or len(line)>1024:raise ObservationUnavailable('unbounded lookup base record')
        match=re.fullmatch(r'([QRC]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) ([0-9]{1,4}) ([1-9][0-9]{0,19}) (.*)',line)
        if match is None:raise ObservationUnavailable('unknown lookup base record')
        kind=match[1];uid,pid,tid,call,sequence=map(int,match.groups()[1:6])
        if (not (uid==1999 or 2000<=uid<6096) or pid>=2**31 or tid>=2**31
                or call not in (SYSCALLS|{437}) or sequence>=2**64
                or re.fullmatch(r'-?[0-9]{1,20}(?: -?[0-9]{1,20}){'+str(counts[kind]-1)+'}',match[7]) is None):
            raise ObservationUnavailable('foreign lookup base observation')
        values=tuple(map(int,match[7].split()));attempt=values[0]
        if not 1<=attempt<=64 or any(not -2**63<=v<2**64 for v in values):
            raise ObservationUnavailable('lookup base value overflow')
        pair=pairs.setdefault((uid,pid,tid,call,sequence,attempt),{})
        if kind in pair:raise ObservationUnavailable('duplicate lookup base record')
        pair[kind]=values[1:]
    result=[];ordinals={}
    for (uid,pid,tid,call,sequence,attempt),pair in sorted(pairs.items()):
        if not {'Q','R'}<=set(pair):
            if complete:raise ObservationUnavailable('incomplete lookup base')
            continue
        dfd,flags,mount_namespace,user_namespace=pair['Q'];status,=pair['R']
        if (not -2**31<=dfd<2**31 or not 0<=flags<2**32 or not -4095<=status<=0
                or not 0<mount_namespace<2**32 or not 0<user_namespace<2**32):
            raise ObservationUnavailable('invalid lookup base context')
        expected={'Q','R','C'} if status==0 else {'Q','R'}
        if set(pair)!=expected:
            if complete:raise ObservationUnavailable('lookup base object unavailable')
            continue
        base=None
        if status==0:
            base=dict(zip(BASE_FIELDS,pair['C']))
            limits={key:(64 if key in ('inode','dentry','mount') else 16 if key=='mode' else 32) for key in BASE_FIELDS}
            if (any(not 0<=base[key]<2**bits for key,bits in limits.items())
                    or not base['dentry'] or not base['mount']):
                raise ObservationUnavailable('invalid lookup base object')
        key=(uid,pid,tid,call,sequence);ordinals.setdefault(key,[]).append(attempt)
        result.append({'uid':uid,'pid':pid,'tid':tid,'syscall':call,'sequence':sequence,'attempt':attempt,
            'dfd':dfd,'entry_flags':flags,'mount_namespace':mount_namespace,'user_namespace':user_namespace,
            'init_result':status,'base':base})
    if complete and any(values!=list(range(1,len(values)+1)) for values in ordinals.values()):
        raise ObservationUnavailable('lookup base attempt sequence incomplete')
    return result


def lookup_base_witness(events,pid,uid,path,call,result,*,dfd,user_namespace,mount_namespace,expected=None,init_result=0):
    info=expected.stat() if expected is not None else None
    for event in events:
        if (event['pid']!=event['tid'] or event['pid']!=pid or event['uid']!=uid
                or event['path']!=path or event['syscall']!=call or event['result']!=result):continue
        attempts=event['lookup_attempts']
        if not attempts:continue
        valid=True
        for attempt in attempts:
            base=attempt['base']
            if (attempt['dfd']!=dfd or attempt['user_namespace']!=user_namespace
                    or attempt['mount_namespace']!=mount_namespace or attempt['init_result']!=init_result):valid=False;break
            if init_result!=0:
                if base is not None:valid=False;break
                continue
            if (base is None or base['flags']&256 and (base['path_seq']!=base['dentry_seq'] or base['path_seq']&1)):
                valid=False;break
            if info is not None and (base['device'],base['inode'],base['mode'])!=((os.major(info.st_dev)<<20)|os.minor(info.st_dev),info.st_ino,info.st_mode):
                valid=False;break
        if valid:return
    raise ObservationUnavailable('original lookup base witness missing')


def _metadata_fields(pair,*,statx=False):
    fields=dict(zip(STAT_FIELDS,pair['J']));fields.update(zip(STAT_TIMES,pair['K']))
    limits={'device':32,'inode':64,'mode':16,'nlink':32,'owner_uid':32,'owner_gid':32,
        'rdev':32,'blksize':32,'blocks':64,'result_mask':32}
    times=STAT_TIMES
    if statx:
        fields.update(zip(STATX_FIELDS,pair['O']))
        limits.update({key:64 for key in ('attributes','attributes_mask','mnt_id','change_cookie','subvol')})
        limits.update({key:32 for key in STATX_FIELDS[7:]})
        times=(*times,'btime_sec','btime_nsec')
    if (any(not 0<=fields[key]<2**bits for key,bits in limits.items())
            or not -2**63<=fields['size']<2**63
            or any(not -2**63<=fields[key]<2**63 for key in times[::2])
            or any(not 0<=fields[key]<10**9 for key in times[1::2])):
        raise ObservationUnavailable('metadata fields outside original ABI')
    return fields


def statx_bytes(fields):
    """Pinned x86-64 cp_statx ABI, for the controlled user-copy witness only."""
    data=bytearray(256)
    struct.pack_into('<IIQIIIH',data,0,fields['result_mask']&~0x40000000,fields['blksize'],
        fields['attributes']&~(1<<63),fields['nlink'],fields['owner_uid'],fields['owner_gid'],fields['mode'])
    struct.pack_into('<QQQQ',data,32,fields['inode'],fields['size']&((1<<64)-1),fields['blocks'],fields['attributes_mask'])
    for name,offset in (('atime',64),('btime',80),('ctime',96),('mtime',112)):
        struct.pack_into('<qI',data,offset,fields[name+'_sec'],fields[name+'_nsec'])
    struct.pack_into('<IIII',data,128,fields['rdev']>>20,fields['rdev']&((1<<20)-1),
        fields['device']>>20,fields['device']&((1<<20)-1))
    for name,fmt,offset in (('mnt_id','Q',144),('dio_mem_align','I',152),('dio_offset_align','I',156),
            ('subvol','Q',160),('atomic_write_unit_min','I',168),('atomic_write_unit_max','I',172),
            ('atomic_write_segments_max','I',176),('dio_read_offset_align','I',180),('atomic_write_unit_max_opt','I',184)):
        struct.pack_into('<'+fmt,data,offset,fields[name])
    return bytes(data)


def metadata_queries(records,*,complete=True):
    """Decode original x86-64 fstat VFS data and actual syscall delivery.

    kstat fields remain raw, including internal dev_t and signed times. This
    observes fstat only, not general statx, path resolution or input closure.
    """
    if type(records) is not list or len(records)>40000:raise ObservationUnavailable('unbounded metadata observation')
    counts={'F':2,'H':2,'J':11,'K':6,'L':1,'Z':1,'U':8,'P':2,'O':14,'Y':1};pairs={}
    for line in records:
        if type(line) is not str or len(line)>1024:raise ObservationUnavailable('unbounded metadata record')
        match=re.fullmatch(r'([FHJKLZUPOY]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,19}) (.*)',line)
        if match is None:raise ObservationUnavailable('unknown metadata record')
        kind=match[1];uid,pid,tid,sequence=map(int,match.groups()[1:5])
        if (not (uid==1999 or 2000<=uid<6096) or pid>=2**31 or tid>=2**31 or sequence>=2**64):
            raise ObservationUnavailable('foreign metadata observation')
        count=3 if kind=='H' and len(match[6].split())==3 else counts[kind]
        if re.fullmatch(r'-?[0-9]{1,20}(?: -?[0-9]{1,20}){'+str(count-1)+'}',match[6]) is None:
            raise ObservationUnavailable('unknown metadata fields')
        values=tuple(map(int,match[6].split()))
        if any(not -2**63<=v<2**64 for v in values):raise ObservationUnavailable('metadata value overflow')
        pair=pairs.setdefault((uid,pid,tid,sequence),{})
        if kind in pair:raise ObservationUnavailable('duplicate metadata record')
        pair[kind]=values
    queries=[]
    for (uid,pid,tid,sequence),pair in pairs.items():
        if 'U' in pair:
            if 'Z' not in pair:
                if complete:raise ObservationUnavailable('incomplete pathname metadata')
                continue
            call,raw_fd,fd,raw_flags,flags,raw_mask,mask,user_namespace=pair['U'];result,=pair['Z']
            if (call not in (262,332) or not 0<=raw_fd<2**64 or not -2**31<=fd<2**31
                    or fd!=((raw_fd+2**31)%2**32)-2**31 or not 0<user_namespace<2**32
                    or not 0<=raw_flags<2**64 or flags!=(raw_flags&0xffffffff)
                    or not 0<=raw_mask<2**64 or mask!=(raw_mask&0xffffffff)
                    or call==262 and (mask!=0 or raw_mask!=0) or not -4095<=result<=0):
                raise ObservationUnavailable('pathname metadata arguments differ')
            fields=None;copy_result=None
            if 'P' in pair:
                expected={'U','P','J','K','Y','Z'}|({'O'} if call==332 else set())
                if set(pair)!=expected:
                    if complete:raise ObservationUnavailable('incomplete copied metadata')
                    continue
                kind,namespace=pair['P'];copy_result,=pair['Y']
                if kind!=(1 if call==332 else 0) or namespace!=user_namespace or copy_result!=result:
                    raise ObservationUnavailable('metadata copy binding differs')
                fields=_metadata_fields(pair,statx=call==332)
            elif set(pair)!={'U','Z'} or result==0:
                if complete:raise ObservationUnavailable('metadata copy unavailable')
                continue
            queries.append({'uid':uid,'pid':pid,'tid':tid,'sequence':sequence,'syscall':call,
                'raw_fd':raw_fd,'fd':fd,'raw_flags':raw_flags,'flags':flags,'raw_mask':raw_mask,'mask':mask,
                'user_namespace':user_namespace,'vfs_result':None,'copy_result':copy_result,'result':result,
                'stat':fields,'delivered':result==0})
            continue
        if not {'F','H','L','Z'}<=set(pair):
            if complete:raise ObservationUnavailable('incomplete metadata observation')
            continue
        raw_fd,fd=pair['F'];original_fd,user_namespace=pair['H'][:2];vfs_result,=pair['L'];result,=pair['Z']
        mount_namespace=pair['H'][2] if len(pair['H'])==3 else None
        if (not 0<=raw_fd<2**64 or not -2**31<=fd<2**31 or fd!=original_fd
                or fd!=((raw_fd+2**31)%2**32)-2**31 or not 0<user_namespace<2**32
                or not -4095<=vfs_result<=0 or not -4095<=result<=0
                or vfs_result<0 and result!=vfs_result):
            raise ObservationUnavailable('metadata syscall binding differs')
        if mount_namespace is not None and not 0<mount_namespace<2**32:
            raise ObservationUnavailable('invalid descriptor mount namespace')
        expected={'F','H','J','K','L','Z'} if vfs_result==0 else {'F','H','L','Z'}
        if set(pair)!=expected:
            if complete:raise ObservationUnavailable('metadata result fields unavailable')
            continue
        fields=None
        if vfs_result==0:
            fields=_metadata_fields(pair)
        queries.append({'uid':uid,'pid':pid,'tid':tid,'sequence':sequence,'syscall':5,
            'raw_fd':raw_fd,'fd':fd,'user_namespace':user_namespace,'mount_namespace':mount_namespace,'vfs_result':vfs_result,
            'result':result,'stat':fields,'delivered':result==0})
    return queries


def _expected_metadata(path):
    info=path.stat();expected={'device':(os.major(info.st_dev)<<20)|os.minor(info.st_dev),'inode':info.st_ino,
        'mode':info.st_mode,'nlink':info.st_nlink,'owner_uid':info.st_uid,'owner_gid':info.st_gid,
        'rdev':(os.major(info.st_rdev)<<20)|os.minor(info.st_rdev),'size':info.st_size,
        'blksize':info.st_blksize,'blocks':info.st_blocks}
    for name,value in (('atime',info.st_atime_ns),('mtime',info.st_mtime_ns),('ctime',info.st_ctime_ns)):
        expected[name+'_sec'],expected[name+'_nsec']=divmod(value,10**9)
    return expected


def metadata_witness(queries,pid,uid,path,user_namespace):
    expected=_expected_metadata(path)
    if not any(q['pid']==q['tid']==pid and q['uid']==uid and q['user_namespace']==user_namespace
            and q['syscall']==5 and q['fd']>=0 and q['vfs_result']==q['result']==0 and q['stat'] is not None
            and all(q['stat'][key]==value for key,value in expected.items()) for q in queries):
        raise ObservationUnavailable('original fstat metadata witness missing')


def path_metadata_witness(queries,pid,uid,path,user_namespace,*,lookup,flags,dirpath=None,digest=None):
    expected=_expected_metadata(path)
    selected=[q for q in queries if q['pid']==q['tid']==pid and q['uid']==uid
        and q['user_namespace']==user_namespace]
    directory=_expected_metadata(dirpath) if dirpath is not None else None
    for call in (262,332):
        matches=[]
        for q in selected:
            if (q['syscall']!=call or q.get('path')!=lookup or q.get('flags')!=flags
                    or q.get('mask')!=(8191 if call==332 else 0)
                    or q.get('copy_result')!=0 or q['result']!=0 or q['stat'] is None
                    or any(q['stat'][key]!=value for key,value in expected.items())):continue
            if directory is None:
                if q['fd']!=-100:continue
            elif not any(base['syscall']==5 and base['fd']==q['fd']>=0 and base['sequence']<q['sequence']
                    and base['vfs_result']==base['result']==0 and base['stat'] is not None
                    and all(base['stat'][key]==directory[key] for key in ('device','inode','mode')) for base in selected):continue
            if call==332 and digest is not None and hashlib.sha256(statx_bytes(q['stat'])).hexdigest()!=digest:continue
            matches.append(q)
        if not matches:raise ObservationUnavailable('original path metadata witness missing')


def open_queries(records,events,*,complete=True):
    """Match copied kernel open arguments and installed objects to syscall exit."""
    pairs={};keys=('uid','pid','tid','syscall','sequence')
    for line in records:
        match=re.fullmatch(r'([WV]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) (2|257|437) ([1-9][0-9]{0,19}) (.*)',line)
        if match is None:raise ObservationUnavailable('unknown kernel open record')
        kind=match[1];key=tuple(map(int,match.groups()[1:6]));tail=match[7].split(' ')
        if len(tail)!=(8 if kind=='W' else 11) or any(re.fullmatch(r'-?[0-9]{1,20}',part) is None for part in tail):
            raise ObservationUnavailable('invalid kernel open arguments')
        values=list(map(int,tail));pair=pairs.setdefault(key,{})
        if kind in pair:raise ObservationUnavailable('duplicate kernel open record')
        if kind=='W':
            fields=('dfd','flags','mode','acc_mode','intent','lookup_flags','user_namespace','mount_namespace')
            if not -2**31<=values[0]<2**31 or not 0<=values[2]<2**16 or any(not 0<=value<2**32 for value in values[1:6]):
                raise ObservationUnavailable('kernel open argument outside ABI')
        else:
            fields=('fd','flags','file_mode','device','inode','mode','inode_generation','dentry','mount','user_namespace','mount_namespace')
            if (not 0<=values[0]<2**31 or any(not 0<=values[i]<2**32 for i in (1,2,3,6))
                    or not 0<values[4]<2**64 or not 0<=values[5]<2**16
                    or any(not 0<values[i]<2**64 for i in (7,8))):
                raise ObservationUnavailable('installed file outside ABI')
        if any(not 0<value<2**64 for value in values[-2:]):raise ObservationUnavailable('open namespace absent')
        pair[kind]=dict(zip(fields,values))
    observed={tuple(event[key] for key in keys):event for event in events if event['syscall'] in (2,257,437)}
    if complete and set(pairs)!=set(observed):raise ObservationUnavailable('open lacks original syscall')
    for key,event in observed.items():
        pair=pairs.get(key,{})
        if 'W' not in pair or event['result']>=0 and 'V' not in pair:
            if complete:raise ObservationUnavailable('open lacks original kernel evidence')
            continue
        arguments=pair['W'];installed=pair.get('V')
        if (event['result']<0 and installed is not None or installed is not None and
                (installed['fd']!=event['result'] or any(installed[name]!=arguments[name]
                    for name in ('user_namespace','mount_namespace')))):
            raise ObservationUnavailable('installed file differs from original syscall')
        event['open']={'arguments':arguments,'installed':installed}


def readable_open(event):
    opened=event.get('open');installed=None if opened is None else opened.get('installed')
    return (event.get('syscall') in (2,257,437) and type(event.get('result')) is int and event['result']>=0
        and type(installed) is dict and installed.get('fd')==event['result']
        and type(installed.get('file_mode')) is int and bool(installed['file_mode']&1)
        and type(installed.get('flags')) is int and not installed['flags']&0x200000)


def decode(data,*,complete=True,with_directories=False,with_metadata=False,path_scope='fixture'):
    if path_scope not in ('fixture','all'):raise ObservationUnavailable('unknown pathname scope')
    if not isinstance(data,bytes) or len(data)>MAX_BYTES:raise ObservationUnavailable('unbounded observation')
    if complete and (not data or not data.endswith(b'\n')):raise ObservationUnavailable('incomplete observation')
    if not complete and not data.endswith(b'\n'):data=data.rpartition(b'\n')[0]+b'\n'
    try:lines=data.decode('ascii').splitlines()
    except UnicodeError:raise ObservationUnavailable('unknown pathname encoding') from None
    pairs={};directories=[];metadata=[];bases=[];opens=[];count=None;records=0;loader_records=0
    for line in lines:
        if not line:continue
        if (re.fullmatch(r'Program ID: [1-9][0-9]{0,9}',line)
                or line=='Attaching to 1 functions'
                or line in ('The verifier log:','The verifier log: ')
                or re.fullmatch(r'processed [0-9]{1,10} insns \(limit 1000000\) max_states_per_insn [0-9]{1,10} total_states [0-9]{1,10} peak_states [0-9]{1,10} mark_read [0-9]{1,10}',line)):
            loader_records+=1
            if loader_records>4*(2+sum(probe_roles(path_scope).values()))+8:raise ObservationUnavailable('unbounded loader summaries')
            continue
        try:record=json.loads(line,object_pairs_hook=cache._unique_object)
        except (ValueError,TypeError):raise ObservationUnavailable('invalid kernel record framing') from None
        if type(record) is dict and record.get('type')=='helper_error':
            if (set(record)=={'type','msg','helper','retcode','line','col'}
                    and record['helper']=='map_lookup_elem' and type(record['retcode']) is int and record['retcode']==0
                    and type(record['msg']) is str and 1<=len(record['msg'])<=256
                    and record['msg'].isascii() and all(ord(c)>=32 for c in record['msg'])
                    and type(record['line']) is int and 1<=record['line']<=512
                    and type(record['col']) is int and 1<=record['col']<=512):
                continue  # bpftrace's specified zero default for an absent map key.
            raise ObservationUnavailable('kernel helper failed')
        if type(record) is not dict or set(record)!={'type','data'}:raise ObservationUnavailable('unknown kernel record')
        if record['type']=='attached_probes':
            if (type(record['data']) is not dict or set(record['data'])!={'probes'}
                    or type(record['data']['probes']) is not int or not 1<=record['data']['probes']<=3+sum(probe_roles(path_scope).values())):
                raise ObservationUnavailable('unknown attachment record')
            continue
        if record['type']=='map':
            if (count is not None or type(record['data']) is not dict or set(record['data'])!={'@emitted'}
                    or type(record['data']['@emitted']) is not int or not 0<=record['data']['@emitted']<=40000):
                raise ObservationUnavailable('invalid event counter')
            count=record['data']['@emitted'];continue
        if record['type']!='printf' or type(record['data']) is not str or not record['data'].endswith('\n'):
            raise ObservationUnavailable('unknown kernel event')
        line=record['data'][:-1]  # Preserve embedded newlines; never reinterpret them as events.
        if path_scope=='all' and line[:2] in ('W ','V '):
            opens.append(line);records+=1
            if records>40000:raise ObservationUnavailable('observation limit exceeded')
            continue
        if line[:2] in ('Q ','R ','C '):
            bases.append(line);records+=1
            if records>40000:raise ObservationUnavailable('observation limit exceeded')
            continue
        if line[:2] in ('F ','H ','J ','K ','L ','Z ','U ','P ','O ','Y '):
            metadata.append(line);records+=1
            if records>40000:raise ObservationUnavailable('observation limit exceeded')
            continue
        if line[:2] in ('B ','I ','D ','A ','T ','G '):
            directories.append(line);records+=1
            if records>40000:raise ObservationUnavailable('observation limit exceeded')
            continue
        match=re.fullmatch(r'([NE]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) ([0-9]{1,4}) ([1-9][0-9]{0,19}) (.*)',line)
        if match is None:raise ObservationUnavailable('unknown observation record')
        kind=match[1];uid,pid,tid,call,sequence=map(int,match.groups()[1:6])
        if (pid>=2**31 or tid>=2**31 or call not in (SYSCALLS|({437} if path_scope=='all' else set())) or sequence>=2**64
                or not (uid==1999 or 2000<=uid<6096)):
            raise ObservationUnavailable('foreign kernel observation')
        pair=pairs.setdefault((uid,pid,tid,call,sequence),{})
        if kind=='N':
            chunk=re.fullmatch(r'([0-9]{1,4}) ([1-9][0-9]{0,3}) ([0-9a-f]{2}(?: [0-9a-f]{2})*)',match[7])
            if chunk is None:raise ObservationUnavailable('unknown pathname chunk')
            offset,total=int(chunk[1]),int(chunk[2]);data=bytes.fromhex(chunk[3])
            chunks=pair.setdefault('N',{})
            if (not 0<=offset<total<PATH_BYTES or offset%CHUNK_BYTES
                    or len(data)!=min(CHUNK_BYTES,total-offset) or b'\0' in data
                    or offset in chunks or pair.get('length',total)!=total):
                raise ObservationUnavailable('incomplete pathname chunk')
            pair['length']=total;chunks[offset]=data
        else:
            if kind in pair:raise ObservationUnavailable('duplicate kernel observation')
            if re.fullmatch(r'-?[0-9]{1,20}',match[7]) is None:raise ObservationUnavailable('unknown syscall result')
            result=int(match[7])
            if not -4095<=result<2**31:raise ObservationUnavailable('unknown syscall result')
            pair[kind]=result
        records+=1
        if len(pairs)>20000 or records>40000:raise ObservationUnavailable('observation limit exceeded')
    if complete and (count is None or count!=records or not (pairs or directories or metadata)):
        raise ObservationUnavailable('event count unavailable')
    events=[]
    for (uid,pid,tid,call,sequence),pair in pairs.items():
        if (set(pair)!={'N','E','length'}
                or set(pair['N'])!=set(range(0,pair['length'],CHUNK_BYTES))):
            if complete:raise ObservationUnavailable('incomplete lookup observation')
            continue
        path=b''.join(pair['N'][offset] for offset in range(0,pair['length'],CHUNK_BYTES))
        parts=path.split(b'/')
        if parts[-1]==b'':parts=parts[:-1]
        if ((path_scope=='fixture' and any(part in (b'',b'.',b'..') for part in parts[1:]))
                or not (uid==1999 and (re.fullmatch(rb'/ci-observer-witness-[a-z0-9-]+',path) or path==b'/proc/sys/kernel/random')
                    or 2000<=uid<6096 and (path_scope=='all' or path.startswith(b'/workspace/')
                        or path in (b'src/lib.rs',b'src/main.rs',b'build.rs')))):
            raise ObservationUnavailable('foreign kernel pathname')
        events.append({'uid':uid,'pid':pid,'tid':tid,'syscall':call,'sequence':sequence,'result':pair['E'],'path':path})
    if path_scope=='all':open_queries(opens,events,complete=complete)
    queries=directory_queries(directories,complete=complete)
    stats=metadata_queries(metadata,complete=complete)
    lookup={(e['uid'],e['pid'],e['tid'],e['syscall'],e['sequence']):e for e in events}
    for event in events:event['lookup_attempts']=[]
    for base in lookup_bases(bases,complete=complete):
        event=lookup.get(tuple(base[k] for k in ('uid','pid','tid','syscall','sequence')))
        if event is None:
            if complete:raise ObservationUnavailable('lookup base has no original query')
        else:event['lookup_attempts'].append(base)
    for query in stats:
        if query['syscall'] in (262,332):
            event=lookup.get(tuple(query[k] for k in ('uid','pid','tid','syscall','sequence')))
            if event is None:
                if complete:raise ObservationUnavailable('pathname metadata lacks original lookup')
                query['path']=None
            else:
                if event['result']!=query['result']:raise ObservationUnavailable('pathname metadata outcome differs')
                query['path']=event['path']
    if with_metadata:return events,queries,stats
    return (events,queries) if with_directories else events


# Multi-return callbacks run in reverse registration order on the pinned
# runtime. Register emission first; the captured nonce must prove scan-before-
# emission before target execution. Link info must resolve the wildcard to
# exactly the original getname_flags address.
_PROGRAM=r'''
struct observer_filename { const char *name; };
rawtracepoint:sys_enter / (uid >= 2000 && uid < 6096) || uid == 1999 / {
 @number[tid] = arg1;
 @sequence[tid] = @sequence[tid] + 1;
 @seen[tid]=(uint64)0; @ready[tid]=(uint64)0;
 @directory_attempt[tid]=(uint64)0; @directory_active[tid]=(uint64)0;
 @fstat_active[tid]=(uint64)0;
 @copy_active[tid]=(uint64)0;
 @base_depth[tid]=(uint64)0; @base_count[tid]=(uint64)0;
 if (arg1==262 || arg1==332) {
  $regs=(struct pt_regs *)arg0;
  @path_fd[tid]=(uint64)$regs->di;
  if (arg1==262) { @path_flags[tid]=(uint64)$regs->r10; @path_mask[tid]=(uint64)0; }
  else { @path_flags[tid]=(uint64)$regs->dx; @path_mask[tid]=(uint64)$regs->r10; }
 }
 if (arg1==5) {
  $regs=(struct pt_regs *)arg0;
  printf("F %u %u %u %llu %llu %lld\n",uid,pid,tid,@sequence[tid],(uint64)$regs->di,(int64)(int32)$regs->di);
  @emitted=sum(1);
 }
 if (arg1==217) {
  $regs=(struct pt_regs *)arg0;
  printf("B %u %u %u %llu %llu %llu\n",uid,pid,tid,@sequence[tid],(uint64)$regs->di,(uint64)$regs->dx);
  @emitted=sum(1);
 }
}
kprobe:iterate_di*r / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==217 / {
 $file=(struct file *)arg0;
 $inode=$file->f_inode;
 $task=(struct task_struct *)curtask;
 @directory_context[tid]=arg1;
 @directory_active[tid]=@sequence[tid];
 printf("I %u %u %u %llu %llu %llu %llu %lld %llu %llu\n",uid,pid,tid,@sequence[tid],
  (uint64)$inode->i_sb->s_dev,(uint64)$inode->i_ino,(uint64)$inode->i_mode,(int64)$file->f_pos,
  (uint64)$task->nsproxy->mnt_ns->ns.inum,(uint64)$task->cred->user_ns->ns.inum);
 @emitted=sum(1);
}
kretprobe:iterate_di*r / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==217 / {
 if (@directory_active[tid]>0 && @directory_active[tid]==@sequence[tid]) {
  $context=(struct dir_context *)@directory_context[tid];
  printf("T %u %u %u %llu %lld %lld\n",uid,pid,tid,@sequence[tid],(int64)(int32)retval,(int64)$context->pos);
  @directory_active[tid]=(uint64)0;
  @emitted=sum(1);
 }
}
kprobe:filldir6*4 / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==217 / {
 if (@directory_active[tid]>0 && @directory_active[tid]==@sequence[tid] && @directory_context[tid]==arg0) {
  @directory_attempt[tid]=@directory_attempt[tid]+1;
  $owner_uid=uid; $owner_pid=pid; $owner_tid=tid;
  $entry=@sequence[tid]; $attempt=@directory_attempt[tid];
  $name=(uint8 *)arg1; $length=(uint64)arg2;
  $position=(int64)arg3; $inode=(uint64)arg4; $type=(uint64)arg5;
  $start=(uint64)0; $chunks=(uint64)0;
  if ($length>0 && $length<=255) {
   while ($start<$length) {
    $size=$length-$start;
    if ($size>63) { $size=63; }
    printf("D %u %u %u %llu %llu %llu %llu %lld %llu %llu %rh\n",
     $owner_uid,$owner_pid,$owner_tid,$entry,$attempt,$inode,$type,$position,$start,$length,buf($name+$start,$size));
    $start=$start+63; $chunks=$chunks+1;
   }
  } else { printf("invalid kernel directory name length\n"); $chunks=1; }
  @emitted=sum($chunks);
 }
}
kretprobe:filldir6*4 / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==217 / {
 if (@directory_active[tid]>0 && @directory_active[tid]==@sequence[tid]) {
  printf("A %u %u %u %llu %llu %u\n",uid,pid,tid,@sequence[tid],@directory_attempt[tid],(uint8)retval);
  @emitted=sum(1);
 }
}
kprobe:vfs*_fstat / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==5 / {
 @fstat_active[tid]=@sequence[tid]; @fstat_stat[tid]=arg1;
 $task=(struct task_struct *)curtask;
 printf("H %u %u %u %llu %lld %llu %llu\n",uid,pid,tid,@sequence[tid],(int64)(int32)arg0,
  (uint64)$task->cred->user_ns->ns.inum,(uint64)$task->nsproxy->mnt_ns->ns.inum);
 @emitted=sum(1);
}
kretprobe:vfs*_fstat / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==5 / {
 if (@fstat_active[tid]>0 && @fstat_active[tid]==@sequence[tid]) {
  $status=(int64)(int32)retval;
  if ($status==0) {
   $stat=(struct kstat *)@fstat_stat[tid];
   printf("J %u %u %u %llu %llu %llu %llu %llu %llu %llu %llu %lld %llu %llu %llu\n",uid,pid,tid,@sequence[tid],
    (uint64)$stat->dev,(uint64)$stat->ino,(uint64)$stat->mode,(uint64)$stat->nlink,
    (uint64)$stat->uid.val,(uint64)$stat->gid.val,(uint64)$stat->rdev,(int64)$stat->size,
    (uint64)$stat->blksize,(uint64)$stat->blocks,(uint64)$stat->result_mask);
   printf("K %u %u %u %llu %lld %lld %lld %lld %lld %lld\n",uid,pid,tid,@sequence[tid],
    (int64)$stat->atime.tv_sec,(int64)$stat->atime.tv_nsec,(int64)$stat->mtime.tv_sec,
    (int64)$stat->mtime.tv_nsec,(int64)$stat->ctime.tv_sec,(int64)$stat->ctime.tv_nsec);
   @emitted=sum(2);
  }
  printf("L %u %u %u %llu %lld\n",uid,pid,tid,@sequence[tid],$status);
  @emitted=sum(1); @fstat_active[tid]=(uint64)0;
 }
}
kprobe:path_i*nit / ((uid >= 2000 && uid < 6096) || uid == 1999) && @seen[tid] / {
 @base_depth[tid]=@base_depth[tid]+1;
 $depth=@base_depth[tid];
 if ($depth<=16) {
  @base_seq[tid,$depth]=(uint64)0;
  $nd=(struct nameidata *)arg0;
  if ((uint64)$nd->pathname==@original_name[tid]) {
   @base_count[tid]=@base_count[tid]+1;
   @base_nd[tid,$depth]=arg0; @base_seq[tid,$depth]=@sequence[tid];
   @base_attempt[tid,$depth]=@base_count[tid];
   $task=(struct task_struct *)curtask;
   printf("Q %u %u %u %llu %llu %llu %lld %llu %llu %llu\n",uid,pid,tid,@number[tid],@sequence[tid],
    @base_count[tid],(int64)$nd->dfd,(uint64)(uint32)arg1,
    (uint64)$task->nsproxy->mnt_ns->ns.inum,(uint64)$task->cred->user_ns->ns.inum);
   @emitted=sum(1);
  }
 } else { printf("lookup base nesting limit exceeded\n"); @emitted=sum(1); }
}
kretprobe:path_i*nit / ((uid >= 2000 && uid < 6096) || uid == 1999) && @seen[tid] / {
 $depth=@base_depth[tid];
 if ($depth>0 && $depth<=16 && @base_seq[tid,$depth]>0 && @base_seq[tid,$depth]==@sequence[tid]) {
  $nd=(struct nameidata *)@base_nd[tid,$depth];
  $attempt=@base_attempt[tid,$depth];
  $status=(int64)0;
  if ((uint64)retval>=(uint64)0xfffffffffffff001) { $status=(int64)retval; }
  printf("R %u %u %u %llu %llu %llu %lld\n",uid,pid,tid,@number[tid],@sequence[tid],$attempt,$status);
  @emitted=sum(1);
  if ($status==0) {
   if ((uint64)retval!=@original_name[tid]) { printf("lookup base pathname changed\n"); @emitted=sum(1); }
   else {
    $inode=$nd->inode; $dentry=$nd->path.dentry;
    if ((uint64)$inode==0 || (uint64)$dentry==0 || (uint64)$nd->path.mnt==0) {
     printf("lookup base object missing\n"); @emitted=sum(1);
    } else {
     printf("C %u %u %u %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu\n",uid,pid,tid,@number[tid],@sequence[tid],$attempt,
      (uint64)$inode->i_sb->s_dev,(uint64)$inode->i_ino,(uint64)$inode->i_mode,
      (uint64)$dentry,(uint64)$nd->path.mnt,(uint64)$inode->i_generation,
      (uint64)$nd->seq,(uint64)$dentry->d_seq.seqcount.sequence,(uint64)$nd->flags,(uint64)$nd->state);
     @emitted=sum(1);
    }
   }
  }
  @base_seq[tid,$depth]=(uint64)0;
 }
 if ($depth>0) { @base_depth[tid]=$depth-1; }
 else { printf("lookup base return without entry\n"); @emitted=sum(1); }
}
__COPY_METADATA_PROBES__
kretprobe:getname_flags*
/ ((uid >= 2000 && uid < 6096) || uid == 1999) && (uint64)retval > 4095 && (uint64)retval < (uint64)0xfffffffffffff001 / {
 if (@ready[tid]>0 && @ready[tid]==@sequence[tid]) {
  $name=((struct observer_filename *)retval)->name;
  if (@original_name[tid]!=(uint64)$name) { printf("foreign kernel pathname pointer\n"); }
  $length=@length[tid];
  $owner_uid=uid; $owner_pid=pid; $owner_tid=tid;
  $call=@number[tid]; $entry=@sequence[tid];
  $offset=(uint64)0;
  $chunks=(uint64)0;
  if ($length>0 && $length<=4096) {
   while ($offset+63<=$length) {
    printf("N %u %u %u %lld %llu %llu %llu %rh\n",$owner_uid,$owner_pid,$owner_tid,(int64)$call,$entry,
     $offset,$length,buf($name+$offset,63));
    $chunks=$chunks+1; $offset=$offset+63;
   }
   if ($offset<$length) {
    $size=$length-$offset;
    if ($size>0 && $size<63) {
     printf("N %u %u %u %lld %llu %llu %llu %rh\n",$owner_uid,$owner_pid,$owner_tid,(int64)$call,$entry,
      $offset,$length,buf($name+$offset,$size));
     $chunks=$chunks+1;
    } else { printf("invalid kernel pathname tail\n"); $chunks=$chunks+1; }
   }
  } else {
   printf("invalid kernel pathname length\n"); $chunks=1;
  }
  @emitted=sum($chunks);
  @ready[tid]=(uint64)0;
  @seen[tid] = 1;
 }
}
kretprobe:getname_flags*
/ ((uid >= 2000 && uid < 6096) || uid == 1999) && (uint64)retval > 4095 && (uint64)retval < (uint64)0xfffffffffffff001 / {
 $name = ((struct observer_filename *)retval)->name;
 $prefix = *(uint64 *)$name;
 if ((@number[tid]==2 || @number[tid]==4 || @number[tid]==6 || @number[tid]==21 ||
      @number[tid]==89 || @number[tid]==257 || @number[tid]==262 || @number[tid]==267 ||
      @number[tid]==269 || @number[tid]==332 || @number[tid]==439) &&
     (($prefix==0x6170736b726f772f && *(uint16 *)($name+8)==0x6563 && *(uint8 *)($name+10)==47) ||
      (uid==1999 && $prefix==0x79732f636f72702f) ||
      $prefix==0x6573626f2d69632f ||
      ($prefix==0x2e62696c2f637273 && *(uint16 *)($name+8)==0x7372 && *(uint8 *)($name+10)==0) ||
      ($prefix==0x6e69616d2f637273 && *(uint16 *)($name+8)==0x722e && *(uint16 *)($name+10)==0x73) ||
      ($prefix==0x73722e646c697562 && *(uint8 *)($name+8)==0))) {
  $length=(uint64)0;
  $word=*(uint64 *)$name;
  while ($length<4096 && (($word-(uint64)0x0101010101010101) & ~$word & (uint64)0x8080808080808080)==0) {
   $length=$length+8;
   if ($length<4096) { $word=*(uint64 *)($name+$length); }
  }
  if ($length<4096) {
   $zeros=($word-(uint64)0x0101010101010101) & ~$word & (uint64)0x8080808080808080;
   if (($zeros & (uint64)0xffffffff)==0) { $length=$length+4; $zeros=$zeros>>32; }
   if (($zeros & (uint64)0xffff)==0) { $length=$length+2; $zeros=$zeros>>16; }
   if (($zeros & (uint64)0xff)==0) { $length=$length+1; }
  }
  @length[tid]=$length;
  @original_name[tid]=(uint64)$name;
  @ready[tid]=@sequence[tid];
  if (@number[tid]==262 || @number[tid]==332) {
   $task=(struct task_struct *)curtask;
   printf("U %u %u %u %llu %llu %llu %lld %llu %llu %llu %llu %llu\n",uid,pid,tid,@sequence[tid],@number[tid],
    @path_fd[tid],(int64)(int32)@path_fd[tid],@path_flags[tid],(uint64)(uint32)@path_flags[tid],
    @path_mask[tid],(uint64)(uint32)@path_mask[tid],(uint64)$task->cred->user_ns->ns.inum);
   @emitted=sum(1);
  }
 }
}
rawtracepoint:sys_exit / (uid >= 2000 && uid < 6096) || uid == 1999 / {
 if (@number[tid]==5) {
  printf("Z %u %u %u %llu %lld\n",uid,pid,tid,@sequence[tid],(int64)arg1);
  @emitted=sum(1);
 }
 if ((@number[tid]==262 || @number[tid]==332) && @seen[tid]) {
  printf("Z %u %u %u %llu %lld\n",uid,pid,tid,@sequence[tid],(int64)arg1);
  @emitted=sum(1);
 }
 if (@number[tid]==217) {
  printf("G %u %u %u %llu %lld\n",uid,pid,tid,@sequence[tid],(int64)arg1);
  @emitted=sum(1);
 }
 if (@seen[tid]) {
  printf("E %u %u %u %lld %llu %lld\n",uid,pid,tid,(int64)@number[tid],@sequence[tid],(int64)arg1);
  @emitted = sum(1);
  @seen[tid]=(uint64)0;
 }
}
END { clear(@seen); clear(@ready); clear(@original_name); clear(@length); clear(@sequence); clear(@number);
 clear(@directory_attempt); clear(@directory_active); clear(@directory_context);
 clear(@fstat_active); clear(@fstat_stat); clear(@copy_active); clear(@path_fd); clear(@path_flags); clear(@path_mask);
 clear(@base_depth); clear(@base_count); clear(@base_seq); clear(@base_nd); clear(@base_attempt); }
'''
_COPY_BASIC=r'''
 printf("J %u %u %u %llu %llu %llu %llu %llu %llu %llu %llu %lld %llu %llu %llu\n",uid,pid,tid,@sequence[tid],
  (uint64)$stat->dev,(uint64)$stat->ino,(uint64)$stat->mode,(uint64)$stat->nlink,
  (uint64)$stat->uid.val,(uint64)$stat->gid.val,(uint64)$stat->rdev,(int64)$stat->size,
  (uint64)$stat->blksize,(uint64)$stat->blocks,(uint64)$stat->result_mask);
 printf("K %u %u %u %llu %lld %lld %lld %lld %lld %lld\n",uid,pid,tid,@sequence[tid],
  (int64)$stat->atime.tv_sec,(int64)$stat->atime.tv_nsec,(int64)$stat->mtime.tv_sec,
  (int64)$stat->mtime.tv_nsec,(int64)$stat->ctime.tv_sec,(int64)$stat->ctime.tv_nsec);
'''
_COPY_EXTRA=r'''
 printf("O %u %u %u %llu %llu %llu %lld %lld %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu\n",uid,pid,tid,@sequence[tid],
  (uint64)$stat->attributes,(uint64)$stat->attributes_mask,(int64)$stat->btime.tv_sec,(int64)$stat->btime.tv_nsec,
  (uint64)$stat->mnt_id,(uint64)$stat->change_cookie,(uint64)$stat->subvol,(uint64)$stat->dio_mem_align,
  (uint64)$stat->dio_offset_align,(uint64)$stat->dio_read_offset_align,(uint64)$stat->atomic_write_unit_min,
  (uint64)$stat->atomic_write_unit_max,(uint64)$stat->atomic_write_unit_max_opt,(uint64)$stat->atomic_write_segments_max);
'''
_COPY_TEMPLATE=r'''
kprobe:__FUNCTION__ / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==__CALL__ && @seen[tid] / {
 @copy_active[tid]=@sequence[tid];
 $stat=(struct kstat *)arg0; $task=(struct task_struct *)curtask;
 printf("P %u %u %u %llu %u %llu\n",uid,pid,tid,@sequence[tid],__KIND__,(uint64)$task->cred->user_ns->ns.inum);
 __BASIC__
 __EXTRA__
 @emitted=sum(__COUNT__);
}
kretprobe:__FUNCTION__ / ((uid >= 2000 && uid < 6096) || uid == 1999) && @number[tid]==__CALL__ && @seen[tid] / {
 if (@copy_active[tid]>0 && @copy_active[tid]==@sequence[tid]) {
  printf("Y %u %u %u %llu %lld\n",uid,pid,tid,@sequence[tid],(int64)(int32)retval);
  @emitted=sum(1); @copy_active[tid]=(uint64)0;
 }
}
'''
_PROGRAM=_PROGRAM.replace('__COPY_METADATA_PROBES__','\n'.join(_COPY_TEMPLATE
    .replace('__FUNCTION__',function).replace('__CALL__',str(call)).replace('__KIND__',str(kind))
    .replace('__BASIC__',_COPY_BASIC).replace('__EXTRA__',_COPY_EXTRA if kind else '').replace('__COUNT__',str(3+kind))
    for function,call,kind in (('cp*_new_stat',262,0),('cp*_statx',332,1))))
_WITNESS=r'''
import ctypes,errno,os,sys
libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
result=libc.syscall(ctypes.c_long(257),ctypes.c_long(-100),ctypes.c_char_p(os.fsencode(sys.argv[1])),
 ctypes.c_long(os.O_RDONLY|os.O_CLOEXEC),ctypes.c_long(0))
assert result==-1 and ctypes.get_errno()==errno.ENOENT
descriptor=os.open('/proc/sys/kernel/random',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
try:
 metadata=ctypes.create_string_buffer(144)
 assert libc.syscall(ctypes.c_long(5),ctypes.c_long(descriptor),metadata)==0
 assert libc.syscall(ctypes.c_long(262),ctypes.c_long(-100),ctypes.c_char_p(b'/proc/sys/kernel/random'),metadata,ctypes.c_int(0))==0
 extended=ctypes.create_string_buffer(256)
 assert libc.syscall(ctypes.c_long(332),ctypes.c_long(-100),ctypes.c_char_p(b'/proc/sys/kernel/random'),ctypes.c_uint(0),ctypes.c_uint(8191),extended)==0
 buffer=ctypes.create_string_buffer(288)
 for _ in range(32):
  result=libc.syscall(ctypes.c_long(217),ctypes.c_long(descriptor),buffer,ctypes.c_uint(288))
  assert result>=0
  if result==0:break
 else:raise AssertionError('directory witness did not reach EOF')
finally:os.close(descriptor)
'''


_OPEN_PROGRAM=r'''
kprobe:do_file_o*pen
/ ((uid>=2000 && uid<6096) || uid==1999) && @seen[tid] &&
  (@number[tid]==2 || @number[tid]==257 || @number[tid]==437) / {
 $name=(struct filename *)arg1;
 if ((uint64)$name->name==@original_name[tid]) {
  $op=(struct open_flags *)arg2; $task=(struct task_struct *)curtask;
  printf("W %u %u %u %llu %llu %lld %llu %llu %llu %llu %llu %llu %llu\n",
   uid,pid,tid,@number[tid],@sequence[tid],(int64)(int32)arg0,
   (uint64)(uint32)$op->open_flag,(uint64)$op->mode,(uint64)(uint32)$op->acc_mode,
   (uint64)(uint32)$op->intent,(uint64)(uint32)$op->lookup_flags,
   (uint64)$task->cred->user_ns->ns.inum,(uint64)$task->nsproxy->mnt_ns->ns.inum);
  @emitted=sum(1);
 } else { printf("kernel open copied-name mismatch\n"); @emitted=sum(1); }
}
kprobe:fd_instal*l
/ ((uid>=2000 && uid<6096) || uid==1999) && @seen[tid] &&
  (@number[tid]==2 || @number[tid]==257 || @number[tid]==437) / {
 $file=(struct file *)arg1; $inode=$file->f_inode; $task=(struct task_struct *)curtask;
 if ((uint64)$inode>0 && (uint64)$file->f_path.dentry>0 && (uint64)$file->f_path.mnt>0) {
  printf("V %u %u %u %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu\n",
   uid,pid,tid,@number[tid],@sequence[tid],(uint64)(uint32)arg0,
   (uint64)$file->f_flags,(uint64)$file->f_mode,(uint64)$inode->i_sb->s_dev,
   (uint64)$inode->i_ino,(uint64)$inode->i_mode,(uint64)$inode->i_generation,
   (uint64)$file->f_path.dentry,(uint64)$file->f_path.mnt,
   (uint64)$task->cred->user_ns->ns.inum,(uint64)$task->nsproxy->mnt_ns->ns.inum);
  @emitted=sum(1);
 } else { printf("kernel installed file missing\n"); @emitted=sum(1); }
}
'''


def observation_program(path_scope):
    if path_scope=='fixture':return _PROGRAM
    if path_scope!='all':raise ObservationUnavailable('unknown pathname scope')
    # Keep the original syscall/credential gates and witness selection. General
    # unit paths retain their literal spelling; resolution belongs to the index.
    marker='     (($prefix=='
    if _PROGRAM.count(marker)!=1:raise ObservationUnavailable('pathname gate changed')
    return _PROGRAM.replace(marker,'     ((uid>=2000 && uid<6096) || ($prefix==').replace('@number[tid]==439)', '@number[tid]==439 || @number[tid]==437)')+_OPEN_PROGRAM


class Observer:
    def __init__(self,admission,*,deadline,path_scope='fixture'):
        if (os.getresuid()!=(0,0,0) or os.getresgid()!=(0,0,0) or admission is None
                or admission.owner!=os.getpid() or not time.monotonic()<deadline<=time.monotonic()+901):
            raise ObservationUnavailable('private root admission required')
        cache.validate_binding(admission.binding)
        if cache._read_record(admission.target.parent/'control.json')!={'version':1,
                'binding':admission.binding,'active_command':admission.command}:
            raise ObservationUnavailable('foreign original command')
        self.admission=admission;self.deadline=deadline;self.owner=os.getpid();self.child=None
        self.root=admission.target/('.kernel-observer-probe-'+str(uuid.uuid4()));self.root.mkdir(mode=0o700)
        self.control={'version':1,'kind':'kernel-copied-filename-probe','binding':admission.binding,
            'command':admission.command,'shipping_authority':False}
        cache._write_record(self.root/'control.json',self.control)
        self.witness='/ci-observer-witness-'+str(uuid.uuid4())+'-'+'x'*192;self.witness_pid=None
        self.path_scope=path_scope;self.program=observation_program(path_scope);self.probe_count=2+sum(probe_roles(path_scope).values())
        self.stdout=self.root/'stdout';self.stderr=self.root/'stderr';self.output=None;self.error=None
        self.report=None;self.census_before=None;self.census_after=None;self.census_error=False
        self.birth=None;self.kernel_address=None
        self.directory_path=Path('/proc/sys/kernel/random');self.directory_members=None
        self.user_namespace=Path('/proc/self/ns/user').stat().st_ino
        self.mount_namespace=Path('/proc/self/ns/mnt').stat().st_ino

    def __enter__(self):
        try:self.start();return self
        except BaseException:self.stop();raise

    def __exit__(self,*unused):self.stop()

    def _data(self):
        if self.stdout.stat().st_size>MAX_BYTES or self.stderr.stat().st_size>MAX_BYTES:
            raise ObservationUnavailable('observation limit exceeded')
        return self.stdout.read_bytes()

    def start(self):
        # This private guest mounts sysfs during boot. bpftrace also needs
        # tracefs for probe discovery and successful original-probe detach.
        tracefs=Path('/sys/kernel/tracing')
        if not (tracefs/'kprobe_events').is_file():
            subprocess.run(['/usr/bin/mount','-t','tracefs','tracefs',str(tracefs)],
                stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,
                check=True,timeout=min(5,max(.001,self.deadline-time.monotonic())))
        if not (tracefs/'kprobe_events').is_file():
            raise OSError(errno.ENOENT,'private kernel trace filesystem unavailable')
        self.kernel_address=_function_address(self.path_scope)
        self.directory_members={os.fsencode(p.name):(p.stat().st_ino,4 if p.is_dir() else 8)
            for p in self.directory_path.iterdir()}
        binary=Path('/usr/bin/bpftrace');info=binary.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid!=0 or info.st_mode&0o022:
            raise ObservationUnavailable('untrusted kernel observer')
        descriptor=os.open(binary,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|getattr(os,'O_NOATIME',0))
        try:
            digest=hashlib.sha256();count=0
            while True:
                block=os.read(descriptor,1024*1024)
                if not block:break
                count+=len(block)
                if count>64*1024**2:raise ObservationUnavailable('unbounded observer executable')
                digest.update(block)
            self.binary_sha256=digest.hexdigest()
        finally:os.close(descriptor)
        self.output=self.stdout.open('xb');self.error=self.stderr.open('xb')
        os.chmod(self.stdout,0o600);os.chmod(self.stderr,0o600)
        self.child=subprocess.Popen([str(binary),'-v','-kk','-q','-B','none','-f','json','-e',self.program],
            stdin=subprocess.DEVNULL,stdout=self.output,stderr=self.error,start_new_session=True,
            env={'PATH':os.defpath,'BPFTRACE_MAX_STRLEN':'64','BPFTRACE_STR_TRUNC_TRAILER':'',
                '__BPFTRACE_NOTIFY_PROBES_ATTACHED':'1','BPFTRACE_LOG_SIZE':str(3*1024**2),'HOME':'/tmp'})
        self.birth=_birth(self.child.pid,self.owner)
        deadline=min(self.deadline,time.monotonic()+10);origin=os.getpid()
        def drop():
            libc=ctypes.CDLL(None,use_errno=True)
            if libc.prctl(38,1,0,0,0)!=0:os._exit(125)
            os.setgroups([]);os.setgid(1999);os.setuid(1999)
            if libc.prctl(1,signal.SIGKILL,0,0,0)!=0 or os.getppid()!=origin:os._exit(125)
        while time.monotonic()<deadline:
            if self.child.poll() is not None:
                raise OSError(startup_errno(self.stderr.read_bytes()),'private kernel observer startup failed')
            if self.stderr.stat().st_size>MAX_BYTES:raise ObservationUnavailable('unbounded startup diagnostics')
            if b'__BPFTRACE_NOTIFY_PROBES_ATTACHED\n' not in self.stderr.read_bytes():
                time.sleep(.02);continue
            if self.census_before is None:
                try:self.census_before=kernel_census(self.child,self.owner,self.birth,self.kernel_address,
                    self.root/'census-descriptors-before.json',probe_count=self.probe_count)
                except (OSError,ObservationUnavailable) as error:
                    cache._write_record(self.root/'census-failure.json',{'phase':'before-witness',
                        'category':type(error).__name__,'errno':getattr(error,'errno',None),
                        'reason':str(error) if isinstance(error,ObservationUnavailable) else None})
                    raise
                cache._write_record(self.root/'census-before.json',self.census_before)
                validate_census({'before':self.census_before,'after':self.census_before},path_scope=self.path_scope)
            witness=subprocess.Popen([sys.executable,'-I','-S','-c',_WITNESS,self.witness],
                stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,
                env={'PATH':os.defpath},preexec_fn=drop)
            try:status=witness.wait(timeout=min(3,max(.001,deadline-time.monotonic())))
            finally:
                if witness.poll() is None:witness.kill();witness.wait(timeout=2)
            if status!=0:raise ObservationUnavailable('original witness unavailable')
            for _ in range(10):
                paths,queries,metadata=decode(self._data(),complete=False,with_directories=True,with_metadata=True,path_scope=self.path_scope)
                if any(e['uid']==1999 and e['pid']==witness.pid and e['syscall']==257
                        and e['result']==-errno.ENOENT and e['path']==self.witness.encode()
                        for e in paths):
                    try:
                        directory_witness(queries,witness.pid,1999,self.directory_path,self.directory_members)
                        metadata_witness(metadata,witness.pid,1999,self.directory_path,self.user_namespace)
                        path_metadata_witness(metadata,witness.pid,1999,self.directory_path,self.user_namespace,
                            lookup=b'/proc/sys/kernel/random',flags=0)
                        lookup_base_witness(paths,witness.pid,1999,self.witness.encode(),257,-errno.ENOENT,
                            dfd=-100,user_namespace=self.user_namespace,mount_namespace=self.mount_namespace)
                    except ObservationUnavailable:pass
                    else:self.witness_pid=witness.pid;return
                time.sleep(.01)
        raise OSError(errno.ETIMEDOUT,'kernel observation readiness unproven')

    def stop(self):
        if self.child is not None and self.child.poll() is None:
            try:
                if self.census_before is not None and self.census_after is None:
                    try:
                        self.census_after=kernel_census(self.child,self.owner,self.birth,self.kernel_address,
                            self.root/'census-descriptors-after.json',probe_count=self.probe_count)
                        cache._write_record(self.root/'census-after.json',self.census_after)
                        validate_census({'before':self.census_before,'after':self.census_after},path_scope=self.path_scope)
                    except (OSError,ObservationUnavailable) as error:
                        self.census_error=True
                        try:cache._write_record(self.root/'census-failure.json',
                            {'phase':'before-observer-detach','category':type(error).__name__,'errno':getattr(error,'errno',None)})
                        except OSError:pass  # Failure remains fatal even when diagnostics cannot be saved.
            finally:
                self.child.send_signal(signal.SIGINT)
                try:self.child.wait(timeout=5)
                except subprocess.TimeoutExpired:self.child.kill();self.child.wait(timeout=2)
        for stream in (self.output,self.error):
            if stream is not None and not stream.closed:stream.close()

    def capture(self):
        """Drain the original collector without claiming complete unit inputs."""
        self.stop()
        if self.child is None or self.child.returncode!=0 or self.witness_pid is None:
            raise ObservationUnavailable('original kernel observer did not finish')
        if self.census_error:raise ObservationUnavailable('original kernel census failed')
        census=validate_census({'before':self.census_before,'after':self.census_after},path_scope=self.path_scope)
        events,queries,metadata=decode(self._data(),with_directories=True,with_metadata=True,path_scope=self.path_scope)
        if b'lost' in self.stderr.read_bytes().lower():raise ObservationUnavailable('lost kernel observations')
        directory_witness(queries,self.witness_pid,1999,self.directory_path,self.directory_members)
        metadata_witness(metadata,self.witness_pid,1999,self.directory_path,self.user_namespace)
        if cache._read_record(self.root/'control.json')!=self.control:
            raise ObservationUnavailable('foreign observation allocation')
        return events,queries,metadata,census

    def finish(self,unit_pid,compilers,directories,cursor_directory,stat_file,statx_digest,source_root,mount_namespace,content_probe,compiler_probe):
        events,queries,metadata,census=self.capture()
        if len(directories)!=4:raise ObservationUnavailable('original directory scopes missing')
        summaries=[directory_witness(queries,*scope) for scope in directories]
        directory_cursor_witness(queries,unit_pid,2004,cursor_directory)
        metadata_witness(metadata,self.witness_pid,1999,self.directory_path,self.user_namespace)
        metadata_witness(metadata,unit_pid,2004,stat_file,self.user_namespace)
        if not cache._digest(statx_digest):raise ObservationUnavailable('statx user-copy witness missing')
        path_metadata_witness(metadata,unit_pid,2004,stat_file.parent/'src/lib.rs',self.user_namespace,
            lookup=b'src/lib.rs',flags=256,dirpath=stat_file.parent,digest=statx_digest)
        stats=[q for q in metadata if q['uid']==2004 and q['pid']==q['tid']==unit_pid
            and q['user_namespace']==self.user_namespace]
        for outcome,base in ((0,source_root),(-errno.ENOENT,stat_file.parent)):
            lookup_base_witness(events,unit_pid,2004,b'src/main.rs',262,outcome,dfd=-100,
                user_namespace=self.user_namespace,mount_namespace=mount_namespace,expected=base)
        descriptor_calls=[e for e in events if e['pid']==e['tid']==unit_pid and e['uid']==2004
            and e['syscall']==262 and e['path']==b'src/lib.rs' and e['result']==0]
        if not any(e['lookup_attempts'] and e['lookup_attempts'][0]['dfd']>=0 for e in descriptor_calls):
            raise ObservationUnavailable('original descriptor lookup base missing')
        for event in descriptor_calls:
            if event['lookup_attempts'] and event['lookup_attempts'][0]['dfd']>=0:
                lookup_base_witness([event],unit_pid,2004,b'src/lib.rs',262,0,dfd=event['lookup_attempts'][0]['dfd'],
                    user_namespace=self.user_namespace,mount_namespace=mount_namespace,expected=stat_file.parent)
        lookup_base_witness(events,unit_pid,2004,b'src/main.rs',262,-errno.EBADF,dfd=-1,
            user_namespace=self.user_namespace,mount_namespace=mount_namespace,init_result=-errno.EBADF)
        if not any(q['fd']==-1 and q['vfs_result']==q['result']==-errno.EBADF and q['stat'] is None for q in stats):
            raise ObservationUnavailable('original invalid-fd fstat witness missing')
        if not any(q['fd']>=0 and q['vfs_result']==0 and q['result']==-errno.EFAULT
                and q['stat'] is not None and q['stat']['inode']==stat_file.stat().st_ino for q in stats):
            raise ObservationUnavailable('original failed fstat copy witness missing')
        if not any(q['syscall']==332 and q.get('path')==b'src/lib.rs'
                and q.get('copy_result')==q['result']==-errno.EFAULT and q['stat'] is not None for q in stats):
            raise ObservationUnavailable('original failed statx copy witness missing')
        if not any(q['syscall']==332 and q.get('path')==b'/workspace/target/deps/missing'
                and q['result']==-errno.ENOENT and q.get('copy_result') is None and q['stat'] is None for q in stats):
            raise ObservationUnavailable('original statx lookup failure witness missing')
        if summaries[0]['reads']<3 or summaries[0]['rejected_attempts']<1:
            raise ObservationUnavailable('original directory short-read witness missing')
        selected=[e for e in events if e['uid']==2004 and e['pid']==unit_pid and e['tid']==unit_pid]
        if (not any(e['syscall']==257 and e['result']>=0 and e['path']==b'/workspace/target/deps/input' for e in selected)
                or not any(e['syscall']==257 and e['result']==-errno.ENOENT and e['path']==b'/workspace/target/deps/missing' for e in selected)):
            raise ObservationUnavailable('kernel lookup witnesses missing')
        if not any(e['syscall']==257 and e['result']==-errno.ENOENT
                and e['path']==b'/workspace/'+b'x/'*2042 for e in selected):
            raise ObservationUnavailable('full length kernel pathname witness missing')
        if (not any(e['syscall']==257 and e['result']>=0
                and e['path']==b'/workspace/target/deps/raw-\xff' for e in selected)
                or not any(e['syscall']==257 and e['result']==-errno.ENOENT
                    and e['path']==b'/workspace/target/deps/missing-\xff\n ' for e in selected)):
            raise ObservationUnavailable('binary kernel pathname witnesses missing')
        sources={'build-script':b'build.rs','library':b'src/lib.rs','binary':b'src/main.rs'}
        if set(compilers)!=set(sources):raise ObservationUnavailable('compiler scope unavailable')
        for role,path in sources.items():
            pid,uid=compilers[role]
            if not any(e['pid']==pid and e['uid']==uid and e['syscall']==257 and e['result']>=0
                    and e['path'] in (path,b'/workspace/'+path) for e in events):
                raise ObservationUnavailable('original compiler source lookup missing')
        library_pid,library_uid=compilers['library']
        if not any(e['pid']==library_pid and e['uid']==library_uid and e['syscall']==257
                and e['result']>=0 and e['path'].endswith(b'/out/generated.rs') for e in events):
            raise ObservationUnavailable('original generated input lookup missing')
        content=content_probe.observe_content(events,metadata,unit_pid,self.user_namespace,mount_namespace)
        directory_selection=content_probe.observe_directories(queries,unit_pid,self.user_namespace,mount_namespace)
        descriptor_selection=content_probe.observe_descriptors(metadata,unit_pid,self.user_namespace,mount_namespace)
        compiler_probe.record_invalidations(events,metadata,self.user_namespace)
        self.report={**self.control,'kernel_release':os.uname().release,'unit_pid':unit_pid,'unit_uid':2004,
            'program_sha256':hashlib.sha256(self.program.encode()).hexdigest(),'binary_sha256':self.binary_sha256,
            'witness_observed':True,'positive_query_observed':True,'negative_query_observed':True,'max_path_query_observed':True,
            'binary_path_query_observed':True,
            'compiler_queries_observed':True,'library_generated_query_observed':True,
            'after_kernel_copy':True,'events':len(events),'original_observer_cleanup':True,
            'directory_reads':len(queries),'directory_entries':sum(len(q['entries']) for q in queries),
            'directory_witness_observed':True,'binary_directory_name_observed':True,
            'directory_short_read_observed':True,'directory_eof_observed':True,'cargo_directory_queries_observed':True,
            'directory_cursor_query_observed':True,
            'metadata_queries':len(metadata),'fstat_query_observed':True,
            'fstat_invalid_fd_observed':True,'fstat_failed_copy_observed':True,
            'path_metadata_query_observed':True,'statx_copy_verified':True,
            'path_metadata_failed_copy_observed':True,'path_metadata_lookup_failure_observed':True,
            'lookup_base_attempts':sum(len(e['lookup_attempts']) for e in events),
            'lookup_base_query_observed':True,'lookup_changed_cwd_observed':True,'lookup_invalid_base_observed':True,
            'content_selection':content,
            'directory_selection':directory_selection,
            'descriptor_selection':descriptor_selection,
            'filesystem_coverage_complete':False,'missed_probe_census_complete':True,'probe_census':census,
            'input_closure_complete':False,'allocation_removed':False}
        if cache._read_record(self.root/'control.json')!=self.control:
            raise ObservationUnavailable('foreign observation allocation')
        import shutil
        shutil.rmtree(self.root);self.report['allocation_removed']=True
        return self.report


def validate(value,binding,command):
    keys={'version','kind','binding','command','shipping_authority','kernel_release','unit_pid','unit_uid',
        'program_sha256','binary_sha256','witness_observed','positive_query_observed','negative_query_observed',
        'after_kernel_copy','events','original_observer_cleanup','filesystem_coverage_complete',
        'missed_probe_census_complete','input_closure_complete','allocation_removed',
        'compiler_queries_observed','library_generated_query_observed','max_path_query_observed','probe_census',
        'binary_path_query_observed','content_selection','directory_selection','descriptor_selection'}
    keys|={'directory_reads','directory_entries','directory_witness_observed','binary_directory_name_observed',
        'directory_short_read_observed','directory_eof_observed','cargo_directory_queries_observed','directory_cursor_query_observed'}
    keys|={'metadata_queries','fstat_query_observed','fstat_invalid_fd_observed','fstat_failed_copy_observed'}
    keys|={'path_metadata_query_observed','statx_copy_verified','path_metadata_failed_copy_observed','path_metadata_lookup_failure_observed'}
    keys|={'lookup_base_attempts','lookup_base_query_observed','lookup_changed_cwd_observed','lookup_invalid_base_observed'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['kind']!='kernel-copied-filename-probe' or value['binding']!=binding or value['command']!=command
            or type(value['unit_pid']) is not int or not 0<value['unit_pid']<2**31
            or type(value['unit_uid']) is not int or value['unit_uid']!=2004
            or type(value['events']) is not int or not 3<=value['events']<=20000
            or any(type(value[key]) is not int or not 1<=value[key]<=40000 for key in ('directory_reads','directory_entries','metadata_queries','lookup_base_attempts'))
            or type(value['kernel_release']) is not str or re.fullmatch(r'[A-Za-z0-9+_.-]{1,128}',value['kernel_release']) is None
            or not cache._digest(value['program_sha256']) or not cache._digest(value['binary_sha256'])
            or any(value[key] is not True for key in ('witness_observed','positive_query_observed',
                'negative_query_observed','after_kernel_copy','original_observer_cleanup','allocation_removed',
                'binary_path_query_observed',
                'compiler_queries_observed','library_generated_query_observed','max_path_query_observed','missed_probe_census_complete',
                'directory_witness_observed','binary_directory_name_observed','directory_short_read_observed',
                'directory_eof_observed','cargo_directory_queries_observed'))
            or any(value[key] is not True for key in ('fstat_query_observed','fstat_invalid_fd_observed','fstat_failed_copy_observed'))
            or any(value[key] is not True for key in ('path_metadata_query_observed','statx_copy_verified',
                'path_metadata_failed_copy_observed','path_metadata_lookup_failure_observed'))
            or any(value[key] is not True for key in ('lookup_base_query_observed','lookup_changed_cwd_observed','lookup_invalid_base_observed'))
            or value['directory_cursor_query_observed'] is not True
            or any(value[key] is not False for key in ('shipping_authority','filesystem_coverage_complete','input_closure_complete'))):
        raise ObservationUnavailable('kernel observation is incomplete or foreign')
    validate_census(value['probe_census'])
    content=value['content_selection']
    if (type(content) is not dict or set(content)!={'queries','binding_sha256','content_sha256','unrelated_sha256',
            'introduced_sha256','kernel_bases_bound','unrelated_content_unchanged','negative_lookup_changed_identity',
            'input_closure_complete','shipping_authority'} or type(content['queries']) is not int or content['queries']!=2
            or any(not cache._digest(content[key]) for key in ('binding_sha256','content_sha256','unrelated_sha256','introduced_sha256'))
            or content['content_sha256']!=content['unrelated_sha256'] or content['content_sha256']==content['introduced_sha256']
            or any(content[key] is not True for key in ('kernel_bases_bound','unrelated_content_unchanged','negative_lookup_changed_identity'))
            or content['input_closure_complete'] is not False or content['shipping_authority'] is not False):
        raise ObservationUnavailable('bound content selection unavailable')
    directories=value['directory_selection']
    if (type(directories) is not dict or set(directories)!={'queries','unbound_calls','binding_sha256',
            'content_sha256','unrelated_sha256','introduced_sha256','kernel_namespaces_bound',
            'rewound_scan_unbound','membership_changed_identity','input_closure_complete','shipping_authority'}
            or type(directories['queries']) is not int or not 5<=directories['queries']<=40000
            or type(directories['unbound_calls']) is not int or not 3<=directories['unbound_calls']<=40000
            or any(not cache._digest(directories[key]) for key in ('binding_sha256','content_sha256','unrelated_sha256','introduced_sha256'))
            or directories['content_sha256']!=directories['unrelated_sha256']
            or directories['content_sha256']==directories['introduced_sha256']
            or any(directories[key] is not True for key in ('kernel_namespaces_bound','rewound_scan_unbound','membership_changed_identity'))
            or directories['input_closure_complete'] is not False or directories['shipping_authority'] is not False):
        raise ObservationUnavailable('bound directory selection unavailable')
    descriptors=value['descriptor_selection']
    if (type(descriptors) is not dict or set(descriptors)!={'queries','unbound_calls','binding_sha256','metadata_sha256',
            'replacement_metadata_sha256','kernel_namespaces_bound','failed_calls_unbound','replacement_metadata_changed',
            'input_closure_complete','shipping_authority'}
            or type(descriptors['queries']) is not int or not 2<=descriptors['queries']<=40000
            or type(descriptors['unbound_calls']) is not int or not 2<=descriptors['unbound_calls']<=40000
            or any(not cache._digest(descriptors[key]) for key in ('binding_sha256','metadata_sha256','replacement_metadata_sha256'))
            or descriptors['metadata_sha256']==descriptors['replacement_metadata_sha256']
            or any(descriptors[key] is not True for key in ('kernel_namespaces_bound','failed_calls_unbound','replacement_metadata_changed'))
            or descriptors['input_closure_complete'] is not False or descriptors['shipping_authority'] is not False):
        raise ObservationUnavailable('bound descriptor selection unavailable')
    return value
