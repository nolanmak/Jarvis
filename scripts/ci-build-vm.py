#!/usr/bin/env python3
"""Controller CI adapter for the existing no-NIC build VM. Never executes tests on host."""
import argparse,fcntl,hashlib,importlib.util,json,os,re,stat,sys,struct,subprocess,tempfile
from pathlib import Path

BASE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('ci_build_runtime',BASE/'codex-build-vm.py')
vm=importlib.util.module_from_spec(spec);spec.loader.exec_module(vm)
cache_spec=importlib.util.spec_from_file_location('ci_cache_generation',BASE/'ci-cache-generation.py')
cache_generation=importlib.util.module_from_spec(cache_spec);cache_spec.loader.exec_module(cache_generation)
CACHE_PROGRAM=(BASE/'ci-cache-generation.py').read_bytes()


def private_bytes(path,limit=2*1024*1024):
    path=Path(path)
    if not path.is_absolute() or path.resolve()!=path:raise vm.Unavailable('controller CI input path is not literal')
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
    with os.fdopen(fd,'rb') as stream:
        info=os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1 or info.st_mode&0o077 or info.st_size>limit:raise vm.Unavailable('controller CI input is not private and bounded')
        return stream.read(limit+1)


class InheritedStorage:
    """Keep the actual inherited open-file description; opening /proc/.../fd is NOT a dup."""
    def __init__(self,policy,scratch):
        if set(policy)!={'fd','fds','path','device','inode','floor_bytes'}:raise vm.Unavailable('invalid inherited storage policy')
        if any(type(policy[key]) is not int or policy[key]<0 for key in ('fd','device','inode','floor_bytes')):raise vm.Unavailable('invalid inherited storage values')
        self.fd=policy['fd'];self.fds=policy['fds']
        if not isinstance(self.fds,list) or not 1<=len(self.fds)<=64 or self.fds[0]!=self.fd or len(set(self.fds))!=len(self.fds) or any(type(fd) is not int or fd<=2 for fd in self.fds):raise vm.Unavailable('invalid inherited storage descriptors')
        self.identities={}
        for fd in self.fds:
            info=os.fstat(fd)
            if info.st_uid!=os.getuid() or not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)) or info.st_mode&(0o002 if stat.S_ISDIR(info.st_mode) else 0o022):raise vm.Unavailable('invalid inherited target lease')
            self.identities[fd]=(info.st_dev,info.st_ino)
        self.path=Path(policy['path']);self.identity=(policy['device'],policy['inode']);self.floor=policy['floor_bytes'];self.scratch=Path(scratch)
        if self.fd<=2 or not self.path.is_absolute() or self.path.name!='disk-build.lock' or self.path.resolve()!=self.path:raise vm.Unavailable('invalid inherited storage lease')
        self.check()
        # A separate file description must be unable to take this same lock.
        probe=os.open(self.path,os.O_RDWR|os.O_NOFOLLOW|os.O_CLOEXEC)
        try:
            try:fcntl.flock(probe,fcntl.LOCK_EX|fcntl.LOCK_NB)
            except BlockingIOError:pass
            else:raise vm.Unavailable('storage admission lock is not held')
        finally:os.close(probe)
    def check(self):
        for fd,expected in self.identities.items():
            current=os.fstat(fd)
            if (current.st_dev,current.st_ino)!=expected:raise vm.Unavailable('target storage identity changed')
        info=os.fstat(self.fd);current=self.path.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_mode&0o022 or info.st_nlink!=1
                or (info.st_dev,info.st_ino)!=self.identity or (current.st_dev,current.st_ino)!=self.identity):raise vm.Unavailable('inherited storage identity changed')
        space=os.statvfs(self.scratch)
        if space.f_bavail*space.f_frsize<self.floor:raise vm.Unavailable('managed storage floor unavailable')


