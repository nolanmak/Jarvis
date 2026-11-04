#!/usr/bin/env python3
"""Private unit filesystem mechanics; not a provenance or passing-CI authority.

The admitted root planner owns namespace entry and original-unit cleanup. Pure
filesystem helpers are also exercised without privilege; their results alone
never establish trusted producer identity, input closure, or reuse authority.
"""
import ctypes
import copy as copy_module
import errno
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess

spec=importlib.util.spec_from_file_location('unit_view_cache',Path(__file__).with_name('ci-cache-generation.py'))
cache=importlib.util.module_from_spec(spec);spec.loader.exec_module(cache)
FreshnessHold=cache.FreshnessHold
MAX_ENTRIES=100000
MAX_BYTES=4*1024**3


def relative(value):
    raw=os.fsencode(value)
    if (not raw or len(raw)>4096 or raw.startswith(b'/') or b'\0' in raw
            or any(part in (b'',b'.',b'..') for part in raw.split(b'/'))):
        raise FreshnessHold('input_census_unknown')
    return Path(os.fsdecode(raw))


def literal(path):
    path=Path(path)
    if not path.is_absolute() or path.resolve()!=path or path.is_symlink():raise FreshnessHold('invalidation_unsafe')
    return path


def _key(info):
    return (info.st_dev,info.st_ino,info.st_size,info.st_uid,info.st_gid,info.st_mode,info.st_mtime_ns,info.st_ctime_ns)


def _read(path,owner,sealed,writer=None):
    descriptor=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK|getattr(os,'O_NOATIME',0))
    try:
        before=os.fstat(descriptor);mode=stat.S_IMODE(before.st_mode)
        if (not stat.S_ISREG(before.st_mode) or before.st_uid!=owner or mode&0o7000
                or (hasattr(os,'listxattr') and os.listxattr(descriptor))
                or (sealed and mode not in (0o444,0o555)) or before.st_nlink!=1
                or before.st_size>MAX_BYTES):raise FreshnessHold('artifact_provenance_unknown')
        count=0;digest=hashlib.sha256()
        while True:
            block=os.read(descriptor,1024*1024)
            if not block:break
            count+=len(block)
            if count>before.st_size:raise FreshnessHold('input_census_unknown')
            digest.update(block)
            if writer is not None:
                remaining=memoryview(block)
                while remaining:
                    written=os.write(writer,remaining)
                    if written<=0:raise OSError(errno.EIO,'unit output copy failed')
                    remaining=remaining[written:]
        if count!=before.st_size or _key(os.fstat(descriptor))!=_key(before) or _key(path.lstat())!=_key(before):
            raise FreshnessHold('input_census_unknown')
        return mode,count,digest.hexdigest()
    finally:os.close(descriptor)


def inventory(root,*,owner,sealed):
    root=literal(root);rows=[];total=0
    def walk(directory,prefix):
        nonlocal total
        info=directory.lstat();mode=stat.S_IMODE(info.st_mode)
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=owner or mode&0o7000
                or (hasattr(os,'listxattr') and os.listxattr(directory,follow_symlinks=False))
                or (sealed and mode!=0o555)):raise FreshnessHold('artifact_provenance_unknown')
        rows.append([os.fsencode(prefix).hex(),'directory',mode])
        descriptor=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC|getattr(os,'O_NOATIME',0))
        try:names=os.listdir(descriptor)
        finally:os.close(descriptor)
        for entry in [directory/name for name in sorted(names,key=os.fsencode)]:
            name=prefix/entry.name;info=entry.lstat()
            if stat.S_ISDIR(info.st_mode):walk(entry,name)
            elif stat.S_ISREG(info.st_mode):
                mode,size,digest=_read(entry,owner,sealed);total+=size
                rows.append([os.fsencode(name).hex(),'file',mode,size,digest])
            else:raise FreshnessHold('artifact_provenance_unknown')
            if len(rows)>MAX_ENTRIES or total>MAX_BYTES:raise FreshnessHold('input_census_unknown')
    walk(root,Path('.'));return rows


def clone_inputs(source,destination,expected,*,owner,unit):
    source=literal(source);destination=literal(destination)
    if inventory(source,owner=owner,sealed=True)!=expected:raise FreshnessHold('input_census_unknown')
    destination.mkdir(mode=0o700)
    if os.geteuid()==0:os.chown(destination,unit,unit)
    bindings=[]
    for row in expected[1:]:
        name=relative(bytes.fromhex(row[0]));path=destination/name
        if row[1]=='directory':
            path.mkdir(mode=0o700)
            if os.geteuid()==0:os.chown(path,unit,unit)
        else:
            path.touch(mode=0o444);bindings.append([row[0],str(source/name),row])
    if inventory(source,owner=owner,sealed=True)!=expected:raise FreshnessHold('input_census_unknown')
    return bindings


def _output_directory(path,owner):
    info=path.lstat()
    if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=owner or info.st_mode&0o7000
            or (hasattr(os,'listxattr') and os.listxattr(path,follow_symlinks=False))):
        raise FreshnessHold('artifact_provenance_unknown')


def copy_outputs(source,destination,names,*,owner,controller,directories=None,controller_prefixes=None):
    source=literal(source);destination=literal(destination)
    directories=[] if directories is None else directories
    prefixes=set()
    if controller_prefixes is not None:
        if type(controller_prefixes) is not list or len(controller_prefixes)>512:
            raise FreshnessHold('input_census_unknown')
        prefixes={Path('.') if name=='.' else relative(name) for name in controller_prefixes}
    def check_parent(parent):
        controlled=parent.relative_to(source) in prefixes
        _output_directory(parent,controller if controlled else owner)
        if controlled and stat.S_IMODE(parent.lstat().st_mode)!=0o555:
            raise FreshnessHold('artifact_provenance_unknown')
    if (not isinstance(names,list) or not isinstance(directories,list)
            or not 1<=len(names)+len(directories)<=MAX_ENTRIES):raise FreshnessHold('input_census_unknown')
    names=[relative(name) for name in names];directories=[relative(name) for name in directories]
    if len(set(names+directories))!=len(names)+len(directories):raise FreshnessHold('input_census_unknown')
    destination.mkdir(mode=0o700);total=0
    for name in sorted(directories,key=lambda name:(len(name.parts),os.fsencode(name))):
        if (source/name).resolve()!=source/name:raise FreshnessHold('artifact_provenance_unknown')
        for parent in [source,*[source/Path(*name.parts[:i]) for i in range(1,len(name.parts)+1)]]:
            check_parent(parent)
        (destination/name).mkdir(parents=True,mode=0o700,exist_ok=True)
    for name in names:
        original=source/name
        if original.resolve()!=original:raise FreshnessHold('artifact_provenance_unknown')
        for parent in [source,*[source/Path(*name.parts[:i]) for i in range(1,len(name.parts))]]:
            check_parent(parent)
        output=destination/name;output.parent.mkdir(parents=True,mode=0o700,exist_ok=True)
        descriptor=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_CLOEXEC|os.O_NOFOLLOW,0o600)
        try:
            mode,size,_digest=_read(original,owner,False,descriptor);total+=size
            if total>MAX_BYTES:raise FreshnessHold('input_census_unknown')
            os.fchmod(descriptor,0o555 if mode&0o111 else 0o444);os.fsync(descriptor)
        finally:os.close(descriptor)
    for base,dirs,_files in os.walk(destination,topdown=False):
        directory=Path(base);directory.chmod(0o555)
        descriptor=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
    return inventory(destination,owner=controller,sealed=True)


def copy_output_roots(source,destination,roots,*,owner,controller):
    """Inventory only declared producer roots after original-scope quiescence.

    The root planner must establish that barrier before calling this primitive.
    A returned sealed byte inventory is not itself a producer/reuse capability.
    Preserve empty directories and unknown names, never capture sibling inputs.
    """
    source,destination=map(literal,(source,destination))
    if not isinstance(roots,list) or not 1<=len(roots)<=MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
    roots=sorted((relative(root) for root in roots),key=lambda root:root.parts)
    if any(a==b or a in b.parents for a,b in zip(roots,roots[1:])):
        raise FreshnessHold('input_census_unknown')
    snapshots={};names=[];directories=[];size=0;count=0
    for root in roots:
        snapshot=inventory(source/root,owner=owner,sealed=False);snapshots[root]=snapshot
        for row in snapshot:
            count+=1
            name=os.fsdecode(root/Path(os.fsdecode(bytes.fromhex(row[0]))))
            if row[1]=='directory':directories.append(name)
            else:names.append(name);size+=row[3]
            if count>MAX_ENTRIES or size>MAX_BYTES:raise FreshnessHold('input_census_unknown')
    result=copy_outputs(source,destination,names,owner=owner,controller=controller,directories=directories)
    if any(inventory(source/root,owner=owner,sealed=False)!=snapshot for root,snapshot in snapshots.items()):
        raise FreshnessHold('input_census_unknown')
    return result


def capture_roots(root_fd,destination,roots,*,owners,check,allow_missing=False):
    """Copy selected visible roots through held descriptors, without authority.

    The controller must prove pause, scope and alias closure separately. Original
    metadata is recorded, not emulated by these detached root-owned byte copies.
    Partial destinations remain uncommitted if any validation or copy fails.
    """
    if (type(root_fd) is not int or root_fd<0 or not stat.S_ISDIR(os.fstat(root_fd).st_mode)
            or type(roots) is not list or not 1<=len(roots)<=64
            or type(owners) not in (tuple,list) or not owners
            or any(type(owner) is not int or owner<0 for owner in owners) or not callable(check)
            or type(allow_missing) is not bool):
        raise FreshnessHold('input_census_unknown')
    roots=sorted((relative(name) for name in roots),key=lambda name:name.parts)
    if any(a==b or a in b.parents for a,b in zip(roots,roots[1:])):raise FreshnessHold('input_census_unknown')
    destination=literal(destination);parent=destination.parent.stat()
    if parent.st_uid!=os.geteuid() or parent.st_mode&0o022:raise FreshnessHold('invalidation_unsafe')
    check();destination.mkdir(mode=0o700);output_identity=destination.stat()
    flags=os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK|getattr(os,'O_NOATIME',0)
    def metadata(info,descriptor):
        if (info.st_uid not in owners or info.st_gid not in owners or info.st_mode&0o7000
                or (hasattr(os,'listxattr') and os.listxattr(descriptor))
                or (info.st_dev,info.st_ino)==(output_identity.st_dev,output_identity.st_ino)):
            raise FreshnessHold('artifact_provenance_unknown')
        return list(_key(info))
    def scan(copying):
        rows=[];total=0;record_bytes=0;missing=[]
        def append(row):
            nonlocal record_bytes
            rows.append(row);record_bytes+=len(json.dumps(row,separators=(',',':')).encode())
            if len(rows)>MAX_ENTRIES or record_bytes>16*1024**2:raise FreshnessHold('input_census_unknown')
        def visit(parent_fd,name,prefix):
            nonlocal total
            check();relative(prefix)
            named=os.stat(name,dir_fd=parent_fd,follow_symlinks=False)
            if not (stat.S_ISREG(named.st_mode) or stat.S_ISDIR(named.st_mode)):
                raise FreshnessHold('artifact_provenance_unknown')
            descriptor=os.open(name,flags|(os.O_DIRECTORY if stat.S_ISDIR(named.st_mode) else 0),dir_fd=parent_fd)
            try:
                before=os.fstat(descriptor);original=metadata(before,descriptor)
                if _key(named)!=_key(before):raise FreshnessHold('input_census_unknown')
                if stat.S_ISDIR(before.st_mode):
                    append([os.fsencode(prefix).hex(),'directory',original])
                    names=sorted(os.listdir(descriptor),key=os.fsencode)
                    if copying:(destination/prefix).mkdir(parents=True,mode=0o700,exist_ok=False)
                    for child in names:visit(descriptor,child,prefix/child)
                    if sorted(os.listdir(descriptor),key=os.fsencode)!=names:raise FreshnessHold('input_census_unknown')
                else:
                    if before.st_nlink!=1 or before.st_size>MAX_BYTES-total:raise FreshnessHold('artifact_provenance_unknown')
                    os.fsync(descriptor);check();count=0;digest=hashlib.sha256();writer=None
                    try:
                        if copying:
                            output=destination/prefix;output.parent.mkdir(parents=True,mode=0o700,exist_ok=True)
                            writer=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_CLOEXEC|os.O_NOFOLLOW,0o600)
                        while True:
                            check();block=os.read(descriptor,1024**2)
                            if not block:break
                            count+=len(block)
                            if count>before.st_size:raise FreshnessHold('input_census_unknown')
                            digest.update(block)
                            if writer is not None:
                                remaining=memoryview(block)
                                while remaining:
                                    check();written=os.write(writer,remaining)
                                    if written<=0:raise OSError(errno.EIO,'input capture copy failed')
                                    remaining=remaining[written:]
                        if count!=before.st_size:raise FreshnessHold('input_census_unknown')
                        if writer is not None:
                            os.fchmod(writer,0o555 if before.st_mode&0o111 else 0o444);os.fsync(writer)
                    finally:
                        if writer is not None:os.close(writer)
                    total+=count;append([os.fsencode(prefix).hex(),'file',original,digest.hexdigest()])
                check()
                if (_key(os.fstat(descriptor))!=_key(before)
                        or _key(os.stat(name,dir_fd=parent_fd,follow_symlinks=False))!=_key(before)):
                    raise FreshnessHold('input_census_unknown')
            finally:os.close(descriptor)
        for root in roots:
            descriptor=os.dup(root_fd)
            try:
                for component in root.parts[:-1]:
                    child=os.open(component,flags|os.O_DIRECTORY,dir_fd=descriptor)
                    try:metadata(os.fstat(child),child)
                    except BaseException:os.close(child);raise
                    os.close(descriptor);descriptor=child
                try:os.stat(root.name,dir_fd=descriptor,follow_symlinks=False)
                except FileNotFoundError:
                    if not allow_missing:raise
                    missing.append(os.fsencode(root).hex());continue
                visit(descriptor,root.name,root)
            finally:os.close(descriptor)
        return rows,total,missing
    try:
        entries,total,missing=scan(True)
        if scan(False)!=(entries,total,missing):raise FreshnessHold('input_census_unknown')
    except OSError as error:
        if error.errno in (errno.ELOOP,errno.ENOTDIR):raise FreshnessHold('invalidation_unsafe') from None
        raise
    for base,_dirs,_files in os.walk(destination,topdown=False):
        check();path=Path(base);path.chmod(0o555)
        descriptor=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
    check()
    expected={b'.'.hex():[b'.'.hex(),'directory',0o555]}
    for name,kind,original,*digest in entries:
        path=relative(bytes.fromhex(name))
        for parent in path.parents:
            raw=os.fsencode(parent).hex();expected[raw]=[raw,'directory',0o555]
        expected[name]=([name,'directory',0o555] if kind=='directory' else
            [name,'file',0o555 if original[5]&0o111 else 0o444,original[2],digest[0]])
    sealed=inventory(destination,owner=os.geteuid(),sealed=True)
    if {row[0]:row for row in sealed}!=expected:raise FreshnessHold('input_census_unknown')
    check()
    return {'version':1,'kind':'ci-paused-root-capture','roots':[os.fsencode(root).hex() for root in roots],
        'entries':entries,'copied_entries':len(sealed),'copied_bytes':total,
        'payload_sha256':hashlib.sha256(json.dumps(sealed,sort_keys=True,separators=(',',':')).encode()).hexdigest(),
        'original_metadata_emulated':False,'input_closure_complete':False,'shipping_authority':False,
        **({'missing_roots':missing} if allow_missing else {})}


