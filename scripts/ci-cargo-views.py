#!/usr/bin/env python3
"""Private unit filesystem mechanics; not a provenance or passing-CI authority.

The admitted root planner owns namespace entry and original-unit cleanup. Pure
filesystem helpers are also exercised without privilege; their results alone
never establish trusted producer identity, input closure, or reuse authority.
"""
import ctypes
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


def capture_roots(root_fd,destination,roots,*,owners,check):
    """Copy selected visible roots through held descriptors, without authority.

    The controller must prove pause, scope and alias closure separately. Original
    metadata is recorded, not emulated by these detached root-owned byte copies.
    Partial destinations remain uncommitted if any validation or copy fails.
    """
    if (type(root_fd) is not int or root_fd<0 or not stat.S_ISDIR(os.fstat(root_fd).st_mode)
            or type(roots) is not list or not 1<=len(roots)<=64
            or type(owners) not in (tuple,list) or not owners
            or any(type(owner) is not int or owner<0 for owner in owners) or not callable(check)):
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
        rows=[];total=0;record_bytes=0
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
                visit(descriptor,root.name,root)
            finally:os.close(descriptor)
        return rows,total
    try:
        entries,total=scan(True)
        if scan(False)!=(entries,total):raise FreshnessHold('input_census_unknown')
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
        'original_metadata_emulated':False,'input_closure_complete':False,'shipping_authority':False}


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


def enter(root,unit,source,artifacts,bindings=(),source_bindings=None,declared_outputs=None,*,transport=None):
    phase=['planner_view_enter']
    try:return _enter(root,unit,source,artifacts,bindings,phase,source_bindings,declared_outputs,transport)
    except BaseException as error:
        error.unit_view_stage=phase[0];raise


def _enter(root,unit,source,artifacts,bindings,phase,source_bindings=None,declared_outputs=None,transport=None):
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
