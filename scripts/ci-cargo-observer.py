#!/usr/bin/env python3
"""Private kernel-copied pathname feasibility probe, not an input-proof API.

Only selected lookup spellings and getdents64 actor results are observed. The
ten original kernel attachments require a bounded miss census. This does not establish full
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
    ('filldir64',0):1,('filldir64',1):1,('vfs_fstat',0):1,('vfs_fstat',1):1}
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


def validate_census(value):
    if type(value) is not dict or set(value)!={'before','after'}:
        raise ObservationUnavailable('kernel census unavailable')
    for sample in value.values():
        if (type(sample) is not dict or set(sample)!={'pid','birth','programs','links'}
                or type(sample['pid']) is not int or not 0<sample['pid']<2**31
                or type(sample['birth']) is not int or not 0<sample['birth']<2**64
                or type(sample['programs']) is not list or len(sample['programs'])!=PROBE_COUNT
                or type(sample['links']) is not list or len(sample['links'])!=PROBE_COUNT):
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
                        or (link['function'],link['flags']) not in PROBE_ROLES):
                    raise ObservationUnavailable('kernel return probe census failed')
                role=(link['function'],link['flags']);roles[role]=roles.get(role,0)+1
            else:raise ObservationUnavailable('unknown kernel attachment')
        if raw!={'sys_enter','sys_exit'} or roles!=PROBE_ROLES or linked!=set(programs):
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
                    or len(rows)!=1 or re.fullmatch(r'0[ \t]+(getname_flags|iterate_dir|filldir64|vfs_fstat)\+0x0/0x[1-9a-f][0-9a-f]{0,15}',rows[0]) is None):
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


def _function_address():
    found={name:[] for name in ('getname_flags','iterate_dir','filldir64','vfs_fstat')};size=0
    with Path('/proc/kallsyms').open('rb') as stream:
        for line in stream:
            size+=len(line)
            if size>64*1024**2:raise ObservationUnavailable('unbounded kernel symbol census')
            parts=line.split()
            if len(parts)==3 and parts[2] in (b'getname_flags',b'iterate_dir',b'filldir64',b'vfs_fstat') and parts[1] in (b't',b'T'):
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


def kernel_census(child,owner,birth,address,diagnostic):
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
        if len(data)>16384 or len(rows)>=32:raise ObservationUnavailable('unbounded original BPF descriptor census')
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
        descriptor,capacity=pair['B'];device,inode,mode,start_position=pair['I']
        status,position=pair['T'];result,=pair['G']
        if (descriptor>=2**31 or capacity>=2**32 or not 0<=device<2**32 or not 0<=inode<2**64
                or not 0<=mode<=65535 or not stat.S_ISDIR(mode) or not -2**31<=status<2**31
                or not -2**63<=start_position<2**63
                or not -2**63<=position<2**63 or not -4095<=result<=capacity
                or set(attempts)!=set(range(1,len(attempts)+1))):
            raise ObservationUnavailable('inconsistent directory call')
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


def metadata_queries(records,*,complete=True):
    """Decode original x86-64 fstat VFS data and actual syscall delivery.

    kstat fields remain raw, including internal dev_t and signed times. This
    observes fstat only, not general statx, path resolution or input closure.
    """
    if type(records) is not list or len(records)>40000:raise ObservationUnavailable('unbounded metadata observation')
    counts={'F':2,'H':2,'J':11,'K':6,'L':1,'Z':1};pairs={}
    for line in records:
        if type(line) is not str or len(line)>1024:raise ObservationUnavailable('unbounded metadata record')
        match=re.fullmatch(r'([FHJKLZ]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,19}) (.*)',line)
        if match is None:raise ObservationUnavailable('unknown metadata record')
        kind=match[1];uid,pid,tid,sequence=map(int,match.groups()[1:5])
        if (not (uid==1999 or 2000<=uid<6096) or pid>=2**31 or tid>=2**31 or sequence>=2**64):
            raise ObservationUnavailable('foreign metadata observation')
        if re.fullmatch(r'-?[0-9]{1,20}(?: -?[0-9]{1,20}){'+str(counts[kind]-1)+'}',match[6]) is None:
            raise ObservationUnavailable('unknown metadata fields')
        values=tuple(map(int,match[6].split()))
        if any(not -2**63<=v<2**64 for v in values):raise ObservationUnavailable('metadata value overflow')
        pair=pairs.setdefault((uid,pid,tid,sequence),{})
        if kind in pair:raise ObservationUnavailable('duplicate metadata record')
        pair[kind]=values
    queries=[]
    for (uid,pid,tid,sequence),pair in pairs.items():
        if not {'F','H','L','Z'}<=set(pair):
            if complete:raise ObservationUnavailable('incomplete metadata observation')
            continue
        raw_fd,fd=pair['F'];original_fd,user_namespace=pair['H'];vfs_result,=pair['L'];result,=pair['Z']
        if (not 0<=raw_fd<2**64 or not -2**31<=fd<2**31 or fd!=original_fd
                or fd!=((raw_fd+2**31)%2**32)-2**31 or not 0<user_namespace<2**32
                or not -4095<=vfs_result<=0 or not -4095<=result<=0
                or vfs_result<0 and result!=vfs_result):
            raise ObservationUnavailable('metadata syscall binding differs')
        expected={'F','H','J','K','L','Z'} if vfs_result==0 else {'F','H','L','Z'}
        if set(pair)!=expected:
            if complete:raise ObservationUnavailable('metadata result fields unavailable')
            continue
        fields=None
        if vfs_result==0:
            fields=dict(zip(STAT_FIELDS,pair['J']));fields.update(zip(STAT_TIMES,pair['K']))
            limits={'device':32,'inode':64,'mode':16,'nlink':32,'owner_uid':32,'owner_gid':32,
                'rdev':32,'blksize':32,'blocks':64,'result_mask':32}
            if (any(not 0<=fields[key]<2**bits for key,bits in limits.items())
                    or not -2**63<=fields['size']<2**63
                    or any(not -2**63<=fields[key]<2**63 for key in STAT_TIMES[::2])
                    or any(not 0<=fields[key]<10**9 for key in STAT_TIMES[1::2])):
                raise ObservationUnavailable('metadata fields outside original ABI')
        queries.append({'uid':uid,'pid':pid,'tid':tid,'sequence':sequence,'syscall':5,
            'raw_fd':raw_fd,'fd':fd,'user_namespace':user_namespace,'vfs_result':vfs_result,
            'result':result,'stat':fields,'delivered':result==0})
    return queries


def metadata_witness(queries,pid,uid,path,user_namespace):
    info=path.stat();expected={'device':(os.major(info.st_dev)<<20)|os.minor(info.st_dev),'inode':info.st_ino,
        'mode':info.st_mode,'nlink':info.st_nlink,'owner_uid':info.st_uid,'owner_gid':info.st_gid,
        'rdev':(os.major(info.st_rdev)<<20)|os.minor(info.st_rdev),'size':info.st_size,
        'blksize':info.st_blksize,'blocks':info.st_blocks}
    for name,value in (('atime',info.st_atime_ns),('mtime',info.st_mtime_ns),('ctime',info.st_ctime_ns)):
        expected[name+'_sec'],expected[name+'_nsec']=divmod(value,10**9)
    if not any(q['pid']==q['tid']==pid and q['uid']==uid and q['user_namespace']==user_namespace
            and q['fd']>=0 and q['vfs_result']==q['result']==0 and q['stat'] is not None
            and all(q['stat'][key]==value for key,value in expected.items()) for q in queries):
        raise ObservationUnavailable('original fstat metadata witness missing')


def decode(data,*,complete=True,with_directories=False,with_metadata=False):
    if not isinstance(data,bytes) or len(data)>MAX_BYTES:raise ObservationUnavailable('unbounded observation')
    if complete and (not data or not data.endswith(b'\n')):raise ObservationUnavailable('incomplete observation')
    if not complete and not data.endswith(b'\n'):data=data.rpartition(b'\n')[0]+b'\n'
    try:lines=data.decode('ascii').splitlines()
    except UnicodeError:raise ObservationUnavailable('unknown pathname encoding') from None
    pairs={};directories=[];metadata=[];count=None;records=0;loader_records=0
    for line in lines:
        if not line:continue
        if (re.fullmatch(r'Program ID: [1-9][0-9]{0,9}',line)
                or line=='Attaching to 1 functions'
                or line in ('The verifier log:','The verifier log: ')
                or re.fullmatch(r'processed [0-9]{1,10} insns \(limit 1000000\) max_states_per_insn [0-9]{1,10} total_states [0-9]{1,10} peak_states [0-9]{1,10} mark_read [0-9]{1,10}',line)):
            loader_records+=1
            if loader_records>48:raise ObservationUnavailable('unbounded loader summaries')
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
                    or type(record['data']['probes']) is not int or not 1<=record['data']['probes']<=16):
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
        if line[:2] in ('F ','H ','J ','K ','L ','Z '):
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
        if (pid>=2**31 or tid>=2**31 or call not in SYSCALLS or sequence>=2**64
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
        if (any(part in (b'',b'.',b'..') for part in parts[1:])
                or not (uid==1999 and re.fullmatch(rb'/ci-observer-witness-[a-z0-9-]+',path)
                    or 2000<=uid<6096 and (path.startswith(b'/workspace/')
                        or path in (b'src/lib.rs',b'src/main.rs',b'build.rs')))):
            raise ObservationUnavailable('foreign kernel pathname')
        events.append({'uid':uid,'pid':pid,'tid':tid,'syscall':call,'result':pair['E'],'path':path})
    queries=directory_queries(directories,complete=complete)
    stats=metadata_queries(metadata,complete=complete)
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
 @directory_context[tid]=arg1;
 @directory_active[tid]=@sequence[tid];
 printf("I %u %u %u %llu %llu %llu %llu %lld\n",uid,pid,tid,@sequence[tid],
  (uint64)$inode->i_sb->s_dev,(uint64)$inode->i_ino,(uint64)$inode->i_mode,(int64)$file->f_pos);
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
 printf("H %u %u %u %llu %lld %llu\n",uid,pid,tid,@sequence[tid],(int64)(int32)arg0,(uint64)$task->cred->user_ns->ns.inum);
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
 }
}
rawtracepoint:sys_exit / (uid >= 2000 && uid < 6096) || uid == 1999 / {
 if (@number[tid]==5) {
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
 clear(@fstat_active); clear(@fstat_stat); }
'''
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
 buffer=ctypes.create_string_buffer(288)
 for _ in range(32):
  result=libc.syscall(ctypes.c_long(217),ctypes.c_long(descriptor),buffer,ctypes.c_uint(288))
  assert result>=0
  if result==0:break
 else:raise AssertionError('directory witness did not reach EOF')
finally:os.close(descriptor)
'''


class Observer:
    def __init__(self,admission,*,deadline):
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
        self.program=_PROGRAM
        self.stdout=self.root/'stdout';self.stderr=self.root/'stderr';self.output=None;self.error=None
        self.report=None;self.census_before=None;self.census_after=None;self.census_error=False
        self.birth=None;self.kernel_address=None
        self.directory_path=Path('/proc/sys/kernel/random');self.directory_members=None
        self.user_namespace=Path('/proc/self/ns/user').stat().st_ino

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
        self.kernel_address=_function_address()
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
                    self.root/'census-descriptors-before.json')
                except (OSError,ObservationUnavailable) as error:
                    cache._write_record(self.root/'census-failure.json',{'phase':'before-witness',
                        'category':type(error).__name__,'errno':getattr(error,'errno',None),
                        'reason':str(error) if isinstance(error,ObservationUnavailable) else None})
                    raise
                cache._write_record(self.root/'census-before.json',self.census_before)
                validate_census({'before':self.census_before,'after':self.census_before})
            witness=subprocess.Popen([sys.executable,'-I','-S','-c',_WITNESS,self.witness],
                stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,
                env={'PATH':os.defpath},preexec_fn=drop)
            try:status=witness.wait(timeout=min(3,max(.001,deadline-time.monotonic())))
            finally:
                if witness.poll() is None:witness.kill();witness.wait(timeout=2)
            if status!=0:raise ObservationUnavailable('original witness unavailable')
            for _ in range(10):
                paths,queries,metadata=decode(self._data(),complete=False,with_directories=True,with_metadata=True)
                if any(e['uid']==1999 and e['pid']==witness.pid and e['syscall']==257
                        and e['result']==-errno.ENOENT and e['path']==self.witness.encode()
                        for e in paths):
                    try:
                        directory_witness(queries,witness.pid,1999,self.directory_path,self.directory_members)
                        metadata_witness(metadata,witness.pid,1999,self.directory_path,self.user_namespace)
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
                            self.root/'census-descriptors-after.json')
                        cache._write_record(self.root/'census-after.json',self.census_after)
                        validate_census({'before':self.census_before,'after':self.census_after})
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

    def finish(self,unit_pid,compilers,directories,cursor_directory,stat_file):
        self.stop()
        if self.child is None or self.child.returncode!=0 or self.witness_pid is None:
            raise ObservationUnavailable('original kernel observer did not finish')
        if self.census_error:raise ObservationUnavailable('original kernel census failed')
        census=validate_census({'before':self.census_before,'after':self.census_after})
        events,queries,metadata=decode(self._data(),with_directories=True,with_metadata=True)
        if b'lost' in self.stderr.read_bytes().lower():raise ObservationUnavailable('lost kernel observations')
        directory_witness(queries,self.witness_pid,1999,self.directory_path,self.directory_members)
        if len(directories)!=4:raise ObservationUnavailable('original directory scopes missing')
        summaries=[directory_witness(queries,*scope) for scope in directories]
        directory_cursor_witness(queries,unit_pid,2004,cursor_directory)
        metadata_witness(metadata,self.witness_pid,1999,self.directory_path,self.user_namespace)
        metadata_witness(metadata,unit_pid,2004,stat_file,self.user_namespace)
        stats=[q for q in metadata if q['uid']==2004 and q['pid']==q['tid']==unit_pid
            and q['user_namespace']==self.user_namespace]
        if not any(q['fd']==-1 and q['vfs_result']==q['result']==-errno.EBADF and q['stat'] is None for q in stats):
            raise ObservationUnavailable('original invalid-fd fstat witness missing')
        if not any(q['fd']>=0 and q['vfs_result']==0 and q['result']==-errno.EFAULT
                and q['stat'] is not None and q['stat']['inode']==stat_file.stat().st_ino for q in stats):
            raise ObservationUnavailable('original failed fstat copy witness missing')
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
        'binary_path_query_observed'}
    keys|={'directory_reads','directory_entries','directory_witness_observed','binary_directory_name_observed',
        'directory_short_read_observed','directory_eof_observed','cargo_directory_queries_observed','directory_cursor_query_observed'}
    keys|={'metadata_queries','fstat_query_observed','fstat_invalid_fd_observed','fstat_failed_copy_observed'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['kind']!='kernel-copied-filename-probe' or value['binding']!=binding or value['command']!=command
            or type(value['unit_pid']) is not int or not 0<value['unit_pid']<2**31
            or type(value['unit_uid']) is not int or value['unit_uid']!=2004
            or type(value['events']) is not int or not 3<=value['events']<=20000
            or any(type(value[key]) is not int or not 1<=value[key]<=40000 for key in ('directory_reads','directory_entries','metadata_queries'))
            or type(value['kernel_release']) is not str or re.fullmatch(r'[A-Za-z0-9+_.-]{1,128}',value['kernel_release']) is None
            or not cache._digest(value['program_sha256']) or not cache._digest(value['binary_sha256'])
            or any(value[key] is not True for key in ('witness_observed','positive_query_observed',
                'negative_query_observed','after_kernel_copy','original_observer_cleanup','allocation_removed',
                'binary_path_query_observed',
                'compiler_queries_observed','library_generated_query_observed','max_path_query_observed','missed_probe_census_complete',
                'directory_witness_observed','binary_directory_name_observed','directory_short_read_observed',
                'directory_eof_observed','cargo_directory_queries_observed'))
            or any(value[key] is not True for key in ('fstat_query_observed','fstat_invalid_fd_observed','fstat_failed_copy_observed'))
            or value['directory_cursor_query_observed'] is not True
            or any(value[key] is not False for key in ('shipping_authority','filesystem_coverage_complete','input_closure_complete'))):
        raise ObservationUnavailable('kernel observation is incomplete or foreign')
    validate_census(value['probe_census'])
    return value
