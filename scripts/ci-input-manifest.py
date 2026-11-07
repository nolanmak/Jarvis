#!/usr/bin/env python3
"""Controller input observations; these are not CI/freshness capabilities.

The guest controller owns the roots and must exclude live writers before the
final snapshot. Source Git census, producer/artifact provenance, original host
quiescence and complete CI coverage remain independent requirements. Neither a
content digest nor this observation alone permits artifact reuse or a verdict.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import ctypes
import struct
import sys


_spec=importlib.util.spec_from_file_location('ci_manifest_cache',Path(__file__).with_name('ci-cache-generation.py'))
_cache=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(_cache)
FreshnessHold=_cache.FreshnessHold
OBSERVATION_FORMAT=1


def _encoded(value):
    return json.dumps(value,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()


def _roots(roots):
    if (not isinstance(roots,dict) or len(roots)>64
            or any(not isinstance(name,str) or not name or len(name)>128
                   or not all(c.isascii() and (c.isalnum() or c in '_-') for c in name) for name in roots)):
        raise FreshnessHold('input_census_unknown')
    result={}
    for name,path in sorted(roots.items()):
        path=Path(path)
        if not path.is_absolute() or path.resolve()!=path or not (stat.S_ISDIR(path.lstat().st_mode) or stat.S_ISREG(path.lstat().st_mode)):
            raise FreshnessHold('input_census_unknown')
        if any(path==other or path.is_relative_to(other) or other.is_relative_to(path) for other in result.values()):
            raise FreshnessHold('input_census_unknown')
        result[name]=path
    return result


def _declared_target(path,roots,hops=0):
    """Resolve every link hop, refusing an excursion through unobserved data.

    Common literal ancestors allow relative links between declared roots. They
    were already checked by _roots; they are not additional input namespaces.
    Directory aliases are recorded, never recursively expanded by the census.
    """
    path=Path(path)
    if hops>64 or not path.is_absolute() or len(str(path).encode())>4096:
        raise FreshnessHold('input_census_unknown')
    current=Path(path.anchor);parts=path.parts[1:]
    for index,part in enumerate(parts):
        current=current.parent if part=='..' else current/part
        if not any(current==root or current.is_relative_to(root) or root.is_relative_to(current) for root in roots.values()):
            raise FreshnessHold('input_census_unknown')
        info=current.lstat()
        if stat.S_ISLNK(info.st_mode):
            target=Path(os.readlink(current));target=target if target.is_absolute() else current.parent/target
            return _declared_target(target.joinpath(*parts[index+1:]),roots,hops+1)
        if index<len(parts)-1 and not stat.S_ISDIR(info.st_mode):raise FreshnessHold('input_census_unknown')
    for name,root in roots.items():
        if current==root or current.is_relative_to(root):
            if not (current.is_dir() or current.is_file()):raise FreshnessHold('input_census_unknown')
            return name,current.relative_to(root).as_posix()
    raise FreshnessHold('input_census_unknown')


def snapshot(roots,*,max_files=100_000,max_bytes=8*1024**3):
    """Content/mode identity, independent of wall clock and directory names.

    Link targets must remain in declared namespaces at every resolution hop.
    Special files and changes during capture remain unknown input. The caller
    still owns the original-Git/producer provenance of the declared roots.
    """
    try:
        roots=_roots(roots);entries=[];files=total=0
        if type(max_files) is not int or not 0<max_files<=1_000_000 or type(max_bytes) is not int or max_bytes<=0:
            raise FreshnessHold('input_census_unknown')
        def visit(name,root,path):
            nonlocal files,total
            info=path.lstat();relative=path.relative_to(root).as_posix()
            if len(relative.encode())>4096 or info.st_dev!=root.stat().st_dev:
                raise FreshnessHold('input_census_unknown')
            if stat.S_ISDIR(info.st_mode):
                entries.append([name,relative,'directory',stat.S_IMODE(info.st_mode)])
                for child in sorted(path.iterdir()):visit(name,root,child)
            elif stat.S_ISLNK(info.st_mode):
                link=os.readlink(path);target=Path(link)
                target=target if target.is_absolute() else path.parent/target
                target_name,target_relative=_declared_target(target,roots)
                after=path.lstat()
                if (info.st_dev,info.st_ino,info.st_mtime_ns,info.st_ctime_ns)!=(after.st_dev,after.st_ino,after.st_mtime_ns,after.st_ctime_ns):
                    raise FreshnessHold('input_census_unknown')
                entries.append([name,relative,'symlink',link,target_name,target_relative])
            elif stat.S_ISREG(info.st_mode):
                files+=1;total+=info.st_size
                if files>max_files or total>max_bytes:raise FreshnessHold('input_census_unknown')
                fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK)
                with os.fdopen(fd,'rb') as stream:
                    before=os.fstat(stream.fileno());digest=hashlib.sha256();read=0
                    while block:=stream.read(1024*1024):
                        read+=len(block)
                        if read>info.st_size:raise FreshnessHold('input_census_unknown')
                        digest.update(block)
                    after=os.fstat(stream.fileno())
                stable=lambda s:(s.st_dev,s.st_ino,s.st_mode,s.st_nlink,s.st_size,s.st_mtime_ns,s.st_ctime_ns)
                if stable(info)!=stable(before) or stable(before)!=stable(after) or read!=before.st_size:
                    raise FreshnessHold('input_census_unknown')
                entries.append([name,relative,'file',stat.S_IMODE(info.st_mode),read,digest.hexdigest()])
            else:raise FreshnessHold('input_census_unknown')
            if len(entries)>max_files*2+64:raise FreshnessHold('input_census_unknown')
        for name,root in roots.items():visit(name,root,root)
        return {'format_version':OBSERVATION_FORMAT,'kind':'ci-input-snapshot',
                'sha256':hashlib.sha256(_encoded(entries)).hexdigest(),'files':files,'bytes':total,'entries':entries}
    except (OSError,ValueError,UnicodeError,RuntimeError):
        raise FreshnessHold('input_census_unknown') from None


class ChangeJournal:
    def __init__(self,roots,*,scope=None):
        self.fd=-1;self.closed=False;self.watches={};self.count=0;self.changed=set();self.digest=hashlib.sha256()
        self.scope=scope
        try:
            self.roots=_roots(roots)
            if self.scope is not None:
                try:self.scope.begin_observation()
                except Exception:raise FreshnessHold('cleanup_uncertain') from None
            if sys.platform!='linux':raise FreshnessHold('input_census_unknown')
            libc=ctypes.CDLL(None,use_errno=True)
            libc.inotify_init1.argtypes=[ctypes.c_int];libc.inotify_init1.restype=ctypes.c_int
            libc.inotify_add_watch.argtypes=[ctypes.c_int,ctypes.c_char_p,ctypes.c_uint32]
            libc.inotify_add_watch.restype=ctypes.c_int
            self.fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
            if self.fd<0:raise FreshnessHold('input_census_unknown')
            # MODIFY, ATTRIB, CLOSE_WRITE, moves, creates/deletes, self moves/
            # deletion. Access/read events deliberately do not count as writes.
            mask=0x00000fce|0x02000000  # DONT_FOLLOW; directories also require ONLYDIR
            def failed(_):raise FreshnessHold('input_census_unknown')
            for name,root in self.roots.items():
                walk=os.walk(root,followlinks=False,onerror=failed) if root.is_dir() else [(root,[],[])]
                for directory,subdirs,_files in walk:
                    directory=Path(directory)
                    if directory.is_symlink() or directory.resolve()!=directory or len(self.watches)>=100_000:
                        raise FreshnessHold('input_census_unknown')
                    wd=libc.inotify_add_watch(self.fd,os.fsencode(directory),mask|(0x01000000 if directory.is_dir() else 0))
                    if wd<0 or wd in self.watches:raise FreshnessHold('input_census_unknown')
                    self.watches[wd]=(name,directory.relative_to(root).as_posix())
                    subdirs[:]=[child for child in subdirs if not (directory/child).is_symlink()]
            self.before=snapshot(self.roots)
            self._drain()
        except FreshnessHold:
            self.close();raise
        except Exception:
            self.close();raise FreshnessHold('input_census_unknown') from None

    def _drain(self):
        while True:
            try:data=os.read(self.fd,65536)
            except BlockingIOError:return
            except OSError:raise FreshnessHold('input_census_unknown') from None
            if not data:raise FreshnessHold('input_census_unknown')
            offset=0
            while offset<len(data):
                if len(data)-offset<16:raise FreshnessHold('metadata_torn')
                wd,mask,cookie,length=struct.unpack_from('iIII',data,offset);offset+=16
                if length>4096 or offset+length>len(data):raise FreshnessHold('metadata_torn')
                raw=data[offset:offset+length].rstrip(b'\0');offset+=length
                # Overflow, unmount or a lost watch cannot certify unchanged
                # inputs. Preserve an explicit prerequisite instead of a pass.
                if mask & (0x00004000|0x00002000|0x00008000) or wd not in self.watches:
                    raise FreshnessHold('input_census_unknown')
                self.count+=1
                if self.count>100_000:raise FreshnessHold('input_census_unknown')
                name,relative=self.watches[wd];self.changed.add(name)
                event=[name,relative,mask,cookie,raw.decode('utf-8')]
                self.digest.update(_encoded(event)+b'\n')

    def close(self):
        if self.fd>=0:
            try:os.close(self.fd)
            except OSError:pass
            finally:self.fd=-1

    def __del__(self):
        self.close()

    def finish(self,stopped=None):
        """Finish after the controller has independently excluded live writers.

        This proves whether any watched namespace changed, not the bytes of
        every intermediate write or complete compiler/artifact provenance.
        A newly created subtree counts as changed even before it can be watched.
        """
        if self.closed or self.fd<0:raise FreshnessHold('input_census_unknown')
        self.closed=True
        try:
            if self.scope is not None and (stopped is None or not callable(getattr(stopped,'matches',None)) or not stopped.matches(self.scope)):
                raise FreshnessHold('cleanup_uncertain')
            self._drain();after=snapshot(self.roots);self._drain()
            changed=bool(self.changed) or self.before['sha256']!=after['sha256']
            return {'format_version':OBSERVATION_FORMAT,'kind':'ci-input-write-observation',
                    'changed':changed,'changed_roots':sorted(self.changed),'observed_events':self.count,
                    'events_sha256':self.digest.hexdigest(),'before_sha256':self.before['sha256'],
                    'after_sha256':after['sha256'],'guest_worker_quiescence':stopped.observation() if self.scope is not None else None,
                    'shipping_authority':False}
        except (OSError,ValueError,UnicodeError):
            raise FreshnessHold('input_census_unknown') from None
        finally:self.close()


def validate_observation(value,guest_workers,outputs_count):
    """Validate transport only. This is not complete artifact provenance."""
    try:
        fields={'format_version','kind','changed','changed_roots','observed_events','events_sha256',
                'before_sha256','after_sha256','guest_worker_quiescence','shipping_authority'}
        if (not isinstance(value,dict) or set(value)!=fields or type(value['format_version']) is not int
                or value['format_version']!=OBSERVATION_FORMAT or value['kind']!='ci-input-write-observation'
                or type(value['changed']) is not bool or value['shipping_authority'] is not False
                or type(value['observed_events']) is not int or not 0<=value['observed_events']<=100_000
                or not isinstance(value['changed_roots'],list) or len(value['changed_roots'])>64
                or any(not isinstance(name,str) or name not in {'output_'+str(i) for i in range(outputs_count)} for name in value['changed_roots'])
                or len(set(value['changed_roots']))!=len(value['changed_roots'])
                or any(not _cache._digest(value[key]) for key in ('events_sha256','before_sha256','after_sha256'))):
            raise FreshnessHold('input_census_unknown')
        if value['guest_worker_quiescence'] not in guest_workers['scopes']:
            raise FreshnessHold('cleanup_uncertain')
        if (bool(value['changed_roots'])!=(value['observed_events']>0)
                or value['changed']!=(value['observed_events']>0 or value['before_sha256']!=value['after_sha256'])
                or value['observed_events']==0 and value['events_sha256']!=hashlib.sha256(b'').hexdigest()):
            raise FreshnessHold('input_census_unknown')
        return value
    except (TypeError,ValueError,KeyError):raise FreshnessHold('input_census_unknown') from None
