#!/usr/bin/env python3
"""Exec the local adapter with secrets from an owner-private file."""
import os
from pathlib import Path
import stat
import sys


def main():
    if len(sys.argv) != 3:
        raise SystemExit('usage: start-runpod-adapter.py ENV_FILE SERVER')
    credential, server = map(Path, sys.argv[1:])
    if not credential.is_absolute() or not server.is_absolute():
        raise SystemExit('adapter paths must be absolute')
    descriptor = os.open(credential, os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0))
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise SystemExit('adapter credential file must be owner-private')
        lines = stream.read().splitlines()
    values = {}
    for line in lines:
        if not line or line.startswith('#'):
            continue
        key, marker, value = line.partition('=')
        if marker != '=' or key not in {'RUNPOD_API_KEY', 'ADAPTER_API_KEY'} or not value or key in values:
            raise SystemExit('adapter credential file is invalid')
        values[key] = value
    if set(values) != {'RUNPOD_API_KEY', 'ADAPTER_API_KEY'}:
        raise SystemExit('adapter credential file is incomplete')
    environment = dict(os.environ)
    environment.update(values)
    os.execve(sys.executable, [sys.executable, '-u', str(server)], environment)


if __name__ == '__main__':
    main()
