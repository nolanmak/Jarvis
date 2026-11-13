#!/usr/bin/env python3
"""Explicit owner capacity increases for the currently deployed legacy store.

Preview is read-only. Applying requires both reviewed hashes. This never grants
execution, changes per-session reservations, prunes state or migrates bindings.
"""
import argparse
from contextlib import ExitStack,closing
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import sqlite3
import stat
import time


class Hold(Exception):pass
class PublicationUnknown(Hold):pass


def close_descriptor(fd):
    """Release a descriptor without replacing an established publication outcome."""
    try:os.close(fd)
    except OSError:pass


def require(value,reason):
    """Refuse an unsafe operation with a bounded public reason code."""
    if not value:raise Hold(reason)


def identity(info):
    """Capture inode, owner and link metadata for later object-identity checks."""
    return (info.st_dev,info.st_ino,info.st_uid,info.st_gid,info.st_mode,info.st_nlink)
def digest(data):
    """Fingerprint exact policy bytes without returning their private contents."""
    return hashlib.sha256(data).hexdigest()


def owned(info,directory=False,limit=2*1024**2):
    """Check the expected private owner, node kind, link count and size bound."""
    return ((stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode))
        and info.st_uid==os.geteuid() and not info.st_mode&0o077
        and (directory or info.st_nlink==1 and info.st_size<=limit))


def private(parent,name):
    """Read one pinned private file and refuse link, size or identity changes."""
    descriptor=os.open(name,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK,dir_fd=parent)
    with os.fdopen(descriptor,'rb') as stream:
        before=os.fstat(stream.fileno());require(owned(before),'policy-not-private')
        data=stream.read(2*1024**2+1);after=os.fstat(stream.fileno())
        require(len(data)<=2*1024**2 and identity(before)==identity(after)
            and (before.st_size,before.st_mtime_ns,before.st_ctime_ns)==(after.st_size,after.st_mtime_ns,after.st_ctime_ns)
            and identity(after)==identity(os.stat(name,dir_fd=parent,follow_symlinks=False)),'policy-changed')
        return data,after


def no_duplicates(pairs):
    """Reject ambiguous JSON fields before deriving an approved candidate."""
    result={}
    for key,value in pairs:
        require(key not in result,'duplicate-policy-field');result[key]=value
    return result


def state_format(state):
    """Classify the locked legacy ledger without creating or ignoring sidecars."""
    try:os.stat('authority-epoch.json',dir_fd=state,follow_symlinks=False)
    except FileNotFoundError:pass
    else:return 'binding-aware'
    name='runtime-retirement.sqlite'
    before=os.stat(name,dir_fd=state,follow_symlinks=False)
    require(owned(before,limit=128*1024**2),'ledger-not-private')
    for suffix in ('-wal','-shm','-journal'):
        try:os.stat(name+suffix,dir_fd=state,follow_symlinks=False)
        except FileNotFoundError:continue
        raise Hold('native-ledger-recovery-required')
    # Use SQLite's VFS, never a raw descriptor/copy of a live database. Opening
    # and closing raw database FDs could release this process's SQLite locks.
    # This standalone command holds the original StateLock around every read.
    # The unix-none VFS refuses WAL instead of creating shared-memory files.
    # Do not use immutable=1: it can ignore pending WAL authority and report a
    # legacy-looking base schema. Existing sidecars require explicit recovery.
    deadline=time.monotonic()+2
    with closing(sqlite3.connect(f'file:/proc/self/fd/{state}/{name}?mode=ro&vfs=unix-none',uri=True,timeout=0)) as db:
        db.set_progress_handler(lambda:int(time.monotonic()>deadline),1000)
        objects=list(db.execute('SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name'))
        legacy=(objects==[
            ('index','sqlite_autoindex_retired_owner_1','retired_owner',None),
            ('table','retired_owner','retired_owner','CREATE TABLE retired_owner(audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation))')]
            and db.execute('PRAGMA journal_mode').fetchone()==('delete',))
    require(identity(before)==identity(os.stat(name,dir_fd=state,follow_symlinks=False)),'ledger-changed')
    return 'legacy-schema' if legacy else 'unknown'


