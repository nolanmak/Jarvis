#!/usr/bin/env python3
"""Identity of trusted read-only VM runtime exports, separate from Git inputs."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import posixpath
import stat
import sys


class IdentityUnknown(ValueError):
    pass


def _encoded(value):
    return json.dumps(value,sort_keys=True,separators=(',',':')).encode()


def _metadata(info,readable):
    return [info.st_dev,info.st_ino,info.st_mode,info.st_uid,info.st_gid,info.st_size,
            info.st_mtime_ns,info.st_ctime_ns,readable]


def census(roots,*,content):
    content_hash=hashlib.sha256();metadata_hash=hashlib.sha256();files=0;total=0;nodes=0
    writes=('/workspace','/build-cache','/ci-cargo-home','/cargo','/tmp','/home/worker','/ci-runtime')
    def add(role,relative,kind,value,metadata):
        metadata_hash.update(_encoded([role,relative,kind,metadata])+b'\0')
        if content:content_hash.update(_encoded([role,relative,kind,value])+b'\0')
    def visit(role,path,relative,guest):
        nonlocal files,total,nodes
        nodes+=1
        if nodes>2_000_000:raise IdentityUnknown('runtime census exceeds bounds')
        before=path.lstat();mode=before.st_mode
        if role!='guest-runtime' and before.st_uid not in (0,os.geteuid()):raise IdentityUnknown('runtime owner unknown')
        if stat.S_ISLNK(mode):
            readable=True
            target=os.readlink(path)
            resolved=posixpath.normpath(posixpath.join(posixpath.dirname(guest),target))
            if any(resolved==prefix or resolved.startswith(prefix+'/') for prefix in writes):
                raise IdentityUnknown('runtime link enters worker writes')
            add(role,relative,'link',target,_metadata(before,True))
        elif stat.S_ISDIR(mode):
            # /usr is the existing read-only guest export, including shared
            # data directories such as system fonts. Runtime.load separately
            # validates the configured executable trust roots. Census these
            # data directories too; a host-side change invalidates the run.
            if role!='guest-runtime' and (mode&0o002 or (mode&0o020 and before.st_gid not in (0,os.getegid()))):
                raise IdentityUnknown('runtime directory is mutable')
            readable=os.access(path,os.R_OK|os.X_OK,effective_ids=True)
            add(role,relative,'directory',None,_metadata(before,readable))
            if readable:
                for child in sorted(path.iterdir(),key=lambda child:child.name):
                    name=child.name if not relative else relative+'/'+child.name
                    visit(role,child,name,posixpath.join(guest,child.name))
        elif stat.S_ISREG(mode):
            if role!='guest-runtime' and (mode&0o002 or (mode&0o020 and before.st_gid not in (0,os.getegid()))):
                raise IdentityUnknown('runtime file is mutable')
            readable=os.access(path,os.R_OK,effective_ids=True);value='unavailable'
            if readable and content:
                digest=hashlib.sha256()
                descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
                with os.fdopen(descriptor,'rb') as stream:
                    if _metadata(os.fstat(stream.fileno()),True)!=_metadata(before,True):
                        raise IdentityUnknown('runtime input replaced')
                    while block:=stream.read(1024*1024):digest.update(block)
                    if _metadata(os.fstat(stream.fileno()),True)!=_metadata(before,True):
                        raise IdentityUnknown('runtime input changed')
                value=digest.hexdigest()
            files+=1;total+=before.st_size
            if total>2*1024**4:raise IdentityUnknown('runtime input bytes exceed bounds')
            add(role,relative,'file',value,_metadata(before,readable))
        else:raise IdentityUnknown('unsupported runtime input')
        after_readable=True if stat.S_ISLNK(mode) else os.access(path,os.R_OK|os.X_OK if stat.S_ISDIR(mode) else os.R_OK,effective_ids=True)
        if _metadata(path.lstat(),after_readable)!=_metadata(before,readable):
            raise IdentityUnknown('runtime changed during census')
    for role,raw in sorted(roots.items()):
        path=Path(raw)
        if not path.is_absolute() or path.resolve()!=path:raise IdentityUnknown('runtime root is not literal')
        guest='/usr' if role=='guest-runtime' else '/toolchain' if role=='toolchain' else '/runtime/'+role
        visit(role,path,'',guest)
    return {'version':1,'content_sha256':content_hash.hexdigest() if content else None,
            'metadata_sha256':metadata_hash.hexdigest(),'files':files,'bytes':total}


def runtime_roots(config):
    roots={'guest-runtime':Path('/usr')}
    for key in ('qemu','kernel','busybox','firmware','data_dir','library_dir','module_dir','toolchain'):
        if key in config:roots[key]=Path(config[key]).resolve(strict=True)
    for index,path in enumerate(config['modules']):roots['module-'+str(index)]=Path(path).resolve(strict=True)
    # A complete parent export already covers its configured children.
    return {key:path for key,path in roots.items() if not any(
        other!=key and path!=parent and path.is_relative_to(parent) for other,parent in roots.items())}


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--config',type=Path,required=True)
    parser.add_argument('--metadata-only',action='store_true');args=parser.parse_args()
    spec=importlib.util.spec_from_file_location('build_vm',Path(__file__).with_name('codex-build-vm.py'))
    vm=importlib.util.module_from_spec(spec);spec.loader.exec_module(vm)
    try:
        runtime=vm.Runtime.load(args.config)
        print(json.dumps(census(runtime_roots(runtime.config),content=not args.metadata_only),sort_keys=True));return 0
    except (IdentityUnknown,OSError,ValueError):
        print('trusted runtime identity unavailable',file=sys.stderr);return 2


if __name__=='__main__':raise SystemExit(main())
