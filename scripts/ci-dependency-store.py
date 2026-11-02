#!/usr/bin/env python3
"""Privileged dependency publication inside an exclusively admitted CI image.

Only the controller may prepare an external-only producer. The host adapter
constructs its source from pinned dependency declarations and an empty root crate;
no candidate or main-branch application source is mounted in that producer. Publication is a
later controller phase, after the host supplies the original command's verified
quiescence acknowledgement. Candidate generation directories are never producers.
These records are cache provenance, not passing CI or merge authorization.
"""
import hashlib
import importlib.util
import json
import os
import posixpath
from pathlib import Path
import stat
import tempfile
import shutil

BASE=Path(__file__).resolve().parent
def _module(name,filename):
    spec=importlib.util.spec_from_file_location(name,BASE/filename)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    return module
cache=_module('seed_cache','ci-cache-generation.py')
artifacts=_module('seed_artifacts','ci-dependency-artifacts.py')
FreshnessHold=cache.FreshnessHold


def _encoded(value):
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def _digest(value):
    return hashlib.sha256(_encoded(value)).hexdigest()


def _binding(value):
    keys={'authority_sha256','cache_identity_sha256','contract_sha256','source_inputs_sha256',
          'dependency_inputs_sha256','source_head'}
    if (not isinstance(value,dict) or set(value)!=keys
            or any(not cache._digest(value[key]) for key in keys-{'source_head'})
            or not isinstance(value['source_head'],str) or len(value['source_head']) not in (40,64)
            or any(char not in '0123456789abcdef' for char in value['source_head'])):
        raise FreshnessHold('artifact_provenance_unknown')


def _index_path(directory,binding):
    _binding(binding)
    namespace=_root(directory)/'ci-dependency-index';cache._directory(namespace,0o700,create=True)
    key={name:binding[name] for name in ('authority_sha256','cache_identity_sha256','contract_sha256','dependency_inputs_sha256')}
    return namespace/(_digest(key)+'.json')


def index_seed(directory,identity,binding):
    read_seed(directory,identity,binding)
    path=_index_path(directory,binding)
    # Preserve replaced metadata, including unknown/torn prior index entries.
    if path.exists() or path.is_symlink():
        retired=path.parent/'superseded';cache._directory(retired,0o700,create=True)
        os.rename(path,retired/(path.stem+'-'+__import__('uuid').uuid4().hex+'.json'))
    cache._write_record(path,{'version':1,'seed':identity})


def lookup_seed(directory,binding):
    path=_index_path(directory,binding)
    if not path.exists() and not path.is_symlink():return None
    value=_document(path)
    if set(value)!={'version','seed'} or type(value['version']) is not int or value['version']!=1:
        raise FreshnessHold('metadata_torn')
    read_seed(directory,value['seed'],binding)
    return value['seed']


def _document(path):
    try:
        fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
        with os.fdopen(fd,'rb') as stream:
            info=os.fstat(stream.fileno())
            if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid() or info.st_nlink!=1
                    or info.st_mode&0o077 or info.st_size>16*1024*1024):
                raise FreshnessHold('metadata_torn')
            value=json.loads(stream.read(),object_pairs_hook=cache._unique_object)
        if not isinstance(value,dict):raise FreshnessHold('metadata_torn')
        return value
    except (OSError,ValueError,UnicodeError):
        raise FreshnessHold('metadata_torn') from None


def _root(directory):
    directory=Path(directory)
    if not directory.is_absolute() or directory.resolve()!=directory:
        raise FreshnessHold('invalidation_unsafe')
    cache._directory(directory,0o700,create=False)
    return directory


def _producer(directory,producer):
    if not cache._uuid(producer):raise FreshnessHold('generation_mismatch')
    path=_root(directory)/'ci-dependency-producers'/producer
    cache._directory(path.parent,0o700,create=False)
    cache._directory(path,0o700,create=False)
    control=_document(path/'control.json')
    _producer_control(control,producer)
    return path,control


