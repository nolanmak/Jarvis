#!/usr/bin/env python3
"""Read-only Q_GETFMT experiment; never changes or synchronizes quotas.

Linux x86-64 only: syscall 443 is verified against the installed UAPI before use.
Paths are held through directory descriptors. Results are samples, not capacity
grants, filesystem-profile certification, or evidence from the deployed worker.
"""
import argparse
import ctypes
import errno
import json
import os
from pathlib import Path
import platform
import re
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directories', nargs='+', type=Path)
    args = parser.parse_args()
    if not sys.platform.startswith('linux') or platform.machine() != 'x86_64' or os.geteuid() == 0:
        parser.error('requires non-root Linux x86-64')
    header = Path('/usr/include/x86_64-linux-gnu/asm/unistd_64.h').read_text()
    if not re.search(r'^#define\s+__NR_quotactl_fd\s+443\s*$', header, re.M):
        parser.error('installed UAPI does not confirm quotactl_fd syscall number')
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    samples = []
    for index, directory in enumerate(args.directories):
        fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        try:
            info = os.fstat(fd)
            queries = []
            for kind, name in enumerate(('user', 'group', 'project')):
                output = ctypes.c_uint32()
                ctypes.set_errno(0)
                result = libc.syscall(ctypes.c_long(443), ctypes.c_int(fd),
                    ctypes.c_uint32((0x800004 << 8) | kind), ctypes.c_int(0), ctypes.byref(output))
                error = ctypes.get_errno() if result < 0 else 0
                queries.append({'type': name,
                    'state': 'active' if result == 0 else 'inactive' if error == errno.ESRCH else 'unknown',
                    'errno': errno.errorcode.get(error, str(error)) if error else None,
                    'format': output.value if result == 0 else None})
            samples.append({'sample': index,
                'device': {'major': os.major(info.st_dev), 'minor': os.minor(info.st_dev)},
                'quotas': queries})
        finally:
            os.close(fd)
    print(json.dumps({'experiment_only': True, 'samples': samples}, indent=2))


if __name__ == '__main__':
    main()
