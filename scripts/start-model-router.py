#!/usr/bin/env python3
"""Read owner-private 9Router credentials and exec its pinned server."""
import os
from pathlib import Path
import stat
import sys


def main():
    if len(sys.argv) != 4:
        raise SystemExit('usage: start-model-router.py ENV_FILE NODE SERVER')
    credential, node, server = map(Path, sys.argv[1:])
    if not all(path.is_absolute() for path in (credential, node, server)):
        raise SystemExit('router paths must be absolute')
    flags = os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0)
    descriptor = os.open(credential, flags)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise SystemExit('router credential file must be owner-private')
        lines = stream.read().splitlines()
    values = {}
    for line in lines:
        if not line or line.startswith('#'):
            continue
        key, marker, value = line.partition('=')
        if marker != '=' or key not in {'JWT_SECRET', 'INITIAL_PASSWORD'} or not value or key in values:
            raise SystemExit('router credential file is invalid')
        values[key] = value
    if set(values) != {'JWT_SECRET', 'INITIAL_PASSWORD'}:
        raise SystemExit('router credential file is incomplete')
    environment = dict(os.environ)
    environment.update(values)
    os.execve(node, [str(node), str(server)], environment)


if __name__ == '__main__':
    main()