def _producer_control(control,producer):
    if (not isinstance(control,dict) or set(control)!={'version','kind','producer','command','binding','worker_uid','worker_gid','state'}
            or type(control['version']) is not int or control['version']!=1
            or control['kind']!='isolated-external-dependency-producer' or control['producer']!=producer
            or not cache._uuid(control['command']) or control['state'] not in ('building','produced')
            or any(type(control[key]) is not int or control[key]<=0 for key in ('worker_uid','worker_gid'))):
        raise FreshnessHold('artifact_provenance_unknown')
    _binding(control['binding'])


def _producer_history(path,producer):
    _,control=_producer(path.parent.parent,producer)
    original={'control':control,'produced':None,'published':None}
    for name in ('produced','published'):
        child=path/(name+'.json')
        if child.exists() or child.is_symlink():original[name]=_document(child)
    _validate_producer_history(original,producer)
    return original


def _validate_producer_history(original,producer):
    if not isinstance(original,dict) or set(original)!={'control','produced','published'}:
        raise FreshnessHold('metadata_torn')
    _producer_control(original['control'],producer)
    produced=original['produced'];published=original['published']
    command=original['control']['command']
    if produced is not None and (not isinstance(produced,dict)
            or set(produced)!={'version','command','units','source_trees'}
            or type(produced['version']) is not int or produced['version']!=1
            or produced['command']!=command or not isinstance(produced['units'],list)
            or not isinstance(produced['source_trees'],list)):
        raise FreshnessHold('metadata_torn')
    if published is not None and (not isinstance(published,dict)
            or set(published)!={'version','seed','ack'} or type(published['version']) is not int
            or published['version']!=1 or not cache._digest(published['seed'])
            or not isinstance(published['ack'],dict) or published['ack'].get('command')!=command):
        raise FreshnessHold('metadata_torn')
    if len(_encoded(original))>8*1024**2:raise FreshnessHold('metadata_torn')


def _fsync_directory(path):
    descriptor=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
    try:os.fsync(descriptor)
    finally:os.close(descriptor)


def _retirement_ack(control,ack):
    if (not isinstance(ack,dict) or set(ack)!={'command','authority_sha256','cache_identity_sha256','quiescence_sha256'}
            or ack['command']!=control['command'] or not cache._digest(ack['quiescence_sha256'])
            or any(ack[key]!=control['binding'][key] for key in ('authority_sha256','cache_identity_sha256'))):
        raise FreshnessHold('cleanup_uncertain')


