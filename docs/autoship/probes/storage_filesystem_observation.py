#!/usr/bin/env python3
"""Read-only Linux x86-64 filesystem UUID/counter experiment.

The generic UAPI is _IOR(0x15, 0, struct fsuuid2), whose length byte and UUID
occupy 17 bytes. This script never invokes a SET ioctl or reads a block device.
It prints comparison facts, not stable UUIDs or an admission/release capability.
Its replacement experiment mutates only private disposable directories.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import platform
import sys
import tempfile


GETFSUUID = (2 << 30) | (17 << 16) | (0x15 << 8)


def identity(fd):
    result = bytearray(17)
    fcntl.ioctl(fd, GETFSUUID, result, True)
    if result[0] != 16 or not any(result[1:]):
        raise ValueError('requires a nonzero 16-byte filesystem UUID')
    info = os.fstat(fd)
    return info.st_dev, info.st_ino, bytes(result[1:])


def open_directory(path):
    return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)


def replacement_probe():
    with tempfile.TemporaryDirectory(prefix='autoship-fs-identity-') as name:
        root = Path(name)
        selected = root / 'selected'
        selected.mkdir(mode=0o700)
        original = open_directory(selected)
        try:
            before = identity(original)
            selected.rename(root / 'original')
            selected.mkdir(mode=0o700)
            replacement = open_directory(selected)
            try:
                after = identity(replacement)
                facts = {'held_object_unchanged': identity(original) == before,
                    'replacement_detected': after != before,
                    'same_filesystem_despite_distinct_directories':
                        after[0] == before[0] and after[2] == before[2] and after[1] != before[1]}
            finally:
                os.close(replacement)
            if not all(facts.values()):
                raise RuntimeError('disposable directory identity expectations failed')
            return facts
        finally:
            os.close(original)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directories', nargs='+', type=Path)
    args = parser.parse_args()
    if not sys.platform.startswith('linux') or platform.machine() != 'x86_64' or os.geteuid() == 0:
        parser.error('requires non-root Linux x86-64')
    samples = []
    identities = []
    for index, path in enumerate(args.directories):
        fd = open_directory(path)
        try:
            before = identity(fd)
            counters = os.fstatvfs(fd)
            after = identity(fd)
            if before != after:
                raise RuntimeError('filesystem/object identity changed during sample')
            identities.append(before)
            samples.append({'sample': index, 'device': {'major': os.major(before[0]),
                'minor': os.minor(before[0])}, 'uuid_length': len(before[2]),
                'identity_stable': True, 'fragment_bytes': counters.f_frsize,
                'available_bytes': counters.f_bavail * counters.f_frsize,
                'available_inodes': counters.f_favail})
        finally:
            os.close(fd)
    comparisons = []
    for left, first in enumerate(identities):
        for right in range(left + 1, len(identities)):
            second = identities[right]
            comparisons.append({'samples': [left, right],
                'same_uuid': first[2] == second[2], 'same_device': first[0] == second[0],
                'identity_collision': (first[2] == second[2]) != (first[0] == second[0])})
    print(json.dumps({'experiment_only': True, 'samples': samples, 'comparisons': comparisons,
        'directory_replacement': replacement_probe()}, indent=2))


if __name__ == '__main__':
    main()
