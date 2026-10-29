#!/usr/bin/env python3
"""Prove active auto-ship locks belong to the independent worker, not the daemon."""
import argparse
import fcntl
import json
from pathlib import Path

def birth(path):
    return path.read_text().rsplit(')',1)[1].split()[19]

def isolated(state, proc, daemon, worker, locks=None):
    try:
        if daemon <= 1 or worker <= 1 or daemon == worker:
            return False
        host=json.loads((state/'autopr-host.json').read_text())
        if host.get('external') is not True or host.get('pid') != daemon:
            return False
        if host.get('birth') != birth(proc/str(daemon)/'stat'):
            return False
        worker_proc=proc/str(worker)
        if b'auto-pr-worker' not in (worker_proc/'cmdline').read_bytes().split(b'\0'):
            return False
        group=(worker_proc/'cgroup').read_text()
        if not any('augmentagent-autopr.service' in line.split('/') for line in group.splitlines()) or group==(proc/str(daemon)/'cgroup').read_text():
            return False
        for lock in locks if locks is not None else [state/'self-improve.lock',state/'self-improve-resume.lock']:
            if not lock.exists(): continue
            with lock.open('rb') as handle:
                try:
                    fcntl.flock(handle,fcntl.LOCK_EX|fcntl.LOCK_NB)
                    fcntl.flock(handle,fcntl.LOCK_UN)
                    continue
                except BlockingIOError:
                    pass
            meta=lock.stat()
            owned=False
            for fd in (worker_proc/'fd').iterdir():
                try:
                    actual=fd.stat()
                    if (actual.st_dev,actual.st_ino)!=(meta.st_dev,meta.st_ino): continue
                    for line in (worker_proc/'fdinfo'/fd.name).read_text().splitlines():
                        fields=line.split()
                        if (len(fields)>5 and fields[0]=='lock:' and fields[2]=='FLOCK'
                            and fields[4]=='WRITE' and int(fields[5])==worker):
                            owned=True
                except (OSError,ValueError): continue
            if not owned:return False
        return True
    except (OSError,ValueError,KeyError,IndexError):
        return False

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state',type=Path,required=True)
    parser.add_argument('--proc',type=Path,default=Path('/proc'))
    parser.add_argument('--daemon',type=int,required=True)
    parser.add_argument('--worker',type=int,required=True)
    parser.add_argument('--lock',type=Path,action='append')
    args=parser.parse_args()
    raise SystemExit(0 if isolated(args.state,args.proc,args.daemon,args.worker,args.lock) else 1)
