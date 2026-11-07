#!/usr/bin/env python3
"""Decode experimental pathname observations, never a complete input proof.

Only the hex-string, PID-prefixed trace format is supported. These observations
do not prove collector ownership, frozen inputs, a complete syscall/input set or
producer quiescence. They cannot authorize artifact reuse or CI.
"""
import base64
import re


class LookupUnavailable(ValueError):
    pass


_PATH_ARGUMENT={'open':0,'openat':1,'openat2':1,'stat':0,'lstat':0,
    'newfstatat':1,'statx':1,'access':0,'faccessat':1,'faccessat2':1,
    'readlink':0,'readlinkat':1}
_SUPPORTED=set(_PATH_ARGUMENT)|{'getdents','getdents64','chdir','fchdir'}
_HEX=re.compile(r'(?:\\x[0-9a-fA-F]{2})*\Z')


def _bytes(value):
    if not _HEX.fullmatch(value):raise LookupUnavailable('non-hex or abbreviated path')
    result=bytes.fromhex(value.replace('\\x',''))
    if len(result)>4096 or b'\0' in result:raise LookupUnavailable('invalid pathname')
    return result


def _arguments(text):
    result=[];begin=0;stack=[];quoted=False
    for index,char in enumerate(text):
        if char=='"':quoted=not quoted
        if quoted:continue
        if char in '([{<':stack.append(char)
        elif char in ')]}>':
            if not stack or '([{<'[')]}>'.index(char)]!=stack.pop():
                raise LookupUnavailable('unbalanced trace arguments')
        elif char==',' and not stack:
            result.append(text[begin:index].strip());begin=index+1
    if quoted or stack:raise LookupUnavailable('incomplete trace arguments')
    result.append(text[begin:].strip())
    return result


def _failure(pid,name,arguments,code):
    if name not in _PATH_ARGUMENT:raise LookupUnavailable('unsupported failed query')
    fields=_arguments(arguments);index=_PATH_ARGUMENT[name]
    if len(fields)<=index:raise LookupUnavailable('missing path argument')
    field=fields[index]
    if not (field.startswith('"') and field.endswith('"')):raise LookupUnavailable('pathname not decoded')
    path=_bytes(field[1:-1]);directory=None
    if index:
        descriptor=re.fullmatch(r'(?:AT_FDCWD|[0-9]+)<([^<>]*)>',fields[0])
        if descriptor is None:raise LookupUnavailable('directory descriptor not decoded')
        directory=_bytes(descriptor[1])
        if not directory.startswith(b'/'):raise LookupUnavailable('directory is not literal absolute data')
    intent='metadata'
    if name in ('open','openat','openat2'):
        if len(fields)<=index+1:raise LookupUnavailable('missing open flags')
        flags=fields[index+1]
        if any(flag in flags for flag in ('O_WRONLY','O_RDWR','O_CREAT','O_TRUNC','O_APPEND')):intent='write'
        elif 'O_RDONLY' in flags:intent='read'
        elif 'O_PATH' in flags:intent='metadata'
        else:intent='unknown'
    return {'pid':pid,'syscall':name,'directory':directory,'path':path,
        'errno':code,'intent':intent,'arguments':arguments}


def _calls(trace,max_bytes,max_lines):
    if (not isinstance(trace,bytes) or type(max_bytes) is not int or not 0<max_bytes<=16*1024**2
            or type(max_lines) is not int or not 0<max_lines<=200000 or len(trace)>max_bytes
            or trace and not trace.endswith(b'\n')):raise LookupUnavailable('unbounded or truncated trace')
    try:text=trace.decode('ascii')
    except UnicodeError:raise LookupUnavailable('trace is not hex-safe ASCII') from None
    pending={}
    for number,line in enumerate(text.splitlines(),1):
        if number>max_lines or len(line)>1024**2:raise LookupUnavailable('trace limit exceeded')
        match=re.fullmatch(r'\s*([0-9]+)\s+(.*)',line)
        if match is None:raise LookupUnavailable('missing process identity')
        pid=int(match[1]);body=match[2]
        if not 0<pid<2**32:raise LookupUnavailable('invalid process identity')
        if body.startswith('--- ') or body.startswith('+++ '):continue
        resumed=re.fullmatch(r'<\.\.\. ([a-z0-9_]+) resumed>(.*)',body)
        if resumed:
            prior=pending.pop(pid,None)
            if prior is None or prior[0]!=resumed[1]:raise LookupUnavailable('foreign or missing syscall entry')
            body=prior[1]+resumed[2]
        elif body.endswith('<unfinished ...>'):
            entry=re.match(r'([a-z0-9_]+)\(',body)
            if entry is None or pid in pending or len(pending)>=4096:raise LookupUnavailable('ambiguous syscall entry')
            pending[pid]=(entry[1],body[:-len('<unfinished ...>')]);continue
        elif pid in pending:raise LookupUnavailable('missing syscall return')
        call=re.fullmatch(r'([a-z0-9_]+)\((.*)\)\s+=\s+(.+)',body)
        if call is None or call[1] not in _SUPPORTED:raise LookupUnavailable('unknown syscall format')
        yield pid,call[1],call[2],call[3]
    if pending:raise LookupUnavailable('incomplete syscall trace')


def failed_queries(trace,*,max_bytes=16*1024**2,max_lines=200000):
    """Return failed lookup data without resolving symlinks or collapsing '..'.

    Reassemble interleaved syscall entry/return lines only for their original
    PID and syscall. Truncated failures, missing resumes and unknown formats
    refuse observation. Successful operations and their fields are not validated
    or represented by this API; this is not a complete read-set decoder.
    """
    result=[]
    for pid,name,arguments,value in _calls(trace,max_bytes,max_lines):
        failure=re.fullmatch(r'-1 (E[A-Z0-9]+)(?: \(.*\))?',value)
        if failure:
            if '...' in arguments:raise LookupUnavailable('abbreviated failed query')
            result.append(_failure(pid,name,arguments,failure[1]))
        elif re.match(r'[0-9]+(?:\s|<|$)',value) is None:raise LookupUnavailable('unknown syscall result')
    return result