def reclaim_producers(directory,protected_producers,*,cache_identity,eligible=None,limit=2):
    """Retire known inactive payloads before workers, under existing admission.

    Only the original host owner can release its source lifetime and provide
    the original producer's verified host acknowledgement in ``eligible``.
    Protected IDs alone never authorize deleting another session's producer.
    Already committed retirement intents may recover under reconciled whole-VM
    cleanup/storage admission. Preserve metadata durably before deletion; unknown
    ownership and contradictory replay state are never adopted or removed.
    Published seed inodes and installed hard links are independent of these
    producer paths and are not reclaimed here.
    """
    import itertools
    eligible=[] if eligible is None else eligible
    if (not isinstance(protected_producers,list) or len(protected_producers)>4096
            or not cache._digest(cache_identity)
            or any(not cache._uuid(value) for value in protected_producers)
            or len(set(protected_producers))!=len(protected_producers)
            or type(limit) is not int or not 1<=limit<=2
            or not isinstance(eligible,list) or len(eligible)>4096
            or not shutil.rmtree.avoids_symlink_attacks):
        raise FreshnessHold('cleanup_uncertain')
    releases={}
    for item in eligible:
        if (not isinstance(item,dict) or set(item)!={'producer','ack'} or not cache._uuid(item['producer'])
                or item['producer'] in releases or not isinstance(item['ack'],dict)):
            raise FreshnessHold('cleanup_uncertain')
        releases[item['producer']]=item['ack']
    root=_root(directory);producers=root/'ci-dependency-producers'
    if not producers.exists() and not producers.is_symlink():return []
    cache._directory(producers,0o700,create=False)
    history=root/'ci-producer-retirements';cache._directory(history,0o700,create=True)
    pending=history/'pending';completed=history/'completed'
    for path in (pending,completed):cache._directory(path,0o700,create=True)
    # Completed history is outside the pending discovery window. Old receipts
    # cannot starve interrupted retirement recovery as the archive grows.
    candidates=[path.stem for path in itertools.islice(pending.iterdir(),1024) if path.suffix=='.json']
    candidates.extend(releases)
    reclaimed=[];seen=set()
    for producer in candidates:
        if len(reclaimed)>=limit:break
        if not cache._uuid(producer) or producer in seen or producer in protected_producers:continue
        seen.add(producer);path=producers/producer;receipt=pending/(producer+'.json')
        try:
            finished=completed/(producer+'.json')
            if finished.exists() or finished.is_symlink():
                prior=_document(finished)
                if (set(prior)!={'version','state','original','authorization'} or prior['version']!=2
                        or prior['state']!='reclaimed' or producer not in releases
                        or prior['authorization']!=releases[producer]):continue
                _validate_producer_history(prior['original'],producer)
                _retirement_ack(prior['original']['control'],prior['authorization'])
                if prior['original']['control']['binding']['cache_identity_sha256']!=cache_identity:continue
                if path.exists() or path.is_symlink():continue
                reclaimed.append(producer)
                continue
            intent=_document(receipt) if receipt.exists() or receipt.is_symlink() else None
            if intent is not None:
                if (set(intent)!={'version','state','original','authorization'} or type(intent['version']) is not int
                        or intent['version']!=2 or intent['state'] not in ('reclaiming','reclaimed')):
                    raise FreshnessHold('metadata_torn')
                original=intent['original'];_validate_producer_history(original,producer)
                authorization=intent['authorization']
                if producer in releases and releases[producer]!=authorization:raise FreshnessHold('cleanup_uncertain')
            else:
                original=_producer_history(path,producer)
                authorization=releases[producer]
            _retirement_ack(original['control'],authorization)
            if original['control']['binding']['cache_identity_sha256']!=cache_identity:
                raise FreshnessHold('cleanup_uncertain')
            if original['published'] is not None and original['published']['ack']!=authorization:
                raise FreshnessHold('cleanup_uncertain')
            if path.exists() or path.is_symlink():
                if intent is not None and intent['state']=='reclaimed':raise FreshnessHold('cleanup_uncertain')
                cache._directory(path,0o700,create=False)
                children={child.name for child in path.iterdir()}
                if children-{'control.json','produced.json','published.json','target','cargo-home'}:
                    raise FreshnessHold('invalidation_unsafe')
                for name in ('control','produced','published'):
                    child=path/(name+'.json')
                    if child.exists() or child.is_symlink():
                        if _document(child)!=original[name]:raise FreshnessHold('metadata_torn')
                    elif original[name] is not None and intent is None:raise FreshnessHold('metadata_missing')
                control=original['control']
                for name in children&{'target','cargo-home'}:
                    info=(path/name).lstat()
                    if (not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode)
                            or info.st_dev!=root.stat().st_dev or info.st_uid!=control['worker_uid']
                            or info.st_gid!=control['worker_gid'] or info.st_mode&0o077):
                        raise FreshnessHold('invalidation_unsafe')
        except (FreshnessHold,OSError,ValueError,TypeError,KeyError):continue
        if intent is None:cache._write_record(receipt,{'version':2,'state':'reclaiming','original':original,'authorization':authorization})
        try:
            if path.exists():shutil.rmtree(path)
            _fsync_directory(producers)
            cache._write_record(receipt,{'version':2,'state':'reclaimed','original':original,'authorization':authorization})
            os.rename(receipt,completed/receipt.name)
            _fsync_directory(completed);_fsync_directory(pending)
        except OSError:raise FreshnessHold('cleanup_uncertain') from None
        reclaimed.append(producer)
    return reclaimed


