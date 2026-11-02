#!/usr/bin/env python3
"""Cargo unit selection for an isolated seed producer; never a reuse proof.

The caller still must prove producer identity/quiescence, seal actual bytes and
bind compiler/configuration/native generated inputs before publication.
"""
import importlib.util
from pathlib import Path
import re

spec=importlib.util.spec_from_file_location('seed_inputs',Path(__file__).with_name('ci-dependency-seed.py'))
inputs=importlib.util.module_from_spec(spec);spec.loader.exec_module(inputs)
InputsUnknown=inputs.InputsUnknown


def _build_directory(path,name,target_root):
    if not path.is_relative_to(target_root) or path.parent.parent.name!='build':
        raise InputsUnknown()
    directory=path.parent
    if re.fullmatch(re.escape(name)+r'-[0-9a-f]{16}',directory.name) is None:
        raise InputsUnknown()
    profile=directory.parent.parent
    if not 1<=len(profile.relative_to(target_root).parts)<=2:
        raise InputsUnknown()
    return str((profile/'.fingerprint'/directory.name).relative_to(target_root)),str(directory.relative_to(target_root))


def _record(selected,unit):
    fingerprint=unit['fingerprint_directory']
    if fingerprint in selected and selected[fingerprint]!=unit:
        raise InputsUnknown()
    selected[fingerprint]=unit


def select(metadata,messages,*,target_root,source_root='/workspace'):
    eligible=inputs.eligible_packages(metadata,source_root=source_root)
    packages={package['id']:package for package in metadata['packages']}
    target_root=inputs._path(target_root)
    if not isinstance(messages,list) or len(messages)>100_000:
        raise InputsUnknown()
    selected={}
    for message in messages:
        if not isinstance(message,dict):
            raise InputsUnknown()
        reason=message.get('reason')
        if reason in ('compiler-message','build-finished'):
            continue
        if reason not in ('compiler-artifact','build-script-executed') or not isinstance(message.get('package_id'),str) or message['package_id'] not in packages:
            raise InputsUnknown()
        key=message['package_id']
        if key not in eligible:
            continue
        if re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_-]*',eligible[key]['name']) is None:
            raise InputsUnknown()
        if reason=='build-script-executed':
            out=inputs._path(message.get('out_dir'))
            if out.name!='out':
                raise InputsUnknown()
            fingerprint,directory=_build_directory(out,eligible[key]['name'],target_root)
            declarations={}
            for field in ('linked_libs','linked_paths','cfgs'):
                values=message.get(field)
                if not isinstance(values,list) or len(values)>4096 or any(not inputs._text(value) for value in values):
                    raise InputsUnknown()
                declarations[field]=list(values)
            values=message.get('env')
            if (not isinstance(values,list) or len(values)>4096 or any(not isinstance(pair,list)
                    or len(pair)!=2 or any(not isinstance(value,str) or len(value)>65536 or '\0' in value for value in pair) for pair in values)):
                raise InputsUnknown()
            declarations['env']=[list(pair) for pair in values]
            _record(selected,{'package_id':key,'source':eligible[key]['source'],'target_kind':['custom-build-run'],
                'fingerprint_directory':fingerprint,'directory_trees':[directory],'files':[],
                'declarations':declarations,'unit_identity_status':'requires_producer_fingerprint'})
            continue
        features=message.get('features');target=message.get('target');profile=message.get('profile')
        if (not isinstance(features,list) or any(not isinstance(value,str) for value in features)
                or len(set(features))!=len(features) or not set(features)<=set(eligible[key]['features'])
                or not isinstance(target,dict) or not isinstance(target.get('kind'),list)
                or target['kind'] not in (['lib'],['rlib'],['proc-macro'],['dylib'],['staticlib'],['cdylib'],['custom-build'])
                or not isinstance(target.get('name'),str)
                or re.fullmatch(r'[A-Za-z_][A-Za-z0-9_-]*',target['name']) is None
                or not isinstance(profile,dict) or set(profile)!={'opt_level','debuginfo','debug_assertions','overflow_checks','test'}
                or profile['opt_level'] not in ('0','1','2','3','s','z')
                or type(profile['debuginfo']) is not int or not 0<=profile['debuginfo']<=2
                or any(type(profile[field]) is not bool for field in ('debug_assertions','overflow_checks','test'))
                or type(message.get('fresh')) is not bool):
            raise InputsUnknown()
        package_root=inputs._path(packages[key]['manifest_path']).parent
        if not inputs._path(target.get('src_path')).is_relative_to(package_root):
            raise InputsUnknown()
        filenames=message.get('filenames')
        if not isinstance(filenames,list) or not 1<=len(filenames)<=16:
            raise InputsUnknown()
        if target['kind']==['custom-build']:
            paths=[inputs._path(value) for value in filenames]
            locations=[_build_directory(path,eligible[key]['name'],target_root) for path in paths]
            if (any(path.name!='build-script-build' for path in paths) or len(set(locations))!=1
                    or message.get('executable') not in filenames):
                raise InputsUnknown()
            fingerprint,directory=locations[0]
            _record(selected,{'package_id':key,'source':eligible[key]['source'],'features':sorted(features),
                'profile':dict(profile),'target_name':target['name'],'target_kind':['custom-build'],
                'files':[str(path.relative_to(target_root)) for path in paths],'fingerprint_directory':fingerprint,
                'directory_trees':[directory],'unit_identity_status':'requires_producer_fingerprint'})
            continue
        files=[];unit_hash=None;profile_root=None;dep_info=None
        crate=target['name'].replace('-','_')
        for filename in filenames:
            path=inputs._path(filename)
            if not path.is_relative_to(target_root) or path.parent.name!='deps':
                raise InputsUnknown()
            matched=re.fullmatch(r'(?:lib)?'+re.escape(crate)+r'-([0-9a-f]{16})\.(?:rlib|rmeta|so|dylib|dll|a|lib)',path.name)
            if matched is None:
                raise InputsUnknown()
            current_hash=matched[1];current_root=path.parent.parent
            if unit_hash is not None and (current_hash!=unit_hash or current_root!=profile_root):
                raise InputsUnknown()
            unit_hash=current_hash;profile_root=current_root
            files.append(str(path.relative_to(target_root)))
            dep_info=str((path.parent/(crate+'-'+unit_hash+'.d')).relative_to(target_root))
        fingerprint=str((profile_root/'.fingerprint'/(eligible[key]['name']+'-'+unit_hash)).relative_to(target_root))
        unit={'package_id':key,'source':eligible[key]['source'],'features':sorted(features),
              'profile':dict(profile),'target_name':target['name'],'target_kind':list(target['kind']),
              'files':sorted(set(files)),'fingerprint_directory':fingerprint,'dep_info':dep_info,
              'directory_trees':[],'unit_identity_status':'requires_producer_fingerprint'}
        _record(selected,unit)
    return [selected[key] for key in sorted(selected)]
