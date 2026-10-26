#!/usr/bin/env python3
"""Read-only, secret-free route receipts for explicit native acceptance probes."""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
from urllib.parse import urlsplit


def default_path(variable, filename):
    return Path(os.environ.get(variable) or Path(os.environ.get('XDG_CONFIG_HOME', Path.home()/'.config'))/'augmentagent'/filename)


def private_config(path):
    path=path.absolute()
    if not path.exists():
        return path, None, None
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
    with os.fdopen(fd,'rb') as stream:
        info=os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_mode&0o077 or info.st_size>1024*1024:
            raise ValueError('operator route config must be private and bounded')
        data=stream.read()
    return path, json.loads(data), hashlib.sha256(data).hexdigest()


def snapshot(router_path, selection_path):
    router_path, router, router_hash=private_config(router_path)
    selection_path, selection, selection_hash=private_config(selection_path)
    allowed={'claude','codex','qwen','glm'}
    selected=(selection or {}).get('default')
    if selected is not None and selected not in allowed:
        raise ValueError('invalid operator selection')
    details={'mode':'direct','models':{},'endpoint':None}
    if router is not None:
        mode=router.get('mode')
        if router.get('version')!=1 or mode not in allowed|{'auto','direct'}:
            raise ValueError('invalid operator router mode')
        endpoint=urlsplit(router.get('base_url',''))
        if endpoint.scheme not in ('http','https') or not endpoint.hostname or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment:
            raise ValueError('router endpoint cannot be safely described')
        models={}
        for provider, tiers in router.get('models',{}).items():
            if provider not in allowed or not isinstance(tiers,dict):
                raise ValueError('invalid operator model mapping')
            models[provider]={}
            for tier, model in tiers.items():
                if tier not in ('quality','fast') or not isinstance(model,str) or not re.fullmatch(r'[A-Za-z0-9_./:-]{1,160}',model):
                    raise ValueError('invalid operator model tier')
                models[provider][tier]=model
        details={'mode':mode,'models':models,'endpoint':{'scheme':endpoint.scheme,'host':endpoint.hostname,'port':endpoint.port,'path':endpoint.path}}
    evidence={'version':1,'router_path':str(router_path),'router_sha256':router_hash,'selection_path':str(selection_path),
        'selection_sha256':selection_hash,'default_selection':selected,'operator_route':details,
        'scope':'issue builder honors operator route; independent roles remain native-pinned; adoption fixture explicitly pins its whole pipeline to native Codex'}
    environment={'AUGMENTAGENT_MODEL_ROUTER_CONFIG':str(router_path),'AUGMENTAGENT_MODEL_SELECTION_CONFIG':str(selection_path)}
    return evidence,environment


def unchanged(evidence):
    current,_=snapshot(Path(evidence['router_path']),Path(evidence['selection_path']))
    return current==evidence