def prepare_isolated_producer(directory,binding,producer,command,worker_uid,worker_gid):
    _binding(binding)
    if (not cache._uuid(producer) or not cache._uuid(command)
            or any(type(value) is not int or value<=0 for value in (worker_uid,worker_gid))):
        raise FreshnessHold('artifact_provenance_unknown')
    root=_root(directory)/'ci-dependency-producers';cache._directory(root,0o700,create=True)
    path=root/producer
    if path.exists() or path.is_symlink():raise FreshnessHold('generation_mismatch')
    path.mkdir(mode=0o700)
    cache._write_record(path/'control.json',{'version':1,'kind':'isolated-external-dependency-producer',
        'producer':producer,'command':command,'binding':binding,'worker_uid':worker_uid,'worker_gid':worker_gid,'state':'building'})
    for name in ('target','cargo-home'):
        (path/name).mkdir(mode=0o700);os.chown(path/name,worker_uid,worker_gid)
    return path/'target',path/'cargo-home'


def record_isolated_output(directory,producer,command,metadata,messages):
    path,control=_producer(directory,producer)
    if control['command']!=command or control['state']!='building':
        raise FreshnessHold('artifact_provenance_unknown')
    try:
        reusable=artifacts.inputs.reusable_packages(metadata)
        units=artifacts.select(metadata,messages,target_root='/workspace/target')
    except artifacts.InputsUnknown:raise FreshnessHold('input_census_unknown') from None
    if any(unit['package_id'] not in reusable or unit['target_kind'] not in (['lib'],['rlib']) for unit in units):
        raise FreshnessHold('artifact_provenance_unknown')
    packages={package['id']:package for package in metadata['packages']}
    homes=set()
    for unit in units:
        package=artifacts.inputs._path(packages[unit['package_id']]['manifest_path']).parent
        prefix=artifacts.inputs._path('/ci-cargo-home')
        if not package.is_relative_to(prefix):raise FreshnessHold('artifact_provenance_unknown')
        relative=str(package.relative_to(prefix));homes.add(relative)
        unit['package_source_relative']=relative
    cache._write_record(path/'produced.json',{'version':1,'command':command,'units':units,'source_trees':sorted(homes)})
    control['state']='produced';cache._write_record(path/'control.json',control)


def _relative(value):
    if (not isinstance(value,str) or not value or value.startswith('/') or '\\' in value
            or any(part in ('','.','..') for part in value.split('/'))):
        raise FreshnessHold('artifact_provenance_unknown')
    return Path(value)


def _files_under(root,relative):
    path=root/_relative(relative)
    if path.resolve()!=path:raise FreshnessHold('artifact_provenance_unknown')
    if path.is_file():return [relative]
    if not path.is_dir():raise FreshnessHold('artifact_provenance_unknown')
    selected=[]
    for base,dirs,files in os.walk(path,followlinks=False):
        for name in dirs:
            if (Path(base)/name).is_symlink():raise FreshnessHold('artifact_provenance_unknown')
        for name in files:
            entry=Path(base)/name
            if entry.is_symlink():raise FreshnessHold('artifact_provenance_unknown')
            selected.append(str(entry.relative_to(root)))
            if len(selected)>100_000:raise FreshnessHold('artifact_provenance_unknown')
    return selected


def _file_info(path):
    if path.resolve()!=path:raise FreshnessHold('artifact_provenance_unknown')
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
    with os.fdopen(fd,'rb') as stream:
        info=os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size>8*1024**3:
            raise FreshnessHold('artifact_provenance_unknown')
        digest=hashlib.sha256()
        while block:=stream.read(1024*1024):digest.update(block)
    return {'sha256':digest.hexdigest(),'size':info.st_size,'mtime_ns':info.st_mtime_ns,
            'mode':0o555 if info.st_mode&0o111 else 0o444}


