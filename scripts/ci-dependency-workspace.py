#!/usr/bin/env python3
"""Construct an external-only producer from verified dependency declarations.

The generated workspace contains no application source, scripts, configuration
hooks or Git metadata. Cargo resolves it offline, then verify_resolution checks
the exact approved graph/checksums before any dependency can execute.
"""
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import tomllib

spec=importlib.util.spec_from_file_location('workspace_seed_inputs',Path(__file__).with_name('ci-dependency-seed.py'))
inputs=importlib.util.module_from_spec(spec);spec.loader.exec_module(inputs)
InputsUnknown=inputs.InputsUnknown
REGISTRY='registry+https://github.com/rust-lang/crates.io-index'
NAME='jarvis-external-dependency-seed'
EMPTY_SOURCE=b'// Controller-owned dependency producer; no application code.\n'
SOURCE_PATHS={'Cargo.toml','src/lib.rs','seed-inputs.json','seed-lock.toml'}


def _lock(text):
    try:value=tomllib.loads(text)
    except (ValueError,TypeError):raise InputsUnknown() from None
    if type(value.get('version')) is not int or value['version'] not in (3,4) or not isinstance(value.get('package'),list):
        raise InputsUnknown()
    rows={}
    for row in value['package']:
        if not isinstance(row,dict) or not inputs._text(row.get('name'),256) or not inputs._text(row.get('version'),256):
            raise InputsUnknown()
        source=row.get('source')
        if source is None:continue
        if not inputs._text(source):raise InputsUnknown()
        key=(row['name'],row['version'],source)
        if key in rows:raise InputsUnknown()
        rows[key]=row
    return rows


def source_files(metadata,lockfile,*,source_root='/workspace'):
    reusable=inputs.reusable_packages(metadata,source_root=source_root)
    packages={package['id']:package for package in metadata['packages']}
    nodes={node['id']:node for node in metadata['resolve']['nodes']}
    # Git inputs retain their normal offline downloads/fresh compilation. A
    # registry seed never silently changes a Git branch source ID into a rev.
    selected={key for key in reusable if reusable[key]['source']==REGISTRY}
    while True:
        closed={key for key in selected if set(nodes[key]['dependencies'])<=selected}
        if closed==selected:break
        selected=closed
    locked=_lock(lockfile);approved={}
    manifest=['[package]',f'name = "{NAME}"','version = "0.0.0"','edition = "2021"',
              'publish = false','[lib]','path = "src/lib.rs"','[workspace]','resolver = "2"','[dependencies]']
    quote=lambda value:json.dumps(value,ensure_ascii=False)
    for index,key in enumerate(sorted(selected)):
        package=packages[key];version=package.get('version')
        if (not inputs._text(version,128) or re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_-]*',package['name']) is None
                or re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.+-]+)?',version) is None):
            raise InputsUnknown()
        row=locked.get((package['name'],version,REGISTRY))
        if row is None or not isinstance(row.get('checksum'),str) or re.fullmatch(r'[0-9a-f]{64}',row['checksum']) is None:
            raise InputsUnknown()
        features=list(reusable[key]['features'])
        approved[key]={'name':package['name'],'version':version,'source':REGISTRY,'checksum':row['checksum'],
                       'features':features,'dependencies':sorted(nodes[key]['dependencies'])}
        manifest.append(f'dependency_{index} = {{ package = {quote(package["name"])}, version = {quote("="+version)}, '
                        f'default-features = false, features = [{", ".join(quote(feature) for feature in features)}] }}')
    if any(row.get('name')==NAME and row.get('version')=='0.0.0' and row.get('source') is None
           for row in tomllib.loads(lockfile)['package']):raise InputsUnknown()
    # Retain the original locked rows while resolving the synthetic root. This
    # preserves explicitly pinned yanked versions; a fresh resolution alone
    # would reject them or select different transitive inputs.
    dependencies=[f'{value["name"]} {value["version"]} ({value["source"]})' for value in approved.values()]
    seed_lock=lockfile+'\n[[package]]\nname = '+quote(NAME)+'\nversion = "0.0.0"\ndependencies = [\n'
    seed_lock+=''.join('  '+quote(value)+',\n' for value in sorted(dependencies))+']\n'
    policy={'version':1,'kind':'isolated-external-inputs','packages':approved}
    return {'Cargo.toml':('\n'.join(manifest)+'\n').encode(),
            'src/lib.rs':EMPTY_SOURCE,
            'seed-lock.toml':seed_lock.encode(),
            'seed-inputs.json':json.dumps(policy,sort_keys=True,separators=(',',':')).encode()}


