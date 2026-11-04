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
                    flags=os.O_RDONLY|os.O_DIRECTORY if kind=='directory' else os.O_RDONLY
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
        encoded=json.dumps(plan,sort_keys=True,separators=(',',':')).encode()
        if len(encoded)>16*1024**2:raise FreshnessHold('input_census_unknown')
        check();cache._write_record(destination/'plan.json',plan);check()
        return plan

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
