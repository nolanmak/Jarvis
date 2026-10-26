#!/usr/bin/env python3
"""Explicit 512MiB delegated cgroup reservation proof; no models or VMs.

Run only in an isolated transient service, never inside the production service:
 systemd-run --user --quiet --wait --pipe --collect \
   --unit=autoship-resource-reservation-proof \
   -p 'Delegate=cpu memory pids' -p DelegateSubgroup=daemon \
   -p MemoryMax=512M -p TasksMax=64 -p CPUQuota=100% \
   /usr/bin/python3 /absolute/path/to/this-script.py
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location('resources', Path(__file__).with_name('run-resource-scope.py'))
resources = importlib.util.module_from_spec(spec)
spec.loader.exec_module(resources)
MIB = 1024**2


def policy(mib):
    return {'memory_bytes': mib*MIB, 'pids': 8, 'cpu_percent': 20}


def worker(barrier):
    scope = None
    deadline = time.monotonic()+10
    while not barrier.exists():
        if time.monotonic()>deadline:
            raise RuntimeError('concurrent admission barrier timed out')
        time.sleep(.005)
    try:
        try:
            scope = resources.RunScope(policy(256))
        except RuntimeError as error:
            # Fixed controller diagnostics only; no model or credentials here.
            print(json.dumps({'admitted': False, 'reason': str(error)}), flush=True)
            return
        print(json.dumps({'admitted': True, 'path': str(scope.path),
                          'identity': scope.identity,
                          'current_bytes': int((scope.path/'memory.current').read_text())}), flush=True)
        # Parent retains the idle reservation until it explicitly releases it.
        sys.stdin.readline()
    finally:
        if scope is not None:
            scope.close()


def read_result(child):
    if not select.select([child.stdout], [], [], 15)[0]:
        raise RuntimeError('reservation contender did not return before proof deadline')
    line = child.stdout.readline()
    if not line:
        raise RuntimeError('reservation contender exited without a result')
    return json.loads(line)


def release(child):
    if child.poll() is None:
        child.stdin.close()  # EOF invokes the worker's verified scope cleanup.
    status = child.wait(timeout=10)
    if status != 0:
        raise RuntimeError('reservation worker cleanup failed')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--worker', type=Path)
    args=parser.parse_args()
    if args.worker:
        worker(args.worker)
        return
    leaf=resources.current_group()
    parent=leaf.parent
    if leaf.name!='daemon' or not parent.name.startswith('autoship-resource-reservation-proof'):
        parser.error('requires its own autoship-resource-reservation-proof transient unit')
    if resources.finite(parent/'memory.max') != 512*MIB:
        parser.error('proof requires the declared 512MiB unit ceiling')
    if list(parent.glob('run-*')):
        parser.error('proof requires an empty isolated run namespace')
    import tempfile
    children=[]
    baseline=None
    replacement=None
    with tempfile.TemporaryDirectory(prefix='resource-reservation-proof-') as temporary:
        try:
            baseline=resources.RunScope(policy(128))
            assert not (baseline.path/'cgroup.procs').read_text().strip()
            barrier=Path(temporary)/'start'
            for _ in range(2):
                children.append(subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),'--worker',str(barrier)],
                    stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=None,text=True))
            barrier.write_text('start')
            results=[read_result(child) for child in children]
            admitted=[i for i,row in enumerate(results) if row['admitted']]
            assert len(admitted)==1, 'concurrent reservations must admit exactly one contender'
            rejected=results[1-admitted[0]]
            assert rejected['reason']=='aggregate memory reservations exhausted', rejected
            winner=results[admitted[0]]
            assert winner['current_bytes']<8*MIB, 'idle guest should not need RAM pressure to reserve capacity'
            assert not (Path(winner['path'])/'cgroup.procs').read_text().strip()
            try:
                unexpected=resources.RunScope(policy(256))
            except RuntimeError as error:
                assert str(error)=='aggregate memory reservations exhausted', str(error)
            else:
                unexpected.close()
                raise AssertionError('idle sibling ceilings were overcommitted')
            release(children[admitted[0]])
            assert not Path(winner['path']).exists(), 'release must remove the verified-empty cgroup'
            replacement=resources.RunScope(policy(256))
            assert replacement.identity['reservation']['requested_bytes']==256*MIB
            assert replacement.identity['reservation']['reserved_before_bytes']>=128*MIB
            replacement.close();replacement=None
            baseline.close();baseline=None
            for child in children:
                release(child)
            assert not list(parent.glob('run-*')), 'proof left a reservation behind'
            print(json.dumps({'passed':True,'unit_memory_bytes':512*MIB,
                'idle_sibling_reservations':True,'concurrent_admissions':len(admitted),
                'release_allows_replacement':True,'remaining_scopes':0,
                'admission':winner['identity']['reservation']},sort_keys=True))
        finally:
            for child in children:
                if child.poll() is None:
                    release(child)
            if replacement is not None:
                replacement.close()
            if baseline is not None:
                baseline.close()


if __name__=='__main__':
    main()
