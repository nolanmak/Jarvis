#!/usr/bin/env python3
"""Decode experimental failed-path observations, never a complete input proof.

Only the hex-string, PID-prefixed trace format is supported. These observations
do not prove collector ownership, frozen inputs, successful reads, directory
membership or producer quiescence. They cannot authorize artifact reuse or CI.
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


def failed_queries(trace,*,max_bytes=16*1024**2,max_lines=200000):
    """Return failed lookup data without resolving symlinks or collapsing '..'.

    Reassemble interleaved syscall entry/return lines only for their original
    PID and syscall. Truncated failures, missing resumes and unknown formats
    refuse observation. Successful operations and their fields are not validated
    or represented by this API; this is not a complete read-set decoder.
    """
    if (not isinstance(trace,bytes) or type(max_bytes) is not int or not 0<max_bytes<=16*1024**2
            or type(max_lines) is not int or not 0<max_lines<=200000 or len(trace)>max_bytes
            or trace and not trace.endswith(b'\n')):raise LookupUnavailable('unbounded or truncated trace')
    try:text=trace.decode('ascii')
    except UnicodeError:raise LookupUnavailable('trace is not hex-safe ASCII') from None
    pending={};result=[]
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
        failure=re.fullmatch(r'-1 (E[A-Z0-9]+)(?: \(.*\))?',call[3])
        if failure:
            if '...' in body:raise LookupUnavailable('abbreviated failed query')
            result.append(_failure(pid,call[1],call[2],failure[1]))
        elif re.match(r'[0-9]+(?:\s|<|$)',call[3]) is None:raise LookupUnavailable('unknown syscall result')
    if pending:raise LookupUnavailable('incomplete syscall trace')
    return result


def observation(events):
    """Lossless diagnostic serialization; deliberately carries no reuse claim."""
    encode=lambda value:None if value is None else base64.b64encode(value).decode('ascii')
    return {'version':1,'kind':'failed-path-query-observations',
        'events':[{**event,'directory':encode(event['directory']),'path':encode(event['path'])} for event in events],
        'input_closure_complete':False,'shipping_authority':False}
