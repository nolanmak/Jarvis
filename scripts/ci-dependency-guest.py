#!/usr/bin/env python3
"""Privileged seed phases in the existing supervised, no-NIC CI VM."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil

BASE=Path(__file__).resolve().parent
def _load(name,file):
    spec=importlib.util.spec_from_file_location(name,BASE/file)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module
store=_load('guest_seed_store','ci-dependency-store.py')
workspace=_load('guest_seed_workspace','ci-dependency-workspace.py')
FreshnessHold=store.FreshnessHold


def _result(value,kind):
    try:result=json.loads(value['stdout'])
    except (ValueError,KeyError,TypeError):raise FreshnessHold('artifact_provenance_unknown') from None
    if value.get('exit_code')!=0 or not isinstance(result,dict) or result.get('version')!=1 or result.get('kind')!=kind:
        raise FreshnessHold('artifact_provenance_unknown')
    return result


def produce(action,binding,command,uid,gid,run_worker,bind_cache,hide_cache,phase=lambda _:None):
    phase('prepare_producer')
    if os.geteuid()!=0:raise FreshnessHold('invalidation_unsafe')
    identity=action['source_sha256'];producer=action['producer']
    try:workspace.verify_source(Path('/workspace'),identity,owner_uid=uid)
    except workspace.InputsUnknown:raise FreshnessHold('input_census_unknown') from None
    # The shared source stays read-only. Only a private generated workspace is
    # writable during resolution; its fixed files cannot be replaced by a worker.
    private=Path('/root/dependency-workspace');private.mkdir(mode=0o1777)
    os.chmod(private,0o1777);(private/'src').mkdir(mode=0o555)
    for name in workspace.SOURCE_PATHS:
        destination=private/name;shutil.copyfile(Path('/workspace')/name,destination)
        destination.chmod(0o444)
    mount=['/usr/bin/mount','--bind',str(private),'/workspace']
    import subprocess
    subprocess.run(mount,check=True)
    Path('/workspace/target').mkdir(mode=0o755)
    target,home=store.prepare_isolated_producer('/build-cache',binding,producer,command,uid,gid)
    phase('bind_producer_cache')
    bind_cache(target)
    phase('hide_producer_cache')
    cache=hide_cache()
    phase('resolve_dependencies')
    resolved=_result(run_worker(['/usr/bin/python3','-I','/ci-tools/ci-dependency-driver.py','resolve','--identity',identity]),'isolated-resolution')
    policy=json.loads(Path('/workspace/seed-inputs.json').read_bytes())
    phase('freeze_resolution')
    try:workspace.verify_resolution(resolved['metadata'],resolved['lockfile'],policy)
    except workspace.InputsUnknown:raise FreshnessHold('input_census_unknown') from None
    lock=Path('/workspace/Cargo.lock');info=lock.lstat()
    import stat
    if (not stat.S_ISREG(info.st_mode) or info.st_nlink!=1 or info.st_uid!=uid
            or lock.read_text()!=resolved['lockfile']):raise FreshnessHold('input_census_unknown')
    os.chown(lock,0,0);lock.chmod(0o444);Path('/workspace').chmod(0o555)
    phase('compile_dependencies')
    compiled=_result(run_worker(['/usr/bin/python3','-I','/ci-tools/ci-dependency-driver.py','compile','--identity',identity]),'isolated-compilation')
    phase('record_producer')
    store.record_isolated_output(cache,producer,command,resolved['metadata'],compiled['messages'])
    # The host must still prove whole-VM quiescence. This disposition is never
    # a passing required check or an acknowledgement for publication.
    return {'version':1,'kind':'produced','producer':producer,'command':command,
            'dependency_inputs_sha256':binding['dependency_inputs_sha256'],
            'resolved_lock_sha256':hashlib.sha256(resolved['lockfile'].encode()).hexdigest(),
            'cargo_exit_code':compiled['cargo_exit_code'],'shipping_authority':False}


def publish(action,binding):
    if os.geteuid()!=0:raise FreshnessHold('invalidation_unsafe')
    identity=store.publish('/build-cache',action['producer'],action['ack'])
    manifest=store.read_seed('/build-cache',identity,binding)
    return {'version':1,'kind':'published','seed':identity,'producer':action['producer'],
            'units':len(manifest['units']),'dependency_inputs_sha256':binding['dependency_inputs_sha256'],
            'shipping_authority':False}


def install(action,binding,target,home):
    if os.geteuid()!=0:raise FreshnessHold('invalidation_unsafe')
    identity=store.install('/build-cache',action['seed'],binding,target,home)
    return {'version':1,'kind':'installed','seed':identity,
            'dependency_inputs_sha256':binding['dependency_inputs_sha256'],'shipping_authority':False}
