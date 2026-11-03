#!/usr/bin/env python3
"""Admitted, temporary-write legacy recovery rehearsal; never CI authority."""
import hashlib
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import socket
import shutil
import stat
import subprocess
import sys
import tempfile
import time

BASE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('preview_ci_adapter',BASE/'ci-build-vm.py')
adapter=importlib.util.module_from_spec(spec);spec.loader.exec_module(adapter)
vm=adapter.vm


class PreviewUnavailable(ValueError):pass


INVENTORY_SOURCE=r'''
def protected_inventory(root):
    root=Path(root);device=root.stat().st_dev;digest=hashlib.sha256();memo={};files=0;total=0;entries=0
    capacity=os.statvfs(root).f_blocks*os.statvfs(root).f_frsize
    excluded=('target/debug/incremental','target/release/incremental','ci-legacy-incremental-retirements')
    def walk(path,relative):
        nonlocal files,total,entries
        name=os.fsencode(str(relative)).hex();text=str(relative)
        if any(text==prefix or text.startswith(prefix+'/') for prefix in excluded):return
        info=path.lstat();entries+=1
        if entries>200000 or info.st_dev!=device:raise PreviewUnavailable('protected inventory outside bounds')
        row=[name,info.st_ino,info.st_mode,info.st_uid,info.st_gid]
        if stat.S_ISDIR(info.st_mode):
            descriptor=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NOATIME)
            try:names=sorted(os.listdir(descriptor),key=os.fsencode)
            finally:os.close(descriptor)
            row.append('directory')
        elif stat.S_ISREG(info.st_mode):
            files+=1;key=(info.st_dev,info.st_ino,info.st_size,info.st_mode,info.st_mtime_ns,info.st_ctime_ns)
            if key not in memo:
                total+=info.st_size
                if total>capacity or info.st_size>capacity:raise PreviewUnavailable('protected bytes outside bounds')
                descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC|os.O_NOATIME)
                try:
                    before=os.fstat(descriptor);content=hashlib.sha256();count=0
                    while block:=os.read(descriptor,1024*1024):
                        count+=len(block)
                        if count>info.st_size:raise PreviewUnavailable('protected file changed')
                        content.update(block)
                    after=os.fstat(descriptor)
                    if (count!=info.st_size or (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)!=key[:3]+key[4:]
                            or (after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns)!=key[:3]+key[4:]):
                        raise PreviewUnavailable('protected file changed')
                    memo[key]=content.hexdigest()
                finally:os.close(descriptor)
            row+=['file',info.st_nlink,info.st_size,info.st_mtime_ns,info.st_ctime_ns,memo[key]]
        elif stat.S_ISLNK(info.st_mode):row+=['symlink',os.fsencode(os.readlink(path)).hex()]
        else:row+=['special',info.st_rdev]
        row.append([(os.fsencode(key).hex(),os.getxattr(path,key,follow_symlinks=False).hex())
            for key in sorted(os.listxattr(path,follow_symlinks=False),key=os.fsencode)])
        digest.update(json.dumps(row,separators=(',',':')).encode()+b'\n')
        if stat.S_ISDIR(info.st_mode):
            for child in names:walk(path/child,relative/child)
    walk(root,Path('.'))
    return {'sha256':digest.hexdigest(),'files':files,'bytes':total}
'''
exec(INVENTORY_SOURCE)


def hash_image(path):
    path=Path(path)
    descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC|os.O_NOATIME)
    try:
        before=os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_uid!=os.getuid() or before.st_nlink!=1:
            raise PreviewUnavailable('base image ownership unavailable')
        content=hashlib.sha256();count=0
        while block:=os.read(descriptor,1024*1024):content.update(block);count+=len(block)
        after=os.fstat(descriptor)
        identity=lambda value:{'device':value.st_dev,'inode':value.st_ino,'size':value.st_size,
            'mode':value.st_mode,'uid':value.st_uid,'gid':value.st_gid,'mtime_ns':value.st_mtime_ns,'ctime_ns':value.st_ctime_ns}
        if count!=before.st_size or identity(before)!=identity(after) or identity(path.lstat())!=identity(before):
            raise PreviewUnavailable('base image changed')
        return {'identity':identity(before),'sha256':content.hexdigest()}
    finally:os.close(descriptor)


