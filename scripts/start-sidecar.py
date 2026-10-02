#!/usr/bin/env python3
"""Own a sidecar socket across exec and recover it safely."""

import fcntl
import os
from pathlib import Path
import socket
import stat
import sys


FETCH_KEYS = {'FIRECRAWL_API_KEY', 'BRIGHTDATA_API_KEY', 'BRIGHTDATA_ZONE'}
VOICE_KEYS = {
    'AUGMENTAGENT_DISCORD_STT_PROVIDER', 'AUGMENTAGENT_DISCORD_TTS_PROVIDER',
    'DEEPGRAM_API_KEY', 'ELEVENLABS_API_KEY', 'ELEVENLABS_VOICE_ID',
}


def load_credentials(path, allowed_keys, label, required=False, allow_blank=False):
    if not path.is_absolute():
        raise SystemExit(f'{label} credential path must be absolute')
    if not path.exists() and not path.is_symlink():
        if required:
            raise SystemExit(f'{label} credential file is missing')
        return
    parent = path.parent.lstat()
    if not stat.S_ISDIR(parent.st_mode) or parent.st_uid != os.getuid() or parent.st_mode & 0o077:
        raise SystemExit(f'{label} credential directory must be owner-private')
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0))
    except OSError as error:
        raise SystemExit(f'{label} credential file cannot be opened safely') from error
    with os.fdopen(descriptor, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise SystemExit(f'{label} credential file must be owner-private (mode 0600)')
        content = stream.read(16385)
    if len(content) > 16384:
        raise SystemExit(f'{label} credential file is too large')
    values = {}
    for line in content.decode('utf-8').splitlines():
        if not line or line.startswith('#'):
            continue
        key, separator, value = line.partition('=')
        if separator != '=' or key not in allowed_keys or key in values or '\x00' in value or (not value and not allow_blank):
            raise SystemExit(f'{label} credential file has an invalid or duplicate key')
        if key in {'AUGMENTAGENT_DISCORD_STT_PROVIDER', 'AUGMENTAGENT_DISCORD_TTS_PROVIDER'} and value not in {'deepgram', 'elevenlabs'}:
            raise SystemExit('voice credential file has an invalid provider')
        values[key] = value
    os.environ.update(values)


def main():
    if len(sys.argv) < 4 or sys.argv[1] not in {'browser', 'fetch', 'renderer', 'discord-voice'}:
        raise SystemExit('usage: start-sidecar.py browser|fetch|renderer|discord-voice SOCKET COMMAND [ARG ...]')
    name, raw_socket, *command = sys.argv[1:]
    if name == 'fetch':
        os.environ['DOTENV_CONFIG_PATH'] = '/dev/null'
        credential = os.environ.get('AUGMENTAGENT_FETCH_CREDENTIALS')
        if credential:
            load_credentials(Path(credential), FETCH_KEYS, 'fetch')
    elif name == 'discord-voice':
        os.environ.pop('NODE_OPTIONS', None)
        os.environ.pop('DISCORD_BOT_TOKEN', None)
        os.environ['DOTENV_CONFIG_PATH'] = '/dev/null'
        credential = os.environ.get('AUGMENTAGENT_VOICE_CREDENTIALS')
        if not credential:
            raise SystemExit('voice credential path is missing')
        load_credentials(Path(credential), VOICE_KEYS, 'voice', required=True, allow_blank=True)
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