def _sealed_units(path,units,files):
    sealed=[]
    expected={'rustc','features','declared_features','profile','rustflags','config','target','path','compile_kind','deps','local'}
    for unit in units:
        if not isinstance(unit,dict) or unit.get('unit_identity_status')!='requires_producer_fingerprint':
            raise FreshnessHold('artifact_provenance_unknown')
        prefix='target/'+str(_relative(unit.get('fingerprint_directory')))+'/'
        fingerprints={name:info['sha256'] for name,info in files.items() if name.startswith(prefix) and name.endswith('.json')}
        if not fingerprints:raise FreshnessHold('artifact_provenance_unknown')
        for name in fingerprints:
            source=path/_relative(name)
            try:
                fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
                with os.fdopen(fd,'rb') as stream:
                    if os.fstat(stream.fileno()).st_size>1024*1024:raise FreshnessHold('artifact_provenance_unknown')
                    value=json.loads(stream.read(),object_pairs_hook=cache._unique_object)
                if (not isinstance(value,dict) or set(value)!=expected
                        or any(type(value[key]) is not int or not 0<=value[key]<2**64
                               for key in ('rustc','profile','config','target','path','compile_kind'))
                        or not isinstance(value['features'],str) or not isinstance(value['declared_features'],str)
                        or not isinstance(value['deps'],list) or not isinstance(value['local'],list)
                        or not isinstance(value['rustflags'],list) or any(not isinstance(flag,str) for flag in value['rustflags'])):
                    raise FreshnessHold('artifact_provenance_unknown')
                if 'features' in unit and sorted(json.loads(value['features']))!=unit['features']:
                    raise FreshnessHold('artifact_provenance_unknown')
            except (OSError,ValueError,UnicodeError,TypeError):
                raise FreshnessHold('artifact_provenance_unknown') from None
        compiler_inputs=_compiler_inputs(path,unit,files)
        sealed.append({**unit,'unit_identity_status':'sealed_producer_fingerprint',
                       'fingerprint_metadata_sha256':_digest(fingerprints),
                       'compiler_inputs_sha256':_digest(compiler_inputs)})
    return sealed


def _make_words(text):
    words=[];word=[];escaped=False
    for char in text:
        if escaped:word.append(char);escaped=False
        elif char=='\\':escaped=True
        elif char.isspace():
            if word:words.append(''.join(word));word=[]
        else:word.append(char)
    if escaped:raise FreshnessHold('input_census_unknown')
    if word:words.append(''.join(word))
    return words


def _compiler_inputs(path,unit,files):
    relative=str(_relative(unit.get('package_source_relative')))
    package=artifacts.inputs._path('/ci-cargo-home')/relative
    dep=path/'target'/_relative(unit.get('dep_info'))
    try:
        if dep.stat().st_size>8*1024*1024:raise FreshnessHold('input_census_unknown')
        text=dep.read_text().replace('\\\n','')
    except (OSError,UnicodeError):raise FreshnessHold('input_census_unknown') from None
    allowed={'CARGO_MANIFEST_DIR','CARGO_MANIFEST_PATH',
        *('CARGO_PKG_'+name for name in ('NAME','VERSION','VERSION_MAJOR','VERSION_MINOR','VERSION_PATCH','VERSION_PRE',
          'AUTHORS','DESCRIPTION','HOMEPAGE','REPOSITORY','LICENSE','LICENSE_FILE','RUST_VERSION','README'))}
    sources={};environment={}
    for line in text.splitlines():
        if not line.strip():continue
        if line.startswith('# env-dep:'):
            name,separator,value=line[len('# env-dep:'):].partition('=')
            if name not in allowed:raise FreshnessHold('artifact_provenance_unknown')
            environment[name]=value if separator else None
            continue
        escaped=False;separator=None
        for index,char in enumerate(line):
            if escaped:escaped=False
            elif char=='\\':escaped=True
            elif char==':':separator=index;break
        if separator is None:raise FreshnessHold('input_census_unknown')
        for value in _make_words(line[separator+1:]):
            if not value.startswith('/'):raise FreshnessHold('input_census_unknown')
            source=artifacts.inputs._path(posixpath.normpath(value))
            if not source.is_relative_to(package):raise FreshnessHold('artifact_provenance_unknown')
            name='cargo-home/'+str(source.relative_to(artifacts.inputs._path('/ci-cargo-home')))
            if name not in files:raise FreshnessHold('artifact_provenance_unknown')
            sources[name]=files[name]['sha256']
    if not sources:raise FreshnessHold('input_census_unknown')
    for name,expected in (('CARGO_MANIFEST_DIR',str(package)),('CARGO_MANIFEST_PATH',str(package/'Cargo.toml'))):
        if name in environment and environment[name]!=expected:raise FreshnessHold('artifact_provenance_unknown')
    return {'sources':sources,'cargo_environment':environment}