def preview_command(config,initrd,shares,image,qmp):
    command=vm.qemu_command(config,initrd,shares,image)
    for index,arg in enumerate(command[:-1]):
        if arg=='-drive' and 'id=buildcache,' in command[index+1]:
            command[index+1]+=',snapshot=on'
    command+=['-S','-qmp','unix:'+str(qmp)+',server=on,wait=off']
    return command


def validate_guest_result(value):
    keys={'version','kind','view','protected_before','protected_after','available_before','available_after','retired',
        'input_closure_complete','full_ci','shipping_authority'}
    if (type(value) is not dict or set(value)!=keys or type(value['version']) is not int or value['version']!=1
            or value['kind']!='ci-legacy-recovery-preview' or value['view']!='temporary-writes'
            or any(value[k] is not False for k in ('input_closure_complete','full_ci','shipping_authority'))
            or value['protected_before']!=value['protected_after']
            or any(type(value[k]) is not int or value[k]<0 for k in ('available_before','available_after'))
            or type(value['retired']) is not list or len(value['retired'])>32
            or any(not adapter.cache_generation._uuid(v) for v in value['retired'])
            or len(set(value['retired']))!=len(value['retired'])):
        raise PreviewUnavailable('invalid recovery preview outcome')
    for key in ('protected_before','protected_after'):
        row=value[key]
        if (type(row) is not dict or set(row)!={'sha256','files','bytes'}
                or not adapter.cache_generation._digest(row['sha256'])
                or any(type(row[k]) is not int or row[k]<0 for k in ('files','bytes'))):
            raise PreviewUnavailable('invalid protected inventory')
    return value


GUEST='''import hashlib,json,os,stat,sys\nfrom pathlib import Path\nclass PreviewUnavailable(ValueError):pass\n'''+INVENTORY_SOURCE+r'''
try:
    cache={};exec(compile(Path('/ci-tools/cache-generation.py').read_bytes(),'/ci-tools/cache-generation.py','exec'),cache)
    job=json.loads(Path('/job.json').read_text());root=Path('/build-cache')
    before=protected_inventory(root);space=os.statvfs(root);available=space.f_bavail*space.f_frsize
    retired=cache['reclaim_legacy_incremental'](root,job['binding'],job['command'],job['uid'],job['gid'],
        allow_closed_links=job['allow_closed_links'])
    after=protected_inventory(root);space=os.statvfs(root)
    result={'version':1,'kind':'ci-legacy-recovery-preview','view':'temporary-writes',
        'protected_before':before,'protected_after':after,'available_before':available,
        'available_after':space.f_bavail*space.f_frsize,'retired':retired,
        'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
except Exception as error:
    result={'error':'recovery-preview-unavailable','category':type(error).__name__}
with Path('/root/control/outcome.json').open('w') as stream:
    json.dump(result,stream);stream.flush();os.fsync(stream.fileno())
'''


def qmp_connect(path,process,deadline):
    while time.monotonic()<deadline:
        if process.poll() is not None:raise PreviewUnavailable('paused VM exited')
        channel=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);channel.settimeout(5)
        try:channel.connect(str(path));return channel
        except (FileNotFoundError,ConnectionRefusedError):channel.close();time.sleep(.05)
    raise PreviewUnavailable('paused VM monitor unavailable')


def qmp_command(stream,name):
    stream.write(json.dumps({'execute':name}).encode()+b'\n');stream.flush()
    for _ in range(32):
        line=stream.readline(65537)
        if not line or len(line)>65536:break
        value=json.loads(line)
        if 'return' in value:return value['return']
        if 'error' in value:break
    raise PreviewUnavailable('paused VM monitor failed')


