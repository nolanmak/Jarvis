#!/usr/bin/env python3
"""Guest-only worker boundary using the existing descendant supervisor.

The controller keeps the cleanup receipt outside worker writes. This boundary
precedes input capture; it does not replace whole-VM host quiescence/storage
admission and grants no CI verdict or merge authority.
"""
import hashlib
import importlib.util
import os
from pathlib import Path
import stat
import sys
import uuid

_spec=importlib.util.spec_from_file_location('ci_scope_cache',Path(__file__).with_name('ci-cache-generation.py'))
_cache=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(_cache)
FreshnessHold=_cache.FreshnessHold

_DROP=r'''
import ctypes,os,sys
uid,gid=int(sys.argv[1]),int(sys.argv[2])
if uid<=0 or gid<=0 or not sys.argv[3:]:raise SystemExit(125)
libc=ctypes.CDLL(None,use_errno=True)
if libc.prctl(38,1,0,0,0)!=0:raise SystemExit(125)
if os.geteuid()==0:
    os.setgroups([]);os.setgid(gid);os.setuid(uid)
elif os.getresuid()!=(uid,uid,uid) or os.getresgid()!=(gid,gid,gid):raise SystemExit(125)
os.umask(0o022)
os.execvpe(sys.argv[3],sys.argv[3:],os.environ)
'''


class StoppedWorkers:
    def __init__(self,token,scope):
        if token is not _TOKEN:raise FreshnessHold('cleanup_uncertain')
        self._scope=scope

    def matches(self,scope):
        return self._scope is scope and scope.owner==os.getpid() and scope.closed

    def observation(self):
        return {'version':1,'kind':'ci-guest-workers-quiescent','state':'all_descendants_reaped',
                'scope_id':self._scope.directory.name.removeprefix('worker-'),
                'receipt_sha256':self._scope.receipt_sha256,'shipping_authority':False}


_TOKEN=object()


def validate_quiescence(value,*,require_worker):
    try:
        if (not isinstance(value,dict) or set(value)!={'version','kind','worker_free','scopes','shipping_authority'}
                or type(value['version']) is not int or value['version']!=1 or value['kind']!='ci-guest-command-quiescence'
                or value['worker_free'] is not True or value['shipping_authority'] is not False
                or not isinstance(value['scopes'],list) or len(value['scopes'])>8 or require_worker and not value['scopes']):
            raise FreshnessHold('cleanup_uncertain')
        seen=set()
        for scope in value['scopes']:
            if (not isinstance(scope,dict) or set(scope)!={'version','kind','state','scope_id','receipt_sha256','shipping_authority'}
                    or type(scope['version']) is not int or scope['version']!=1 or scope['kind']!='ci-guest-workers-quiescent'
                    or scope['state']!='all_descendants_reaped' or scope['shipping_authority'] is not False
                    or not _cache._uuid(scope['scope_id']) or scope['scope_id'] in seen
                    or scope['receipt_sha256']!=hashlib.sha256(b'all-descendants-reaped\n').hexdigest()):
                raise FreshnessHold('cleanup_uncertain')
            seen.add(scope['scope_id'])
        return value
    except (ValueError,TypeError,KeyError):raise FreshnessHold('cleanup_uncertain') from None


