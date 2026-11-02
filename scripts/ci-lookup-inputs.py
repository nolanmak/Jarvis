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
from pathlib import Path


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
    if isinstance(value,tuple):return [_wire(item) for item in value]
    return value


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
            for path in self._nodes:
                parent=path
                while parent!=b'/':
                    parent=parent.rpartition(b'/')[0] or b'/'
                    if parent not in self._nodes:self._routes.add(parent)
            for path in self._nodes:
                parent=path.rpartition(b'/')[0] or b'/'
                if path not in self._roots and self._nodes.get(parent,('unknown',))[0]!='directory':_hold()
            if len(self._nodes)>200064:_hold()
        except (ValueError,TypeError,UnicodeError,KeyError):_hold()

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