def verify_resolution(metadata,lockfile,policy,*,source_root='/workspace'):
    if (not isinstance(policy,dict) or set(policy)!={'version','kind','packages'}
            or type(policy['version']) is not int or policy['version']!=1
            or policy['kind']!='isolated-external-inputs' or not isinstance(policy['packages'],dict)):
        raise InputsUnknown()
    reusable=inputs.reusable_packages(metadata,source_root=source_root)
    expected=policy['packages'];locked=_lock(lockfile)
    if reusable.keys()!=expected.keys():raise InputsUnknown()
    nodes={node['id']:node for node in metadata['resolve']['nodes']}
    root=inputs._path(source_root)
    local_roots=0
    for package in metadata['packages']:
        key=package['id']
        if package['source'] is None:
            local_roots+=1;targets=package.get('targets')
            if (package['name']!=NAME or package.get('version')!='0.0.0'
                    or inputs._path(package['manifest_path'])!=root/'Cargo.toml'
                    or not isinstance(targets,list) or len(targets)!=1 or targets[0].get('kind')!=['lib']
                    or inputs._path(targets[0].get('src_path'))!=root/'src/lib.rs'
                    or set(nodes[key]['dependencies'])!=set(expected)):
                raise InputsUnknown()
            continue
        if key not in expected:raise InputsUnknown()
        wanted=expected[key]
        if (set(wanted)!={'name','version','source','checksum','features','dependencies'}
                or any(package.get(field)!=wanted[field] for field in ('name','version','source'))
                or sorted(nodes[key]['features'])!=wanted['features']
                or sorted(nodes[key]['dependencies'])!=wanted['dependencies']):
            raise InputsUnknown()
        row=locked.get((wanted['name'],wanted['version'],wanted['source']))
        if row is None or row.get('checksum')!=wanted['checksum']:raise InputsUnknown()
    if local_roots!=1:raise InputsUnknown()
    return True


def _source_identity(files):
    if (not isinstance(files,dict) or set(files)!=SOURCE_PATHS or files.get('src/lib.rs')!=EMPTY_SOURCE
            or any(not isinstance(data,bytes) or len(data)>16*1024*1024 for data in files.values())):
        raise InputsUnknown()
    def unique(pairs):
        value={}
        for key,item in pairs:
            if key in value:raise InputsUnknown()
            value[key]=item
        return value
    try:
        manifest=tomllib.loads(files['Cargo.toml'].decode())
        policy=json.loads(files['seed-inputs.json'],object_pairs_hook=unique)
        locked=_lock(files['seed-lock.toml'].decode())
    except (UnicodeError,ValueError,TypeError):raise InputsUnknown() from None
    if (set(manifest)!={'package','lib','workspace','dependencies'}
            or manifest['package']!={'name':NAME,'version':'0.0.0','edition':'2021','publish':False}
            or type(manifest['package'].get('publish')) is not bool
            or manifest['lib']!={'path':'src/lib.rs'} or manifest['workspace']!={'resolver':'2'}
            or not isinstance(manifest['dependencies'],dict)
            or not isinstance(policy,dict) or set(policy)!={'version','kind','packages'}
            or type(policy['version']) is not int or policy['version']!=1
            or policy['kind']!='isolated-external-inputs' or not isinstance(policy['packages'],dict)):
        raise InputsUnknown()
    expected=[]
    for package in policy['packages'].values():
        if (not isinstance(package,dict) or set(package)!={'name','version','source','checksum','features','dependencies'}
                or not inputs._text(package['name'],256) or not inputs._text(package['version'],128)
                or not isinstance(package['checksum'],str) or re.fullmatch(r'[0-9a-f]{64}',package['checksum']) is None
                or package['source']!=REGISTRY or not isinstance(package['features'],list)
                or any(not isinstance(feature,str) for feature in package['features'])
                or not isinstance(package['dependencies'],list)):
            raise InputsUnknown()
        row=locked.get((package['name'],package['version'],package['source']))
        if row is None or row.get('checksum')!=package['checksum']:raise InputsUnknown()
        expected.append({'package':package['name'],'version':'='+package['version'],
                         'default-features':False,'features':package['features']})
    actual=list(manifest['dependencies'].values())
    canonical=lambda value:json.dumps(value,sort_keys=True,separators=(',',':'))
    if sorted(map(canonical,actual))!=sorted(map(canonical,expected)):
        raise InputsUnknown()
    rows={name:hashlib.sha256(data).hexdigest() for name,data in sorted(files.items())}
    return hashlib.sha256(canonical(rows).encode()).hexdigest()


def write_source(root,files):
    identity=_source_identity(files);root=Path(root)
    if (not root.is_absolute() or root.resolve()!=root or not root.is_dir() or any(root.iterdir())
            or root.stat().st_uid!=os.getuid()):raise InputsUnknown()
    (root/'src').mkdir(mode=0o700)
    for name,data in files.items():
        fd=os.open(root/name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
        with os.fdopen(fd,'wb') as stream:
            stream.write(data);stream.flush();os.fsync(stream.fileno())
    for path in (root/'src',root):
        fd=os.open(path,os.O_RDONLY|os.O_DIRECTORY)
        try:os.fsync(fd)
        finally:os.close(fd)
    return identity


def verify_source(root,identity,*,owner_uid=None):
    root=Path(root);owner_uid=os.getuid() if owner_uid is None else owner_uid
    if (not root.is_absolute() or root.resolve()!=root or not root.is_dir()
            or {path.name for path in root.iterdir()}!={'Cargo.toml','src','seed-inputs.json','seed-lock.toml'}
            or (root/'src').is_symlink() or not (root/'src').is_dir()
            or {path.name for path in (root/'src').iterdir()}!={'lib.rs'}):raise InputsUnknown()
    files={}
    try:
        for name in SOURCE_PATHS:
            fd=os.open(root/name,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
            with os.fdopen(fd,'rb') as stream:
                info=os.fstat(stream.fileno())
                if (not stat.S_ISREG(info.st_mode) or info.st_uid!=owner_uid or info.st_nlink!=1
                        or info.st_mode&0o022 or info.st_size>16*1024*1024):raise InputsUnknown()
                files[name]=stream.read()
    except OSError:raise InputsUnknown() from None
    observed=_source_identity(files)
    if observed!=identity:raise InputsUnknown()
    return observed