class WorkerScope:
    def __init__(self,control,uid,gid,argv):
        self.owner=os.getpid();self.closed=False;self.receipt_sha256=None
        self._process=None;self._observation_started=False;control=Path(control)
        try:
            info=control.lstat()
            if (not control.is_absolute() or control.resolve()!=control or not stat.S_ISDIR(info.st_mode)
                    or info.st_uid!=os.geteuid() or info.st_mode&0o077
                    or type(uid) is not int or uid<=0 or type(gid) is not int or gid<=0
                    or not isinstance(argv,list) or not argv or len(argv)>4096
                    or any(not isinstance(arg,str) or '\0' in arg or len(arg)>1024*1024 for arg in argv)):
                raise FreshnessHold('cleanup_uncertain')
            self.directory=control/('worker-'+str(uuid.uuid4()));self.directory.mkdir(mode=0o700)
            self.receipt=self.directory/'quiescent'
            self.argv=[sys.executable,'-I',str(Path(__file__).with_name('provider-supervisor.py')),str(self.receipt),
                       sys.executable,'-I','-c',_DROP,str(uid),str(gid),*argv]
        except (OSError,ValueError):raise FreshnessHold('cleanup_uncertain') from None

    def attach(self,process):
        if self.owner!=os.getpid() or self._process is not None or self.closed:
            raise FreshnessHold('cleanup_uncertain')
        self._process=process

    def inherited_fds(self):return ()

    def controller_environment(self):return None

    def begin_observation(self):
        if self.owner!=os.getpid() or self.closed or self._process is not None or self._observation_started:
            raise FreshnessHold('cleanup_uncertain')
        self._observation_started=True

    def verify(self):
        try:
            if self.owner!=os.getpid() or self.closed or self._process is None or self._process.poll() is None:
                raise FreshnessHold('cleanup_uncertain')
            fd=os.open(self.receipt,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NONBLOCK)
            with os.fdopen(fd,'rb') as stream:
                info=os.fstat(stream.fileno());value=stream.read(64)
            if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid() or info.st_nlink!=1
                    or stat.S_IMODE(info.st_mode)!=0o600 or value!=b'all-descendants-reaped\n'):
                raise FreshnessHold('cleanup_uncertain')
            self.receipt_sha256=hashlib.sha256(value).hexdigest();self.closed=True
            return StoppedWorkers(_TOKEN,self)
        except (OSError,ValueError):raise FreshnessHold('cleanup_uncertain') from None


_CONTROLLER_TOKEN=object()


class _ControllerScope(WorkerScope):
    def __init__(self,token,control,configuration,kind):
        if kind not in ('probe','execution','repository-execution') or token is not _CONTROLLER_TOKEN or os.getresuid()!=(0,0,0) or os.getresgid()!=(0,0,0):
            raise FreshnessHold('invalidation_unsafe')
        control=Path(control);info=control.lstat()
        if control.resolve()!=control or not stat.S_ISDIR(info.st_mode) or info.st_uid!=0 or info.st_mode&0o077:
            raise FreshnessHold('invalidation_unsafe')
        self.owner=os.getpid();self.closed=False;self.receipt_sha256=None;self._process=None;self._observation_started=False
        self.directory=control/('worker-'+str(uuid.uuid4()));self.directory.mkdir(mode=0o700)
        self.receipt=self.directory/'quiescent'
        spec=importlib.util.spec_from_file_location('admitted_planner_probe',Path(__file__).with_name('ci-cargo-planner.py'))
        planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)
        planner._source(configuration['target'],configuration['binding'],configuration['command'])
        value=dict(configuration,version=3,kind=kind+'-supervisor',parent=os.getpid(),receipt=str(self.receipt))
        self._bootstrap=planner._sealed(value)
        self.argv=[sys.executable,'-I',str(Path(__file__).with_name('ci-cargo-planner.py')),'--'+kind+'-supervisor',str(self._bootstrap)]

    def inherited_fds(self):
        if self.owner!=os.getpid() or self._bootstrap is None:raise FreshnessHold('cleanup_uncertain')
        return (self._bootstrap,)

    def controller_environment(self):
        if self.owner!=os.getpid():raise FreshnessHold('cleanup_uncertain')
        return {'PATH':os.defpath}

    def attach(self,process):
        super().attach(process)
        os.close(self._bootstrap);self._bootstrap=None


def controller_probe_scope(control,configuration):
    """Fixed measured probe only; ordinary WorkerScope still refuses root."""
    return _ControllerScope(_CONTROLLER_TOKEN,control,configuration,'probe')


def controller_execution_scope(control,configuration):
    """Pinned Cargo executor only; the root helper never execs candidate argv."""
    return _ControllerScope(_CONTROLLER_TOKEN,control,configuration,'execution')


def controller_repository_scope(control,configuration):
    return _ControllerScope(_CONTROLLER_TOKEN,control,configuration,'repository-execution')
