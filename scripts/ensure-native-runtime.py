#!/usr/bin/env python3
"""Reconcile only an explicitly selected native runtime; never change grants."""
import importlib.util
import argparse
import json
import os
import shlex
import shutil
import stat
import subprocess
import sys
from pathlib import Path

spec = importlib.util.spec_from_file_location('native_provision', Path(__file__).with_name('provision-native-vm-probe.py'))
provision = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provision)


def selected_policy(selection):
    try:
        fd=os.open(selection,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    with os.fdopen(fd,'r') as stream:
        info=os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid() or info.st_mode & 0o077 or info.st_nlink!=1 or info.st_size>65536:
            raise ValueError('native runtime selection must be owner-private and bounded')
        entries=[]
        for line in stream.read(65537).splitlines():
            entries.extend(shlex.split(line,comments=True))
    if len(entries)!=1 or not entries[0].startswith('AUGMENTAGENT_NATIVE_VM_POLICY='):
        raise ValueError('invalid explicit native runtime selection')
    value=entries[0].split('=',1)[1]
    if not Path(value).is_absolute() or '\n' in value or '\r' in value:
        raise ValueError('native runtime selection must be an absolute single-line path')
    return Path(value)


def resolve_executable(environment):
    program=environment.get('CODEX_CLI') or 'codex'
    path=Path(program) if Path(program).is_absolute() else shutil.which(program,path=environment.get('PATH','/usr/bin:/bin'))
    if not path:
        raise ValueError('selected native executable unavailable')
    return Path(path).resolve(strict=True)


def service_executable():
    # Read only the two values used to resolve this executable. Never print a
    # process environment or set credential environment variables.
    for unit in ('augmentagent-autopr.service','augmentagent.service'):
        result=subprocess.run(['systemctl','--user','show',unit,'-p','MainPID','--value'],capture_output=True,text=True,timeout=5)
        pid=result.stdout.strip()
        if result.returncode or not pid.isdigit() or int(pid)==0:
            continue
        proc=Path('/proc')/pid
        if proc.stat().st_uid!=os.geteuid():
            raise ValueError('native runtime service owner differs')
        with (proc/'environ').open('rb') as stream:
            data=stream.read(4*1024*1024+1)
        if len(data)>4*1024*1024:
            raise ValueError('native runtime service environment exceeds bound')
        environment={}
        for entry in data.split(b'\0'):
            key,separator,value=entry.partition(b'=')
            if separator and key in (b'PATH',b'CODEX_CLI'):
                environment[key.decode()]=os.fsdecode(value)
        return resolve_executable(environment)
    return resolve_executable(os.environ)


def reconcile(selection, native=None):
    policy=selected_policy(selection)
    if policy is None:
        return 'not-selected'
    baseline=provision.helper('native-provider-vm.py').private_bytes(policy,2*1024**2)
    declaration=json.loads(baseline)
    if not set(declaration.get('providers',{})).intersection(('codex','qwen','glm')):
        return 'no-codex-runtime'
    native=Path(native).resolve(strict=True) if native else service_executable()
    provision.refresh_runtime_policy(policy,native,only_if_needed=True)
    return 'current' if policy.read_bytes()==baseline else 'refreshed'


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    default=Path(os.environ.get('XDG_CONFIG_HOME',Path.home()/'.config'))/'augmentagent/autoship-native.env'
    parser.add_argument('--selection-file',type=Path,default=default)
    parser.add_argument('--native-executable',type=Path)
    args=parser.parse_args()
    try:
        status=reconcile(args.selection_file,args.native_executable)
    except Exception as error:
        print('native-runtime-reconciliation-failed: '+type(error).__name__+': '+str(error),file=sys.stderr)
        return 1
    if status not in ('not-selected','no-codex-runtime'):
        print('native-runtime-'+status)
    return 0


if __name__=='__main__':
    raise SystemExit(main())
