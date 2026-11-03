#!/usr/bin/env python3
"""Selective content observations from controller censuses and decoded lookups.

The caller must establish the original process/unit attribution and immutable
execution view independently. A self-consistent census is not authenticated
source or producer evidence. This module never grants artifact/CI authority.
Metadata/credential identity, complete syscall coverage, outputs and original
quiescence still belong to the protected collector and unit protocol.
"""
import base64
import hashlib
import importlib.util
import json
import copy
import os
from pathlib import Path
import stat


_spec=importlib.util.spec_from_file_location('lookup_cache',Path(__file__).with_name('ci-cache-generation.py'))
_cache=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(_cache)
FreshnessHold=_cache.FreshnessHold


def _hold():
    raise FreshnessHold('input_census_unknown')


def _encoded(value):
    return json.dumps(value,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()


def _path(value,*,absolute=False):
    if not isinstance(value,bytes) or not value or len(value)>4096 or b'\0' in value:_hold()
    if absolute and (not value.startswith(b'/') or b'//' in value
            or any(part in (b'.',b'..') for part in value.split(b'/'))):_hold()
    return value


def _wire(value):
    if isinstance(value,bytes):return {'bytes_b64':base64.b64encode(value).decode('ascii')}
    if isinstance(value,(tuple,list)):return [_wire(item) for item in value]
    if isinstance(value,dict):return {key:_wire(item) for key,item in value.items()}
    return value


def _unwire(value,depth=0):
    if depth>64:_hold()
    if type(value) is dict:
        if set(value)=={'bytes_b64'}:
            try:decoded=base64.b64decode(value['bytes_b64'],validate=True)
            except (ValueError,TypeError):_hold()
            if len(decoded)>4096 or base64.b64encode(decoded).decode('ascii')!=value['bytes_b64']:_hold()
            return decoded
        return {key:_unwire(item,depth+1) for key,item in value.items()}
    if type(value) is list:return [_unwire(item,depth+1) for item in value]
    if value is not None and type(value) not in (str,int,bool):_hold()
    return value


class UnitInvalidations:
    """Durable, monotonic invalidation intent for one admitted planner command.

    The root planner supplies the dependency graph and selected-input digests.
    This journal never deletes fingerprints, clears pending work, or authorizes
    reuse. Original scope drainage must be proved separately before mutation.
    A single controller owns the journal; stale writers are refused.
    """
    _MAX_BYTES=1024*1024

    def __init__(self,path,*,binding,command):
        _cache.validate_binding(binding)
        if not isinstance(command,str) or not command or len(command)>256:_hold()
        self.path=Path(path);self.binding=copy.deepcopy(binding);self.command=command
        self._parent()
        self._record=self._read()
        if self._record is None:
            self._record={'version':1,'kind':'ci-unit-invalidations','binding':self.binding,
                'command':command,'units':{},'pending':[],'shipping_authority':False}
            self._save(self._record,initial=True)
        self._validate(self._record)

    def _parent(self):
        try:
            info=self.path.parent.lstat()
            if (self.path.parent.resolve()!=self.path.parent or not stat.S_ISDIR(info.st_mode)
                    or info.st_uid!=os.geteuid() or stat.S_IMODE(info.st_mode)!=0o700):_hold()
        except OSError:_hold()

    def _read(self):
        self._parent()
        try:self.path.lstat()
        except FileNotFoundError:return None
        return _cache._read_record(self.path,max_bytes=self._MAX_BYTES)

    @staticmethod
    def _name(name):
        return (isinstance(name,str) and 0<len(name)<=128
            and all(c in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-.:' for c in name))

    def _validate(self,value):
        try:
            if (type(value) is not dict or set(value)!={'version','kind','binding','command','units','pending','shipping_authority'}
                    or type(value['version']) is not int or value['version']!=1 or value['kind']!='ci-unit-invalidations'
                    or value['binding']!=self.binding or value['command']!=self.command
                    or value['shipping_authority'] is not False or type(value['units']) is not dict
                    or len(value['units'])>1024 or type(value['pending']) is not list):_hold()
            units=value['units'];fingerprints=set();finished=set()
            for name,row in units.items():
                if (not self._name(name) or type(row) is not dict
                        or set(row)!={'dependencies','content_sha256','fingerprints'}
                        or not _cache._digest(row['content_sha256'])):_hold()
                deps=row['dependencies'];paths=row['fingerprints']
                if (type(deps) is not list or len(deps)>64 or any(not self._name(d) or d not in units or d==name for d in deps)
                        or len(set(deps))!=len(deps) or type(paths) is not list or not 0<len(paths)<=32):_hold()
                for path in paths:
                    if not isinstance(path,str) or len(path)>512:_hold()
                    parts=path.split('/')
                    if (len(parts)!=3 or parts[0] not in ('debug','release') or parts[1]!='.fingerprint'
                            or not self._name(parts[2]) or parts[2] in ('.','..') or path in fingerprints):_hold()
                    fingerprints.add(path)
            # Iterative DAG validation also bounds malformed persisted graphs.
            while len(finished)<len(units):
                ready={name for name,row in units.items() if name not in finished and set(row['dependencies'])<=finished}
                if not ready:_hold()
                finished.update(ready)
            pending=value['pending']
            if any(not self._name(name) or name not in units for name in pending) or pending!=sorted(set(pending)):_hold()
            if any(name not in pending and any(dep in pending for dep in row['dependencies']) for name,row in units.items()):_hold()
        except (KeyError,TypeError,ValueError):_hold()

    def _save(self,value,*,initial=False):
        self._validate(value)
        if len(_encoded(value))>self._MAX_BYTES:_hold()
        if self._read()!=(None if initial else self._record):_hold()
        _cache._write_record(self.path,value)
        self._record=copy.deepcopy(value)

    def register(self,name,*,dependencies,content_sha256,fingerprints):
        if not self._name(name) or name in self._record['units']:_hold()
        value=self.state()
        value['units'][name]=copy.deepcopy({'dependencies':dependencies,'content_sha256':content_sha256,'fingerprints':fingerprints})
        if type(dependencies) is list and any(dep in value['pending'] for dep in dependencies):
            value['pending']=sorted([*value['pending'],name])
        self._save(value)

    def changed(self,digests):
        if type(digests) is not dict or len(digests)>1024:_hold()
        value=self.state();pending=set(value['pending'])
        for name,digest in digests.items():
            if name not in value['units'] or not _cache._digest(digest):_hold()
            if digest!=value['units'][name]['content_sha256']:pending.add(name)
        while True:
            more={name for name,row in value['units'].items() if any(dep in pending for dep in row['dependencies'])}-pending
            if not more:break
            pending.update(more)
        value['pending']=sorted(pending)
        self._save(value)
        return self.pending()

    def pending(self):return list(self._record['pending'])

    def state(self):return copy.deepcopy(self._record)


class LookupCensus:
    """Detached content facts for the exact virtual roots supplied by C.

    The existing v1 input snapshot format is consumed unchanged. This is not a
    new freshness version, input-proof constructor or mount/freeze capability.
    Common ancestors route between declared roots, but cannot attest absence or
    membership outside those roots. Symlinks are resolved before '..'.
    """
    def __init__(self,namespaces):
        self._nodes={};self._roots=set();self._routes=set()
        try:
            if not isinstance(namespaces,dict) or not 0<len(namespaces)<=64:_hold()
            for name,(prefix,snapshot) in namespaces.items():
                if not isinstance(name,str) or not isinstance(prefix,str):_hold()
                prefix=_path(prefix.encode(),absolute=True).rstrip(b'/') or b'/'
                if prefix in self._roots:_hold()
                self._roots.add(prefix)
                keys={'format_version','kind','sha256','files','bytes','entries'}
                if (not isinstance(snapshot,dict) or set(snapshot)!=keys
                        or type(snapshot['format_version']) is not int or snapshot['format_version']!=1
                        or snapshot['kind']!='ci-input-snapshot' or not _cache._digest(snapshot['sha256'])
                        or not isinstance(snapshot['entries'],list) or len(snapshot['entries'])>200064
                        or hashlib.sha256(_encoded(snapshot['entries'])).hexdigest()!=snapshot['sha256']):_hold()
                files=total=0;seen=set()
                for row in snapshot['entries']:
                    if not isinstance(row,list) or len(row)<4 or row[0]!=name or not isinstance(row[1],str):_hold()
                    relative=row[1].encode()
                    if relative!=b'.' and (relative.startswith(b'/') or any(p in (b'',b'.',b'..') for p in relative.split(b'/'))):_hold()
                    path=prefix if relative==b'.' else prefix.rstrip(b'/')+b'/'+relative
                    _path(path,absolute=True)
                    if path in seen:_hold()
                    seen.add(path);kind=row[2]
                    if kind=='directory' and len(row)==4:node=(kind,row[3])
                    elif kind=='file' and len(row)==6:
                        if type(row[4]) is not int or row[4]<0 or not _cache._digest(row[5]):_hold()
                        files+=1;total+=row[4];node=(kind,row[3],row[4],row[5])
                    elif kind=='symlink' and len(row)==6:
                        if not all(isinstance(value,str) for value in row[3:]):_hold()
                        node=(kind,_path(row[3].encode()))
                    else:_hold()
                    if kind!='symlink' and (type(row[3]) is not int or not 0<=row[3]<=0o7777):_hold()
                    if path in self._nodes and (kind!='directory' or self._nodes[path]!=node):_hold()
                    self._nodes[path]=node
                if prefix not in seen or type(snapshot['files']) is not int or type(snapshot['bytes']) is not int or (files,total)!=(snapshot['files'],snapshot['bytes']):_hold()
            self._finish_routes()
        except (ValueError,TypeError,UnicodeError,KeyError):_hold()

    @classmethod
    def from_inventory(cls,namespaces):
        """Consume bounded detached byte-path inventories, without authority."""
        value=cls.__new__(cls);value._nodes={};value._roots=set();value._routes=set()
        try:
            if type(namespaces) is not dict or not 0<len(namespaces)<=64:_hold()
            for name,(prefix,rows) in namespaces.items():
                prefix=_path(prefix.encode(),absolute=True).rstrip(b'/') or b'/'
                if prefix in value._roots or type(rows) is not list or not 0<len(rows)<=200064 or rows[0][0]!='2e':_hold()
                value._roots.add(prefix);seen=set()
                for row in rows:
                    if type(row) is not list or len(row) not in (3,5) or type(row[0]) is not str:_hold()
                    relative=bytes.fromhex(row[0])
                    if relative.hex()!=row[0] or (relative!=b'.' and (relative.startswith(b'/')
                            or any(part in (b'',b'.',b'..') for part in relative.split(b'/')))):_hold()
                    path=prefix if relative==b'.' else prefix.rstrip(b'/')+b'/'+relative
                    _path(path,absolute=True)
                    if path in seen or type(row[2]) is not int or not 0<=row[2]<=0o7777:_hold()
                    seen.add(path)
                    if row[1]=='directory' and len(row)==3:node=('directory',row[2])
                    elif row[1]=='file' and len(row)==5 and type(row[3]) is int and row[3]>=0 and _cache._digest(row[4]):
                        node=('file',row[2],row[3],row[4])
                    else:_hold()
                    if path in value._nodes and value._nodes[path]!=node:_hold()
                    value._nodes[path]=node
                    if len(value._nodes)>200064:_hold()
            value._finish_routes();return value
        except (ValueError,TypeError,UnicodeError,KeyError,IndexError,AttributeError):_hold()

    def _finish_routes(self):
        for path in self._nodes:
            parent=path
            while parent!=b'/':
                parent=parent.rpartition(b'/')[0] or b'/'
                if parent not in self._nodes:self._routes.add(parent)
        for path in self._nodes:
            parent=path.rpartition(b'/')[0] or b'/'
            if path not in self._roots and self._nodes.get(parent,('unknown',))[0]!='directory':_hold()
        if len(self._nodes)>200064:_hold()

    def _anchor(self,path):
        _path(path)
        roots=[root for root in self._roots if path==root or path.startswith(root.rstrip(b'/')+b'/')]
        if not roots:_hold()
        root=max(roots,key=len)
        return root,path[len(root):].lstrip(b'/').split(b'/') if path!=root else []

    def _resolve(self,path,directory,*,follow=True):
        _path(path);facts=[];hops=0
        if path.startswith(b'/'):current,remaining=self._anchor(path)
        else:
            current=_path(directory,absolute=True);remaining=path.split(b'/')
            if current not in self._nodes and current not in self._routes:_hold()
        while remaining:
            part=remaining.pop(0);node=self._nodes.get(current)
            if node is None and current not in self._routes:_hold()
            if node is not None:
                facts.append((current,node[0],node[1]))
                if node[0]!='directory':return current,None,'ENOTDIR',facts
            if part in (b'',b'.'):continue
            if part==b'..':
                current=current.rpartition(b'/')[0] or b'/'
                continue
            child=current.rstrip(b'/')+b'/'+part
            value=self._nodes.get(child)
            if value is None:
                if child in self._routes:
                    current=child;continue
                if node is None:_hold()
                facts.append((child,'absent'))
                return child,None,'ENOENT',facts
            if value[0]=='symlink' and (remaining or follow):
                hops+=1
                if hops>40:
                    facts.append((child,'symlink-limit'));return child,None,'ELOOP',facts
                facts.append((child,'symlink',value[1]))
                if value[1].startswith(b'/'):
                    current,target=self._anchor(value[1])
                else:target=value[1].split(b'/')
                remaining=target+remaining
            else:current=child
        node=self._nodes.get(current)
        if node is None:_hold()
        facts.append((current,*node))
        return current,node,None,facts

    def _members(self,path):
        prefix=path.rstrip(b'/')+b'/'
        return tuple(sorted((name[len(prefix):],node[0]) for name,node in self._nodes.items()
            if name.startswith(prefix) and b'/' not in name[len(prefix):]))

    def observe(self,queries,*,cwd=None,require_original_results=True):
        """Bind selected content facts, or refresh the original selectors.

        Refresh may differ from the old execution's outcomes; it does not turn
        those outcomes into a verdict about current source. Complete directory
        reads require the original end-of-directory event and matching census.
        """
        try:
            if not isinstance(queries,list) or len(queries)>200000 or type(require_original_results) is not bool:_hold()
            facts=set();directories={}
            for query in queries:
                if not isinstance(query,dict) or query.get('intent') not in ('read','metadata','directory') or 'device' in query or query.get('descriptor_kind','path')!='path':_hold()
                syscall=query.get('syscall')
                if syscall in ('getdents','getdents64'):
                    path=_path(query.get('directory'),absolute=True)
                    if query.get('outcome')!='success' or not isinstance(query.get('entries'),list) or type(query.get('result')) is not int:_hold()
                    members,ended=directories.setdefault(path,({},False))
                    for item in query['entries']:
                        name=_path(item.get('name'))
                        if b'/' in name or not isinstance(item.get('type'),str):_hold()
                        if name not in (b'.',b'..'):
                            if name in members and members[name]!=item['type']:_hold()
                            members[name]=item['type']
                    directories[path]=(members,ended or query['result']==0)
                    continue
                if syscall not in ('open','openat','openat2','stat','lstat','newfstatat','statx','readlink','readlinkat'):_hold()
                path=_path(query.get('path'));directory=query.get('directory') or cwd
                nofollow=syscall in ('lstat','readlink','readlinkat') or 'AT_SYMLINK_NOFOLLOW' in query.get('arguments','')
                if 'O_NOFOLLOW' in query.get('arguments',''):_hold()
                resolved,node,error,selected=self._resolve(path,directory,follow=not nofollow)
                if syscall in ('readlink','readlinkat') and node is not None and node[0]!='symlink':error='EINVAL'
                if require_original_results:
                    if query.get('outcome')!=('failure' if error else 'success') or query.get('errno')!=error:_hold()
                    if 'resolved_path' in query and query['resolved_path']!=resolved:_hold()
                    if 'link_bytes' in query and (node is None or node[0]!='symlink' or query['link_bytes']!=node[1]):_hold()
                facts.add(('query',syscall,path,directory,error))
                facts.update(selected)
            types={'directory':'DT_DIR','file':'DT_REG','symlink':'DT_LNK'}
            for path,(observed,ended) in directories.items():
                if not ended:_hold()
                resolved,node,error,selected=self._resolve(path,None)
                if error is not None or node[0]!='directory':_hold()
                members=self._members(resolved)
                if require_original_results and (set(observed)!={name for name,_kind in members}
                        or any(observed[name] not in (types[kind],'DT_UNKNOWN') for name,kind in members)):_hold()
                facts.update(selected);facts.add(('directory-members',path,members))
            encoded=sorted((_wire(item) for item in facts),key=_encoded)
            return {'version':1,'kind':'ci-unit-lookup-content-observation',
                'sha256':hashlib.sha256(_encoded(encoded)).hexdigest(),'facts':encoded,
                'input_closure_complete':False,'shipping_authority':False}
        except (ValueError,TypeError,KeyError,AttributeError,UnicodeError):_hold()


class _ContentSelection:
    def __init__(self,queries,observation,binding,layout):
        self._queries=copy.deepcopy(queries);self._observation=copy.deepcopy(observation)
        self.binding_sha256=binding;self._layout=layout

    @property
    def observation(self):return copy.deepcopy(self._observation)

    def refresh(self,index):
        if type(index) is not FrozenLookupIndex or index._layout!=self._layout:_hold()
        index._verify()
        return index.census.observe(self._queries,require_original_results=False)

    def record(self):
        return {'version':1,'kind':'ci-lookup-read-selection','queries':_wire(self._queries),
            'observation':copy.deepcopy(self._observation),'binding_sha256':self.binding_sha256,
            'layout':_wire(self._layout),'input_closure_complete':False,'shipping_authority':False}

    @classmethod
    def restore(cls,value,index):
        """Recheck a caller-authenticated record against its original trees.

        The record itself is not producer or collector authority. The caller
        must bind it to the retained original command's protected capture.
        """
        try:
            if (type(value) is not dict or set(value)!={'version','kind','queries','observation','binding_sha256',
                    'layout','input_closure_complete','shipping_authority'} or type(value['version']) is not int
                    or value['version']!=1 or value['kind']!='ci-lookup-read-selection'
                    or not _cache._digest(value['binding_sha256']) or type(index) is not FrozenLookupIndex
                    or value['input_closure_complete'] is not False or value['shipping_authority'] is not False):_hold()
            index._verify();queries=_unwire(value['queries']);layout=_unwire(value['layout'])
            if tuple(layout)!=index._layout or index.census.observe(queries)!=value['observation']:_hold()
            return cls(queries,value['observation'],value['binding_sha256'],index._layout)
        except (ValueError,TypeError,KeyError,AttributeError,UnicodeError):_hold()


class FrozenLookupIndex:
    """Bind a selected query subset to controller-frozen literal inventories.

    The caller still establishes original collector/producer authority and
    quiescence. Neither a root-owned tree nor this index proves complete inputs
    or grants artifact reuse. Unknown bases and mount aliases are refused.
    """
    def __init__(self,namespaces,*,anchors=None,kernel_roots=None,owner,overlays=False):
        spec=importlib.util.spec_from_file_location('lookup_views',Path(__file__).with_name('ci-cargo-views.py'))
        self.views=importlib.util.module_from_spec(spec);spec.loader.exec_module(self.views)
        self.owner=owner;self._trees=[];self._identities=[];self._bases={};self._objects={}
        try:
            if type(owner) is not int or not 0<=owner<2**32 or type(namespaces) is not dict or not 0<len(namespaces)<=64 or type(overlays) is not bool:_hold()
            prefixes=[_path(value[0].encode(),absolute=True).rstrip(b'/') or b'/' for value in namespaces.values()]
            if len(set(prefixes))!=len(prefixes):_hold()
            content={}
            for name,(prefix,root,rows) in namespaces.items():
                root=self.views.literal(root);rows=copy.deepcopy(rows)
                if self._inventory(root)!=rows:_hold()
                prefix=_path(prefix.encode(),absolute=True).rstrip(b'/') or b'/'
                self._trees.append((root,rows));visible=[]
                for row in rows:
                    relative=bytes.fromhex(row[0]);physical=root if relative==b'.' else root/os.fsdecode(relative)
                    virtual=prefix if relative==b'.' else prefix.rstrip(b'/')+b'/'+relative
                    if overlays and any(other!=prefix and other.startswith(prefix.rstrip(b'/')+b'/')
                            and (virtual==other or virtual.startswith(other+b'/')) for other in prefixes):continue
                    visible.append(row)
                    info=physical.lstat();self._remember(physical,info)
                    self._objects[virtual]=self._object(info)
                    if stat.S_ISDIR(info.st_mode):self._anchor(virtual,physical,info)
                content[name]=(prefix.decode(),visible)
            anchors={} if anchors is None else anchors
            if type(anchors) is not dict or len(anchors)>64:_hold()
            for prefix,root in anchors.items():
                virtual=_path(prefix.encode(),absolute=True).rstrip(b'/') or b'/'
                root=self.views.literal(root);info=root.lstat()
                if not stat.S_ISDIR(info.st_mode) or info.st_uid!=owner or stat.S_IMODE(info.st_mode)!=0o555:_hold()
                self._remember(root,info);self._anchor(virtual,root,info)
            # A departed private mount namespace cannot be inspected here.
            # Only the admitted controller may supply its original root object,
            # captured at the supervised exec stop before releasing user code.
            # This anchor routes absolute names; the census still refuses facts
            # outside the explicitly frozen input trees.
            kernel_roots={} if kernel_roots is None else copy.deepcopy(kernel_roots)
            if type(kernel_roots) is not dict or set(kernel_roots)-{'/'}:_hold()
            for prefix,identity in kernel_roots.items():
                if type(identity) is not dict or set(identity)!={'device','inode','mode'}:_hold()
                if (any(type(identity[key]) is not int for key in identity)
                        or not 0<=identity['device']<2**32 or not 0<identity['inode']<2**64
                        or not 0<=identity['mode']<2**16 or not stat.S_ISDIR(identity['mode'])):_hold()
                key=tuple(identity[name] for name in ('device','inode','mode'))
                if key in self._bases and self._bases[key]!=b'/':_hold()
                self._bases[key]=b'/'
            self.census=LookupCensus.from_inventory(content)
            if any(virtual not in self.census._nodes and virtual not in self.census._routes for virtual in self._bases.values()):_hold()
            self._layout=tuple(sorted(self.census._roots));self._verify()
        except (OSError,ValueError,TypeError,KeyError,UnicodeError,IndexError,AttributeError,self.views.FreshnessHold):_hold()

    @staticmethod
    def _object(info):return ((os.major(info.st_dev)<<20)|os.minor(info.st_dev),info.st_ino,info.st_mode)

    def _inventory(self,root):
        if root.is_file():return [['2e','file',*self.views._read(root,self.owner,True)]]
        return self.views.inventory(root,owner=self.owner,sealed=True)

    def _remember(self,path,info):
        self._identities.append((path,(*self.views._key(info),info.st_nlink,info.st_atime_ns)))

    def _anchor(self,virtual,path,info):
        key=self._object(info)
        if key in self._bases and self._bases[key]!=virtual:_hold()
        self._bases[key]=virtual

    def _verify(self):
        try:
            if any(self._inventory(root)!=rows for root,rows in self._trees):_hold()
            for path,identity in self._identities:
                info=path.lstat()
                if (*self.views._key(info),info.st_nlink,info.st_atime_ns)!=identity:_hold()
        except (OSError,ValueError,self.views.FreshnessHold):_hold()

    def select(self,events,metadata,*,pid,uid,user_namespace,mount_namespace,threaded=False):
        self._verify();queries=[];contexts=[]
        try:
            if (type(threaded) is not bool or type(events) is not list or not 0<len(events)<=200000
                    or type(metadata) is not list or len(metadata)>200000):_hold()
            keys=('uid','pid','tid','syscall','sequence');observed={}
            for record in metadata:
                key=tuple(record[k] for k in keys)
                if key in observed:_hold()
                observed[key]=record
            for event in events:
                # In threaded mode the collector's actual kernel TGID still
                # binds the original supervised process. Resolve each query
                # through its own captured base and namespaces; never borrow
                # the main thread's cwd or a sampled /proc thread directory.
                if (not threaded and event['pid']!=event['tid']
                        or type(event['tid']) is not int or not 0<event['tid']<2**31
                        or event['pid']!=pid or event['uid']!=uid
                        or event['syscall'] not in (262,332)):_hold()
                record=observed.get(tuple(event[k] for k in keys))
                if (record is None or record['user_namespace']!=user_namespace or record['path']!=event['path']
                        or record['result']!=event['result'] or type(record['flags']) is not int
                        or record['flags']&~0x900 or event['result'] not in (0,-2,-20,-40)):_hold()
                bases=[]
                for attempt in event['lookup_attempts']:
                    base=attempt['base']
                    if (attempt['init_result']!=0 or attempt['user_namespace']!=user_namespace
                            or attempt['mount_namespace']!=mount_namespace or attempt['dfd']!=record['fd']
                            or base is None or base['flags']&0x18000000
                            or base['flags']&256 and (base['path_seq']!=base['dentry_seq'] or base['path_seq']&1)):_hold()
                    virtual=self._bases.get(tuple(base[k] for k in ('device','inode','mode')))
                    if virtual is None:_hold()
                    bases.append(virtual)
                if not bases or len(set(bases))!=1:_hold()
                path=_path(event['path']);directory=bases[0]
                if path.startswith(b'/') and directory!=b'/':_hold()
                if event['result']==0:
                    resolved,node,error,_facts=self.census._resolve(path,directory,follow=not record['flags']&256)
                    if error is not None or record['stat'] is None or self._objects.get(resolved)!=tuple(record['stat'][k] for k in ('device','inode','mode')):_hold()
                elif record['stat'] is not None or record['copy_result'] is not None:_hold()
                queries.append({'pid':pid,'syscall':'newfstatat' if event['syscall']==262 else 'statx',
                    'path':path,'directory':directory,'intent':'metadata','outcome':'success' if event['result']==0 else 'failure',
                    'errno':{0:None,-2:'ENOENT',-20:'ENOTDIR',-40:'ELOOP'}[event['result']],
                    'arguments':'AT_SYMLINK_NOFOLLOW' if record['flags']&256 else '', 'result':event['result']})
                contexts.append((event,record))
            observation=self.census.observe(queries)
            binding=hashlib.sha256(_encoded(_wire(contexts))).hexdigest()
            return _ContentSelection(queries,observation,binding,self._layout)
        except (ValueError,TypeError,KeyError,AttributeError,UnicodeError):_hold()

    def _open_query(self,event,*,pid,uid,user_namespace,mount_namespace):
        try:
            opened=event['open'];arguments=opened['arguments'];installed=opened['installed']
            if (event['pid']!=pid or event['uid']!=uid or event['syscall'] not in (2,257,437)
                    or arguments['user_namespace']!=user_namespace or arguments['mount_namespace']!=mount_namespace
                    or type(arguments['flags']) is not int or arguments['flags']&~(0x80000|0x8000|0x100|0x800|0x20000)
                    or event['result']<0):_hold()
            bases=[]
            for attempt in event['lookup_attempts']:
                base=attempt['base']
                if (attempt['init_result']!=0 or attempt['dfd']!=arguments['dfd']
                        or attempt['user_namespace']!=user_namespace or attempt['mount_namespace']!=mount_namespace
                        or base is None or base['flags']&0x18000000
                        or base['flags']&256 and (base['path_seq']!=base['dentry_seq'] or base['path_seq']&1)):_hold()
                virtual=self._bases.get(tuple(base[key] for key in ('device','inode','mode')))
                if virtual is None:_hold()
                bases.append(virtual)
            if not bases or len(set(bases))!=1:_hold()
            path=_path(event['path']);directory=bases[0]
            if path.startswith(b'/') and directory!=b'/':_hold()
            resolved,node,error,_facts=self.census._resolve(path,directory)
            if (error is not None or node[0]!='file' or installed is None or installed['fd']!=event['result']
                    or not installed['file_mode']&1 or installed['flags']&0x200000
                    or installed['user_namespace']!=user_namespace or installed['mount_namespace']!=mount_namespace
                    or self._objects.get(resolved)!=tuple(installed[key] for key in ('device','inode','mode'))):_hold()
            return {'syscall':{2:'open',257:'openat',437:'openat2'}[event['syscall']],
                'pid':pid,'path':path,'directory':directory,'intent':'read','outcome':'success','errno':None,
                'result':event['result'],'arguments':''}
        except (ValueError,TypeError,KeyError,AttributeError,UnicodeError):_hold()

    def select_opens(self,events,*,pid,uid,user_namespace,mount_namespace):
        self._verify()
        if type(events) is not list or not 0<len(events)<=200000:_hold()
        queries=[self._open_query(event,pid=pid,uid=uid,user_namespace=user_namespace,mount_namespace=mount_namespace) for event in events]
        return _ContentSelection(queries,self.census.observe(queries),hashlib.sha256(_encoded(_wire(events))).hexdigest(),self._layout)

    def select_read_subset(self,events,*,pid,uid,user_namespace,mount_namespace):
        """Retain unsupported queries explicitly; this never proves coverage."""
        self._verify()
        if type(events) is not list or len(events)>200000:_hold()
        queries=[];selected=[];unbound=[]
        for event in events:
            try:query=self._open_query(event,pid=pid,uid=uid,user_namespace=user_namespace,mount_namespace=mount_namespace)
            except FreshnessHold:unbound.append({key:event[key] for key in ('pid','tid','uid','syscall','sequence')})
            else:queries.append(query);selected.append(event)
        self._verify()
        return _ContentSelection(queries,self.census.observe(queries),hashlib.sha256(_encoded(_wire(selected))).hexdigest(),self._layout),unbound
