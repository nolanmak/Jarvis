#!/usr/bin/env python3
"""Select external Cargo inputs for a controller-produced dependency seed.

Selection alone grants no artifact, producer, quiescence or CI authority. The
producer must run separately from candidate writers, and the controller must
verify its completion before sealing or installing any selected output.
"""
from collections import deque
from pathlib import PurePosixPath
import re


class InputsUnknown(Exception):
    def __init__(self):
        super().__init__('dependency input census unavailable')


def _text(value, limit=4096):
    return isinstance(value,str) and 0<len(value)<=limit and all(ord(char)>=32 for char in value)


def _external(source):
    if source is None:
        return False
    if source in ('registry+https://github.com/rust-lang/crates.io-index',
                  'registry+sparse+https://index.crates.io/'):
        return True
    if isinstance(source,str) and re.fullmatch(r'git\+https://[^\s#]+#(?:[0-9a-f]{40}|[0-9a-f]{64})',source):
        return True
    raise InputsUnknown()


def _path(value):
    if not _text(value) or not value.startswith('/') or '\\' in value:
        raise InputsUnknown()
    path=PurePosixPath(value)
    if '..' in path.parts or str(path)!=value:
        raise InputsUnknown()
    return path


def eligible_packages(metadata, *, source_root='/workspace'):
    """Exclude every path package and any external unit depending on one.

    This includes excluded workspace members, patched dependencies and local
    vendor packages. A package name or a successful artifact is insufficient:
    provenance follows Cargo's complete resolved package-ID dependency graph.
    Unknown/incomplete graphs cannot certify an empty or partial seed.
    """
    if (not isinstance(metadata,dict) or type(metadata.get('version')) is not int
            or metadata['version']!=1 or not isinstance(metadata.get('packages'),list)
            or not 0<len(metadata['packages'])<=20_000
            or not isinstance(metadata.get('resolve'),dict)
            or not isinstance(metadata['resolve'].get('nodes'),list)):
        raise InputsUnknown()
    source_root=_path(source_root)
    if not _path(metadata.get('workspace_root')).is_relative_to(source_root):
        raise InputsUnknown()
    packages={}
    external={}
    for package in metadata['packages']:
        if (not isinstance(package,dict) or not _text(package.get('id'))
                or not _text(package.get('name'),256) or 'source' not in package
                or package['id'] in packages):
            raise InputsUnknown()
        packages[package['id']]=package
        manifest=_path(package.get('manifest_path'))
        if manifest.name!='Cargo.toml':
            raise InputsUnknown()
        # Cargo source replacement can keep a registry source ID while loading
        # mutable vendored source from the candidate repository. Classify by
        # the entire controller export, not only Cargo's workspace membership.
        external[package['id']]=_external(package['source']) and not manifest.is_relative_to(source_root)
    dependencies={}
    features={}
    edges=0
    for node in metadata['resolve']['nodes']:
        if not isinstance(node,dict) or not _text(node.get('id')) or node['id'] in dependencies:
            raise InputsUnknown()
        children=node.get('dependencies');enabled=node.get('features')
        if (not isinstance(children,list) or not isinstance(enabled,list) or len(enabled)>1024
                or any(not _text(child) or child not in packages for child in children)
                or len(set(children))!=len(children)
                or any(not _text(feature,256) for feature in enabled)
                or len(set(enabled))!=len(enabled)):
            raise InputsUnknown()
        edges+=len(children)
        if edges>500_000:
            raise InputsUnknown()
        dependencies[node['id']]=children
        features[node['id']]=tuple(sorted(enabled))
    if dependencies.keys()!=packages.keys():
        raise InputsUnknown()
    reverse={key:[] for key in packages}
    remaining={key:len(children) for key,children in dependencies.items()}
    for parent,children in dependencies.items():
        for child in children:
            reverse[child].append(parent)
    ready=deque(key for key,count in remaining.items() if count==0)
    compatible={}
    while ready:
        key=ready.popleft()
        compatible[key]=external[key] and all(compatible[child] for child in dependencies[key])
        for parent in reverse[key]:
            remaining[parent]-=1
            if remaining[parent]==0:
                ready.append(parent)
    if len(compatible)!=len(packages):
        raise InputsUnknown()
    return {key:{'name':packages[key]['name'],'source':packages[key]['source'],'features':features[key]}
            for key in sorted(packages) if compatible[key]}