def update(path,requested,*,apply=False,expected_source=None,expected_candidate=None):
    """Preview or explicitly publish one approved capacity increase while retaining state."""
    require(type(apply) is bool,'invalid-operation')
    require(type(requested) is int and 0<requested<=2**64-1,'invalid-capacity')
    if apply:
        require(all(type(h) is str and re.fullmatch('[0-9a-f]{64}',h) for h in (expected_source,expected_candidate)),
            'reviewed-source-and-candidate-hashes-required')
    path=Path(path)
    require(path.is_absolute() and path.name=='policy.json' and path.parent.resolve()==path.parent,'managed-policy-path-required')
    with ExitStack() as stack:
        def descriptor(name,*,parent=None,directory=False):
            """Open an existing private coordination object and retain its descriptor."""
            flags=(os.O_RDONLY if directory else os.O_RDWR)|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK
            if directory:flags|=os.O_DIRECTORY
            fd=os.open(name,flags,dir_fd=parent);stack.callback(close_descriptor,fd)
            require(owned(os.fstat(fd),directory),'managed-state-not-private');return fd
        root=descriptor(path.parent,directory=True)
        state=descriptor('state',parent=root,directory=True)
        locks=[]
        for parent,name in ((root,'provision.lock'),(state,'state.lock')):
            fd=descriptor(name,parent=parent)
            try:fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
            except BlockingIOError:raise Hold('native-policy-busy') from None
            locks.append((parent,name,fd))
        def check_context():
            """Recheck original private directories and coordination-file identities."""
            require(owned(os.fstat(root),directory=True) and owned(os.fstat(state),directory=True)
                and identity(os.fstat(root))==identity(path.parent.lstat())
                and identity(os.fstat(state))==identity((path.parent/'state').lstat()),'managed-state-changed')
            for parent,name,fd in locks:
                require(owned(os.fstat(fd)) and identity(os.fstat(fd))==identity(os.stat(name,dir_fd=parent,follow_symlinks=False)),
                    'original-native-policy-lock-changed')
        def confirm(expected,synced):
            """Bind approved bytes and context to the held policy inode that was synced."""
            try:
                os.fsync(root)
                observed,info=private(root,'policy.json')
                require(digest(observed)==expected and identity(info)==identity(os.fstat(synced)),'publication-changed')
                check_context()
            except (OSError,Hold):raise PublicationUnknown('policy-publication-unconfirmed-inspect-before-retry') from None
        data,source_info=private(root,'policy.json');source=digest(data)
        value=json.loads(data,object_pairs_hook=no_duplicates)
        require(type(value) is dict and set(value)=={'version','runtime_config','guest_runtime','state_root','memory_mb','max_state_bytes','max_sessions','providers'},'unsupported-policy')
        require(type(value['version']) is int and value['version']==1
            and type(value['memory_mb']) is int and 512<=value['memory_mb']<=4096
            and type(value['max_sessions']) is int and value['max_sessions']>0
            and type(value['max_state_bytes']) is int and 1024**3<=value['max_state_bytes']<=2**64-1
            and value['state_root']==str(path.parent/'state') and type(value['providers']) is dict,'unsupported-policy')
        check_context()
        kind=state_format(state)
        if apply:require(kind=='legacy-schema','binding-aware-or-unknown-state-requires-migration')
        replay=apply and source==expected_candidate and value['max_state_bytes']==requested
        require(replay or requested>value['max_state_bytes'],'capacity-must-increase')
        candidate=dict(value,max_state_bytes=requested)
        payload=(json.dumps(candidate,sort_keys=True,separators=(',',':'))+'\n').encode()
        candidate_sha=digest(payload)
        report={'schema_version':1,'reason':'already-applied' if replay else 'capacity-preview',
            'source_sha256':source,'candidate_sha256':source if replay else candidate_sha,
            'observed_max_state_bytes':value['max_state_bytes'],'requested_max_state_bytes':requested,
            'memory_mb':value['memory_mb'],'max_sessions':value['max_sessions'],'state_format':kind,
            'apply_supported':kind=='legacy-schema','policy_applied':False,'already_applied':bool(replay),
            'execution_authorized':False,'binding_migration_authorized':False}
        if not apply:return report
        if replay:
            # A matching pathname proves current contents, not durability after
            # a prior rename/sync error. Confirm both before clearing uncertainty.
            fd=None
            try:
                fd=os.open('policy.json',os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW,dir_fd=root)
                require(identity(os.fstat(fd))==identity(source_info),'policy-changed-before-confirmation')
                os.fsync(fd);confirm(source,fd)
            except (OSError,Hold):raise PublicationUnknown('policy-publication-unconfirmed-inspect-before-retry') from None
            finally:
                if fd is not None:close_descriptor(fd)
            return report
        require(source==expected_source and candidate_sha==expected_candidate,'reviewed-policy-hash-mismatch')
        temporary='.capacity-policy-'+secrets.token_hex(16);published=False;anchor=None
        try:
            anchor=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=root)
            stack.callback(close_descriptor,anchor)
            with os.fdopen(anchor,'wb',closefd=False) as stream:
                stream.write(payload);stream.flush();os.fchmod(stream.fileno(),stat.S_IMODE(source_info.st_mode));os.fsync(stream.fileno())
            staged,staged_info=private(root,temporary)
            require(staged==payload and identity(staged_info)==identity(os.fstat(anchor)),'staged-policy-changed')
            observed,info=private(root,'policy.json')
            require(observed==data and identity(info)==identity(source_info),'policy-changed-before-publication')
            check_context()
            try:os.replace(temporary,'policy.json',src_dir_fd=root,dst_dir_fd=root)
            except OSError as error:
                # The rename may have committed despite a lost acknowledgement.
                # Claim no effect only when the exact original still exists.
                try:observed,info=private(root,'policy.json')
                except (OSError,Hold):raise PublicationUnknown('policy-publication-unconfirmed-inspect-before-retry') from None
                if observed==data and identity(info)==identity(source_info):raise error
                raise PublicationUnknown('policy-publication-unconfirmed-inspect-before-retry') from None
            published=True
            confirm(candidate_sha,anchor)
            return dict(report,reason='capacity-policy-applied',policy_applied=True)
        finally:
            if not published and anchor is not None:
                try:
                    remaining=os.stat(temporary,dir_fd=root,follow_symlinks=False);original=os.fstat(anchor)
                    if owned(remaining) and (remaining.st_dev,remaining.st_ino)==(original.st_dev,original.st_ino):
                        os.unlink(temporary,dir_fd=root)
                # Staging cleanup is best effort; its failure must never turn
                # a possibly committed rename into a false no-effect result.
                except OSError:pass


def main():
    """Parse explicit owner options and emit bounded results with typed exit codes."""
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--policy',type=Path,required=True)
    parser.add_argument('--max-state-bytes',type=int,required=True)
    parser.add_argument('--apply',action='store_true')
    parser.add_argument('--expected-source-sha256')
    parser.add_argument('--expected-candidate-sha256')
    args=parser.parse_args()
    try:
        print(json.dumps(update(args.policy,args.max_state_bytes,apply=args.apply,
            expected_source=args.expected_source_sha256,expected_candidate=args.expected_candidate_sha256),sort_keys=True));return 0
    except PublicationUnknown as error:
        print(json.dumps({'schema_version':1,'reason':str(error),'policy_applied':'unknown','execution_authorized':False,'binding_migration_authorized':False}));return 4
    except Hold as error:
        print(json.dumps({'schema_version':1,'reason':str(error),'policy_applied':False,'execution_authorized':False,'binding_migration_authorized':False}));return 3
    except (OSError,ValueError,sqlite3.Error):
        print(json.dumps({'schema_version':1,'reason':'native-policy-evidence-unavailable','policy_applied':False,'execution_authorized':False,'binding_migration_authorized':False}));return 4


if __name__=='__main__':raise SystemExit(main())
