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
import subprocess
import sys
import time
import uuid

spec=importlib.util.spec_from_file_location('observer_cache',Path(__file__).with_name('ci-cache-generation.py'))
cache=importlib.util.module_from_spec(spec);spec.loader.exec_module(cache)
MAX_BYTES=4*1024**2
# Name and result records use separate probes to avoid copying map strings on
# the BPF exit probe stack. The terminator slot never proves a complete name.
PATH_BYTES=128
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


def decode(data,*,complete=True):
    if not isinstance(data,bytes) or len(data)>MAX_BYTES:raise ObservationUnavailable('unbounded observation')
    if complete and (not data or not data.endswith(b'\n')):raise ObservationUnavailable('incomplete observation')
    if not complete and not data.endswith(b'\n'):data=data.rpartition(b'\n')[0]+b'\n'
    try:lines=data.decode('ascii').splitlines()
    except UnicodeError:raise ObservationUnavailable('unknown pathname encoding') from None
    pairs={};count=None;records=0
    for line in lines:
        if not line:continue
        try:record=json.loads(line,object_pairs_hook=cache._unique_object)
        except (ValueError,TypeError):raise ObservationUnavailable('invalid kernel record framing') from None
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
        match=re.fullmatch(r'([NE]) ([0-9]{1,4}) ([1-9][0-9]{0,9}) ([1-9][0-9]{0,9}) ([0-9]{1,4}) ([1-9][0-9]{0,19}) ([-A-Za-z0-9/._]+)',line)
        if match is None:raise ObservationUnavailable('unknown observation record')
        kind=match[1];uid,pid,tid,call,sequence=map(int,match.groups()[1:6])
        if (pid>=2**31 or tid>=2**31 or call not in SYSCALLS or sequence>=2**64
                or not (uid==1999 or 2000<=uid<6096)):
            raise ObservationUnavailable('foreign kernel observation')
        pair=pairs.setdefault((uid,pid,tid,call,sequence),{})
        if kind in pair:raise ObservationUnavailable('duplicate kernel observation')
        if kind=='N':
            path=match[7].encode()
            if (len(path)>=PATH_BYTES-1 or any(part in (b'',b'.',b'..') for part in path.split(b'/')[1:])
                    or not (uid==1999 and re.fullmatch(rb'/ci-observer-witness-[a-z0-9-]+',path)
                        or 2000<=uid<6096 and (path.startswith(b'/workspace/')
                            or path in (b'src/lib.rs',b'src/main.rs',b'build.rs')))):
                raise ObservationUnavailable('foreign kernel pathname')
            pair[kind]=path
        else:
            if re.fullmatch(r'-?[0-9]{1,20}',match[7]) is None:raise ObservationUnavailable('unknown syscall result')
            result=int(match[7])
            if not -4095<=result<2**31:raise ObservationUnavailable('unknown syscall result')
            pair[kind]=result
        records+=1
        if len(pairs)>20000 or records>40000:raise ObservationUnavailable('observation limit exceeded')
    if complete and (count is None or count!=records or not pairs or any(set(pair)!={'N','E'} for pair in pairs.values())):
        raise ObservationUnavailable('event count unavailable')
    return [{'uid':uid,'pid':pid,'tid':tid,'syscall':call,'result':pair['E'],'path':pair['N']}
        for (uid,pid,tid,call,sequence),pair in pairs.items() if set(pair)=={'N','E'}]


