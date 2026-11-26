#!/usr/bin/env python3
"""Forced SSH command: build SHA or legacy SCP read of one named artifact."""
import os
from pathlib import Path
import re
import shlex
import sys

ROOT = Path('/srv/jarvis-remote-builder')
NAMES = {'augmentagent', 'augmentagent-mcp-memory', 'manifest.json'}


def dispatch(command):
    args = shlex.split(command)
    if len(args) == 2 and args[0] == 'build' and re.fullmatch('[0-9a-f]{40}', args[1]):
        script = '/usr/local/lib/jarvis-remote-builder/build.sh'
        os.execv(script, [script, args[1]])
    if len(args) == 3 and args[:2] == ['scp', '-f']:
        parts = args[2].split('/')
        if len(parts) == 2 and re.fullmatch('[0-9a-f]{40}', parts[0]) and parts[1] in NAMES:
            path = ROOT/'artifacts'/parts[0]/parts[1]
            if path.is_file() and not path.is_symlink():
                os.execv('/usr/bin/scp', ['scp', '-f', '--', str(path)])
    raise ValueError('only build SHA or an exact read-only artifact download is allowed')


if __name__ == '__main__':
    try:
        dispatch(os.environ.get('SSH_ORIGINAL_COMMAND', ''))
    except (ValueError, OSError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(75)
