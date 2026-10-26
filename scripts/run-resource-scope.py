#!/usr/bin/env python3
"""Controller-owned descendant ceilings inside a delegated service hierarchy.

No wall-clock deadline and no network policy are implied. The supervising
process stays in DelegateSubgroup=daemon; only its child enters run-*.
"""
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import time
import uuid

CONTROLLERS = {'cpu', 'memory', 'pids'}


class AdmissionRefused(RuntimeError):
    def __init__(self, code, message, **facts):
        super().__init__(message)
        self.code = code
        self.facts = facts


def refusal_receipt(error):
    """Never serialize error strings, filenames, policy bodies or traceback."""
    receipt={'phase':'admission_failed','exhausted':True,'refusal_code':'admission_setup_failed'}
    codes={'host_headroom','aggregate_reservations','ancestor_headroom','declared_ceiling','invalid_policy'}
    fields={'requested_bytes','reserved_bytes','current_bytes','limit_bytes','available_bytes',
            'reservation_slack_bytes','margin_bytes','required_bytes','requested_pids','limit_pids',
            'requested_cpu_percent','limit_cpu_quota','limit_cpu_period','memory_bytes','pids',
            'cpu_percent','minimum','maximum'}
    if isinstance(error,AdmissionRefused) and error.code in codes:
        receipt['refusal_code']=error.code
        receipt['facts']={key:value for key,value in error.facts.items()
                          if key in fields and type(value) is int and 0<=value<=2**64-1}
    else:
        category=type(error).__name__
        receipt['failure_class']=category if category in {
            'RuntimeError','ValueError','TypeError','JSONDecodeError','OSError','FileNotFoundError',
            'PermissionError','BlockingIOError','NotADirectoryError','TimeoutError'} else 'BoundaryFailure'
        if isinstance(error,OSError) and type(error.errno) is int and 0<=error.errno<=4095:
            receipt['errno']=error.errno
    return receipt


def current_group():
    rows = Path('/proc/self/cgroup').read_text().splitlines()
    paths = [row[3:] for row in rows if row.startswith('0::/')]
    if len(paths) != 1:
        raise RuntimeError('unified cgroup membership is unavailable')
    return Path('/sys/fs/cgroup') / paths[0].lstrip('/')


def finite(path):
    value = path.read_text().strip()
    if not value.isdigit() or int(value) <= 0:
        raise RuntimeError('aggregate resource ceiling is not finite')
    return int(value)


def counters(path):
    return {key: int(value) for key, value in
            (line.split() for line in path.read_text().splitlines())}


def available_memory():
    for line in Path('/proc/meminfo').read_text().splitlines():
        if line.startswith('MemAvailable:'):
            return int(line.split()[1]) * 1024
    raise RuntimeError('host available memory unavailable')


def headroom(parent, requested, host_available):
    # Reserve each admitted run's full ceiling, including idle preflight runs.
    # Unremoved groups remain reserved until verified descendant cleanup.
    reserved = 0
    current = 0
    for child in parent.iterdir():
        if not child.is_dir():
            continue
        try:
            usage = int((child / 'memory.current').read_text().strip())
            if child.name.startswith('run-'):
                reserved += finite(child / 'memory.max')
                current += usage
            else:
                reserved += usage
                current += usage
        except FileNotFoundError:
            if child.exists():
                raise RuntimeError('aggregate sibling resource accounting unavailable')
    aggregate_current = int((parent / 'memory.current').read_text().strip())
    # Include kernel charges not visible in child accounting.
    reserved += max(0, aggregate_current - current)
    needed = reserved + requested
    limit=finite(parent / 'memory.max')
    if needed > limit:
        raise AdmissionRefused('aggregate_reservations','aggregate memory reservations exhausted',
                               requested_bytes=requested,reserved_bytes=reserved,current_bytes=aggregate_current,limit_bytes=limit)
    slack = max(0, reserved - aggregate_current)
    if requested + slack + 256 * 1024**2 > host_available:
        raise AdmissionRefused('host_headroom','host memory headroom unavailable',
                               requested_bytes=requested,reservation_slack_bytes=slack,margin_bytes=256*1024**2,
                               required_bytes=requested+slack+256*1024**2,available_bytes=host_available)
    ancestor = parent.parent
    while ancestor != Path('/sys/fs/cgroup') and ancestor.is_relative_to('/sys/fs/cgroup'):
        limit = (ancestor / 'memory.max').read_text().strip()
        if limit != 'max':
            usage=int((ancestor / 'memory.current').read_text().strip())
            if requested + slack > int(limit) - usage:
                raise AdmissionRefused('ancestor_headroom','ancestor memory headroom unavailable',
                                       requested_bytes=requested,reservation_slack_bytes=slack,current_bytes=usage,limit_bytes=int(limit))
        ancestor = ancestor.parent
    return {'reserved_before_bytes': reserved, 'requested_bytes': requested,
            'host_available_bytes': host_available, 'host_margin_bytes': 256 * 1024**2}