def verify_paused_image(process,config,image,scratch):
    expected=image.stat();todo=[process.pid];seen=set();backing=[];overlays=[]
    while todo and len(seen)<32:
        pid=todo.pop();seen.add(pid);root=Path('/proc')/str(pid)
        try:
            todo.extend(int(v) for v in (root/'task'/str(pid)/'children').read_text().split())
            if (root/'exe').resolve()!=Path(config['qemu']).resolve():continue
            for descriptor in (root/'fd').iterdir():
                info=descriptor.stat();target=os.readlink(descriptor)
                flags=next(int(line.split()[1],8) for line in (root/'fdinfo'/descriptor.name).read_text().splitlines() if line.startswith('flags:'))
                if (info.st_dev,info.st_ino)==(expected.st_dev,expected.st_ino):
                    if flags&os.O_ACCMODE!=os.O_RDONLY:raise PreviewUnavailable('base image is not read-only')
                    backing.append({'device':info.st_dev,'inode':info.st_ino,'mode':'read-only'})
                elif (stat.S_ISREG(info.st_mode) and target.startswith(str(scratch)+'/')
                        and flags&os.O_ACCMODE==os.O_RDWR and info.st_size>=65536):
                    overlays.append({'device':info.st_dev,'inode':info.st_ino,'size':info.st_size})
        except FileNotFoundError:continue
    if not backing or not overlays:raise PreviewUnavailable('temporary image lineage unavailable')
    return {'kind':'temporary-writes','base_descriptors':backing,'overlay_descriptors':overlays,'base_read_only_verified_before_guest':True}


@contextlib.contextmanager
def preview_directories(scratch,runtime):
    control=Path(tempfile.mkdtemp(prefix='legacy-preview-',dir=scratch))
    ipc=Path(tempfile.mkdtemp(prefix='aa-preview-',dir=runtime));state={'safe_to_remove':True}
    try:yield control,ipc,state
    finally:
        # Unknown descendants keep their control state, just as ordinary CI.
        if state['safe_to_remove']:
            shutil.rmtree(control);shutil.rmtree(ipc)


