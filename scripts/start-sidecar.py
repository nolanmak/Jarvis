#!/usr/bin/env python3
"""Own a browser/fetch/renderer socket across exec and recover it safely."""

import fcntl
import os
from pathlib import Path
import socket
import stat
import sys


FETCH_KEYS = {'FIRECRAWL_API_KEY', 'BRIGHTDATA_API_KEY', 'BRIGHTDATA_ZONE'}


def load_fetch_credentials(path):
    if not path.is_absolute():
        raise SystemExit('fetch credential path must be absolute')
    if not path.exists() and not path.is_symlink():
        return
    parent = path.parent.lstat()
    if not stat.S_ISDIR(parent.st_mode) or parent.st_uid != os.getuid() or parent.st_mode & 0o077:
        raise SystemExit('fetch credential directory must be owner-private')
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0))
    with os.fdopen(descriptor, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise SystemExit('fetch credential file must be owner-private (mode 0600)')
        content = stream.read(16385)
    if len(content) > 16384:
        raise SystemExit('fetch credential file is too large')
    values = {}
    for line in content.decode('utf-8').splitlines():
        if not line or line.startswith('#'):
            continue
        key, separator, value = line.partition('=')
        if separator != '=' or key not in FETCH_KEYS or not value or key in values:
            raise SystemExit('fetch credential file has an invalid or duplicate key')
        values[key] = value
    os.environ.update(values)


def main():
    if len(sys.argv) < 4 or sys.argv[1] not in {'browser', 'fetch', 'renderer'}:
        raise SystemExit('usage: start-sidecar.py browser|fetch|renderer SOCKET COMMAND [ARG ...]')
    name, raw_socket, *command = sys.argv[1:]
    if name == 'fetch':
        os.environ['DOTENV_CONFIG_PATH'] = '/dev/null'
        credential = os.environ.get('AUGMENTAGENT_FETCH_CREDENTIALS')
        if credential:
            load_fetch_credentials(Path(credential))
    path = Path(raw_socket)
    if not path.is_absolute() or len(os.fsencode(path)) >= 100:
        raise SystemExit('sidecar socket must be an absolute path shorter than 100 bytes')
    directory = path.parent
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise SystemExit('sidecar socket directory must be owner-private')
    lock_path = path.with_name(path.name + '.lock')
    descriptor = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NONBLOCK |
                         getattr(os, 'O_NOFOLLOW', 0), 0o600)
    info = os.fstat(descriptor)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise SystemExit('sidecar socket lock must be owner-private')
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as error:
        raise SystemExit(f'{name} socket is already owned') from error
    try:
        info = path.lstat()
    except FileNotFoundError:
        pass
    else:
        if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
            raise SystemExit('refusing to remove a non-socket or foreign sidecar path')
        peer = socket.socket(socket.AF_UNIX)
        peer.settimeout(0.25)
        try:
            peer.connect(str(path))
        except (ConnectionRefusedError, FileNotFoundError):
            path.unlink()
        else:
            raise SystemExit(f'{name} socket is already listening')
        finally:
            peer.close()
    os.set_inheritable(descriptor, True)
    os.execvpe(command[0], command, os.environ)


if __name__ == '__main__':
    main()
