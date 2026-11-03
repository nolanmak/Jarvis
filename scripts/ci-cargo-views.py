#!/usr/bin/env python3
"""Private unit filesystem mechanics; not a provenance or passing-CI authority.

The admitted root planner owns namespace entry and original-unit cleanup. Pure
filesystem helpers are also exercised without privilege; their results alone
never establish trusted producer identity, input closure, or reuse authority.
"""
import ctypes
import errno
import hashlib
import importlib.util
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
        for entry in sorted(directory.iterdir(),key=lambda p:os.fsencode(p.name)):
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


def copy_outputs(source,destination,names,*,owner,controller):
    source=literal(source);destination=literal(destination)
    if not isinstance(names,list) or not 1<=len(names)<=MAX_ENTRIES:raise FreshnessHold('input_census_unknown')
    names=[relative(name) for name in names]
    if len(set(names))!=len(names):raise FreshnessHold('input_census_unknown')
    destination.mkdir(mode=0o700);total=0
    for name in names:
        original=source/name
        if original.resolve()!=original:raise FreshnessHold('artifact_provenance_unknown')
        for parent in [source,*[source/Path(*name.parts[:i]) for i in range(1,len(name.parts))]]:
            info=parent.lstat()
            if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=owner
                    or (hasattr(os,'listxattr') and os.listxattr(parent,follow_symlinks=False))):
                raise FreshnessHold('artifact_provenance_unknown')
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


def enter(root,unit,source,artifacts,bindings=()):
    phase=['planner_view_enter']
    try:return _enter(root,unit,source,artifacts,bindings,phase)
    except BaseException as error:
        error.unit_view_stage=phase[0];raise


def _enter(root,unit,source,artifacts,bindings,phase):
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
        if expected!=[raw,'file',*_read(original,0,True)]:raise FreshnessHold('input_census_unknown')
        destination=root/'workspace/target'/name
        if destination.resolve()!=destination or not destination.is_file():raise FreshnessHold('invalidation_unsafe')
        _readonly(original,destination)
    phase[0]='planner_view_proc'
    _mount('-t','proc','-o','ro,nosuid,nodev,noexec,hidepid=2','proc',root/'proc')
    phase[0]='planner_view_devices'
    for name in ('null','zero','random','urandom'):
        (root/'dev'/name).touch(mode=0o600);_mount('--bind','/dev/'+name,root/'dev'/name)
    (root/'dev/fd').symlink_to('/proc/self/fd')
    for name,descriptor in (('stdin',0),('stdout',1),('stderr',2)):(root/'dev'/name).symlink_to('/proc/self/fd/'+str(descriptor))
    phase[0]='planner_view_root'
    root.chmod(0o755);os.chroot(root);os.chdir('/')
