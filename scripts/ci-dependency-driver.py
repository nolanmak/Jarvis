#!/usr/bin/env python3
"""Unprivileged, controller-pinned resolve/compile commands for seed inputs.

The privileged guest controller validates resolution and freezes the source
between these two commands. No application code or configuration is present.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

spec=importlib.util.spec_from_file_location('driver_workspace',Path(__file__).with_name('ci-dependency-workspace.py'))
workspace=importlib.util.module_from_spec(spec);spec.loader.exec_module(workspace)


def resolve(identity,*,owner_uid=0):
    root=Path.cwd()
    workspace.verify_source(root,identity,owner_uid=owner_uid,allow_target=True)
    descriptor=os.open('Cargo.lock',os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
    with os.fdopen(descriptor,'wb') as stream:
        stream.write(Path('seed-lock.toml').read_bytes());stream.flush();os.fsync(stream.fileno())
    result=subprocess.run(['/toolchain/bin/cargo','metadata','--format-version','1'],
        stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    if result.returncode or len(result.stdout)>16*1024*1024:
        raise workspace.InputsUnknown()
    metadata=json.loads(result.stdout)
    lockfile=Path('Cargo.lock').read_text()
    policy=json.loads(Path('seed-inputs.json').read_bytes())
    workspace.verify_resolution(metadata,lockfile,policy,source_root=str(root))
    return {'version':1,'kind':'isolated-resolution','metadata':metadata,'lockfile':lockfile}


def compile_units(identity,*,owner_uid=0):
    workspace.verify_source(Path.cwd(),identity,owner_uid=owner_uid,allow_target=True,generated_lock=True)
    # Resolution has already downloaded and checked the complete graph. No
    # further network input may enter while producing these artifacts.
    process=subprocess.Popen(['/toolchain/bin/cargo','build','--lib','--offline','--locked','--keep-going','--message-format=json'],
        stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL)
    messages=[];total=0
    for line in process.stdout:
        total+=len(line)
        if total>32*1024*1024:
            process.terminate();process.wait();raise workspace.InputsUnknown()
        message=json.loads(line)
        if message.get('reason') in ('compiler-artifact','build-script-executed','build-finished'):
            messages.append(message)
        elif message.get('reason')!='compiler-message':
            process.terminate();process.wait();raise workspace.InputsUnknown()
    code=process.wait()
    return {'version':1,'kind':'isolated-compilation','messages':messages,'cargo_exit_code':code}


def main():
    parser=argparse.ArgumentParser();parser.add_argument('operation',choices=['resolve','compile'])
    parser.add_argument('--identity',required=True);args=parser.parse_args()
    try:
        value=resolve(args.identity) if args.operation=='resolve' else compile_units(args.identity)
        print(json.dumps(value,sort_keys=True));return 0
    except (workspace.InputsUnknown,OSError,ValueError,KeyError):
        print('dependency input census unavailable',file=sys.stderr);return 2
if __name__=='__main__':raise SystemExit(main())
