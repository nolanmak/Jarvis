#!/usr/bin/env python3
"""Recover an unexpectedly failed independent worker through the normal updater."""
import argparse
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import time

WORKER='augmentagent-autopr.service'
DAEMON='augmentagent.service'

class Manager:
    def properties(self,unit):
        fields=['ActiveState','Result','MainPID','UnitFileState','Job','FragmentPath','ExecStart']
        command=['systemctl','--user','show',unit]
        for field in fields:command+=['-p',field]
        output=subprocess.run(command,check=True,capture_output=True,text=True,timeout=15).stdout
        return dict(line.split('=',1) for line in output.splitlines() if '=' in line)
    def start(self):
        # Never replace an owner's conflicting stop job or reset failure limits.
        subprocess.run(['systemctl','--user','--job-mode=fail','start',WORKER],check=True,
                       capture_output=True,text=True,timeout=20)

def current_executable(repository,proc,pid):
    binary=repository/'target/release/augmentagent'
    expected=binary.lstat();actual=(proc/str(pid)/'exe').stat()
    return (stat.S_ISREG(expected.st_mode) and expected.st_uid==os.geteuid()
            and not expected.st_mode & 0o022
            and (expected.st_dev,expected.st_ino)==(actual.st_dev,actual.st_ino))

def host(repository,state,proc,manager):
    try:
        daemon=manager.properties(DAEMON);pid=int(daemon.get('MainPID','0'))
        if daemon.get('ActiveState')!='active' or pid<=1:return None
        marker_path=state/'autopr-host.json';meta=marker_path.lstat()
        if not (stat.S_ISREG(meta.st_mode) and meta.st_uid==os.geteuid() and meta.st_size<=4096):return None
        marker=json.loads(marker_path.read_text())
        if not isinstance(marker,dict):return None
        birth=(proc/str(pid)/'stat').read_text().rsplit(')',1)[1].split()[19]
        if marker.get('external') is not True or marker.get('pid')!=pid or marker.get('birth')!=birth:return None
        group=(proc/str(pid)/'cgroup').read_text()
        if not any(DAEMON in line.split('/') for line in group.splitlines()):return None
        return (pid,birth) if current_executable(repository,proc,pid) else None
    except (OSError,ValueError,IndexError):return None

def eligible(worker):
    return (worker.get('ActiveState')=='failed' and worker.get('Result')=='start-limit-hit'
            and worker.get('MainPID')=='0' and worker.get('UnitFileState')=='enabled'
            and worker.get('Job')=='')

def managed(repository,worker):
    try:
        fragment=Path(worker['FragmentPath']);meta=fragment.lstat()
        if not (stat.S_ISREG(meta.st_mode) and meta.st_uid==os.geteuid() and not meta.st_mode & 0o022
                and meta.st_nlink==1 and meta.st_size<=64*1024):return False
        if not fragment.read_text().startswith('# Managed by ensure-autopr-worker.sh\n'):return False
        match=re.match(r'^\{ path=(.*?) ; argv\[\]=(.*?) ; ',worker.get('ExecStart',''))
        launcher=str(repository/'scripts/run-rs.sh')
        return bool(match and Path(match[1]).resolve()==Path(launcher).resolve()
                    and match[2] in [launcher+' auto-pr-worker --dry-run '+dry for dry in ('true','false')])
    except (OSError,KeyError,ValueError):return False

def recover(repository, state, proc, manager):
    before=manager.properties(WORKER)
    if not eligible(before):return {'state':'not-eligible'}
    original_host=host(repository,state,proc,manager)
    if original_host is None or not managed(repository,before):return {'state':'held','reason':'worker-recovery-authority-unproven'}
    # Re-read both sides immediately before requesting the systemd transaction.
    # PID reuse, a mode transition, an owner stop, or a config change invalidates it.
    if manager.properties(WORKER)!=before or host(repository,state,proc,manager)!=original_host:
        return {'state':'held','reason':'worker-recovery-source-changed'}
    manager.start()
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        after=manager.properties(WORKER)
        try:
            pid=int(after.get('MainPID','0'))
            if after.get('ActiveState')=='active' and pid>1 and current_executable(repository,proc,pid):
                command=(proc/str(pid)/'cmdline').read_bytes().split(b'\0')
                group=(proc/str(pid)/'cgroup').read_text()
                if (b'auto-pr-worker' in command and any(WORKER in line.split('/') for line in group.splitlines())
                        and host(repository,state,proc,manager)==original_host):
                    return {'state':'recovered','pid':pid,'daemon_pid':original_host[0]}
        except (OSError,ValueError):pass
        if after.get('ActiveState')=='failed':break
        time.sleep(0.5)
    raise RuntimeError('worker recovery requested but running identity is unverified')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repository',type=Path,required=True)
    parser.add_argument('--state',type=Path,required=True)
    args=parser.parse_args()
    try:
        result=recover(args.repository,args.state,Path('/proc'),Manager())
        print(json.dumps(result))
        if result['state']=='held':raise SystemExit(3)
    except (OSError,RuntimeError,subprocess.SubprocessError):
        print(json.dumps({'state':'failed','reason':'worker-recovery-unverified'}))
        raise SystemExit(1)