@contextlib.contextmanager
def reserve(parent, requested):
    # cgroup directory inode is a common controller-only lock; no writable
    # sidecar or age/PID based cleanup can release another live reservation.
    descriptor = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        # systemd delegates controllers but an unused subtree need not have
        # enabled them yet. Child memory.current appears only after enablement.
        # Do this inside the same admission lock, before reading sibling usage.
        (parent/'cgroup.subtree_control').write_text('+cpu +memory +pids')
        if not CONTROLLERS <= set((parent/'cgroup.subtree_control').read_text().split()):
            raise RuntimeError('delegated controllers failed readback')
        yield headroom(parent, requested, available_memory())
    finally:
        os.close(descriptor)


class RunScope:
    def __init__(self, policy):
        if set(policy) != {'memory_bytes', 'pids', 'cpu_percent'}:
            raise AdmissionRefused('invalid_policy','invalid declared run resource policy')
        for key, low, high in [('memory_bytes', 64*1024*1024, 64*1024**3),
                               ('pids', 8, 4096), ('cpu_percent', 1, 1600)]:
            if type(policy[key]) is not int or not low <= policy[key] <= high:
                raise AdmissionRefused('invalid_policy','invalid declared run resource limit', **{key:policy[key],'minimum':low,'maximum':high})
        leaf = current_group()
        parent = leaf.parent
        if (leaf.name != 'daemon' or not parent.name.endswith('.service')
                or parent.resolve() != parent
                or parent.stat().st_uid != os.getuid()
                or parent.joinpath('cgroup.procs').read_text().strip()):
            raise RuntimeError('run resources require delegated service with daemon leaf')
        if not CONTROLLERS <= set((parent/'cgroup.controllers').read_text().split()):
            raise RuntimeError('required delegated resource controllers unavailable')
        memory = finite(parent/'memory.max')
        pids = finite(parent/'pids.max')
        cpu = (parent/'cpu.max').read_text().split()
        if len(cpu) != 2 or not all(v.isdigit() for v in cpu):
            raise RuntimeError('aggregate CPU quota is not finite')
        quota, period = map(int, cpu)
        if (policy['memory_bytes'] > memory or policy['pids'] > pids
                or policy['cpu_percent'] * period > quota * 100):
            raise AdmissionRefused('declared_ceiling','run ceiling exceeds aggregate service ceiling',
                                   requested_bytes=policy['memory_bytes'],limit_bytes=memory,requested_pids=policy['pids'],
                                   limit_pids=pids,requested_cpu_percent=policy['cpu_percent'],limit_cpu_quota=quota,limit_cpu_period=period)
        with reserve(parent, policy['memory_bytes']) as reservation:
            self.path = parent / ('run-' + uuid.uuid4().hex)
            self.path.mkdir(mode=0o700)
            self.closed = False
            self.fd = None
            try:
                desired = {'memory.max': str(policy['memory_bytes']), 'memory.swap.max': '0',
                           'memory.oom.group': '1', 'pids.max': str(policy['pids']),
                           'cpu.max': f"{policy['cpu_percent'] * 1000} 100000"}
                for name, value in desired.items():
                    (self.path/name).write_text(value)
                actual = {name: (self.path/name).read_text().strip() for name in desired}
                if actual != desired:
                    raise RuntimeError('run resource limits failed readback')
                self.identity = {'host_sha256': hashlib.sha256(Path('/etc/machine-id').read_bytes()).hexdigest(),
                                 'boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                                 'path': str(self.path), 'inode': self.path.stat().st_ino,
                                 'limits': actual, 'reservation': reservation, 'aggregate': {'path': str(parent),
                                 'memory_bytes': memory, 'pids': pids, 'cpu_max': ' '.join(cpu)}}
                self.fd = os.open(self.path/'cgroup.procs', os.O_WRONLY | os.O_CLOEXEC)
            except BaseException:
                self.close()
                raise

    def attach_child(self):
        # preexec_fn in a single-threaded trusted supervisor. No model code has
        # executed yet; forked and setsid descendants inherit this membership.
        os.write(self.fd, str(os.getpid()).encode('ascii'))
        os.close(self.fd)

    def observation(self):
        memory = counters(self.path/'memory.events')
        pids = counters(self.path/'pids.events')
        cpu = counters(self.path/'cpu.stat')
        return {'identity': self.identity, 'memory_events': memory,
                'pids_events': pids, 'cpu_stat': cpu,
                'exhausted': bool(memory.get('oom', 0) or memory.get('oom_kill', 0)
                                  or pids.get('max', 0))}

    def close(self):
        if self.closed:
            return
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
        if self.path.exists():
            (self.path/'cgroup.kill').write_text('1')
            deadline = time.monotonic() + 3
            while counters(self.path/'cgroup.events').get('populated'):
                if time.monotonic() >= deadline:
                    raise RuntimeError('resource subtree cleanup is unverified')
                time.sleep(0.01)
            self.path.rmdir()
        self.closed = True