def publish(directory,producer,ack):
    try:return _publish(directory,producer,ack)
    except artifacts.InputsUnknown:
        raise FreshnessHold('input_census_unknown') from None


def _publish(directory,producer,ack):
    try:path,control=_producer(directory,producer)
    except OSError:raise FreshnessHold('metadata_missing') from None
    binding=control['binding']
    if (control['state']!='produced' or not isinstance(ack,dict)
            or set(ack)!={'command','authority_sha256','cache_identity_sha256','quiescence_sha256'}
            or ack['command']!=control['command'] or ack['authority_sha256']!=binding['authority_sha256']
            or ack['cache_identity_sha256']!=binding['cache_identity_sha256'] or not cache._digest(ack['quiescence_sha256'])):
        raise FreshnessHold('cleanup_uncertain')
    publication=path/'published.json'
    if publication.exists() or publication.is_symlink():
        receipt=_document(publication)
        if (set(receipt)!={'version','seed','ack'} or receipt['version']!=1 or receipt['ack']!=ack
                or not cache._digest(receipt['seed'])):raise FreshnessHold('artifact_provenance_unknown')
        read_seed(directory,receipt['seed'],binding)
        _retire_published_payload(path)
        return receipt['seed']
    produced=_document(path/'produced.json')
    if (set(produced)!={'version','command','units','source_trees'} or type(produced['version']) is not int
            or produced['version']!=1 or produced['command']!=control['command']
            or not isinstance(produced['units'],list) or not isinstance(produced['source_trees'],list)):
        raise FreshnessHold('metadata_torn')
    selected=set()
    for unit in produced['units']:
        for relative in [*unit['files'],unit['fingerprint_directory'],*unit['directory_trees']]:
            selected.update('target/'+name for name in _files_under(path/'target',relative))
        if unit.get('dep_info') is not None:
            selected.update('target/'+name for name in _files_under(path/'target',unit['dep_info']))
    for relative in produced['source_trees']:
        selected.update('cargo-home/'+name for name in _files_under(path/'cargo-home',relative))
    if len(selected)>100_000:raise FreshnessHold('artifact_provenance_unknown')
    files={name:_file_info(path/_relative(name)) for name in sorted(selected)}
    if sum(value['size'] for value in files.values())>8*1024**3:raise FreshnessHold('artifact_provenance_unknown')
    manifest={'version':1,'kind':'trusted-dependency-seed','binding':binding,
        'producer':{'id':producer,'kind':control['kind'],**ack},
        'units':_sealed_units(path,produced['units'],files),'files':files,'shipping_authority':False}
    identity=_digest(manifest)
    seeds=_root(directory)/'ci-trusted-dependencies';cache._directory(seeds,0o700,create=True)
    final=seeds/identity
    if final.exists() or final.is_symlink():
        if read_seed(directory,identity,binding)!=manifest:raise FreshnessHold('metadata_torn')
        cache._write_record(publication,{'version':1,'seed':identity,'ack':ack})
        _retire_published_payload(path)
        return identity
    staging=Path(tempfile.mkdtemp(prefix='.publishing-',dir=seeds))
    # The original VM is positively quiescent. Hard links preserve exact Cargo
    # mtimes and avoid doubling the managed image's artifact growth budget.
    # Changing ownership/mode seals the original inode too; it never runs again.
    for name,info in files.items():
        source=path/_relative(name);destination=staging/'files'/_relative(name)
        destination.parent.mkdir(parents=True,exist_ok=True)
        os.link(source,destination,follow_symlinks=False)
        os.chown(destination,os.geteuid(),os.getegid());os.chmod(destination,info['mode'])
        fd=os.open(destination,os.O_RDONLY|os.O_NOFOLLOW)
        try:os.fsync(fd)
        finally:os.close(fd)
    cache._write_record(staging/'manifest.json',manifest)
    for base,_,_ in os.walk(staging,topdown=False):
        fd=os.open(base,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:os.fsync(fd)
        finally:os.close(fd)
    os.rename(staging,final)
    fd=os.open(seeds,os.O_RDONLY|os.O_DIRECTORY)
    try:os.fsync(fd)
    finally:os.close(fd)
    cache._write_record(publication,{'version':1,'seed':identity,'ack':ack})
    _retire_published_payload(path)
    return identity


def _retire_published_payload(path):
    # Publication retained original host quiescence and the immutable seed now
    # owns every selected inode. Keep the original control and publication
    # receipt; only its known private payload is eligible for bounded cleanup.
    if not shutil.rmtree.avoids_symlink_attacks:raise FreshnessHold('cleanup_uncertain')
    for name in ('target','cargo-home'):
        child=path/name
        if not child.exists() and not child.is_symlink():continue
        info=child.lstat()
        if not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode) or info.st_dev!=path.stat().st_dev:
            raise FreshnessHold('cleanup_uncertain')
        try:shutil.rmtree(child)
        except OSError:raise FreshnessHold('cleanup_uncertain') from None
    descriptor=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
    try:os.fsync(descriptor)
    finally:os.close(descriptor)