def _descriptor(field):
    return _descriptor_info(field)['resolved_path']


def _descriptor_info(field):
    match=re.fullmatch(r'(?:AT_FDCWD|[0-9]+)<([^<>]*)(?:<(char|block) ([0-9]+):([0-9]+)>)?>',field)
    if match is None:raise LookupUnavailable('descriptor path not decoded')
    path=_bytes(match[1])
    if not path.startswith(b'/'):raise LookupUnavailable('descriptor path is not absolute')
    result={'resolved_path':path,'descriptor_kind':'path'}
    if match[2]:
        result.update(descriptor_kind='character-device' if match[2]=='char' else 'block-device',
            device={'major':int(match[3]),'minor':int(match[4])})
    return result


def _directory_query(pid,name,fields,value):
    if len(fields)!=3 or not fields[2].isdigit():raise LookupUnavailable('invalid directory query')
    size=int(fields[2])
    if not 0<size<=1024**2 or not 0<=value<=size:raise LookupUnavailable('directory query limit')
    directory=_descriptor(fields[0]);payload=fields[1]
    if not (payload.startswith('[') and payload.endswith(']')):raise LookupUnavailable('directory entries not decoded')
    entries=[];total=0
    for field in _arguments(payload[1:-1]) if payload!='[]' else []:
        match=re.fullmatch(r'\{d_ino=([0-9]+), d_off=(-?[0-9]+), d_reclen=([0-9]+), d_type=(DT_[A-Z]+), d_name="((?:\\x[0-9a-fA-F]{2})*)"\}',field)
        if match is None:raise LookupUnavailable('unknown directory entry format')
        length=int(match[3]);name_bytes=_bytes(match[5])
        if not name_bytes or b'/' in name_bytes or not 0<length<=4096:raise LookupUnavailable('invalid directory entry')
        total+=length
        entries.append({'inode':int(match[1]),'offset':int(match[2]),'record_length':length,
            'type':match[4],'name':name_bytes})
    if total!=value:raise LookupUnavailable('incomplete directory entries')
    return {'pid':pid,'syscall':name,'directory':directory,'entries':entries,
        'result':value,'outcome':'success','intent':'directory','errno':None}


def path_queries(trace,*,max_bytes=16*1024**2,max_lines=200000):
    """Decode pathname results, opened paths, symlink bytes and directory reads.

    This is a bounded syntax decoder, not a trusted collector. The caller still
    needs original process attribution, a frozen namespace, file-content and
    metadata identity, complete syscall coverage and original unit quiescence.
    Successful relative non-at paths retain directory=None, requiring original
    cwd/lineage evidence; no path is normalized or resolved by this function.
    """
    result=[]
    for pid,name,arguments,value in _calls(trace,max_bytes,max_lines):
        if '...' in arguments:raise LookupUnavailable('abbreviated pathname query')
        failure=re.fullmatch(r'-1 (E[A-Z0-9]+)(?: \(.*\))?',value)
        if failure:
            event=_failure(pid,name,arguments,failure[1])
            result.append({**event,'outcome':'failure','result':-1});continue
        successful=re.fullmatch(r'([0-9]+)(<.*>)?',value)
        if successful is None:raise LookupUnavailable('unknown syscall result')
        number=int(successful[1]);fields=_arguments(arguments)
        if name in ('getdents','getdents64'):
            if successful[2] is not None:raise LookupUnavailable('unexpected directory result path')
            result.append(_directory_query(pid,name,fields,number));continue
        if name not in _PATH_ARGUMENT:raise LookupUnavailable('unsupported pathname state change')
        event=_failure(pid,name,arguments,None)
        event.update(outcome='success',result=number)
        index=_PATH_ARGUMENT[name]
        if name in ('open','openat','openat2'):
            if successful[2] is None:raise LookupUnavailable('opened descriptor path missing')
            event.update(_descriptor_info(value))
        elif name in ('readlink','readlinkat'):
            if len(fields)!=index+3 or not fields[-1].isdigit():raise LookupUnavailable('invalid readlink query')
            output=fields[index+1]
            if not (output.startswith('"') and output.endswith('"')):raise LookupUnavailable('readlink bytes missing')
            target=_bytes(output[1:-1])
            if number!=len(target) or number>int(fields[-1]):raise LookupUnavailable('incomplete readlink result')
            event['link_bytes']=target
        elif name in ('stat','lstat','newfstatat','statx'):
            if number!=0 or successful[2] is not None:raise LookupUnavailable('invalid metadata result')
            position=index+3 if name=='statx' else index+1
            if len(fields)<=position:raise LookupUnavailable('metadata missing')
            metadata=fields[position]
            if not (metadata.startswith('{') and metadata.endswith('}')):raise LookupUnavailable('metadata not decoded')
            event['metadata']=metadata
        elif number!=0 or successful[2] is not None:raise LookupUnavailable('invalid access result')
        result.append(event)
    return result


def observation(events):
    """Lossless diagnostic serialization; deliberately carries no reuse claim."""
    encode=lambda value:None if value is None else base64.b64encode(value).decode('ascii')
    return {'version':1,'kind':'failed-path-query-observations',
        'events':[{**event,'directory':encode(event['directory']),'path':encode(event['path'])} for event in events],
        'input_closure_complete':False,'shipping_authority':False}
