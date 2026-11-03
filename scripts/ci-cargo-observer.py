#!/usr/bin/env python3
"""Private kernel-copied pathname feasibility probe, not an input-proof API.

Only selected lookup spellings are observed. This does not establish full
syscall/metadata/device coverage, missed-kretprobe accounting, compatibility or
reuse authority. It does not emulate or change the target's system calls.
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


class ObservationUnavailable(RuntimeError):
    pass


def startup_errno(error):
    errors=b'\n'.join(line for line in error.lower().splitlines()
        if not line.startswith(b'stdin:') and b'warning:' not in line)
    if b'bpf stack limit is exceeded' in errors:return errno.E2BIG
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
                or type(sample['programs']) is not list or len(sample['programs'])!=4
                or type(sample['links']) is not list or len(sample['links'])!=4):
            raise ObservationUnavailable('original kernel objects unavailable')
        programs={};links=set();raw=set();linked=set();returns=0
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
            elif link['type']=='kprobe_multi':
                if (set(link)!={'id','type','prog_id','tag','function','count','flags','missed'}
                        or link['function']!='getname_flags' or programs[link['prog_id']]['type']!=2
                        or any(type(link[key]) is not int for key in ('count','flags','missed'))
                        or (link['count'],link['flags'],link['missed'])!=(1,1,0)):
                    raise ObservationUnavailable('kernel return probe census failed')
                returns+=1
            else:raise ObservationUnavailable('unknown kernel attachment')
        if raw!={'sys_enter','sys_exit'} or returns!=2 or linked!=set(programs):
            raise ObservationUnavailable('kernel attachment census incomplete')
    if value['before']!=value['after']:
        raise ObservationUnavailable('kernel census identity or counters changed')
    return value


def _fdinfo(data):
    if len(data)>16384:raise ObservationUnavailable('unbounded kernel descriptor metadata')
    try:text=data.decode('ascii')
    except UnicodeError:raise ObservationUnavailable('unknown kernel descriptor encoding') from None
    result={}
    for line in text.splitlines():
        match=re.fullmatch(r'([a-z0-9_]+):[ \t]*(.*)',line)
        if match is None or match[1] in result:raise ObservationUnavailable('ambiguous kernel descriptor metadata')
        result[match[1]]=match[2]
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
    found=[];size=0
    with Path('/proc/kallsyms').open('rb') as stream:
        for line in stream:
            size+=len(line)
            if size>64*1024**2:raise ObservationUnavailable('unbounded kernel symbol census')
            parts=line.split()
            if len(parts)==3 and parts[2]==b'getname_flags' and parts[1] in (b't',b'T'):
                found.append(int(parts[0],16))
    if len(found)!=1 or not 0<found[0]<2**64:raise ObservationUnavailable('kernel function identity unavailable')
    return found[0]


def _return_link(identity,program,address):
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
        if kind!=8 or actual!=identity or prog!=program or count!=1 or flags!=1 or addresses[0]!=address:
            raise OSError(errno.ERANGE,'original return attachment unavailable')
        return {'function':'getname_flags','count':count,'flags':flags,'missed':missed}
    finally:os.close(descriptor)  # Never keep the observer's attachments alive.


def kernel_census(child,owner,birth,address):
    if child.poll() is not None or _birth(child.pid,owner)!=birth:
        raise ObservationUnavailable('original observer is not live')
    root=Path('/proc',str(child.pid));programs=[];links=[]
    descriptors=list((root/'fd').iterdir())
    if len(descriptors)>512:raise ObservationUnavailable('unbounded original descriptor census')
    for path in descriptors:
        target=os.readlink(path)
        if target not in ('anon_inode:bpf-prog','anon_inode:bpf_link','anon_inode:bpf-link'):
            continue
        info=_fdinfo((root/'fdinfo'/path.name).read_bytes())
        if target=='anon_inode:bpf-prog':
            programs.append({'id':_number(info.get('prog_id')),'type':_number(info.get('prog_type')),
                'tag':info.get('prog_tag'),'recursion_misses':_number(info.get('recursion_misses'))})
        else:
            identity=_number(info.get('link_id'));program=_number(info.get('prog_id'));kind=info.get('link_type')
            link={'id':identity,'type':kind,'prog_id':program,'tag':info.get('prog_tag')}
            if kind=='raw_tracepoint':link['tracepoint']=info.get('tp_name')
            elif kind=='kprobe_multi':link.update(_return_link(identity,program,address))
            else:raise ObservationUnavailable('unaccounted observer attachment')
            links.append(link)
    if child.poll() is not None or _birth(child.pid,owner)!=birth:
        raise ObservationUnavailable('original observer changed during census')
    return {'pid':child.pid,'birth':birth,'programs':sorted(programs,key=lambda row:row['id']),
        'links':sorted(links,key=lambda row:row['id'])}


def decode(data,*,complete=True):
    if not isinstance(data,bytes) or len(data)>MAX_BYTES:raise ObservationUnavailable('unbounded observation')
    if complete and (not data or not data.endswith(b'\n')):raise ObservationUnavailable('incomplete observation')
    if not complete and not data.endswith(b'\n'):data=data.rpartition(b'\n')[0]+b'\n'
    try:lines=data.decode('ascii').splitlines()
    except UnicodeError:raise ObservationUnavailable('unknown pathname encoding') from None
    pairs={};count=None;records=0;loader_records=0
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
        match=re.fullmatch(r'([NE]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) ([0-9]{1,4}) ([1-9][0-9]{0,19}) (.*)',line)
        if match is None:raise ObservationUnavailable('unknown observation record')
        kind=match[1];uid,pid,tid,call,sequence=map(int,match.groups()[1:6])
        if (pid>=2**31 or tid>=2**31 or call not in SYSCALLS or sequence>=2**64
                or not (uid==1999 or 2000<=uid<6096)):
            raise ObservationUnavailable('foreign kernel observation')
        pair=pairs.setdefault((uid,pid,tid,call,sequence),{})
        if kind=='N':
            chunk=re.fullmatch(r'([0-9]{1,4}) ([1-9][0-9]{0,3}) ([A-Za-z0-9/._-]+)',match[7])
            if chunk is None:raise ObservationUnavailable('unknown pathname chunk')
            offset,total=int(chunk[1]),int(chunk[2]);data=chunk[3].encode()
            chunks=pair.setdefault('N',{})
            if (not 0<=offset<total<PATH_BYTES or offset%CHUNK_BYTES
                    or len(data)!=min(CHUNK_BYTES,total-offset)
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
    if complete and (count is None or count!=records or not pairs):
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
    return events


_PROGRAM=r'''
struct observer_filename { const char *name; };
rawtracepoint:sys_enter / (uid >= 2000 && uid < 6096) || uid == 1999 / {
 @number[tid] = arg1;
 @sequence[tid] = @sequence[tid] + 1;
 @seen[tid]=(uint64)0; @ready[tid]=(uint64)0;
}
kretprobe:getname_flag[s]
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
kretprobe:getname_flag[s]
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
   while ($offset<$length) {
    printf("N %u %u %u %lld %llu %llu %llu %s\n",$owner_uid,$owner_pid,$owner_tid,(int64)$call,$entry,
     $offset,$length,str($name+$offset,64));
    $chunks=$chunks+1; $offset=$offset+63;
   }
  } else {
   printf("invalid kernel pathname length\n"); $chunks=1;
  }
  @emitted=sum($chunks);
  @ready[tid]=(uint64)0;
  @seen[tid] = 1;
 }
}
rawtracepoint:sys_exit / (uid >= 2000 && uid < 6096) || uid == 1999 / {
 if (@seen[tid]) {
  printf("E %u %u %u %lld %llu %lld\n",uid,pid,tid,(int64)@number[tid],@sequence[tid],(int64)arg1);
  @emitted = sum(1);
  @seen[tid]=(uint64)0;
 }
}
END { clear(@seen); clear(@ready); clear(@original_name); clear(@length); clear(@sequence); clear(@number); }
'''
_WITNESS=r'''
import ctypes,errno,os,sys
libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
result=libc.syscall(ctypes.c_long(257),ctypes.c_long(-100),ctypes.c_char_p(os.fsencode(sys.argv[1])),
 ctypes.c_long(os.O_RDONLY|os.O_CLOEXEC),ctypes.c_long(0))
assert result==-1 and ctypes.get_errno()==errno.ENOENT
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
                self.census_before=kernel_census(self.child,self.owner,self.birth,self.kernel_address)
                cache._write_record(self.root/'census-before.json',self.census_before)
                validate_census({'before':self.census_before,'after':self.census_before})
            witness=subprocess.Popen([sys.executable,'-I','-c',_WITNESS,self.witness],
                stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,
                env={'PATH':os.defpath},preexec_fn=drop)
            try:status=witness.wait(timeout=min(3,max(.001,deadline-time.monotonic())))
            finally:
                if witness.poll() is None:witness.kill();witness.wait(timeout=2)
            if status!=0:raise ObservationUnavailable('original witness unavailable')
            for _ in range(10):
                if any(e['uid']==1999 and e['pid']==witness.pid and e['syscall']==257
                        and e['result']==-errno.ENOENT and e['path']==self.witness.encode()
                        for e in decode(self._data(),complete=False)):
                    self.witness_pid=witness.pid;return
                time.sleep(.01)
        raise OSError(errno.ETIMEDOUT,'kernel observation readiness unproven')

    def stop(self):
        if self.child is not None and self.child.poll() is None:
            try:
                if self.census_before is not None and self.census_after is None:
                    try:
                        self.census_after=kernel_census(self.child,self.owner,self.birth,self.kernel_address)
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

    def finish(self,unit_pid,compilers):
        self.stop()
        if self.child is None or self.child.returncode!=0 or self.witness_pid is None:
            raise ObservationUnavailable('original kernel observer did not finish')
        if self.census_error:raise ObservationUnavailable('original kernel census failed')
        census=validate_census({'before':self.census_before,'after':self.census_after})
        events=decode(self._data())
        if b'lost' in self.stderr.read_bytes().lower():raise ObservationUnavailable('lost kernel observations')
        selected=[e for e in events if e['uid']==2004 and e['pid']==unit_pid and e['tid']==unit_pid]
        if (not any(e['syscall']==257 and e['result']>=0 and e['path']==b'/workspace/target/deps/input' for e in selected)
                or not any(e['syscall']==257 and e['result']==-errno.ENOENT and e['path']==b'/workspace/target/deps/missing' for e in selected)):
            raise ObservationUnavailable('kernel lookup witnesses missing')
        if not any(e['syscall']==257 and e['result']==-errno.ENOENT
                and e['path']==b'/workspace/'+b'x/'*2042 for e in selected):
            raise ObservationUnavailable('full length kernel pathname witness missing')
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
            'compiler_queries_observed':True,'library_generated_query_observed':True,
            'after_kernel_copy':True,'events':len(events),'original_observer_cleanup':True,
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
        'compiler_queries_observed','library_generated_query_observed','max_path_query_observed','probe_census'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['kind']!='kernel-copied-filename-probe' or value['binding']!=binding or value['command']!=command
            or type(value['unit_pid']) is not int or not 0<value['unit_pid']<2**31
            or type(value['unit_uid']) is not int or value['unit_uid']!=2004
            or type(value['events']) is not int or not 3<=value['events']<=20000
            or type(value['kernel_release']) is not str or re.fullmatch(r'[A-Za-z0-9+_.-]{1,128}',value['kernel_release']) is None
            or not cache._digest(value['program_sha256']) or not cache._digest(value['binary_sha256'])
            or any(value[key] is not True for key in ('witness_observed','positive_query_observed',
                'negative_query_observed','after_kernel_copy','original_observer_cleanup','allocation_removed',
                'compiler_queries_observed','library_generated_query_observed','max_path_query_observed','missed_probe_census_complete'))
            or any(value[key] is not False for key in ('shipping_authority','filesystem_coverage_complete','input_closure_complete'))):
        raise ObservationUnavailable('kernel observation is incomplete or foreign')
    validate_census(value['probe_census'])
    return value
