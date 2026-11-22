#!/usr/bin/env python3
"""Updater transport. All remote/integrity/install failures defer (75), never build locally.

Called only by run_update_build; see setup.sh for opt-in configuration and protocol.
SSH host keys must already be verified. SCP uses legacy read protocol because the
forced key permits only exact artifact reads, not a general SFTP session.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile

NAMES = ('augmentagent', 'augmentagent-mcp-memory')
TARGET = 'x86_64-unknown-linux-gnu'


def install(stage, release):
    """Validate both first; per-file atomic replacement, rollback on install error."""
    previous = {}
    for name in NAMES:
        dst = release/name
        if dst.exists() or dst.is_symlink():
            if not stat.S_ISREG(dst.lstat().st_mode):
                raise ValueError('existing artifact is not a regular file')
            backup = stage/('previous-'+name)
            os.link(dst, backup)
            previous[name] = backup
        else:
            previous[name] = None
    installed = []
    try:
        for name in NAMES:
            os.replace(stage/name, release/name)
            installed.append(name)
    except OSError:
        for name in reversed(installed):
            if previous[name] is None:
                (release/name).unlink()
            else:
                os.replace(previous[name], release/name)
        raise
    fd = os.open(release, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024*1024), b''):
            h.update(block)
    return h.hexdigest()


def fetch(host, sha, target, repo):
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        raise ValueError('deployment SHA must be a full Git commit')
    if not re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_.@:-]*', host):
        raise ValueError('invalid SSH host')
    cargo = subprocess.check_output(['git', '-C', str(repo), 'show', sha+':Cargo.toml'], text=True)
    match = re.search(r'\[workspace.package\].*?^version\s*=\s*"([0-9A-Za-z.+-]+)"', cargo, re.M|re.S)
    if not match:
        raise ValueError('requested commit has no workspace version')
    version = match.group(1)
    options = ['-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes',
               '-o', 'ConnectTimeout=10', '-o', 'ServerAliveInterval=30',
               '-o', 'ServerAliveCountMax=3']
    if os.environ.get('AUGMENTAGENT_REMOTE_BUILD_IDENTITY'):
        options += ['-i', os.environ['AUGMENTAGENT_REMOTE_BUILD_IDENTITY'], '-o', 'IdentitiesOnly=yes']
    subprocess.run(['ssh', '-T']+options+[host, 'build '+sha], check=True,
                   timeout=int(os.environ.get('AUGMENTAGENT_REMOTE_BUILD_TIMEOUT', '3600')))
    release = target/'release'
    release.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.remote-update-', dir=release) as temporary:
        stage = Path(temporary)
        for name in ('manifest.json',)+NAMES:
            subprocess.run(['scp', '-O', '-q']+options+[host+':'+sha+'/'+name, str(stage/name)],
                           check=True, timeout=600)
        manifest = json.loads((stage/'manifest.json').read_text())
        if (manifest.get('commit'), manifest.get('target'), manifest.get('version')) != (sha, TARGET, version):
            raise ValueError('builder manifest does not match requested commit/target/version')
        checksums = manifest.get('sha256', {})
        if set(checksums) != set(NAMES):
            raise ValueError('incomplete checksum manifest')
        for name in NAMES:
            path = stage/name
            if not stat.S_ISREG(path.lstat().st_mode) or not 0 < path.stat().st_size <= 1024**3:
                raise ValueError('unsafe downloaded artifact')
            if not re.fullmatch(r'[0-9a-f]{64}', checksums[name]) or digest(path) != checksums[name]:
                raise ValueError('checksum mismatch: '+name)
            path.chmod(0o755)
            with path.open('rb') as stream:
                os.fsync(stream.fileno())
        # Isolate smoke probes from all production databases/configuration.
        env = {'PATH': '/usr/bin:/bin', 'HOME': str(stage),
               'XDG_CONFIG_HOME': str(stage/'config'), 'XDG_STATE_HOME': str(stage/'state'),
               'AUGMENTAGENT_DB': str(stage/'probe.db')}
        probe = subprocess.run([str(stage/NAMES[0]), '--version'], cwd=stage, env=env,
                               capture_output=True, check=True, timeout=15)
        if probe.stdout.decode().strip() != 'augmentagent '+version:
            raise ValueError('version sanity check failed')
        # MCP memory has no --version flag. EOF opens only a private disposable
        # DB and exits, proving its loader/library compatibility too.
        subprocess.run([str(stage/NAMES[1])], input=b'', cwd=stage, env=env,
                       capture_output=True, check=True, timeout=15)
        subprocess.run([sys.executable, str(repo/'scripts/secure-release-executables.py'), str(stage)], check=True)
        install(stage, release)
    print('remote release installed: '+sha, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True)
    parser.add_argument('--sha', required=True)
    parser.add_argument('--target', type=Path, required=True)
    parser.add_argument('--repo', type=Path, required=True)
    args = parser.parse_args()
    try:
        args.target.mkdir(parents=True, exist_ok=True)
        fd = os.open(args.target/'.remote-update.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'a+b') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fetch(args.host, args.sha, args.target, args.repo)
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError) as error:
        print('remote build deferred (75): '+str(error), file=sys.stderr, flush=True)
        return 75
    return 0


if __name__ == '__main__':
    sys.exit(main())