def read_seed(directory,identity,binding):
    try:return _read_seed(directory,identity,binding)
    except (OSError,ValueError,TypeError,KeyError,RuntimeError,artifacts.InputsUnknown):
        raise FreshnessHold('artifact_provenance_unknown') from None


def _read_seed(directory,identity,binding):
    _binding(binding)
    if not cache._digest(identity):raise FreshnessHold('artifact_provenance_unknown')
    path=_root(directory)/'ci-trusted-dependencies'/identity
    cache._directory(path.parent,0o700,create=False);cache._directory(path,0o700,create=False)
    manifest=_document(path/'manifest.json')
    if (set(manifest)!={'version','kind','binding','producer','units','files','shipping_authority'}
            or type(manifest['version']) is not int or manifest['version']!=1
            or manifest['kind']!='trusted-dependency-seed' or manifest['shipping_authority'] is not False
            or _digest(manifest)!=identity):raise FreshnessHold('artifact_provenance_unknown')
    _binding(manifest['binding'])
    producer=manifest['producer']
    if (not isinstance(producer,dict) or set(producer)!={'id','kind','command','authority_sha256','cache_identity_sha256','quiescence_sha256'}
            or producer['kind']!='isolated-external-dependency-producer'
            or not cache._uuid(producer['id']) or not cache._uuid(producer['command'])
            or not cache._digest(producer['quiescence_sha256'])
            or any(producer[key]!=manifest['binding'][key] for key in ('authority_sha256','cache_identity_sha256'))):
        raise FreshnessHold('artifact_provenance_unknown')
    if any(manifest['binding'][key]!=binding[key] for key in
           ('authority_sha256','cache_identity_sha256','contract_sha256','dependency_inputs_sha256')):
        raise FreshnessHold('artifact_provenance_unknown')
    if not isinstance(manifest['files'],dict) or len(manifest['files'])>100_000:
        raise FreshnessHold('metadata_torn')
    if not isinstance(manifest['units'],list) or len(manifest['units'])>100_000:
        raise FreshnessHold('metadata_torn')
    for unit in manifest['units']:
        if not isinstance(unit,dict) or unit.get('unit_identity_status')!='sealed_producer_fingerprint':
            raise FreshnessHold('artifact_provenance_unknown')
        prefix='target/'+str(_relative(unit.get('fingerprint_directory')))+'/'
        fingerprints={name:info['sha256'] for name,info in manifest['files'].items() if name.startswith(prefix) and name.endswith('.json')}
        if not fingerprints or _digest(fingerprints)!=unit.get('fingerprint_metadata_sha256'):
            raise FreshnessHold('artifact_provenance_unknown')
        if not cache._digest(unit.get('compiler_inputs_sha256')):
            raise FreshnessHold('artifact_provenance_unknown')
        if _digest(_compiler_inputs(path/'files',unit,manifest['files']))!=unit['compiler_inputs_sha256']:
            raise FreshnessHold('artifact_provenance_unknown')
    for name,expected in manifest['files'].items():
        file=path/'files'/_relative(name)
        info=file.lstat()
        if (not isinstance(expected,dict) or set(expected)!={'sha256','size','mtime_ns','mode'}
                or expected.get('mode') not in (0o444,0o555) or not cache._digest(expected.get('sha256'))
                or any(type(expected[key]) is not int for key in ('size','mtime_ns','mode'))
                or info.st_uid!=os.geteuid() or stat.S_IMODE(info.st_mode)!=expected['mode'] or _file_info(file)!=expected):
            raise FreshnessHold('artifact_provenance_unknown')
    return manifest