def execute(specification):
    if specification.get('version')!=1:raise PreviewUnavailable('unsupported preview request')
    allow_closed_links=specification.get('allow_closed_links',False)
    if type(allow_closed_links) is not bool:raise PreviewUnavailable('invalid recovery policy')
    scratch=Path(specification['scratch']);image=Path(specification['build_cache'])
    if not image.is_file() or image.resolve()!=image:raise PreviewUnavailable('existing literal base required')
    lease=adapter.InheritedStorage(specification['storage'],scratch)
    adapter.ensure_cache(image,specification['cache_bytes'],lease,allow_create=False)
    runtime=vm.Runtime.load(Path(specification['runtime_config']));config=runtime.config
    info=image.stat();identity=hashlib.sha256(json.dumps([info.st_dev,info.st_ino,info.st_size],separators=(',',':')).encode()).hexdigest()
    workspace=Path(specification['workspace']);outputs=adapter.cache_generation.declared_outputs(workspace)
    source=adapter.cache_generation.source_census(workspace,Path(specification['git_objects']),specification['head'],
        generated_directories=['target',*[name for name,kind in outputs.items() if kind=='directory']],
        generated_files=[name for name,kind in outputs.items() if kind=='file'])
    binding={'generation':specification['generation'],'head':specification['head'],'tree':specification['tree'],
        'authority_sha256':specification['authority_sha256'],'contract_sha256':specification['contract_sha256'],
        'cache_identity_sha256':identity,'source_inputs_sha256':source}
    adapter.cache_generation.validate_binding(binding)
    command_id=specification['command']
    if not adapter.cache_generation._uuid(command_id):raise PreviewUnavailable('invalid preview command')
    original=hash_image(image);lease.check()
    runtime_root=Path('/run/user')/str(os.getuid());runtime_info=runtime_root.stat()
    if runtime_info.st_uid!=os.getuid() or runtime_info.st_mode&0o077:raise PreviewUnavailable('private IPC runtime unavailable')
    with preview_directories(scratch,runtime_root) as (control,ipc,cleanup_state):
        qmp=ipc/'q.sock'
        entries=[(name,stat.S_IFDIR|0o755,b'',0,0) for name in ('bootstrap','dev','proc','sys','tmp','modules','usr','ci-tools','build-cache')]
        entries += [('root',stat.S_IFDIR|0o700,b'',0,0),('root/control',stat.S_IFDIR|0o700,b'',0,0),
            ('dev/console',stat.S_IFCHR|0o600,b'',5,1),('bootstrap/busybox',stat.S_IFREG|0o755,Path(config['busybox']).read_bytes(),0,0)]
        for name in ('sh','mount','insmod','poweroff'):entries.append(('bootstrap/'+name,stat.S_IFLNK|0o777,b'busybox',0,0))
        for name in ('bin','sbin','lib','lib64'):entries.append((name,stat.S_IFLNK|0o777,('usr/'+name).encode(),0,0))
        modules=[]
        for index,path in enumerate(config['modules']):
            entries.append(('modules/'+str(index)+'.ko',stat.S_IFREG|0o600,Path(path).read_bytes(),0,0));modules.append('insmod /modules/'+str(index)+'.ko || poweroff -f')
        job={'binding':binding,'command':command_id,'uid':os.getuid(),'gid':os.getgid(),'allow_closed_links':allow_closed_links}
        entries += [('ci-tools/cache-generation.py',stat.S_IFREG|0o444,adapter.CACHE_PROGRAM,0,0),
            ('guest.py',stat.S_IFREG|0o400,GUEST.encode(),0,0),('job.json',stat.S_IFREG|0o400,json.dumps(job).encode(),0,0)]
        boot='''#!/bootstrap/sh
export PATH=/bootstrap
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
'''+ '\n'.join(modules)+'''
mount -t 9p -o trans=virtio,version=9p2000.L,ro,nosuid,nodev runtime /usr || poweroff -f
mount -t 9p -o trans=virtio,version=9p2000.L,nosuid,nodev,noexec control /root/control || poweroff -f
mount -t ext4 -o nosuid,nodev,discard /dev/vda /build-cache || poweroff -f
/usr/bin/python3 -I /guest.py
/bootstrap/busybox sync
/bootstrap/busybox umount /build-cache
/bootstrap/busybox umount /root/control
poweroff -f
'''
        entries.append(('init',stat.S_IFREG|0o755,boot.encode(),0,0));initrd=control/'initrd.gz';initrd.write_bytes(vm.initrd(entries))
        argv=preview_command(config,initrd,[('runtime',Path('/usr'),True),('control',control,False)],image,qmp)
        environment={'PATH':'/usr/bin:/bin','LD_LIBRARY_PATH':config['library_dir'],'TMPDIR':str(control),
            'JARVIS_STORAGE_LEASE_FD':str(lease.fd),'JARVIS_STORAGE_LEASE_FDS':','.join(map(str,lease.fds))}
        cleanup=control/'cleanup';log=control/'qemu.log'
        with log.open('wb') as output:
            process=subprocess.Popen([sys.executable,'-I',str(BASE/'provider-supervisor.py'),str(cleanup),*argv],
                env=environment,stdin=subprocess.DEVNULL,stdout=output,stderr=output,pass_fds=tuple(lease.fds))
            cleanup_state['safe_to_remove']=False
            try:
                deadline=time.monotonic()+180
                with qmp_connect(qmp,process,deadline) as channel,channel.makefile('rwb',buffering=0) as stream:
                    greeting=stream.readline(65537)
                    if not greeting or len(greeting)>65536 or 'QMP' not in json.loads(greeting):raise PreviewUnavailable('paused VM handshake invalid')
                    qmp_command(stream,'qmp_capabilities')
                    lineage=verify_paused_image(process,config,image,control)
                    qmp_command(stream,'cont')
                while process.poll() is None:
                    lease.check()
                    if time.monotonic()>=deadline:raise PreviewUnavailable('preview deadline exceeded')
                    time.sleep(.1)
                if process.returncode!=0 or cleanup.read_bytes()!=b'all-descendants-reaped\n':raise PreviewUnavailable('preview cleanup not verified')
                result=validate_guest_result(json.loads((control/'outcome.json').read_bytes()))
            finally:
                if process.poll() is None:process.terminate();process.wait()
                cleanup_state['safe_to_remove']=cleanup.is_file() and cleanup.read_bytes()==b'all-descendants-reaped\n'
        observed=hash_image(image)
        if observed!=original:raise PreviewUnavailable('base image integrity changed')
        lease.check()
        return {'version':1,'kind':'ci-legacy-recovery-preview-receipt','binding':binding,'command':command_id,
            'policy':'closed-links' if allow_closed_links else 'strict-single-link',
            'view':lineage,'base_before':original,'base_after':observed,'outcome':result,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}


if __name__=='__main__':
    os.umask(0o077)
    request=adapter.private_bytes(Path(sys.argv[1]),limit=65536)
    try:
        result=execute(json.loads(request));Path(sys.argv[2]).write_text(json.dumps(result,sort_keys=True)+'\n')
    except Exception as error:
        Path(sys.argv[2]).write_text(json.dumps({'version':1,'kind':'ci-legacy-recovery-preview-unavailable','category':type(error).__name__,
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False})+'\n')
        raise