def ensure_cache(path, capacity, lease):
    """Provision only inside the controller's registered, already leased target."""
    path=Path(path)
    if type(capacity) is not int or capacity<64*1024*1024 or capacity%4096:
        raise vm.Unavailable('invalid managed CI cache capacity')
    parent=path.parent
    if not path.is_absolute() or parent.resolve()!=parent:
        raise vm.Unavailable('managed CI cache path is not literal')
    info=parent.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid() or info.st_mode&0o077:
        raise vm.Unavailable('managed CI cache parent is not private')
    lease.check()
    # Target/path locks must accompany the global lease; controller owns their
    # registration. A global-only fixture cannot provision an arbitrary cache.
    if len(lease.fds)<2:raise vm.Unavailable('managed CI cache target lease missing')
    available=os.statvfs(parent);free=available.f_bavail*available.f_frsize
    if path.exists() or path.is_symlink():
        fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
        with os.fdopen(fd,'rb') as stream:
            info=os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1 or info.st_mode&0o077 or info.st_size!=capacity:
                raise vm.Unavailable('managed CI cache identity or capacity changed')
            stream.seek(1024);header=stream.read(1024)
            if len(header)!=1024 or header[56:58]!=b'\x53\xef':raise vm.Unavailable('managed CI cache is not ext4')
            blocks=struct.unpack_from('<I',header,4)[0]
            if struct.unpack_from('<I',header,96)[0]&0x80:blocks|=struct.unpack_from('<I',header,336)[0]<<32
            if blocks*(1024<<struct.unpack_from('<I',header,24)[0])!=capacity:
                raise vm.Unavailable('managed CI filesystem capacity changed')
            growth=max(0,capacity-info.st_blocks*512)
        if free<lease.floor+growth:raise vm.Unavailable('managed CI cache growth reserve unavailable')
        return
    if free<lease.floor+capacity:raise vm.Unavailable('managed CI cache growth reserve unavailable')
    fd,name=tempfile.mkstemp(prefix='.ci-cache-',dir=parent)
    try:
        os.fchmod(fd,0o600);os.ftruncate(fd,capacity)
        # Fixed host tool/argv, no candidate executable or shell. Formatting is
        # storage-admitted; the candidate never receives the host descriptor.
        subprocess.run(['/usr/sbin/mke2fs','-q','-F','-t','ext4','-b','4096',f'/proc/self/fd/{fd}'],
                       pass_fds=(fd,),stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=True)
        os.fsync(fd);lease.check()
        os.link(name,path,follow_symlinks=False);os.unlink(name)
        directory=os.open(parent,os.O_RDONLY|os.O_DIRECTORY);os.fsync(directory);os.close(directory)
    finally:
        os.close(fd)
        try:os.unlink(name)
        except FileNotFoundError:pass


CARGO_OPERATIONS = """import json,subprocess,sys
request=json.loads(sys.argv[1])
selection=[arg for package in request['packages'] for arg in ('-p',package)] or ['--workspace']
commands=[]
if request['operation']=='targeted':commands.append(['/toolchain/bin/cargo','build',*selection])
commands.append(['/toolchain/bin/cargo','test',*selection,'--no-fail-fast','--','--test-threads=1',*request['filters']])
for command in commands:
    result=subprocess.run(command)
    if result.returncode:sys.exit(result.returncode if result.returncode>0 else 1)
"""

def cargo_request(request):
    packages=request.get('packages',[]);filters=request.get('filters',[])
    for values,pattern,label in ((packages,r'[A-Za-z0-9_][A-Za-z0-9_-]*','packages'),(filters,r'[A-Za-z0-9_:]+','filters')):
        if not isinstance(values,list) or len(values)>128 or any(not isinstance(value,str) or len(value)>256 or not re.fullmatch(pattern,value) for value in values):
            raise vm.Unavailable('invalid CI '+label)
    operation=request['operation']
    if operation in ('plan','step','repository') and (packages or filters):raise vm.Unavailable('irrelevant CI cargo arguments')
    if operation=='targeted' and (not packages or filters):raise vm.Unavailable('targeted CI requires packages and no filters')
    if operation=='repository':
        command=request.get('command')
        if not isinstance(command,str) or not command.strip() or '\0' in command or len(command.encode())>65536:raise vm.Unavailable('invalid owner repository command')
    elif request.get('command') not in (None,''):raise vm.Unavailable('irrelevant owner repository command')
    return {'operation':operation,'packages':packages,'filters':filters}


