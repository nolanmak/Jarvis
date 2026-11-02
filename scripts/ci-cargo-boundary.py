#!/usr/bin/env python3
"""Read-only capability probe for the proposed protected Cargo planner.

This does not install a filter, launch a privileged tool, protect metadata or
create a passing proof. Kernel support is necessary, not execution authority.
The supported execution mechanism and its integration remain unfinished.
"""
import argparse
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import platform
import stat
import struct
import sys

_spec=importlib.util.spec_from_file_location('cargo_boundary_cache',Path(__file__).with_name('ci-cache-generation.py'))
_cache=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(_cache)
FreshnessHold=_cache.FreshnessHold


class _Sizes(ctypes.Structure):
    _fields_=[('notification',ctypes.c_ushort),('response',ctypes.c_ushort),('data',ctypes.c_ushort)]


class _Iovec(ctypes.Structure):
    _fields_=[('base',ctypes.c_void_p),('length',ctypes.c_size_t)]


def _kernel_capabilities():
    # Linux x86-64 syscall numbers; observe() rejects all other ABIs first.
    libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long
    def call(number,*arguments):
        value=libc.syscall(number,*arguments)
        if value<0:raise OSError(ctypes.get_errno(),'kernel capability unavailable')
        return value
    action=ctypes.c_uint(0x7fc00000)  # SECCOMP_RET_USER_NOTIF
    call(317,2,0,ctypes.byref(action))  # SECCOMP_GET_ACTION_AVAIL
    sizes=_Sizes();call(317,3,0,ctypes.byref(sizes))  # SECCOMP_GET_NOTIF_SIZES
    if not (80<=sizes.notification<=4096 and 24<=sizes.response<=4096 and 64<=sizes.data<=4096):
        raise ValueError('unsupported notification ABI')
    source=ctypes.create_string_buffer(b'ci-cargo-boundary-probe')
    destination=ctypes.create_string_buffer(len(source))
    local=_Iovec(ctypes.addressof(destination),len(destination))
    remote=_Iovec(ctypes.addressof(source),len(source))
    observed=call(310,os.getpid(),ctypes.byref(local),1,ctypes.byref(remote),1,0)
    if observed!=len(source) or destination.raw!=source.raw:raise ValueError('incomplete process read')
    descriptor=call(434,os.getpid(),0)  # pidfd_open
    os.close(descriptor)
    return {'seccomp_user_notification':True,'process_vm_self_read':True,'pidfd':True,
            'notification_sizes':{'notification':sizes.notification,'response':sizes.response,'data':sizes.data}}


def _elf_interpreter(cargo):
    cargo=Path(cargo)
    if not cargo.is_absolute() or cargo.resolve(strict=True)!=cargo:raise ValueError('noncanonical runtime')
    descriptor=os.open(cargo,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
    with os.fdopen(descriptor,'rb') as stream:
        info=os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode):raise ValueError('nonregular runtime')
        header=stream.read(64)
        if (len(header)!=64 or header[:7]!=b'\x7fELF\x02\x01\x01'
                or struct.unpack_from('<H',header,16)[0] not in (2,3)
                or struct.unpack_from('<H',header,18)[0]!=62
                or struct.unpack_from('<I',header,20)[0]!=1
                or struct.unpack_from('<H',header,52)[0]!=64):raise ValueError('unsupported executable ABI')
        offset=struct.unpack_from('<Q',header,32)[0]
        width,count=struct.unpack_from('<HH',header,54)
        if width!=56 or not 1<=count<=128 or offset<64 or offset+width*count>info.st_size:
            raise ValueError('unsupported program headers')
        interpreter=None
        for index in range(count):
            stream.seek(offset+width*index);row=stream.read(width)
            if len(row)!=width:raise ValueError('truncated runtime')
            if struct.unpack_from('<I',row)[0]!=3:continue  # PT_INTERP
            start=struct.unpack_from('<Q',row,8)[0];length=struct.unpack_from('<Q',row,32)[0]
            if interpreter is not None or not 2<=length<=4096 or start+length>info.st_size:
                raise ValueError('unsupported interpreter')
            stream.seek(start);value=stream.read(length)
            if len(value)!=length or not value.endswith(b'\0') or b'\0' in value[:-1]:raise ValueError('malformed interpreter')
            interpreter=value[:-1].decode('ascii')
            if not interpreter.startswith('/'):raise ValueError('relative interpreter')
        if interpreter is None:raise ValueError('no dynamic interpreter')
        after=os.fstat(stream.fileno())
        if (info.st_size,info.st_mtime_ns,info.st_ctime_ns)!=(after.st_size,after.st_mtime_ns,after.st_ctime_ns):
            raise ValueError('runtime changed during probe')
        return interpreter


def observe(cargo):
    try:
        if sys.platform!='linux' or platform.machine()!='x86_64':raise ValueError('unsupported architecture')
        interpreter=_elf_interpreter(cargo)
        capabilities=_kernel_capabilities()
        return {'version':1,'kind':'ci-cargo-execution-capabilities','architecture':'x86_64',
                'cargo_elf_interpreter':interpreter,**capabilities,
                'enforcement_established':False,'shipping_authority':False}
    except (OSError,ValueError,struct.error) as error:
        raise FreshnessHold('freshness_unknown',str(getattr(error,'errno',None)).encode()) from None


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--cargo',required=True);args=parser.parse_args()
    try:result=observe(args.cargo)
    except FreshnessHold as error:
        print(json.dumps({'version':1,'status':'deferred','freshness_hold':error.diagnostic(),
                          'enforcement_established':False,'shipping_authority':False}))
        return 2
    print(json.dumps(result));return 0


if __name__=='__main__':raise SystemExit(main())