def install(directory,identity,binding,target,home):
    manifest=read_seed(directory,identity,binding)
    source=Path(directory)/'ci-trusted-dependencies'/identity/'files'
    roots={'target':Path(target),'cargo-home':Path(home)}
    # Refuse a conflicting/untrusted existing file before changing any layout.
    for name,expected in manifest['files'].items():
        relative=_relative(name);root=roots.get(relative.parts[0])
        if root is None:raise FreshnessHold('metadata_torn')
        destination=root.joinpath(*relative.parts[1:])
        if destination.resolve()!=destination:raise FreshnessHold('invalidation_unsafe')
        if destination.exists() and (destination.stat().st_uid!=os.geteuid() or _file_info(destination)!=expected):
            raise FreshnessHold('artifact_provenance_unknown')
    # Empty or partially successful producers still need one coherent layout.
    # Never let a ready installation receipt describe worker-owned roots.
    image=_root(directory)
    for root in roots.values():
        if (not root.is_absolute() or root.resolve()!=root or not root.is_relative_to(image)
                or root==image or not root.is_dir() or root.stat().st_dev!=image.stat().st_dev):
            raise FreshnessHold('invalidation_unsafe')
    for root in roots.values():
        os.chown(root,os.geteuid(),os.getegid());os.chmod(root,0o1777)
    for name,expected in manifest['files'].items():
        relative=_relative(name);root=roots[relative.parts[0]]
        chain=[root]
        for component in relative.parts[1:-1]:chain.append(chain[-1]/component)
        for parent in chain:
            parent.mkdir(exist_ok=True)
            if parent.is_symlink() or not parent.is_dir():raise FreshnessHold('invalidation_unsafe')
            os.chown(parent,os.geteuid(),os.getegid());os.chmod(parent,0o1777)
        destination=root.joinpath(*relative.parts[1:])
        if not destination.exists():os.link(source/relative,destination,follow_symlinks=False)
    return identity