def materialize_capture(source,capture,destination,*,parent_uid,unit_uid,check):
    """Create detached private working bytes, never publish reusable output."""
    source,destination=map(literal,(source,destination))
    if (type(capture) is not dict or capture.get('version')!=1 or capture.get('kind')!='ci-paused-root-capture'
            or capture.get('input_closure_complete') is not False or capture.get('shipping_authority') is not False
            or any(type(uid) is not int or uid<0 for uid in (parent_uid,unit_uid))):
        raise FreshnessHold('input_census_unknown')
    def source_digest():
        check();value=inventory(source,owner=os.geteuid(),sealed=True)
        return hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':')).encode()).hexdigest()
    if source_digest()!=capture['payload_sha256']:raise FreshnessHold('input_census_unknown')
    destination.mkdir(mode=0o700);mapping=[];directories=[]
    for raw,kind,original,*digest in capture['entries']:
        check();name=relative(bytes.fromhex(raw));output=destination/name
        if original[3] not in (0,parent_uid):raise FreshnessHold('artifact_provenance_unknown')
        owner=unit_uid if original[3]==parent_uid else 0
        group=unit_uid if original[4]==parent_uid else original[4]
        mode=stat.S_IMODE(original[5])
        if mode&0o7000:raise FreshnessHold('artifact_provenance_unknown')
        if kind=='directory':
            output.mkdir(mode=0o700,parents=True,exist_ok=True);directories.append((output,owner,group,mode,original[6]))
        elif kind=='file':
            output.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
            descriptor=os.open(output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
            try:
                _mode,size,sha=_read(source/name,os.geteuid(),True,descriptor)
                if size!=original[2] or sha!=digest[0]:raise FreshnessHold('input_census_unknown')
                current=os.fstat(descriptor)
                if (current.st_uid,current.st_gid)!=(owner,group):os.fchown(descriptor,owner,group)
                os.fchmod(descriptor,mode);os.utime(descriptor,ns=(original[6],original[6]));os.fsync(descriptor)
            finally:os.close(descriptor)
            if _read(output,owner,False)[1:]!=(original[2],digest[0]):raise FreshnessHold('input_census_unknown')
        else:raise FreshnessHold('input_census_unknown')
        info=output.stat();mapping.append([raw,kind,[original[0],original[1]],[info.st_dev,info.st_ino]])
    for path,owner,group,mode,mtime in reversed(directories):
        check();current=path.stat()
        if (current.st_uid,current.st_gid)!=(owner,group):os.chown(path,owner,group)
        path.chmod(mode);os.utime(path,ns=(mtime,mtime))
        descriptor=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
    if source_digest()!=capture['payload_sha256']:raise FreshnessHold('input_census_unknown')
    return {'version':1,'kind':'ci-contained-working-copy','mapping':mapping,
        'original_metadata_emulated':False,'input_closure_complete':False,'shipping_authority':False}



def _open_beneath(root_fd,name,flags):
    """Resolve each component through owned no-follow directory descriptors."""
    name=relative(name);parent=os.dup(root_fd)
    try:
        for part in name.parts[:-1]:
            child=os.open(part,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW,dir_fd=parent)
            os.close(parent);parent=child
        return os.open(name.name,flags|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK|getattr(os,'O_NOATIME',0),dir_fd=parent)
    finally:os.close(parent)


class RetainedInodes:
    """Own both sides of the initial inode graph through child cleanup.

    These descriptors preserve unlinked/replaced old objects. They are not
    inherited by units, and never establish alias closure or merge authority.
    Allocation uses the controller's unchanged kernel descriptor ceiling.
    """
    def __init__(self,root_fd,capture,working,copy,*,parent_uid,unit_uid,check):
        self.owner=os.getpid();self.closed=False;self.entries=[];self.parent_uid=parent_uid;self.unit_uid=unit_uid
        self.root_fd=None;self.working_fd=None;self.capture=copy_module.deepcopy(capture);self.prefixes={}
        try:
            if (type(root_fd) is not int or not stat.S_ISDIR(os.fstat(root_fd).st_mode)
                    or type(capture) is not dict or capture.get('kind')!='ci-paused-root-capture'
                    or capture.get('version')!=1 or capture.get('shipping_authority') is not False
                    or type(copy) is not dict or copy.get('kind')!='ci-contained-working-copy'
                    or copy.get('version')!=1 or copy.get('shipping_authority') is not False
                    or type(capture.get('entries')) is not list or len(capture['entries'])>MAX_ENTRIES):
                raise FreshnessHold('input_census_unknown')
            maps=copy.get('mapping')
            if (type(maps) is not list or len(maps)!=len(capture['entries'])
                    or len({r[0] for r in maps})!=len(maps)):
                raise FreshnessHold('input_census_unknown')
            mapping={r[0]:r for r in maps};seen=set()
            self.root_fd=os.dup(root_fd)
            self.working_fd=os.open(literal(working),os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW)
            for raw,kind,original,*digest in capture['entries']:
                check();name=relative(bytes.fromhex(raw));pair=mapping.get(raw)
                if (raw in seen or kind not in ('directory','file') or len(original)!=8 or pair is None
                        or len(pair)!=4 or pair[:3]!=[raw,kind,original[:2]]):
                    raise FreshnessHold('input_census_unknown')
                seen.add(raw);entry={'path':raw,'kind':kind,'original_key':original,'initial_digest':digest,
                    'original_fd':None,'working_fd':None};self.entries.append(entry)
                for label,base in (('original',self.root_fd),('working',self.working_fd)):
                    flags=os.O_RDONLY|os.O_DIRECTORY if kind=='directory' else (
                        os.O_RDWR if label=='original' and original[3]==parent_uid else os.O_RDONLY)
                    descriptor=_open_beneath(base,name,flags);entry[label+'_fd']=descriptor
                    info=os.fstat(descriptor)
                    if (stat.S_ISDIR(info.st_mode)!=(kind=='directory') or info.st_mode&0o7000
                            or os.listxattr(descriptor) or kind=='file' and info.st_nlink!=1):
                        raise FreshnessHold('invalidation_unsafe')
                    if label=='original':
                        if list(_key(info))!=original:raise FreshnessHold('input_census_unknown')
                    else:
                        owner=unit_uid if original[3]==parent_uid else original[3]
                        group=unit_uid if original[4]==parent_uid else original[4]
                        if ([info.st_dev,info.st_ino]!=pair[3] or (info.st_uid,info.st_gid)!=(owner,group)
                                or stat.S_IMODE(info.st_mode)!=stat.S_IMODE(original[5]) or info.st_mtime_ns!=original[6]
                                or kind=='file' and info.st_size!=original[2]):
                            raise FreshnessHold('input_census_unknown')
                    if kind=='file' and (len(digest)!=1 or self._read(descriptor,check)[1]!=digest[0]):
                        raise FreshnessHold('input_census_unknown')
                    if label=='working':entry['working_key']=list(_key(info))
                check()
            self.roots=[relative(bytes.fromhex(raw)) for raw in capture['roots']]
            for root in self.roots:
                for prefix in root.parents:
                    if prefix==Path('.') or prefix in self.prefixes:continue
                    fd=_open_beneath(self.working_fd,prefix,os.O_RDONLY|os.O_DIRECTORY)
                    try:self.prefixes[prefix]=list(_key(os.fstat(fd)))
                    finally:os.close(fd)
            self._allocation(check)
        except BaseException as error:
            self.close()
            if isinstance(error,OSError) and error.errno in (errno.EMFILE,errno.ENFILE,errno.ELOOP,errno.ENOTDIR):
                raise FreshnessHold('input_census_unknown') from None
            raise

    def _owned(self):
        if self.owner!=os.getpid() or self.closed:raise FreshnessHold('invalidation_unsafe')

    @staticmethod
    def _read(descriptor,check,writer=None):
        before=os.fstat(descriptor);count=0;digest=hashlib.sha256()
        if not stat.S_ISREG(before.st_mode) or not 0<=before.st_size<=MAX_BYTES:
            raise FreshnessHold('input_census_unknown')
        while count<before.st_size:
            check();block=os.pread(descriptor,min(1024**2,before.st_size-count),count)
            if not block:raise FreshnessHold('input_census_unknown')
            digest.update(block);count+=len(block)
            if writer is not None:
                remaining=memoryview(block)
                while remaining:
                    check();written=os.write(writer,remaining)
                    if written<=0:raise OSError(errno.EIO,'retained inode capture failed')
                    remaining=remaining[written:]
        check()
        if os.pread(descriptor,1,count) or _key(os.fstat(descriptor))!=_key(before):
            raise FreshnessHold('input_census_unknown')
        return count,digest.hexdigest()

    def snapshot_old(self,destination,*,check):
        """Seal post-child old inode bytes, including write→unlink/replacement.

        The caller must prove original child cleanup and keep its parent paused.
        No parent mutation or resume is authorized by this observation.
        """
        self._owned();destination=literal(destination);check();destination.mkdir(mode=0o700)
        rows=[];total=0;record_bytes=0
        for index,entry in enumerate(self.entries):
            check();original=os.fstat(entry['original_fd']);working=os.fstat(entry['working_fd'])
            if list(_key(original))!=entry['original_key']:raise FreshnessHold('input_census_unknown')
            initial=entry['working_key'];kind=entry['kind']
            if (list(_key(working))[:2]!=initial[:2] or (working.st_uid,working.st_gid)!=(initial[3],initial[4])
                    or stat.S_IFMT(working.st_mode)!=stat.S_IFMT(initial[5]) or working.st_mode&0o7000
                    or os.listxattr(entry['working_fd']) or kind=='file' and working.st_nlink not in (0,1)):
                raise FreshnessHold('invalidation_unsafe')
            row={'path':entry['path'],'kind':kind,'original_inode':entry['original_key'][:2],
                'working_key':list(_key(working)),'working_links':working.st_nlink}
            if kind=='file':
                if self._read(entry['original_fd'],check)[1]!=entry['initial_digest'][0]:
                    raise FreshnessHold('input_census_unknown')
                if working.st_size>MAX_BYTES-total:raise FreshnessHold('input_census_unknown')
                name=str(index);writer=os.open(destination/name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
                try:
                    size,digest=self._read(entry['working_fd'],check,writer)
                    os.fchmod(writer,0o444);os.fsync(writer)
                finally:os.close(writer)
                if _read(destination/name,os.geteuid(),True)[1:]!=(size,digest):
                    raise FreshnessHold('input_census_unknown')
                total+=size;row.update(payload=name,size=size,sha256=digest)
            rows.append(row);record_bytes+=len(json.dumps(row,separators=(',',':')).encode())
            if record_bytes>16*1024**2:raise FreshnessHold('input_census_unknown')
        check();destination.chmod(0o555);descriptor=os.open(destination,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
        return {'version':1,'kind':'ci-retained-inode-effects','entries':rows,'copied_bytes':total,
            'effects_materialized':False,'input_closure_complete':False,'shipping_authority':False}

    def _allocation(self,check):
        """The private container may contain only W and its fixed ancestors."""
        allowed=set(self.roots)|set(self.prefixes)
        for prefix in [Path('.'),*self.prefixes]:
            check()
            fd=os.dup(self.working_fd) if prefix==Path('.') else _open_beneath(self.working_fd,prefix,os.O_RDONLY|os.O_DIRECTORY)
            try:
                if prefix!=Path('.'):
                    info=list(_key(os.fstat(fd)));initial=self.prefixes[prefix]
                    if info[:2]!=initial[:2] or info[3:6]!=initial[3:6]:raise FreshnessHold('invalidation_unsafe')
                if any(prefix/name not in allowed for name in os.listdir(fd)):
                    raise FreshnessHold('invalidation_unsafe')
            finally:os.close(fd)

    def prepare_transfer(self,destination,before,*,check):
        """Prepare sealed images and an inode graph; do not mutate the parent.

        Original cleanup and a paused parent are caller-owned preconditions.
        Durable graph preparation is not completion or a reusable publication.
        """
        self._owned();destination,before=map(literal,(destination,before));check()
        def before_digest():
            check();rows=inventory(before,owner=os.geteuid(),sealed=True)
            return hashlib.sha256(json.dumps(rows,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        if before_digest()!=self.capture['payload_sha256']:raise FreshnessHold('input_census_unknown')
        self._allocation(check);destination.mkdir(mode=0o700)
        old=self.snapshot_old(destination/'old',check=check)
        after=capture_roots(self.working_fd,destination/'after',[os.fsdecode(root) for root in self.roots],
            owners=tuple(sorted({0,self.unit_uid})),check=check,allow_missing=True)
        final={};old_map={tuple(entry['working_key'][:2]):entry for entry in self.entries}
        if len(old_map)!=len(self.entries):raise FreshnessHold('invalidation_unsafe')
        for row in after['entries']:
            identity=tuple(row[2][:2])
            if identity in final:raise FreshnessHold('invalidation_unsafe')
            final[identity]=row
        effects={row['path']:row for row in old['entries']};objects=[]
        for entry in self.entries:
            check();initial=entry['working_key'];identity=tuple(initial[:2]);ending=final.get(identity)
            effect=effects[entry['path']];kind=entry['kind'];final_path=ending[0] if ending else None
            if ending is not None and (ending[1]!=kind or ending[2]!=effect['working_key']
                    or kind=='file' and ending[3]!=effect['sha256']):
                raise FreshnessHold('input_census_unknown')
            if (kind=='file' and effect['working_links']!=int(ending is not None)
                    or kind=='directory' and ending is None and effect['working_links']!=0):
                raise FreshnessHold('invalidation_unsafe')
            changed=kind=='file' and effect['sha256']!=entry['initial_digest'][0]
            metadata=effect['working_key'][3:]!=initial[3:]
            moved=final_path!=entry['path']
            if entry['original_key'][3]!=self.parent_uid and (changed or metadata or moved):
                raise FreshnessHold('invalidation_unsafe')
            objects.append({'initial_path':entry['path'],'final_path':final_path,'kind':kind,
                'original_inode':entry['original_key'][:2],'original_key':entry['original_key'],
                'initial_working_key':initial,'working_inode':list(identity),
                'content_changed':changed,'metadata_changed':metadata,'namespace_changed':moved,
                'old_effect':effect})
        for identity,row in final.items():
            if identity in old_map:continue
            check();raw,kind,info,*digest=row
            if (info[3],info[4])!=(self.unit_uid,self.unit_uid):raise FreshnessHold('invalidation_unsafe')
            objects.append({'initial_path':None,'final_path':raw,'kind':kind,'original_inode':None,
                'working_inode':list(identity),'content_changed':kind=='file','metadata_changed':True,
                'namespace_changed':True,'final_key':info,'sha256':digest[0] if digest else None})
        if len(objects)>MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
        self._allocation(check)
        for entry in self.entries:
            check()
            if list(_key(os.fstat(entry['original_fd'])))!=entry['original_key']:
                raise FreshnessHold('input_census_unknown')
            if list(_key(os.fstat(entry['working_fd'])))!=effects[entry['path']]['working_key']:
                raise FreshnessHold('input_census_unknown')
        if before_digest()!=self.capture['payload_sha256']:raise FreshnessHold('input_census_unknown')
        plan={'version':1,'kind':'ci-contained-transfer-plan','state':'prepared',
            'roots':self.capture['roots'],'before_root':str(before),'before':self.capture,
            'after':after,'old_effects':old,'objects':objects,'effects_materialized':False,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        plan['namespace_edges']=namespace_edges(plan)
        encoded=json.dumps(plan,sort_keys=True,separators=(',',':')).encode()
        if len(encoded)>16*1024**2:raise FreshnessHold('input_census_unknown')
        check();cache._write_record(destination/'plan.json',plan);check()
        return plan

    def apply_inode_effects(self,destination,plan,*,check):
        """Apply old-file bytes/mode/mtime through retained original inodes.

        Pure filesystem mechanics. The caller must separately prove exclusive
        writers, original child cleanup and a paused parent. This phase never
        changes namespace edges, resumes a parent or authorizes publication.
        Unknown partial writes remain fenced; only exact before/after images
        and committed original-inode progress can be replayed.
        """
        self._owned();destination=literal(destination);check()
        if (cache._read_record(destination/'plan.json',max_bytes=16*1024**2)!=plan
                or plan.get('kind')!='ci-contained-transfer-plan' or plan.get('state')!='prepared'
                or plan.get('before')!=self.capture or plan.get('effects_materialized') is not False
                or plan.get('shipping_authority') is not False):
            raise FreshnessHold('invalidation_unsafe')
        encode=lambda value:json.dumps(value,sort_keys=True,separators=(',',':')).encode()
        digest=hashlib.sha256(encode(plan)).hexdigest()
        for root,expected in ((literal(plan['before_root']),self.capture['payload_sha256']),
                (destination/'after',plan['after']['payload_sha256'])):
            check();rows=inventory(root,owner=os.geteuid(),sealed=True)
            if hashlib.sha256(encode(rows)).hexdigest()!=expected:raise FreshnessHold('input_census_unknown')
        objects={obj['initial_path']:obj for obj in plan['objects'] if obj['initial_path'] is not None}
        if len(objects)!=len(self.entries) or set(objects)!={e['path'] for e in self.entries}:
            raise FreshnessHold('input_census_unknown')
        targets={};entries={entry['path']:entry for entry in self.entries}
        for raw,entry in entries.items():
            check();obj=objects[raw];effect=obj['old_effect']
            if (obj['kind']!=entry['kind'] or obj['original_key']!=entry['original_key']
                    or obj['original_inode']!=entry['original_key'][:2]
                    or obj['initial_working_key']!=entry['working_key']
                    or obj['working_inode']!=entry['working_key'][:2]
                    or effect['path']!=raw or list(_key(os.fstat(entry['working_fd'])))!=effect['working_key']):
                raise FreshnessHold('input_census_unknown')
            if entry['kind']!='file':continue
            name=effect['payload']
            if type(name) is not str or not name.isascii() or not name.isdigit() or len(name)>6:
                raise FreshnessHold('invalidation_unsafe')
            if _read(destination/'old'/name,os.geteuid(),True)[1:]!=(effect['size'],effect['sha256']):
                raise FreshnessHold('input_census_unknown')
            content=effect['sha256']!=entry['initial_digest'][0]
            metadata=effect['working_key'][3:]!=entry['working_key'][3:]
            if obj['content_changed']!=content or obj['metadata_changed']!=metadata:
                raise FreshnessHold('input_census_unknown')
            if content or metadata:
                if entry['original_key'][3]!=self.parent_uid or fcntl.fcntl(entry['original_fd'],fcntl.F_GETFL)&os.O_ACCMODE!=os.O_RDWR:
                    raise FreshnessHold('invalidation_unsafe')
                targets[raw]=effect
        progress_path=destination/'inode-progress.json'
        birth=Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19]
        if progress_path.exists():
            progress=cache._read_record(progress_path,max_bytes=16*1024**2)
            if (progress.get('kind')!='ci-inode-application-progress' or progress.get('version')!=1
                    or progress.get('plan_sha256')!=digest or progress.get('owner_pid')!=self.owner
                    or progress.get('owner_birth')!=birth or progress.get('state') not in ('applying','inodes_applied')
                    or progress.get('effects_materialized') is not False or progress.get('shipping_authority') is not False
                    or progress.get('input_closure_complete') is not False or progress.get('full_ci') is not False
                    or type(progress.get('completed')) is not list or len(progress['completed'])>len(targets)):
                raise FreshnessHold('invalidation_unsafe')
        else:
            progress={'version':1,'kind':'ci-inode-application-progress','plan_sha256':digest,
                'owner_pid':self.owner,'owner_birth':birth,'state':'applying','active':None,'completed':[],
                'effects_materialized':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        def parent_state():
            done={row['path']:row for row in progress['completed']}
            if (len(done)!=len(progress['completed']) or not set(done)<=set(targets)
                    or progress['active'] is not None and (progress['active'] not in targets or progress['active'] in done)
                    or progress['state']=='inodes_applied' and (set(done)!=set(targets) or progress['active'] is not None)):
                raise FreshnessHold('invalidation_unsafe')
            recovered=set()
            for raw,entry in entries.items():
                check();descriptor=entry['original_fd'];info=os.fstat(descriptor);key=list(_key(info))
                named=_open_beneath(self.root_fd,relative(bytes.fromhex(raw)),os.O_RDONLY|(os.O_DIRECTORY if entry['kind']=='directory' else 0))
                try:
                    if _key(os.fstat(named))!=_key(info):raise FreshnessHold('input_census_unknown')
                finally:os.close(named)
                if (key[:2]!=entry['original_key'][:2] or key[3:5]!=entry['original_key'][3:5]
                        or os.listxattr(descriptor) or entry['kind']=='file' and info.st_nlink!=1):
                    raise FreshnessHold('invalidation_unsafe')
                if entry['kind']=='directory':
                    if key!=entry['original_key']:raise FreshnessHold('input_census_unknown')
                    continue
                size,sha=self._read(descriptor,check)
                if raw in done:
                    row=done[raw]
                    if (row['original_inode']!=entry['original_key'][:2] or key!=row['after_key']
                            or (size,sha)!=(targets[raw]['size'],targets[raw]['sha256'])):
                        raise FreshnessHold('input_census_unknown')
                elif key==entry['original_key'] and sha==entry['initial_digest'][0]:continue
                elif progress['active']==raw:
                    effect=targets[raw]
                    if ((size,sha)!=(effect['size'],effect['sha256'])
                            or stat.S_IMODE(info.st_mode)!=stat.S_IMODE(effect['working_key'][5])
                            or info.st_mtime_ns!=effect['working_key'][6]):
                        raise FreshnessHold('input_census_unknown')
                    recovered.add(raw)
                else:raise FreshnessHold('input_census_unknown')
            return done,recovered
        def persist():
            check()
            if len(encode(progress))>16*1024**2:raise FreshnessHold('input_census_unknown')
            cache._write_record(progress_path,progress)
        done,recovered=parent_state()
        if progress['state']=='inodes_applied':return progress
        persist()
        for raw,effect in targets.items():
            if raw in done:continue
            check();entry=entries[raw];descriptor=entry['original_fd']
            progress['active']=raw;persist();check()
            if raw not in recovered:
                if list(_key(os.fstat(descriptor)))!=entry['original_key']:
                    raise FreshnessHold('input_census_unknown')
                if objects[raw]['content_changed']:
                    source=os.open(destination/'old'/effect['payload'],os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
                    try:
                        offset=0
                        while offset<effect['size']:
                            check();body=os.pread(source,min(1024**2,effect['size']-offset),offset)
                            if not body:raise FreshnessHold('input_census_unknown')
                            remaining=memoryview(body)
                            while remaining:
                                check();written=os.pwrite(descriptor,remaining,offset)
                                if written<=0:raise OSError(errno.EIO,'original inode effect write failed')
                                offset+=written;remaining=remaining[written:]
                        os.ftruncate(descriptor,effect['size'])
                    finally:os.close(source)
                if self._read(descriptor,check)!=(effect['size'],effect['sha256']):
                    raise FreshnessHold('input_census_unknown')
                atime=os.fstat(descriptor).st_atime_ns
                os.fchmod(descriptor,stat.S_IMODE(effect['working_key'][5]))
                os.utime(descriptor,ns=(atime,effect['working_key'][6]))
            check();os.fsync(descriptor)
            row={'path':raw,'original_inode':entry['original_key'][:2],'after_key':list(_key(os.fstat(descriptor)))}
            progress['completed'].append(row);progress['active']=None
            persist()
        parent_state();check();progress['state']='inodes_applied';persist();check()
        return progress

    def close(self):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        if self.closed:return
        self.closed=True
        for entry in self.entries:
            for label in ('original_fd','working_fd'):
                if entry[label] is not None:os.close(entry[label]);entry[label]=None
        for label in ('root_fd','working_fd'):
            descriptor=getattr(self,label)
            if descriptor is not None:os.close(descriptor);setattr(self,label,None)

    def __enter__(self):self._owned();return self
    def __exit__(self,*_args):self.close()




def _rename_noreplace(source_parent,source_name,destination_parent,destination_name):
    library=ctypes.CDLL(None,use_errno=True)
    try:rename=library.renameat2
    except AttributeError:raise FreshnessHold('invalidation_unsafe') from None
    rename.argtypes=[ctypes.c_int,ctypes.c_char_p,ctypes.c_int,ctypes.c_char_p,ctypes.c_uint]
    rename.restype=ctypes.c_int
    if rename(source_parent,source_name,destination_parent,destination_name,1)!=0:
        raise OSError(ctypes.get_errno(),'namespace move unavailable')


def _link_prepared_inode(inode_fd,parent,name):
    library=ctypes.CDLL(None,use_errno=True);link=library.linkat
    link.argtypes=[ctypes.c_int,ctypes.c_char_p,ctypes.c_int,ctypes.c_char_p,ctypes.c_int];link.restype=ctypes.c_int
    if link(-100,os.fsencode('/proc/self/fd/'+str(inode_fd)),parent,name,0x400)!=0:
        raise OSError(ctypes.get_errno(),'prepared inode attachment unavailable')


def _namespace_directory(descriptor,check):
    check();before=os.fstat(descriptor)
    if not stat.S_ISDIR(before.st_mode):raise FreshnessHold('invalidation_unsafe')
    rows=[];record_bytes=0
    for name in sorted(os.listdir(descriptor),key=os.fsencode):
        check();info=os.stat(name,dir_fd=descriptor,follow_symlinks=False)
        row=[os.fsencode(name).hex(),list(_key(info)),info.st_nlink]
        record_bytes+=len(json.dumps(row,separators=(',',':')).encode());rows.append(row)
        if len(rows)>MAX_ENTRIES or record_bytes>16*1024**2:raise FreshnessHold('input_census_unknown')
    if _key(os.fstat(descriptor))!=_key(before):raise FreshnessHold('input_census_unknown')
    return {'key':list(_key(before)),'links':before.st_nlink,'entries':rows}


def apply_namespace_step(intent,*,plan_sha256,action,inode_fd,source_parent=None,source_name=None,
        destination_parent=None,destination_name=None,check):
    """Apply/replay one bound kernel namespace operation using a held inode.

    Caller-owned prerequisites include a paused parent, exclusive writers, the
    original child cleanup and a validated whole-transfer sequence. This step
    alone never completes copyback, resumes a parent or certifies CI. Replay
    accepts an exact before state or the expected inode/whole-parent transition;
    it cannot adopt an object based on its filename.
    """
    intent=literal(intent);check()
    if (type(plan_sha256) is not str or len(plan_sha256)!=64 or any(c not in '0123456789abcdef' for c in plan_sha256)
            or action not in ('move','remove','attach') or type(inode_fd) is not int):
        raise FreshnessHold('invalidation_unsafe')
    if ((source_parent is not None)!=(action in ('move','remove'))
            or (destination_parent is not None)!=(action in ('move','attach'))):
        raise FreshnessHold('invalidation_unsafe')
    def basename(value):
        name=relative(value)
        if len(name.parts)!=1:raise FreshnessHold('invalidation_unsafe')
        return os.fsencode(name)
    source_name=basename(source_name) if source_parent is not None else None
    destination_name=basename(destination_name) if destination_parent is not None else None
    info=os.fstat(inode_fd);is_directory=stat.S_ISDIR(info.st_mode)
    if (not (is_directory or stat.S_ISREG(info.st_mode)) or info.st_mode&0o7000
            or os.listxattr(inode_fd) or action=='attach' and is_directory):
        raise FreshnessHold('invalidation_unsafe')
    inode_identity=[info.st_dev,info.st_ino];parents={};endpoints={}
    for label,descriptor in (('source',source_parent),('destination',destination_parent)):
        if descriptor is None:continue
        parent=os.fstat(descriptor);identity=str(parent.st_dev)+':'+str(parent.st_ino)
        if not stat.S_ISDIR(parent.st_mode) or parent.st_dev!=info.st_dev:raise FreshnessHold('invalidation_unsafe')
        parents[identity]=descriptor;endpoints[label]=identity
    if action=='move' and endpoints['source']==endpoints['destination'] and source_name==destination_name:
        raise FreshnessHold('invalidation_unsafe')
    controller=intent.parent.stat()
    if (controller.st_uid!=os.geteuid() or stat.S_IMODE(controller.st_mode)!=0o700
            or (controller.st_dev,controller.st_ino) in {(os.fstat(fd).st_dev,os.fstat(fd).st_ino) for fd in parents.values()}
            or [controller.st_dev,controller.st_ino]==inode_identity):
        raise FreshnessHold('invalidation_unsafe')
    encode=lambda value:json.dumps(value,sort_keys=True,separators=(',',':')).encode()
    birth=Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19]
    binding={'plan_sha256':plan_sha256,'action':action,'inode':inode_identity,'owner_pid':os.getpid(),
        'owner_birth':birth,'endpoints':endpoints,'source_name':None if source_name is None else source_name.hex(),
        'destination_name':None if destination_name is None else destination_name.hex()}
    def snapshot():
        check();info=os.fstat(inode_fd)
        node={'key':list(_key(info)),'links':info.st_nlink}
        if is_directory:
            node['children']=[] if info.st_nlink==0 else _namespace_directory(inode_fd,check)['entries']
        else:node['size'],node['sha256']=RetainedInodes._read(inode_fd,check)
        result={'inode':node,'parents':{key:_namespace_directory(fd,check) for key,fd in parents.items()}}
        if sum(len(row['entries']) for row in result['parents'].values())>MAX_ENTRIES:
            raise FreshnessHold('input_census_unknown')
        if len(encode(result))>16*1024**2:raise FreshnessHold('input_census_unknown')
        return result
    def persist(value):
        check()
        if len(encode(value))>16*1024**2:raise FreshnessHold('input_census_unknown')
        cache._write_record(intent,value)
    def synced():
        check();os.fsync(inode_fd)
        for fd in parents.values():check();os.fsync(fd)
    if intent.exists():
        record=cache._read_record(intent,max_bytes=16*1024**2)
        if (record.get('kind')!='ci-namespace-step' or record.get('version')!=1 or record.get('binding')!=binding
                or record.get('state') not in ('prepared','applied') or record.get('effects_materialized') is not False
                or record.get('input_closure_complete') is not False or record.get('full_ci') is not False
                or record.get('shipping_authority') is not False):raise FreshnessHold('invalidation_unsafe')
    else:
        synced();before=snapshot();node=before['inode']
        if node['links']!=(0 if action=='attach' else (node['links'] if is_directory else 1)):
            raise FreshnessHold('invalidation_unsafe')
        if source_parent is not None:
            source=dict((r[0],r[1:]) for r in before['parents'][endpoints['source']]['entries']).get(source_name.hex())
            if source!=[node['key'],node['links']]:raise FreshnessHold('input_census_unknown')
        if destination_parent is not None and destination_name.hex() in {r[0] for r in before['parents'][endpoints['destination']]['entries']}:
            raise FreshnessHold('invalidation_unsafe')
        record={'version':1,'kind':'ci-namespace-step','binding':binding,'state':'prepared','before':before,
            'effects_materialized':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        persist(record)
    before=record['before'];current=snapshot()
    def postcondition(actual):return _namespace_step_after(record,actual)
    if record['state']=='applied':
        if current!=record['after'] or not postcondition(current):raise FreshnessHold('input_census_unknown')
        return record
    if current==before:
        check()
        if action=='move':_rename_noreplace(source_parent,source_name,destination_parent,destination_name)
        elif action=='remove':
            if is_directory:os.rmdir(source_name,dir_fd=source_parent)
            else:os.unlink(source_name,dir_fd=source_parent)
        else:_link_prepared_inode(inode_fd,destination_parent,destination_name)
        current=snapshot()
    if not postcondition(current):raise FreshnessHold('input_census_unknown')
    synced();record['state']='applied';record['after']=snapshot()
    if not postcondition(record['after']):raise FreshnessHold('input_census_unknown')
    persist(record);return record


def _namespace_step_after(record,actual):
    before=record['before'];binding=record['binding'];action=binding['action'];endpoints=binding['endpoints']
    inode_identity=binding['inode'];is_directory=stat.S_ISDIR(before['inode']['key'][5])
    source_name=None if binding['source_name'] is None else bytes.fromhex(binding['source_name'])
    destination_name=None if binding['destination_name'] is None else bytes.fromhex(binding['destination_name'])
    old=before['inode'];node=actual['inode']
    expected_links=0 if action=='remove' else (1 if action=='attach' else old['links'])
    ignored={7}|({2,6} if is_directory and action=='remove' else set())
    if (node['links']!=expected_links or any(a!=b for i,(a,b) in enumerate(zip(old['key'],node['key'])) if i not in ignored)
            or (not is_directory and (node['size'],node['sha256'])!=(old['size'],old['sha256']))
            or (is_directory and node['children']!=old['children'])):return False
    expected={key:{r[0]:r[1:] for r in row['entries']} for key,row in before['parents'].items()}
    if 'source' in endpoints:
        if expected[endpoints['source']].pop(source_name.hex(),None)!=[old['key'],old['links']]:return False
    if 'destination' in endpoints:expected[endpoints['destination']][destination_name.hex()]=[old['key'],expected_links]
    deltas={key:0 for key in before['parents']}
    if is_directory:
        if 'source' in endpoints:deltas[endpoints['source']]-=1
        if 'destination' in endpoints:deltas[endpoints['destination']]+=1
    parent_identities={tuple(row['key'][:2]):key for key,row in before['parents'].items()}
    for key,parent in before['parents'].items():
        now=actual['parents'][key]
        if (now['links']!=parent['links']+deltas[key]
                or any(a!=b for i,(a,b) in enumerate(zip(parent['key'],now['key'])) if i not in (2,6,7))):return False
        rows={r[0]:r[1:] for r in now['entries']}
        if set(rows)!=set(expected[key]):return False
        for name,(metadata,links) in expected[key].items():
            observed,observed_links=rows[name];identity=tuple(metadata[:2]);ignore=set()
            if list(identity)==inode_identity:ignore={7}
            if identity in parent_identities:
                ignore={2,6,7};links+=deltas[parent_identities[identity]]
            if observed_links!=links or any(a!=b for i,(a,b) in enumerate(zip(metadata,observed)) if i not in ignore):return False
    return True



def _descriptor_mount_id(descriptor):
    rows=Path('/proc/self/fdinfo/'+str(descriptor)).read_text().splitlines()
    values=[line.split()[1] for line in rows if line.startswith('mnt_id:')]
    if len(values)!=1 or not values[0].isdigit():raise FreshnessHold('invalidation_unsafe')
    return int(values[0])


def apply_namespace_sequence(directory,*,plan_sha256,steps,inodes,parents,check):
    """Sequence held-capability operations with a verified completion prefix.

    The caller owns whole-transfer allocation/image/writer/pause proof and all
    staging/creation capabilities. No parent resume or materialization authority
    is returned here. Only the current active operation can be replayed; finished
    operations are checked against immutable receipts and the whole current
    graph rather than against their obsolete per-step parent snapshots.
    """
    directory=literal(directory);check();control=directory.stat()
    if (control.st_uid!=os.geteuid() or stat.S_IMODE(control.st_mode)!=0o700
            or type(plan_sha256) is not str or len(plan_sha256)!=64
            or any(c not in '0123456789abcdef' for c in plan_sha256)
            or type(steps) is not list or len(steps)>2*MAX_ENTRIES
            or type(inodes) is not dict or type(parents) is not dict or len(inodes)>MAX_ENTRIES):
        raise FreshnessHold('invalidation_unsafe')
    steps=copy_module.deepcopy(steps);inodes=dict(inodes);parents=dict(parents)
    encode=lambda value:json.dumps(value,sort_keys=True,separators=(',',':')).encode()
    def name(value):
        if type(value) is not str:raise FreshnessHold('invalidation_unsafe')
        result=relative(bytes.fromhex(value))
        if len(result.parts)!=1:raise FreshnessHold('invalidation_unsafe')
        return os.fsencode(result)
    def identity(fd):
        info=os.fstat(fd);return str(info.st_dev)+':'+str(info.st_ino)
    def role(key):
        if type(key) is not str or not 0<len(key)<=4096 or '\0' in key:raise FreshnessHold('invalidation_unsafe')
    contexts={};parent_roles={};node_roles={};mounts={};fingerprints={'inodes':{},'parents':{}}
    for key,fd in inodes.items():
        role(key);info=os.fstat(fd);physical=identity(fd)
        if (physical in node_roles.values() or not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode))
                or info.st_mode&0o7000 or os.listxattr(fd)):
            raise FreshnessHold('invalidation_unsafe')
        node_roles[key]=physical;fingerprints['inodes'][key]=[info.st_dev,info.st_ino,_descriptor_mount_id(fd),stat.S_IFMT(info.st_mode)]
        if stat.S_ISDIR(info.st_mode):contexts[physical]=fd
    for key,fd in parents.items():
        role(key);info=os.fstat(fd);physical=identity(fd)
        if not stat.S_ISDIR(info.st_mode):raise FreshnessHold('invalidation_unsafe')
        parent_roles[key]=physical;fingerprints['parents'][key]=[info.st_dev,info.st_ino,_descriptor_mount_id(fd)]
        if physical in contexts and _descriptor_mount_id(contexts[physical])!=_descriptor_mount_id(fd):
            raise FreshnessHold('invalidation_unsafe')
        contexts[physical]=fd
    for physical,fd in contexts.items():
        if [control.st_dev,control.st_ino]==list(_key(os.fstat(fd)))[:2]:raise FreshnessHold('invalidation_unsafe')
        mounts[physical]=_descriptor_mount_id(fd)
    for step in steps:
        if (type(step) is not dict or step.get('action') not in ('move','remove','attach')
                or step.get('inode') not in inodes):raise FreshnessHold('invalidation_unsafe')
        expected={'action','inode'}
        if step['action'] in ('move','remove'):expected|={'source_parent','source_name'}
        if step['action'] in ('move','attach'):expected|={'destination_parent','destination_name'}
        if set(step)!=expected:raise FreshnessHold('invalidation_unsafe')
        for side in ('source','destination'):
            if side+'_parent' in step:
                if step[side+'_parent'] not in parent_roles:raise FreshnessHold('invalidation_unsafe')
                name(step[side+'_name'])
    def snapshot():
        check();nodes={};total=0
        for key,fd in inodes.items():
            info=os.fstat(fd);node={'key':list(_key(info)),'links':info.st_nlink}
            if stat.S_ISDIR(info.st_mode):node['children']=[] if info.st_nlink==0 else _namespace_directory(fd,check)['entries']
            else:
                node['size'],node['sha256']=RetainedInodes._read(fd,check);total+=node['size']
                if total>MAX_BYTES:raise FreshnessHold('input_census_unknown')
            if _key(os.fstat(fd))!=tuple(node['key']):raise FreshnessHold('input_census_unknown')
            nodes[key]=node
        forest={}
        for key,fd in contexts.items():
            info=os.fstat(fd)
            forest[key]=({'key':list(_key(info)),'links':0,'entries':[]} if info.st_nlink==0 else _namespace_directory(fd,check))
        members={tuple(row[1][:2]) for parent in forest.values() for row in parent['entries']}
        members|={tuple(node['key'][:2]) for node in nodes.values()}
        if len(members)>MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
        result={'inodes':nodes,'parents':forest}
        if len(encode(result))>16*1024**2:raise FreshnessHold('input_census_unknown')
        return result
    def preflight(frame):
        members={key:{r[0]:r[1][:2] for r in row['entries']} for key,row in frame['parents'].items()}
        links={key:row['links'] for key,row in frame['parents'].items()}
        links.update({node_roles[key]:row['links'] for key,row in frame['inodes'].items()})
        for step in steps:
            obj=frame['inodes'][step['inode']];physical=node_roles[step['inode']];inode=obj['key'][:2]
            is_dir=stat.S_ISDIR(obj['key'][5]);action=step['action']
            source=parent_roles.get(step.get('source_parent'));destination=parent_roles.get(step.get('destination_parent'))
            if action=='attach' and (is_dir or links[physical]!=0):raise FreshnessHold('invalidation_unsafe')
            if source is not None:
                if links[source]==0 or members[source].get(step['source_name'])!=inode:raise FreshnessHold('input_census_unknown')
                if fingerprints['inodes'][step['inode']][2]!=mounts[source]:raise FreshnessHold('invalidation_unsafe')
                if not is_dir and links[physical]!=1:raise FreshnessHold('invalidation_unsafe')
            if destination is not None:
                if (links[destination]==0 or step['destination_name'] in members[destination]
                        or os.fstat(inodes[step['inode']]).st_dev!=os.fstat(contexts[destination]).st_dev):
                    raise FreshnessHold('invalidation_unsafe')
            if action=='move' and mounts[source]!=mounts[destination]:raise FreshnessHold('invalidation_unsafe')
            if action=='attach' and fingerprints['inodes'][step['inode']][2]!=mounts[destination]:raise FreshnessHold('invalidation_unsafe')
            if action=='remove' and is_dir and members[physical]:raise FreshnessHold('invalidation_unsafe')
            if is_dir and destination is not None:
                ancestors={}
                for parent,rows in members.items():
                    for child in rows.values():
                        child_id=str(child[0])+':'+str(child[1])
                        if child_id in contexts:ancestors[child_id]=parent
                cursor=destination;seen=set()
                while cursor not in seen:
                    if cursor==physical:raise FreshnessHold('invalidation_unsafe')
                    seen.add(cursor)
                    if cursor not in ancestors:break
                    cursor=ancestors[cursor]
            if source is not None:
                members[source].pop(step['source_name'])
                if is_dir:links[source]-=1
            if destination is not None:
                members[destination][step['destination_name']]=inode
                if is_dir:links[destination]+=1
            if action=='remove':links[physical]=0
            elif action=='attach':links[physical]=1
    binding={'plan_sha256':plan_sha256,'steps':steps,'capabilities':fingerprints,'owner_pid':os.getpid(),
        'owner_birth':Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19]}
    path=directory/'sequence.json'
    def persist(record):
        check()
        if len(encode(record))>16*1024**2:raise FreshnessHold('input_census_unknown')
        cache._write_record(path,record)
    def step_binding(index):
        step=steps[index];inode=os.fstat(inodes[step['inode']]);endpoints={}
        for side in ('source','destination'):
            if side+'_parent' in step:endpoints[side]=parent_roles[step[side+'_parent']]
        return {'plan_sha256':plan_sha256,'action':step['action'],'inode':[inode.st_dev,inode.st_ino],
            'owner_pid':binding['owner_pid'],'owner_birth':binding['owner_birth'],'endpoints':endpoints,
            'source_name':step.get('source_name'),'destination_name':step.get('destination_name')}
    def footprint(frame,index):
        return {'inode':frame['inodes'][steps[index]['inode']],
            'parents':{key:frame['parents'][key] for key in set(step_binding(index)['endpoints'].values())}}
    def projected(frame,index,after):
        expected=copy_module.deepcopy(frame);expected['inodes'][steps[index]['inode']]=copy_module.deepcopy(after['inode'])
        updates={tuple(row['key'][:2]):row for row in after['parents'].values()}
        updates[tuple(after['inode']['key'][:2])]=after['inode']
        expected['parents'].update(copy_module.deepcopy(after['parents']))
        for parent in expected['parents'].values():
            changed=updates.get(tuple(parent['key'][:2]))
            if changed is not None:
                parent['key']=changed['key'];parent['links']=changed['links']
                if 'entries' in changed:parent['entries']=copy_module.deepcopy(changed['entries'])
                elif 'children' in changed:parent['entries']=copy_module.deepcopy(changed['children'])
            for row in parent['entries']:
                changed=updates.get(tuple(row[1][:2]))
                if changed is not None:row[1]=changed['key'];row[2]=changed['links']
        for node in expected['inodes'].values():
            changed=updates.get(tuple(node['key'][:2]))
            if changed is not None:
                node['key']=changed['key'];node['links']=changed['links']
            # A changed child's metadata also changes its row in every held
            # ancestor view, even when that ancestor's own inode is unchanged.
            if stat.S_ISDIR(node['key'][5]):
                physical=str(node['key'][0])+':'+str(node['key'][1]);node['children']=copy_module.deepcopy(expected['parents'][physical]['entries'])
        return expected
    if path.exists():
        record=cache._read_record(path,max_bytes=16*1024**2)
        if (record.get('kind')!='ci-namespace-sequence' or record.get('version')!=1 or record.get('binding')!=binding
                or record.get('state') not in ('applying','namespace_sequence_applied')
                or type(record.get('next_step')) is not int or not 0<=record['next_step']<=len(steps)
                or record.get('active_step') not in (None,record['next_step'])
                or record.get('effects_materialized') is not False or record.get('shipping_authority') is not False
                or record.get('full_ci') is not False or record.get('input_closure_complete') is not False
                or type(record.get('step_receipts')) is not list or len(record['step_receipts'])!=record['next_step']):
            raise FreshnessHold('invalidation_unsafe')
        for index,digest in enumerate(record['step_receipts']):
            receipt=cache._read_record(directory/('step-'+str(index)+'.json'),max_bytes=16*1024**2)
            if (receipt.get('state')!='applied' or receipt.get('binding')!=step_binding(index)
                    or hashlib.sha256(encode(receipt)).hexdigest()!=digest):raise FreshnessHold('input_census_unknown')
    else:
        frame=snapshot();preflight(frame)
        record={'version':1,'kind':'ci-namespace-sequence','binding':binding,'state':'applying','next_step':0,
            'active_step':None,'snapshot':frame,'initial_sha256':hashlib.sha256(encode(frame)).hexdigest(),
            'step_receipts':[],'effects_materialized':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        persist(record)
    if record['state']=='namespace_sequence_applied':
        if record['next_step']!=len(steps) or record['active_step'] is not None or snapshot()!=record['snapshot']:
            raise FreshnessHold('input_census_unknown')
        return record
    while record['next_step']<len(steps):
        index=record['next_step'];step=steps[index];step_path=directory/('step-'+str(index)+'.json');current=snapshot()
        if current!=record['snapshot']:
            if record['active_step']!=index or not step_path.exists():raise FreshnessHold('input_census_unknown')
            active=cache._read_record(step_path,max_bytes=16*1024**2)
            if (active.get('binding')!=step_binding(index) or active.get('before')!=footprint(record['snapshot'],index)
                    or not _namespace_step_after(active,footprint(current,index))
                    or projected(record['snapshot'],index,footprint(current,index))!=current):
                raise FreshnessHold('input_census_unknown')
        elif step_path.exists():
            active=cache._read_record(step_path,max_bytes=16*1024**2)
            if active.get('binding')!=step_binding(index) or active.get('before')!=footprint(record['snapshot'],index):
                raise FreshnessHold('input_census_unknown')
        record['active_step']=index;persist(record)
        kwargs={'plan_sha256':plan_sha256,'action':step['action'],'inode_fd':inodes[step['inode']],'check':check}
        for side in ('source','destination'):
            if side+'_parent' in step:kwargs[side+'_parent']=parents[step[side+'_parent']];kwargs[side+'_name']=name(step[side+'_name'])
        applied=apply_namespace_step(step_path,**kwargs)
        if applied['before']!=footprint(record['snapshot'],index):raise FreshnessHold('input_census_unknown')
        expected=projected(record['snapshot'],index,applied['after'])
        if snapshot()!=expected:raise FreshnessHold('input_census_unknown')
        record['snapshot']=expected;record['step_receipts'].append(hashlib.sha256(encode(applied)).hexdigest())
        record['next_step']+=1;record['active_step']=None;persist(record)
    if record['active_step'] is not None or snapshot()!=record['snapshot']:
        raise FreshnessHold('input_census_unknown')
    check();record['state']='namespace_sequence_applied';persist(record);return record



class PreparedTransferObjects:
    """Own new-object capabilities inside one controller lifetime.

    Caller owns allocation/image/alias/pause proof and stage retention under R.
    Named creation without a recorded or still-held inode stays unknown. An
    anonymous file never becomes a prepared replacement based on a pathname.
    These receipts are not materialization, publication or cleanup authority.
    """
    def __init__(self,directory,*,plan_sha256,check):
        self.directory=literal(directory);self.owner=os.getpid();self.uid=os.geteuid();self.gid=os.getegid()
        self.birth=Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19]
        self.check=check;self.closed=False;self.files={};self.directories={};self.file_holder_nonce=os.urandom(32).hex()
        if (type(plan_sha256) is not str or len(plan_sha256)!=64
                or any(c not in '0123456789abcdef' for c in plan_sha256)):
            raise FreshnessHold('invalidation_unsafe')
        self.plan=plan_sha256;info=self.directory.stat();self.control=[info.st_dev,info.st_ino]
        self._owned()

    def _owned(self):
        if self.owner!=os.getpid() or self.closed or self.uid!=os.geteuid() or self.gid!=os.getegid():
            raise FreshnessHold('invalidation_unsafe')
        self.check();info=self.directory.lstat()
        if (not stat.S_ISDIR(info.st_mode) or [info.st_dev,info.st_ino]!=self.control
                or (info.st_uid,info.st_gid)!=(self.uid,self.gid) or stat.S_IMODE(info.st_mode)!=0o700):
            raise FreshnessHold('invalidation_unsafe')

    def _slot(self,kind,key,parent,extra):
        self._owned()
        if type(key) is not str or not 0<len(key)<=4096 or '\0' in key:raise FreshnessHold('invalidation_unsafe')
        info=os.fstat(parent)
        if not stat.S_ISDIR(info.st_mode):raise FreshnessHold('invalidation_unsafe')
        token=hashlib.sha256((kind+':'+key).encode()).hexdigest();path=self.directory/(token+'.json')
        binding={'plan_sha256':self.plan,'object':key,'role':kind,'owner_pid':self.owner,'owner_birth':self.birth,
            'control':self.control,'parent':[info.st_dev,info.st_ino,_descriptor_mount_id(parent)],**extra}
        record=None
        if path.exists():
            record=cache._read_record(path,max_bytes=16*1024**2)
            if (record.get('version')!=1 or record.get('binding')!=binding or record.get('kind')!='ci-prepared-'+kind
                    or record.get('state') not in ('preparing','prepared') or record.get('effects_materialized') is not False
                    or record.get('input_closure_complete') is not False or record.get('full_ci') is not False
                    or record.get('shipping_authority') is not False):raise FreshnessHold('invalidation_unsafe')
        self._budget(token,kind,extra)
        return token,path,binding,record

    def _budget(self,token,kind,extra):
        # All keys share the existing per-transfer object/byte ceilings. A
        # preparing receipt keeps its reservation, and retrying that same key
        # neither charges again nor creates a second anonymous capability.
        entries=list(self.directory.iterdir());used_bytes=0;known=set()
        if len(entries)>MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
        for path in entries:
            record=cache._read_record(path,max_bytes=16*1024**2);binding=record.get('binding',{})
            role=binding.get('role');key=binding.get('object')
            if (role not in ('file','directory') or type(key) is not str or not 0<len(key)<=4096
                    or record.get('kind')!='ci-prepared-'+role or record.get('version')!=1
                    or record.get('state') not in ('preparing','prepared') or binding.get('plan_sha256')!=self.plan
                    or binding.get('owner_pid')!=self.owner or binding.get('owner_birth')!=self.birth
                    or binding.get('control')!=self.control or record.get('effects_materialized') is not False
                    or record.get('input_closure_complete') is not False or record.get('full_ci') is not False
                    or record.get('shipping_authority') is not False):raise FreshnessHold('invalidation_unsafe')
            identity=hashlib.sha256((role+':'+key).encode()).hexdigest()
            if path.name!=identity+'.json' or identity in known:raise FreshnessHold('invalidation_unsafe')
            known.add(identity)
            if role=='file':
                size=binding.get('expected',{}).get('size')
                if type(size) is not int or not 0<=size<=MAX_BYTES:raise FreshnessHold('input_census_unknown')
                used_bytes+=size
        if token not in known:
            if len(known)>=MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
            if kind=='file':used_bytes+=extra['expected']['size']
        if used_bytes>MAX_BYTES:raise FreshnessHold('input_census_unknown')

    def _record(self,path,binding,state,**fields):
        self._owned();record={'version':1,'kind':'ci-prepared-'+binding['role'],'binding':binding,'state':state,
            'effects_materialized':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False,**fields}
        if len(json.dumps(record,sort_keys=True,separators=(',',':')).encode())>16*1024**2:
            raise FreshnessHold('input_census_unknown')
        cache._write_record(path,record);return record

    def prepare_file(self,key,parent,payload,expected):
        self._owned();payload=literal(payload);expected=copy_module.deepcopy(expected)
        if (type(expected) is not dict or set(expected)!={'size','sha256','mode','mtime_ns','uid','gid'}
                or any(type(expected[k]) is not int for k in ('size','mode','mtime_ns','uid','gid'))
                or not 0<=expected['size']<=MAX_BYTES or not 0<=expected['mode']<=0o777
                or not -(2**63)<=expected['mtime_ns']<2**63
                or any(not 0<=expected[k]<2**32-1 for k in ('uid','gid'))
                or type(expected['sha256']) is not str or len(expected['sha256'])!=64
                or any(c not in '0123456789abcdef' for c in expected['sha256'])):
            raise FreshnessHold('invalidation_unsafe')
        token,path,binding,record=self._slot('file',key,parent,{'payload':str(payload),'expected':expected,'holder_nonce':self.file_holder_nonce})
        def verify(entry):
            fd=entry['fd'];info=os.fstat(fd)
            if ([info.st_dev,info.st_ino]!=entry['inode'] or info.st_nlink!=0 or not stat.S_ISREG(info.st_mode)
                    or (info.st_uid,info.st_gid)!=(expected['uid'],expected['gid'])
                    or stat.S_IMODE(info.st_mode)!=expected['mode'] or info.st_mtime_ns!=expected['mtime_ns']
                    or os.listxattr(fd) or RetainedInodes._read(fd,self.check)!=(expected['size'],expected['sha256'])):
                raise FreshnessHold('input_census_unknown')
            return list(_key(info))
        if token in self.files:
            entry=self.files[token];metadata=verify(entry)
            if record is None or record['state']=='prepared' and record.get('inode')!=entry['inode']:
                raise FreshnessHold('invalidation_unsafe')
            self._record(path,binding,'prepared',inode=entry['inode'],metadata=metadata)
            return entry['fd']
        if record is not None and record['state']=='prepared':raise FreshnessHold('cleanup_uncertain')
        self._record(path,binding,'preparing')
        source=None;descriptor=None;ready=False
        try:
            source=os.open(payload,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK|getattr(os,'O_NOATIME',0))
            info=os.fstat(source)
            if (not stat.S_ISREG(info.st_mode) or (info.st_uid,info.st_gid)!=(self.uid,self.gid)
                    or info.st_nlink!=1 or stat.S_IMODE(info.st_mode) not in (0o444,0o555)
                    or info.st_size!=expected['size'] or os.listxattr(source)):
                raise FreshnessHold('input_census_unknown')
            self.check();descriptor=os.open('.',os.O_TMPFILE|os.O_RDWR|os.O_CLOEXEC,0o600,dir_fd=parent)
            if RetainedInodes._read(source,self.check,descriptor)!=(expected['size'],expected['sha256']):
                raise FreshnessHold('input_census_unknown')
            current=os.fstat(descriptor)
            if (current.st_uid,current.st_gid)!=(expected['uid'],expected['gid']):
                os.fchown(descriptor,expected['uid'],expected['gid'])
            os.fchmod(descriptor,expected['mode']);os.utime(descriptor,ns=(expected['mtime_ns'],expected['mtime_ns']))
            self.check();os.fsync(descriptor)
            entry={'fd':descriptor,'inode':list(_key(os.fstat(descriptor)))[:2]};metadata=verify(entry)
            self.files[token]=entry;ready=True
            self._record(path,binding,'prepared',inode=entry['inode'],metadata=metadata)
            return descriptor
        finally:
            if source is not None:os.close(source)
            if descriptor is not None and not ready:os.close(descriptor)

    def prepare_directory(self,key,parent,name):
        self._owned();name=os.fsencode(relative(name))
        if len(relative(name).parts)!=1:raise FreshnessHold('invalidation_unsafe')
        token,path,binding,record=self._slot('directory',key,parent,{'name':name.hex()})
        def named():
            try:return os.stat(name,dir_fd=parent,follow_symlinks=False)
            except FileNotFoundError:return None
        def verify(entry,empty=False):
            fd=entry['fd'];info=os.fstat(fd);found=named()
            if (found is None or [info.st_dev,info.st_ino]!=entry['inode'] or _key(found)!=_key(info)
                    or not stat.S_ISDIR(info.st_mode) or info.st_nlink<2
                    or (info.st_uid,info.st_gid)!=(self.uid,self.gid) or stat.S_IMODE(info.st_mode)!=0o700
                    or os.listxattr(fd) or empty and os.listdir(fd)):
                raise FreshnessHold('input_census_unknown')
            return list(_key(info))
        if record is not None and record['state']=='prepared':
            if token not in self.directories:
                found=named()
                if found is None or [found.st_dev,found.st_ino]!=record.get('inode'):
                    raise FreshnessHold('input_census_unknown')
                descriptor=os.open(name,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW,dir_fd=parent)
                self.directories[token]={'fd':descriptor,'inode':record['inode']}
            entry=self.directories[token]
            if entry['inode']!=record['inode']:raise FreshnessHold('input_census_unknown')
            verify(entry);return entry['fd']
        if record is None:
            if named() is not None:raise FreshnessHold('invalidation_unsafe')
            record=self._record(path,binding,'preparing',before_parent=_namespace_directory(parent,self.check))
        if token not in self.directories:
            if named() is not None or _namespace_directory(parent,self.check)!=record['before_parent']:
                raise FreshnessHold('cleanup_uncertain')
            self.check();os.mkdir(name,mode=0o700,dir_fd=parent)
            descriptor=os.open(name,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW,dir_fd=parent)
            self.directories[token]={'fd':descriptor,'inode':list(_key(os.fstat(descriptor)))[:2]}
        entry=self.directories[token];metadata=verify(entry,empty=True)
        before=record['before_parent'];after=_namespace_directory(parent,self.check)
        rows={row[0]:row[1:] for row in before['entries']};rows[name.hex()]=[metadata,os.fstat(entry['fd']).st_nlink]
        if (dict((row[0],row[1:]) for row in after['entries'])!=rows or after['links']!=before['links']+1
                or any(a!=b for i,(a,b) in enumerate(zip(before['key'],after['key'])) if i not in (2,6,7))):
            raise FreshnessHold('input_census_unknown')
        self.check();os.fsync(entry['fd']);os.fsync(parent)
        self._record(path,binding,'prepared',inode=entry['inode'],metadata=metadata)
        return entry['fd']

    def close(self):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        if self.closed:return
        self.closed=True;failed=False
        for collection in (self.files,self.directories):
            for entry in collection.values():
                try:os.close(entry['fd'])
                except OSError:failed=True
            collection.clear()
        if failed:raise FreshnessHold('cleanup_uncertain')

    def __enter__(self):self._owned();return self
    def __exit__(self,*args):self.close()


class RetainedTransfer:
    """Apply a complete private filesystem plan in its original controller.

    The caller must hold the original mutation lock and prove W, exclusive
    writers, child cleanup and parent pause before every check. This class
    does not supply those proofs, resume a process, or authorize reuse. A lost
    controller cannot adopt these capabilities from the journal.
    """
    def __init__(self,retained,directory,plan,*,check):
        retained._owned();self.held=retained;self.directory=literal(directory)
        self.plan=copy_module.deepcopy(plan);self.check=check;self.owner=os.getpid();self.closed=False
        self.digest=hashlib.sha256(self._encode(plan)).hexdigest();self.record=None;self.committed=None
        self.dirty=False;self.objects=None;self.extra_fds=[];self.stage=None;self.steps=None
        self.nodes={};self.parents={};self.prefix_fds={};self.stage_anchor=None;self.object_snapshot=None
        self.by_key={self._node(obj['working_inode']):obj for obj in plan['objects']}
        self.by_initial={obj['initial_path']:obj for obj in plan['objects'] if obj['initial_path'] is not None}
        self.path=self.directory/'transfer-progress.json'
        self.binding={'plan_sha256':self.digest,'owner_pid':self.owner,
            'owner_birth':Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19],
            'holder_nonce':os.urandom(32).hex()}
        try:
            if (cache._read_record(self.directory/'plan.json',max_bytes=16*1024**2)!=plan
                    or plan.get('before')!=retained.capture or plan.get('namespace_edges')!=namespace_edges(plan)
                    or self.path.exists()):raise FreshnessHold('invalidation_unsafe')
            initial={entry['path']:entry for entry in retained.entries}
            for obj in plan['objects']:
                key=self._node(obj['working_inode'])
                if obj['initial_path'] is not None:
                    entry=initial.get(obj['initial_path'])
                    if (entry is None or entry['kind']!=obj['kind']
                            or entry['working_key'][:2]!=obj['working_inode']
                            or entry['original_key']!=obj['original_key']):raise FreshnessHold('input_census_unknown')
                    self.nodes[key]=entry['original_fd']
                    if obj['kind']=='directory':self.parents[key]=entry['original_fd']
            if len(self.nodes)!=len(initial):raise FreshnessHold('input_census_unknown')
            for root in retained.roots:
                prefix=root.parent
                if prefix in self.prefix_fds:continue
                fd=os.dup(retained.root_fd) if prefix==Path('.') else _open_beneath(retained.root_fd,prefix,os.O_RDONLY|os.O_DIRECTORY)
                self.extra_fds.append(fd);self.prefix_fds[prefix]=fd
                self.parents['prefix:'+os.fsencode(prefix).hex()]=fd
            # A temporary namespace lives only inside an unchanged mutable W
            # directory. No outside allocation or unregistered target is used.
            stable=[]
            by_initial={relative(bytes.fromhex(obj['initial_path'])):obj for obj in plan['objects'] if obj['initial_path'] is not None}
            for path,obj in by_initial.items():
                if (obj['kind']!='directory' or obj['initial_path']!=obj['final_path']
                        or obj['original_key'][3:5]!=[retained.parent_uid,retained.parent_uid]):continue
                if any(parent in by_initial and by_initial[parent]['initial_path']!=by_initial[parent]['final_path'] for parent in path.parents):continue
                stable.append((len(path.parts),os.fsencode(path),self._node(obj['working_inode'])))
            if stable:self.stage_anchor=sorted(stable)[0][2]
            moved={self._node(row['working_inode']) for row in plan['namespace_edges']['detach'] if row['final_path'] is not None}
            if (moved or any(obj['initial_path'] is None and obj['kind']=='directory' for obj in plan['objects'])) and self.stage_anchor is None:
                raise FreshnessHold('invalidation_unsafe')
            self.needs_stage=bool(moved or any(obj['initial_path'] is None and obj['kind']=='directory' for obj in plan['objects']))
            if len(plan['objects'])+int(self.needs_stage)>MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
            total=sum(obj['old_effect']['size'] if obj['initial_path'] is not None else obj['final_key'][2]
                for obj in plan['objects'] if obj['kind']=='file')
            if total>MAX_BYTES:raise FreshnessHold('input_census_unknown')
            # All allocation and changed edges must fit the pinned original
            # mount topology before old bytes can be changed. Cloning does not
            # turn a cross-mount rename into a legal original operation.
            self.allocations={}
            def original_parent(endpoint):
                if 'prefix' in endpoint:return self.prefix_fds[Path(os.fsdecode(bytes.fromhex(endpoint['prefix'])))]
                key=self._node(endpoint['working_inode'])
                if key in self.parents:return self.parents[key]
                return original_parent(final_edges[key]['new_parent'])
            final_edges={self._node(row['working_inode']):row for row in plan['namespace_edges']['attach']}
            for obj in plan['objects']:
                if obj['initial_path'] is None:
                    endpoint=original_parent(final_edges[self._node(obj['working_inode'])]['new_parent'])
                    mount=_descriptor_mount_id(endpoint)
                    candidates=[key for _,_,key in sorted(stable) if _descriptor_mount_id(self.parents[key])==mount]
                    if not candidates:raise FreshnessHold('invalidation_unsafe')
                    self.allocations[self._node(obj['working_inode'])]=self.parents[candidates[0]]
                    if obj['kind']=='directory' and _descriptor_mount_id(self.parents[self.stage_anchor])!=mount:
                        raise FreshnessHold('invalidation_unsafe')
            for row in plan['namespace_edges']['detach']:
                fd=self.nodes[self._node(row['working_inode'])];mount=_descriptor_mount_id(fd)
                if _descriptor_mount_id(original_parent(row['old_parent']))!=mount:raise FreshnessHold('invalidation_unsafe')
                if row['final_path'] is not None and (_descriptor_mount_id(original_parent(row['new_parent']))!=mount
                        or _descriptor_mount_id(self.parents[self.stage_anchor])!=mount):raise FreshnessHold('invalidation_unsafe')
            self.stage_name=os.fsencode('.ci-transfer-'+self.binding['holder_nonce'])
            final_paths={obj['final_path'] for obj in plan['objects'] if obj['final_path'] is not None}
            if self.needs_stage:
                anchor=next(obj for obj in plan['objects'] if self._node(obj['working_inode'])==self.stage_anchor)
                if os.fsencode(relative(bytes.fromhex(anchor['final_path']))/os.fsdecode(self.stage_name)).hex() in final_paths:
                    raise FreshnessHold('invalidation_unsafe')
        except BaseException:
            self.close();raise

    @staticmethod
    def _encode(value):return json.dumps(value,sort_keys=True,separators=(',',':')).encode()

    @staticmethod
    def _node(inode):return 'inode:'+str(inode[0])+':'+str(inode[1])

    def _owned(self):
        if self.owner!=os.getpid() or self.closed:raise FreshnessHold('invalidation_unsafe')
        self.held._owned();self.check()
        if cache._read_record(self.directory/'plan.json',max_bytes=16*1024**2)!=self.plan:
            raise FreshnessHold('input_census_unknown')

    def _images(self):
        for root,sha in ((literal(self.plan['before_root']),self.plan['before']['payload_sha256']),
                (self.directory/'after',self.plan['after']['payload_sha256'])):
            self.check();rows=inventory(root,owner=os.geteuid(),sealed=True)
            if hashlib.sha256(self._encode(rows)).hexdigest()!=sha:raise FreshnessHold('input_census_unknown')
        for entry in self.held.entries:
            obj=self.by_initial[entry['path']]
            if list(_key(os.fstat(entry['working_fd'])))!=obj['old_effect']['working_key']:
                raise FreshnessHold('input_census_unknown')
        self.held._allocation(self.check)
        after={row[0]:row for row in self.plan['after']['entries']}
        children={raw:set() for raw,row in after.items() if row[1]=='directory'}
        for raw in after:
            parent=os.fsencode(relative(bytes.fromhex(raw)).parent).hex()
            if parent in children:children[parent].add(os.fsencode(relative(bytes.fromhex(raw)).name))
        for raw,row in after.items():
            fd=_open_beneath(self.held.working_fd,relative(bytes.fromhex(raw)),os.O_RDONLY|(os.O_DIRECTORY if row[1]=='directory' else 0))
            try:
                if list(_key(os.fstat(fd)))!=row[2]:raise FreshnessHold('input_census_unknown')
                if row[1]=='directory':
                    if set(map(os.fsencode,os.listdir(fd)))!=children[raw]:raise FreshnessHold('input_census_unknown')
                elif self.held._read(fd,self.check)!=(row[2][2],row[3]):raise FreshnessHold('input_census_unknown')
            finally:os.close(fd)
        for raw in self.plan['after'].get('missing_roots',[]):
            try:fd=_open_beneath(self.held.working_fd,relative(bytes.fromhex(raw)),os.O_RDONLY)
            except FileNotFoundError:continue
            else:os.close(fd);raise FreshnessHold('input_census_unknown')

    def _snapshot(self):
        self.check();nodes={};parents={};total=0
        for key,fd in self.nodes.items():
            self.check();info=os.fstat(fd);row={'key':list(_key(info)),'links':info.st_nlink}
            if stat.S_ISREG(info.st_mode):
                row['size'],row['sha256']=self.held._read(fd,self.check);total+=row['size']
                if total>MAX_BYTES:raise FreshnessHold('input_census_unknown')
            if os.listxattr(fd) or info.st_mode&0o7000:raise FreshnessHold('invalidation_unsafe')
            nodes[key]=row
        for key,fd in self.parents.items():
            info=os.fstat(fd)
            parents[key]=({'key':list(_key(info)),'links':0,'entries':[]} if info.st_nlink==0 else _namespace_directory(fd,self.check))
        result={'nodes':nodes,'parents':parents}
        if len(self._encode(result))>16*1024**2:raise FreshnessHold('input_census_unknown')
        return result

    def _persist(self):
        self.check()
        if len(self._encode(self.record))>16*1024**2:raise FreshnessHold('input_census_unknown')
        cache._write_record(self.path,self.record)
        self.committed=copy_module.deepcopy(self.record);self.dirty=False

    def _advance(self,state,**values):
        self.record.update(state=state,snapshot=self._snapshot(),**values);self.dirty=True;self._persist()

    def _verify_snapshot(self):
        if self._snapshot()!=self.record['snapshot']:raise FreshnessHold('input_census_unknown')

    def _prepare_objects(self):
        if self.object_snapshot is not None and self._snapshot()!=self.object_snapshot:
            raise FreshnessHold('input_census_unknown')
        if self.objects is None:
            path=self.directory/'object-receipts';path.mkdir(mode=0o700)
            self.objects=PreparedTransferObjects(path,plan_sha256=self.digest,check=self.check)
            self.object_snapshot=copy_module.deepcopy(self.record['snapshot'])
        if self.needs_stage:
            self.stage=self.objects.prepare_directory('stage',self.parents[self.stage_anchor],self.stage_name)
            self.nodes['stage']=self.stage;self.parents['stage']=self.stage
            self.object_snapshot=self._snapshot()
        for obj in self.plan['objects']:
            if obj['initial_path'] is not None:continue
            key=self._node(obj['working_inode'])
            if obj['kind']=='directory':
                fd=self.objects.prepare_directory(key,self.stage,key.encode())
                self.parents[key]=fd
            else:
                metadata=obj['final_key'];expected={'size':metadata[2],'sha256':obj['sha256'],
                    'uid':self.held.parent_uid,'gid':self.held.parent_uid,'mode':stat.S_IMODE(metadata[5]),'mtime_ns':metadata[6]}
                fd=self.objects.prepare_file(key,self.allocations[key],self.directory/'after'/relative(bytes.fromhex(obj['final_path'])),expected)
            self.nodes[key]=fd;self.object_snapshot=self._snapshot()
        edges=self.plan['namespace_edges'];steps=[]
        def parent(endpoint):
            return self._node(endpoint['working_inode']) if 'working_inode' in endpoint else 'prefix:'+endpoint['prefix']
        for row in edges['detach']:
            key=self._node(row['working_inode'])
            step={'action':'remove' if row['final_path'] is None else 'move','inode':key,
                'source_parent':parent(row['old_parent']),'source_name':row['old_name']}
            if step['action']=='move':step.update(destination_parent='stage',destination_name=key.encode().hex())
            steps.append(step)
        for row in edges['attach']:
            key=self._node(row['working_inode']);obj=self.by_key[key]
            step={'action':'attach' if obj['initial_path'] is None and obj['kind']=='file' else 'move',
                'inode':key,'destination_parent':parent(row['new_parent']),'destination_name':row['new_name']}
            if step['action']=='move':step.update(source_parent='stage',source_name=key.encode().hex())
            steps.append(step)
        if self.needs_stage:
            steps.append({'action':'remove','inode':'stage','source_parent':self.stage_anchor,'source_name':self.stage_name.hex()})
        if len(steps)>2*MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
        self.steps=steps

    def _directory_metadata(self):
        directories=[obj for obj in self.plan['objects'] if obj['kind']=='directory']
        for index,obj in enumerate(directories):
            if index<self.record['next_directory']:continue
            key=self._node(obj['working_inode']);fd=self.nodes[key]
            metadata=obj['old_effect']['working_key'] if obj['initial_path'] is not None else obj['final_key']
            owner=self.held.parent_uid if metadata[3]==self.held.unit_uid else metadata[3]
            group=self.held.parent_uid if metadata[4]==self.held.unit_uid else metadata[4]
            info=os.fstat(fd);wanted=(owner,group,stat.S_IMODE(metadata[5]),metadata[6])
            actual=(info.st_uid,info.st_gid,stat.S_IMODE(info.st_mode),info.st_mtime_ns)
            current=self._snapshot();recovered=False
            if current!=self.record['snapshot']:
                if self.record.get('active_directory')!=index or actual!=wanted:
                    raise FreshnessHold('input_census_unknown')
                before=self.record['snapshot']['nodes'][key]['key'];after=list(_key(info))
                if any(a!=b for i,(a,b) in enumerate(zip(before,after)) if i not in (3,4,5,6,7)):
                    raise FreshnessHold('input_census_unknown')
                projected=copy_module.deepcopy(self.record['snapshot'])
                projected['nodes'][key]['key']=after
                for parent in projected['parents'].values():
                    if parent['key'][:2]==after[:2]:parent['key']=after
                    for row in parent['entries']:
                        if row[1][:2]==after[:2]:row[1]=after
                if projected!=current:raise FreshnessHold('input_census_unknown')
                recovered=True
            if not recovered:
                self.record['active_directory']=index;self.dirty=True;self._persist()
                # Avoid changing ctime for unchanged original directories.
                if actual!=wanted:
                    self.check()
                    if (info.st_uid,info.st_gid)!=(owner,group):os.fchown(fd,owner,group)
                    if stat.S_IMODE(os.fstat(fd).st_mode)!=wanted[2]:os.fchmod(fd,wanted[2])
                    if os.fstat(fd).st_mtime_ns!=wanted[3]:os.utime(fd,ns=(info.st_atime_ns,wanted[3]))
            self.check();os.fsync(fd)
            self._advance('directories',next_directory=index+1,active_directory=None)

    def _final(self):
        expected={obj['final_path']:obj for obj in self.plan['objects'] if obj['final_path'] is not None}
        after={row[0]:row for row in self.plan['after']['entries']}
        if set(expected)!=set(after):raise FreshnessHold('input_census_unknown')
        for obj in self.plan['objects']:
            self.check();fd=self.nodes[self._node(obj['working_inode'])];info=os.fstat(fd)
            metadata=obj['old_effect']['working_key'] if obj['initial_path'] is not None else obj['final_key']
            wanted=(self.held.parent_uid if metadata[3]==self.held.unit_uid else metadata[3],
                self.held.parent_uid if metadata[4]==self.held.unit_uid else metadata[4],stat.S_IMODE(metadata[5]),metadata[6])
            if (info.st_uid,info.st_gid,stat.S_IMODE(info.st_mode),info.st_mtime_ns)!=wanted:
                raise FreshnessHold('input_census_unknown')
            raw=obj['final_path']
            if raw is None:
                if info.st_nlink!=0:raise FreshnessHold('invalidation_unsafe')
            else:
                named=_open_beneath(self.held.root_fd,relative(bytes.fromhex(raw)),os.O_RDONLY|(os.O_DIRECTORY if obj['kind']=='directory' else 0))
                try:
                    if _key(os.fstat(named))!=_key(info):raise FreshnessHold('input_census_unknown')
                finally:os.close(named)
                if obj['kind']=='directory':
                    path=relative(bytes.fromhex(raw))
                    names={os.fsencode(relative(bytes.fromhex(name)).name).hex():self.nodes[self._node(child['working_inode'])] for name,child in expected.items() if relative(bytes.fromhex(name)).parent==path}
                    rows=_namespace_directory(fd,self.check)['entries']
                    if {row[0] for row in rows}!=set(names):raise FreshnessHold('input_census_unknown')
                    for name,key,links in rows:
                        if key!=list(_key(os.fstat(names[name]))):raise FreshnessHold('input_census_unknown')
                elif info.st_nlink!=1:raise FreshnessHold('invalidation_unsafe')
            if obj['kind']=='file':
                effect=obj.get('old_effect',{})
                if self.held._read(fd,self.check)!=(effect.get('size',metadata[2]),effect.get('sha256',obj.get('sha256'))):
                    raise FreshnessHold('input_census_unknown')
        for raw in self.plan['after'].get('missing_roots',[]):
            try:fd=_open_beneath(self.held.root_fd,relative(bytes.fromhex(raw)),os.O_RDONLY)
            except FileNotFoundError:continue
            else:os.close(fd);raise FreshnessHold('input_census_unknown')
        if self.stage is not None and (os.fstat(self.stage).st_nlink!=0 or os.listdir(self.stage)):
            raise FreshnessHold('invalidation_unsafe')

    def apply(self):
        self._owned();self._images()
        if self.path.exists():
            actual=cache._read_record(self.path,max_bytes=16*1024**2)
            if actual!=self.committed and not (self.dirty and actual==self.record):raise FreshnessHold('invalidation_unsafe')
        elif self.committed is not None:raise FreshnessHold('invalidation_unsafe')
        if self.dirty:self._verify_snapshot();self._persist()
        if self.record is None:
            self.record={'version':1,'kind':'ci-retained-transfer','binding':self.binding,'state':'inodes',
                'effects_materialized':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False,
                'snapshot':self._snapshot()}
            self.dirty=True;self._persist()
        if self.record['state']=='inodes':
            self.held.apply_inode_effects(self.directory,self.plan,check=self.check)
            self._advance('objects')
        if self.record['state']=='objects':
            # No namespace operation has started yet. Allocation capabilities
            # remain pinned in this holder; a lost holder is never reconstructed.
            if self.objects is None:self._verify_snapshot()
            self._prepare_objects();self._advance('namespace')
        if self.record['state']=='namespace':
            path=self.directory/'namespace-receipts'
            if not path.exists():self._verify_snapshot();path.mkdir(mode=0o700)
            elif not (path/'sequence.json').exists():self._verify_snapshot()
            apply_namespace_sequence(path,plan_sha256=self.digest,steps=self.steps,
                inodes=self.nodes,parents=self.parents,check=self.check)
            self._advance('directories',next_directory=0)
        if self.record['state']=='directories':
            self._directory_metadata();self._final();self._advance('transfer_applied')
        if self.record['state']!='transfer_applied':raise FreshnessHold('invalidation_unsafe')
        self._verify_snapshot();self._final();return copy_module.deepcopy(self.record)

    def close(self):
        if self.owner!=os.getpid():raise FreshnessHold('invalidation_unsafe')
        if self.closed:return
        self.closed=True
        try:
            if self.objects is not None:self.objects.close()
        finally:
            failed=False
            for fd in self.extra_fds:
                try:os.close(fd)
                except OSError:failed=True
            self.extra_fds.clear()
            if failed:raise FreshnessHold('cleanup_uncertain')

    def __enter__(self):self._owned();return self
    def __exit__(self,*args):self.close()


def namespace_edges(plan):
    """Plan changed parent/name edges, keeping moved descendants attached.

    This is a graph calculation, not permission to mutate a paused parent.
    Detach deepest old edges before changing their ancestors; attach final
    directory parents before their children. Execution/replay remains separate.
    """
    if (type(plan) is not dict or plan.get('kind')!='ci-contained-transfer-plan'
            or plan.get('effects_materialized') is not False or plan.get('shipping_authority') is not False
            or type(plan.get('objects')) is not list or len(plan['objects'])>MAX_ENTRIES):
        raise FreshnessHold('input_census_unknown')
    roots=[relative(bytes.fromhex(raw)) for raw in plan['roots']]
    prefixes={prefix for root in roots for prefix in root.parents}
    indexes={'initial_path':{},'final_path':{}};nodes=[];identities=set()
    for obj in plan['objects']:
        identity=obj['working_inode']
        if (type(identity) is not list or len(identity)!=2 or any(type(i) is not int or i<0 for i in identity)
                or tuple(identity) in identities or obj['kind'] not in ('file','directory')):
            raise FreshnessHold('invalidation_unsafe')
        identities.add(tuple(identity));paths={}
        for field,index in indexes.items():
            raw=obj[field]
            if raw is None:paths[field]=None;continue
            path=relative(bytes.fromhex(raw))
            if path in index or not any(path==root or root in path.parents for root in roots):
                raise FreshnessHold('invalidation_unsafe')
            index[path]=obj;paths[field]=path
        if all(path is None for path in paths.values()):raise FreshnessHold('invalidation_unsafe')
        nodes.append((obj,paths))
    def edge(path,index):
        if path is None:return None,None
        parent=index.get(path.parent)
        if parent is not None:
            if parent['kind']!='directory':raise FreshnessHold('invalidation_unsafe')
            endpoint={'working_inode':parent['working_inode']}
        elif path.parent in prefixes:endpoint={'prefix':os.fsencode(path.parent).hex()}
        else:raise FreshnessHold('input_census_unknown')
        return endpoint,os.fsencode(path.name).hex()
    detach=[];attach=[]
    for obj,paths in nodes:
        old_parent,old_name=edge(paths['initial_path'],indexes['initial_path'])
        new_parent,new_name=edge(paths['final_path'],indexes['final_path'])
        if (old_parent,old_name)==(new_parent,new_name):continue
        row={'working_inode':obj['working_inode'],'kind':obj['kind'],
            'initial_path':obj['initial_path'],'final_path':obj['final_path'],
            'old_parent':old_parent,'old_name':old_name,'new_parent':new_parent,'new_name':new_name}
        if old_parent is not None:detach.append(row)
        if new_parent is not None:attach.append(row)
    detach.sort(key=lambda row:(-len(relative(bytes.fromhex(row['initial_path'])).parts),bytes.fromhex(row['initial_path'])))
    attach.sort(key=lambda row:(len(relative(bytes.fromhex(row['final_path'])).parts),bytes.fromhex(row['final_path'])))
    return {'version':1,'kind':'ci-namespace-edges','detach':detach,'attach':attach,
        'effects_materialized':False,'input_closure_complete':False,'full_ci':False,'shipping_authority':False}


def validate_capture_stdio(capture,descriptors):
    """Refuse direct writable stdio aliases to captured original inodes."""
    if type(descriptors) is not list or len(descriptors)!=2:raise FreshnessHold('input_census_unknown')
    inodes={(row[2][0],row[2][1]) for row in capture['entries'] if row[1]=='file'}
    for descriptor in descriptors:
        info=os.fstat(descriptor)
        if (stat.S_ISREG(info.st_mode) and fcntl.fcntl(descriptor,fcntl.F_GETFL)&os.O_ACCMODE!=os.O_RDONLY
                and (info.st_dev,info.st_ino) in inodes):raise FreshnessHold('invalidation_unsafe')


def validate_stdio(descriptors):
    if len(descriptors)!=3:raise FreshnessHold('input_census_unknown')
    for descriptor in descriptors:
        info=os.fstat(descriptor)
        if not (stat.S_ISREG(info.st_mode) or stat.S_ISFIFO(info.st_mode)
                or stat.S_ISCHR(info.st_mode) and info.st_rdev==os.makedev(1,3)):
            raise FreshnessHold('input_census_unknown')


def _mount(*arguments):
    subprocess.run(['/usr/bin/mount',*map(str,arguments)],check=True,env={'PATH':os.defpath},
        stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=10)


def _readonly(source,destination):
    _mount('--bind',source,destination)
    _mount('-o','remount,bind,ro,nosuid,nodev,noatime',destination)


def input_mount(source,destination,raw,expected,*,owner=0):
    source,destination=map(literal,(source,destination))
    if type(expected) is not list or len(expected)<2 or expected[0]!=raw:
        raise FreshnessHold('input_census_unknown')
    if expected[1]=='tree':
        if (len(expected)!=3 or not destination.is_dir()
                or inventory(source,owner=owner,sealed=True)!=expected[2]):
            raise FreshnessHold('input_census_unknown')
    elif expected[1]=='file':
        if not destination.is_file() or expected!=[raw,'file',*_read(source,owner,True)]:
            raise FreshnessHold('input_census_unknown')
    else:raise FreshnessHold('input_census_unknown')
    _readonly(source,destination)


def source_mounts(workspace,bindings,declared,*,owner=0):
    """Readonly generated inputs, confined to controller-declared placeholders."""
    workspace=literal(workspace);cache.validate_outputs(declared)
    if type(bindings) is not list or len(bindings)>64:raise FreshnessHold('input_census_unknown')
    seen=set()
    for binding in bindings:
        if type(binding) is not list or len(binding)!=3:raise FreshnessHold('input_census_unknown')
        raw,original,expected=binding
        try:name=relative(bytes.fromhex(raw));key=os.fsdecode(name)
        except (ValueError,TypeError):raise FreshnessHold('input_census_unknown') from None
        if (key not in declared or key in seen or name.parts[0] in ('target','.git')
                or type(expected) is not list or len(expected)<2
                or (expected[1]=='tree')!=(declared[key]=='directory')):raise FreshnessHold('input_census_unknown')
        seen.add(key);destination=literal(workspace/name)
        input_mount(original,destination,raw,expected,owner=owner)


def toolchain_view(source,destination):
    """Expose pinned runtime children without chmod on the private source root.

    The installed host toolchain root is commonly 0700/UID1000. Its existing
    public children retain their original inode metadata in read-only mounts;
    the controller creates a traversable virtual root for distinct unit UIDs.
    This prototype does not establish a general runtime-equivalence policy.
    """
    source,destination=map(literal,(source,destination))
    names=('bin','etc','lib','libexec','share')
    if (set(path.name for path in source.iterdir())!=set(names) or any(destination.iterdir())
            or any(not (source/name).is_dir() or (source/name).is_symlink() for name in names)):
        raise FreshnessHold('input_census_unknown')
    for name in names:
        (destination/name).mkdir(mode=0o755);_readonly(source/name,destination/name)
    destination.chmod(0o555)


def enter(root,unit,source,artifacts,bindings=(),source_bindings=None,declared_outputs=None,*,transport=None,working=None):
    phase=['planner_view_enter']
    try:return _enter(root,unit,source,artifacts,bindings,phase,source_bindings,declared_outputs,transport,working)
    except BaseException as error:
        error.unit_view_stage=phase[0];raise


def _enter(root,unit,source,artifacts,bindings,phase,source_bindings=None,declared_outputs=None,transport=None,working=None):
    """Root-only entry from a sealed admitted unit bootstrap, before UID drop.

    The caller supplies controller-owned paths, not candidate mount arguments.
    This mechanics primitive does not itself authenticate an admission token.
    The private planner entry must do that before reaching this function.
    """
    if os.getresuid()!=(0,0,0) or os.getresgid()!=(0,0,0):raise FreshnessHold('invalidation_unsafe')
    if type(unit) is not int or not 2000<=unit<6096:raise FreshnessHold('invalidation_unsafe')
    root,source,artifacts=map(literal,(root,source,artifacts))
    for path,owner in ((root,0),(source,0),(artifacts,unit)):
        info=path.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid!=owner:raise FreshnessHold('invalidation_unsafe')
    validate_stdio([0,1,2]);libc=ctypes.CDLL(None,use_errno=True)
    phase[0]='planner_view_namespace'
    if libc.unshare(0x00020000|0x40000000|0x08000000)!=0:
        raise OSError(ctypes.get_errno(),'unit namespaces unavailable')
    _mount('--make-rprivate','/');os.chdir('/')
    for name in ('usr','toolchain','workspace','proc','dev','dev/shm','etc','etc/alternatives','tmp',
            'home','home/worker','cargo','run','var','var/tmp'):
        (root/name).mkdir(mode=0o755,parents=True,exist_ok=True)
    for name in ('bin','sbin','lib','lib64'):(root/name).symlink_to('usr/'+name)
    for name,target in (('cc','/usr/bin/gcc'),('c++','/usr/bin/g++'),('cpp','/usr/bin/cpp')):
        (root/'etc/alternatives'/name).symlink_to(target)
    for name in ('hosts','jarvis-registry-ca.pem'):
        original=Path('/etc')/name
        if original.exists():
            data=original.read_bytes();(root/'etc'/name).write_bytes(data);(root/'etc'/name).chmod(0o444)
    (root/'etc/passwd').write_text(f'root:x:0:0:root:/root:/bin/sh\nworker:x:{unit}:{unit}:worker:/home/worker:/bin/sh\n')
    (root/'etc/group').write_text(f'root:x:0:\nworker:x:{unit}:\n')
    for name in ('passwd','group'):(root/'etc'/name).chmod(0o444)
    for name in ('tmp','home/worker','cargo','var/tmp','dev/shm'):
        os.chown(root/name,unit,unit);(root/name).chmod(0o700)
    phase[0]='planner_view_runtime'
    _readonly('/usr',root/'usr')
    if Path('/toolchain').is_dir():toolchain_view('/toolchain',root/'toolchain')
    phase[0]='planner_view_source'
    _readonly(source,root/'workspace')
    _mount('--bind',artifacts,root/'workspace/target')
    _mount('-o','remount,bind,rw,nosuid,nodev,noatime',root/'workspace/target')
    phase[0]='planner_view_inputs'
    if not isinstance(bindings,(list,tuple)) or len(bindings)>MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
    for binding in bindings:
        if not isinstance(binding,list) or len(binding)!=3:raise FreshnessHold('input_census_unknown')
        raw,original,expected=binding
        try:name=relative(bytes.fromhex(raw))
        except (TypeError,ValueError):raise FreshnessHold('input_census_unknown') from None
        original=literal(original)
        destination=root/'workspace/target'/name
        input_mount(original,destination,raw,expected)
    source_mounts(root/'workspace',[] if source_bindings is None else source_bindings,{} if declared_outputs is None else declared_outputs)
    if working is not None:
        working=literal(working);info=working.stat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid!=0 or stat.S_IMODE(info.st_mode)!=0o700:
            raise FreshnessHold('invalidation_unsafe')
        for name in ('tmp','home/worker','cargo','var/tmp','dev/shm'):
            original=literal(working/name);info=original.stat()
            if not stat.S_ISDIR(info.st_mode) or info.st_uid!=unit or info.st_gid!=unit:
                raise FreshnessHold('invalidation_unsafe')
            _mount('--bind',original,root/name);_mount('-o','remount,bind,rw,nosuid,nodev,noatime',root/name)
    phase[0]='planner_view_proc'
    _mount('-t','proc','-o','ro,nosuid,nodev,noexec,hidepid=2','proc',root/'proc')
    phase[0]='planner_view_devices'
    for name in ('null','zero','random','urandom'):
        (root/'dev'/name).touch(mode=0o600);_mount('--bind','/dev/'+name,root/'dev'/name)
    (root/'dev/fd').symlink_to('/proc/self/fd')
    for name,descriptor in (('stdin',0),('stdout',1),('stderr',2)):(root/'dev'/name).symlink_to('/proc/self/fd/'+str(descriptor))
    if transport is not None:
        if type(transport) is not dict or set(transport)!={'socket','library'}:raise FreshnessHold('invalidation_unsafe')
        for key,name in (('socket','ci-unit.sock'),('library','ci-cargo-relay.so')):
            original=literal(transport[key]);before=original.lstat()
            if before.st_uid!=0 or before.st_nlink!=1:raise FreshnessHold('authority_changed')
            if key=='socket':
                if not stat.S_ISSOCK(before.st_mode) or before.st_gid!=unit or stat.S_IMODE(before.st_mode)!=0o660:
                    raise FreshnessHold('invalidation_unsafe')
            elif not stat.S_ISREG(before.st_mode) or before.st_mode&0o022 or not 1<=before.st_size<=2*1024**2:
                raise FreshnessHold('authority_changed')
            destination=root/'run'/name;destination.touch(mode=0o600)
            _mount('--bind',original,destination)
            _mount('-o','remount,bind,ro,nosuid,nodev,noatime',destination)
            if _key(original.lstat())!=_key(before) or _key(destination.stat())!=_key(before):
                raise FreshnessHold('authority_changed')
    phase[0]='planner_view_root'
    root.chmod(0o755);os.chroot(root);os.chdir('/')