def execute(request,run=vm.run,progress=None):
    progress={} if progress is None else progress
    progress["stage"]="request_schema"
    required={'version','operation','runtime_config','workspace','state','git_objects','runner','contract','head','base','storage','scratch','build_cache',
              'source_generation','command_id','predecessor','tree','controller_authority_sha256'}
    optional={'id','changed_files','cache_bytes','packages','filters','command'}
    if set(request)-required-optional or not required<=set(request) or request['version']!=2 or request['operation'] not in ('plan','step','targeted','baseline','repository'):raise vm.Unavailable('invalid CI VM request')
    if not cache_generation._uuid(request['source_generation']) or not cache_generation._uuid(request['command_id']) or (request['predecessor'] is not None and not cache_generation._uuid(request['predecessor'])):raise cache_generation.FreshnessHold('generation_mismatch')
    if not cache_generation._digest(request['controller_authority_sha256']):raise cache_generation.FreshnessHold('authority_changed')
    if not all(isinstance(request[key],str) and re.fullmatch(r'[a-f0-9]{40,64}',request[key]) for key in ('head','base')):raise vm.Unavailable('CI candidate binding invalid')
    cargo=cargo_request(request)
    progress['stage']='controller_bundle'
    runner=private_bytes(request['runner']);contract=private_bytes(request['contract'])
    parsed=json.loads(contract)
    if parsed.get('version')!=1:raise vm.Unavailable('unsupported CI contract')
    progress['stage']='private_directories'
    workspace=Path(request['workspace']);state=Path(request['state']);scratch=Path(request['scratch'])
    for path in (workspace,state,scratch):
        info=path.lstat()
        if path.resolve()!=path or not path.is_absolute() or not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid() or info.st_mode&0o077:raise vm.Unavailable('CI snapshot/state/scratch is not private')
    generation=None
    if request['operation']!='plan':
        progress['stage']='source_census'
        outputs=cache_generation.declared_outputs(workspace)
        directories=['target',*[name for name,kind in outputs.items() if kind=='directory']]
        files=[name for name,kind in outputs.items() if kind=='file']
        source_inputs=cache_generation.source_census(workspace,Path(request['git_objects']),request['head'],
            generated_directories=directories,generated_files=files)
        target=workspace/'target'
        target.mkdir(mode=0o700,exist_ok=True)
        for name,kind in outputs.items():
            path=workspace/name
            if kind=='directory':path.mkdir(mode=0o700,exist_ok=True)
            else:
                try:
                    fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
                    os.close(fd)
                except FileExistsError:pass
        generation={'binding':{'generation':request['source_generation'],'head':request['head'],'tree':request['tree'],
            'contract_sha256':hashlib.sha256(contract).hexdigest(),'authority_sha256':request['controller_authority_sha256'],
            'source_inputs_sha256':source_inputs},'command':request['command_id'],'predecessor':request['predecessor'],
            'outputs':outputs}
    # The supplied snapshot has controller-generated Git metadata, never the
    # source worktree's config/hooks. Rebind only its private alternate file.
    progress['stage']='git_alternate'
    alternate=workspace/'.git/objects/info/alternates'
    if alternate.exists():
        if alternate.resolve()!=alternate or not alternate.resolve().is_relative_to(workspace):raise vm.Unavailable('CI Git metadata escapes snapshot')
        fd=os.open(alternate,os.O_WRONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
        with os.fdopen(fd,'wb') as stream:
            info=os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1 or info.st_mode&0o022:raise vm.Unavailable('CI Git alternate is not controller-owned')
            stream.truncate(0);stream.write(b'/ci-git-objects\n');stream.flush();os.fsync(stream.fileno())
    progress['stage']='storage_lease'
    lease=InheritedStorage(request['storage'],scratch)
    progress['stage']='runtime_load'
    runtime=vm.Runtime.load(Path(request['runtime_config']))
    progress['stage']='command_prepare'
    command=['/usr/bin/python3','-I','/ci-tools/runner.py','--contract','/ci-tools/contract.json']
    if request['operation']=='plan':
        changed=request.get('changed_files',[])
        if not isinstance(changed,list) or len(changed)>100000 or any(not isinstance(p,str) or '\0' in p for p in changed):raise vm.Unavailable('invalid CI path inventory')
        command+=['plan','--head',request['head'],'--base',request['base'],'--root','/workspace','--platform','linux','--probe-prerequisites','--vm-runtime']
        for path in changed:command+=['--changed-file',path]
    else:
        if not isinstance(request['build_cache'],str):raise vm.Unavailable('managed CI cache image is required')
        progress['stage']='cache_prepare'
        ensure_cache(request['build_cache'],request.get('cache_bytes'),lease)
        info=Path(request['build_cache']).stat()
        generation['binding']['cache_identity_sha256']=hashlib.sha256(json.dumps([info.st_dev,info.st_ino,info.st_size],separators=(',',':')).encode()).hexdigest()
        cache_generation.validate_binding(generation['binding'])
        if request['operation']=='step':
            identifier=request.get('id')
            if not isinstance(identifier,str) or len(identifier)>4096 or '\0' in identifier:raise vm.Unavailable('invalid CI command identity')
            command+=['step','--vm-runtime','--id',identifier,'--input-head',request['head'],'--input-base',request['base'],'--state-dir','/ci-runtime']
        elif request['operation']=='repository':
            command=['/bin/bash','--noprofile','--norc','-o','pipefail','-c',request['command']]
        else:
            command=['/usr/bin/python3','-I','-c',CARGO_OPERATIONS,json.dumps(cargo,separators=(',',':'))]
    progress['stage']='vm_launch'
    outcome=run(runtime,workspace,command,{},timeout=None,scratch_dir=str(scratch),build_cache=request['build_cache'],storage_lease=lease,
                ci_context={'state':state,'git_objects':Path(request['git_objects']),'runner':runner,'contract':contract,
                            'cache_program':CACHE_PROGRAM,'generation':generation})
    progress['stage']='receipt_validate'
    result={'version':1,'head':request['head'],'base':request['base'],'contract_sha256':hashlib.sha256(contract).hexdigest(),
            'boundary':'existing-build-vm','exit_code':outcome['exit_code'],'stdout':outcome['stdout'],'stderr':outcome['stderr']}
    if outcome['exit_code']==0:
        if request['operation']=='plan':
            plan=json.loads(outcome['stdout'])
            if plan.get('head')!=request['head'] or plan.get('base')!=request['base'] or plan.get('contract_sha256')!=result['contract_sha256']:raise vm.Unavailable('guest CI plan binding mismatch')
            result.update(status='planned',plan=plan)
        else:result['status']='passed'
    elif 'shared CI prerequisite unavailable' in outcome['stderr']:result['status']='prerequisite_hold'
    else:result['status']='failed'
    return result


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--spec',type=Path,required=True);args=parser.parse_args()
    progress={'stage':'request_read'}
    try:result=execute(json.loads(private_bytes(args.spec)),progress=progress)
    except vm.FreshnessUnavailable as error:
        result={'version':1,'status':'prerequisite_hold','boundary':'existing-build-vm',
                'freshness_hold':cache_generation.validate_hold(error.diagnostic),'stage':progress['stage'],'category':'FreshnessHold'}
    except cache_generation.FreshnessHold as error:
        result={'version':1,'status':'prerequisite_hold','boundary':'existing-build-vm','freshness_hold':error.diagnostic(),
                'stage':progress['stage'],'category':'FreshnessHold'}
    except (OSError,ValueError,KeyError,subprocess.SubprocessError) as error:
        # Local diagnostics deliberately omit raw paths, requests and exception text.
        result={'version':1,'status':'prerequisite_hold','boundary':'existing-build-vm','reason':'CI VM setup unavailable','category':type(error).__name__ if type(error).__name__ in ('Unavailable','OSError','PermissionError','FileNotFoundError','ValueError','KeyError','JSONDecodeError','CalledProcessError') else 'SetupError','stage':progress['stage'],'errno':error.errno if isinstance(error,OSError) and type(error.errno) is int and 0<=error.errno<=4096 else None}
    print(json.dumps(result,sort_keys=True))
    return 0 if result['status'] in ('passed','planned') else 2
if __name__=='__main__':raise SystemExit(main())