def declared_scope():
    raw = os.environ.pop('JARVIS_RUN_RESOURCE_POLICY', None)
    return RunScope(json.loads(raw)) if raw is not None else None


def cleanup_receipt(receipt):
    """Controller fallback if a supervisor died before acknowledging cleanup."""
    import stat
    descriptor=os.open(receipt,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    with os.fdopen(descriptor) as stream:
        info=os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink!=1
                or info.st_uid!=os.getuid() or info.st_mode&0o077 or info.st_size>65536):
            raise RuntimeError('invalid resource cleanup receipt')
        receipt_body=json.load(stream)
        identity=receipt_body.get('identity')
    if identity is None:
        if receipt_body.get('phase')!='admission_failed':
            raise RuntimeError('resource cleanup identity missing')
        return
    leaf=current_group()
    path=Path(identity['path'])
    if (leaf.name!='daemon' or path.parent!=leaf.parent
            or not path.name.startswith('run-') or len(path.name)!=36
            or any(c not in '0123456789abcdef' for c in path.name[4:])):
        raise RuntimeError('resource cleanup outside controller service')
    if not path.exists():
        return
    if path.resolve()!=path or path.stat().st_ino!=identity['inode']:
        raise RuntimeError('resource identity changed before cleanup')
    (path/'cgroup.kill').write_text('1')
    deadline=time.monotonic()+3
    while counters(path/'cgroup.events').get('populated'):
        if time.monotonic()>=deadline:
            raise RuntimeError('resource cleanup is unverified')
        time.sleep(0.01)
    path.rmdir()


if __name__=='__main__':
    import sys
    if len(sys.argv)!=3 or sys.argv[1]!='--cleanup':
        raise SystemExit('expected --cleanup receipt')
    cleanup_receipt(sys.argv[2])