_PROGRAM=r'''
struct observer_filename { const char *name; };
rawtracepoint:sys_enter / (uid >= 2000 && uid < 6096) || uid == 1999 / {
 @number[tid] = arg1;
 @sequence[tid] = @sequence[tid] + 1;
 delete(@seen[tid]);
}
kretprobe:getname_flags
/ ((uid >= 2000 && uid < 6096) || uid == 1999) && (uint64)retval > 4095 && (uint64)retval < (uint64)0xfffffffffffff001 / {
 $value = str(((struct observer_filename *)retval)->name, 128);
 if ((@number[tid]==2 || @number[tid]==4 || @number[tid]==6 || @number[tid]==21 ||
      @number[tid]==89 || @number[tid]==257 || @number[tid]==262 || @number[tid]==267 ||
      @number[tid]==269 || @number[tid]==332 || @number[tid]==439) &&
     (strncmp($value,"/workspace/",11)==0 || $value=="src/lib.rs" || $value=="src/main.rs" ||
      $value=="build.rs" || $value == __WITNESS__)) {
  printf("N %u %u %u %lld %llu %s\n",uid,pid,tid,(int64)@number[tid],@sequence[tid],$value);
  @emitted = count(); @seen[tid] = 1;
 }
}
rawtracepoint:sys_exit / (uid >= 2000 && uid < 6096) || uid == 1999 / {
 if (@seen[tid]) {
  printf("E %u %u %u %lld %llu %lld\n",uid,pid,tid,(int64)@number[tid],@sequence[tid],(int64)arg1);
  @emitted = count();
 }
 delete(@seen[tid]); delete(@number[tid]);
}
END { clear(@seen); clear(@sequence); clear(@number); print(@emitted); clear(@emitted); }
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
        self.witness='/ci-observer-witness-'+str(uuid.uuid4());self.witness_pid=None
        self.program=_PROGRAM.replace('__WITNESS__',json.dumps(self.witness))
        self.stdout=self.root/'stdout';self.stderr=self.root/'stderr';self.output=None;self.error=None
        self.report=None

    def __enter__(self):
        try:self.start();return self
        except BaseException:self.stop();raise

    def __exit__(self,*unused):self.stop()

    def _data(self):
        if self.stdout.stat().st_size>MAX_BYTES or self.stderr.stat().st_size>MAX_BYTES:
            raise ObservationUnavailable('observation limit exceeded')
        return self.stdout.read_bytes()

    def start(self):
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
        self.child=subprocess.Popen([str(binary),'-q','-B','none','-f','json','-e',self.program],
            stdin=subprocess.DEVNULL,stdout=self.output,stderr=self.error,start_new_session=True,
            env={'PATH':os.defpath,'BPFTRACE_MAX_STRLEN':str(PATH_BYTES),'HOME':'/tmp'})
        deadline=min(self.deadline,time.monotonic()+10);origin=os.getpid()
        def drop():
            libc=ctypes.CDLL(None,use_errno=True)
            if libc.prctl(38,1,0,0,0)!=0:os._exit(125)
            os.setgroups([]);os.setgid(1999);os.setuid(1999)
            if libc.prctl(1,signal.SIGKILL,0,0,0)!=0 or os.getppid()!=origin:os._exit(125)
        while time.monotonic()<deadline:
            if self.child.poll() is not None:
                raise OSError(startup_errno(self.stderr.read_bytes()),'private kernel observer startup failed')
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
            self.child.send_signal(signal.SIGINT)
            try:self.child.wait(timeout=5)
            except subprocess.TimeoutExpired:self.child.kill();self.child.wait(timeout=2)
        for stream in (self.output,self.error):
            if stream is not None and not stream.closed:stream.close()

    def finish(self,unit_pid,compilers):
        self.stop()
        if self.child is None or self.child.returncode!=0 or self.witness_pid is None:
            raise ObservationUnavailable('original kernel observer did not finish')
        events=decode(self._data())
        if b'lost' in self.stderr.read_bytes().lower():raise ObservationUnavailable('lost kernel observations')
        selected=[e for e in events if e['uid']==2004 and e['pid']==unit_pid and e['tid']==unit_pid]
        if (not any(e['syscall']==257 and e['result']>=0 and e['path']==b'/workspace/target/deps/input' for e in selected)
                or not any(e['syscall']==257 and e['result']==-errno.ENOENT and e['path']==b'/workspace/target/deps/missing' for e in selected)):
            raise ObservationUnavailable('kernel lookup witnesses missing')
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
            'witness_observed':True,'positive_query_observed':True,'negative_query_observed':True,
            'compiler_queries_observed':True,'library_generated_query_observed':True,
            'after_kernel_copy':True,'events':len(events),'original_observer_cleanup':True,
            'filesystem_coverage_complete':False,'missed_probe_census_complete':False,
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
        'compiler_queries_observed','library_generated_query_observed'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['kind']!='kernel-copied-filename-probe' or value['binding']!=binding or value['command']!=command
            or type(value['unit_pid']) is not int or not 0<value['unit_pid']<2**31
            or type(value['unit_uid']) is not int or value['unit_uid']!=2004
            or type(value['events']) is not int or not 3<=value['events']<=20000
            or type(value['kernel_release']) is not str or re.fullmatch(r'[A-Za-z0-9+_.-]{1,128}',value['kernel_release']) is None
            or not cache._digest(value['program_sha256']) or not cache._digest(value['binary_sha256'])
            or any(value[key] is not True for key in ('witness_observed','positive_query_observed',
                'negative_query_observed','after_kernel_copy','original_observer_cleanup','allocation_removed',
                'compiler_queries_observed','library_generated_query_observed'))
            or any(value[key] is not False for key in ('shipping_authority','filesystem_coverage_complete',
                'missed_probe_census_complete','input_closure_complete'))):
        raise ObservationUnavailable('kernel observation is incomplete or foreign')
    return value
