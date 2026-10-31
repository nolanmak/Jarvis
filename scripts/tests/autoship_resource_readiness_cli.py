#!/usr/bin/env python3
"""Packaged CLI resource observation in a disposable delegated 8GiB service.

Pass the managed debug CLI path. No providers, VMs or GitHub operations execute.
"""
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
spec=importlib.util.spec_from_file_location('resources',Path(__file__).parents[1]/'run-resource-scope.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
parent=m.current_group().parent
assert parent.name.startswith('autoship-1703-resource-cli-'), 'requires a disposable dedicated test service'
assert m.finite(parent/'memory.max')==8*1024**3
binary=Path(sys.argv[1]).resolve(strict=True)
with tempfile.TemporaryDirectory(prefix='autoship-resource-cli-') as temporary:
    root=Path(temporary);guest=root/'guest';guest.mkdir(mode=0o700)
    artifact=guest/'artifact';artifact.write_bytes(b'fixture');artifact.chmod(0o600)
    (guest/'manifest.json').write_text(json.dumps({'version':1,'files':[{'path':'artifact','sha256':hashlib.sha256(b'fixture').hexdigest()}]}))
    runtime=root/'runtime.json';runtime.write_text(json.dumps(dict(qemu=str(artifact),kernel=str(artifact),busybox=str(artifact),firmware=str(artifact),
        data_dir=str(guest),library_dir=str(guest),module_dir=str(guest),modules=[str(artifact)],memory_mb=512)));runtime.chmod(0o600)
    policy=root/'policy.json';policy.write_text(json.dumps(dict(version=1,runtime_config=str(runtime),guest_runtime=str(guest),
        state_root=str(root/'state'),memory_mb=512,max_state_bytes=8589934592,max_sessions=16,providers={})));policy.chmod(0o600)
    env={'PATH':'/usr/bin:/bin','HOME':str(root),'XDG_STATE_HOME':str(root/'state'),
        'AUGMENTAGENT_NATIVE_VM_POLICY':str(policy),'AUGMENTAGENT_RUN_RESOURCES':'1'}
    results=[]
    for initialized in (False,True):
        if initialized:
            scope=m.RunScope({'memory_bytes':128*1024**2,'pids':8,'cpu_percent':20});scope.close()
        before=((parent/'cgroup.subtree_control').read_text(),set(parent.glob('run-*')))
        output=subprocess.run([str(binary),'autopr','readiness','--json'],env=env,capture_output=True,text=True,timeout=10)
        assert output.returncode==3, output.stderr
        report=json.loads(output.stdout)
        expected='available' if initialized else 'capacity-unverified'
        assert report['resources']['status']==expected,report['resources']
        assert report['resources']['permits_attempt'] is True
        assert report['ready'] is False and report['resources']['live_verified'] is False
        assert before==((parent/'cgroup.subtree_control').read_text(),set(parent.glob('run-*')))
        assert not (root/'state').exists()
        results.append({'status':expected,'created_state':False,'changed_cgroups':False,'ready':False})
    print(json.dumps({'packaged_cli_resource_observations':results,'live_provider_calls':0,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}))
