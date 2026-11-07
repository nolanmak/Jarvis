"""Crash/reboot fixture oracle on an internally allocated disposable ext4 image.

Not a custody ledger or authority factory. Host code supplies only readonly
phase/nonce files in the guest initrd. No production paths/options are accepted.
"""
import ctypes
import json
import os
from pathlib import Path
import runpy
import signal
import sys


def sync_filesystem(fd):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.syncfs(fd) != 0:
        raise OSError(ctypes.get_errno(), 'fixture syncfs failed')


def main():
    if len(sys.argv) != 1 or os.geteuid() != 0 or os.environ.get('JARVIS_CUSTODY_DISPOSABLE_GUEST') != '1':
        raise RuntimeError('Requires disposable guest only, no paths/options')
    configuration = json.loads(Path('/phase').read_text())
    phase, boundary = configuration['phase'], configuration['boundary']
    nonce = Path('/nonce').read_text()
    if boundary not in ('captured', 'directory-fenced', 'file-fenced:lock', 'complete'):
        raise RuntimeError('unknown fixture boundary')
    module = runpy.run_path('/inventory-probe.py')
    Experiment, flags, immutable = (module[name] for name in ('Experiment', 'flags', 'IMMUTABLE'))
    root = Path('/fixture')
    state, manifest_path = root / 'persistent-state', root / 'fixture-oracle.json'
    if phase == 'prepare':
        state.mkdir(mode=0o700)
        (state / 'owned.json').write_bytes(json.dumps({'fixture': 'preserve', 'nonce': nonce}).encode())
        (state / 'lock').write_bytes(b'')
        probe = Experiment(state)

        def checkpoint(at):
            if at == 'captured':
                # This oracle describes the original fixture, not a production
                # custody intent, journal, grant or completed operation.
                oracle = {'nonce': nonce, 'boundary': boundary,
                          'directory': list(module['identity'](probe.directory)),
                          'inventory': probe.original, 'grants_capacity': False}
                for fd, _ in probe.files.values():
                    os.fsync(fd)
                os.fsync(probe.directory)
                with manifest_path.open('x') as output:
                    json.dump(oracle, output, sort_keys=True)
                    output.flush()
                    os.fsync(output.fileno())
                outer = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(outer)
                finally:
                    os.close(outer)
            if at != boundary:
                return
            for fd, _ in probe.files.values():
                os.fsync(fd)
            os.fsync(probe.directory)
            sync_filesystem(probe.directory)
            print('\n' + json.dumps({'kind': 'persistence_ready', 'nonce': nonce,
                              'boundary': boundary, 'fsync_complete': True,
                              'file_flags': {name: bool(flags(fd) & immutable) for name, (fd, _) in probe.files.items()},
                              'directory_flag': bool(flags(probe.directory) & immutable),
                              'grants_capacity': False}), flush=True)
            # No normal guest unmount or restriction cleanup before host's
            # original supervisor kills QEMU. The next boot owns new FDs.
            while True:
                signal.pause()

        probe.capture(checkpoint)
        checkpoint('complete')
        raise RuntimeError('missing fixture crash boundary')
    if phase != 'recover':
        raise RuntimeError('unknown fixture phase')
    oracle = json.loads(manifest_path.read_text())
    if oracle['nonce'] != nonce or oracle['boundary'] != boundary or oracle['grants_capacity']:
        raise RuntimeError('wrong original fixture oracle')
    probe = Experiment(state)
    try:
        actual = json.loads(json.dumps(probe.inventory()))
        if actual != oracle['inventory'] or list(module['identity'](probe.directory)) != oracle['directory']:
            raise RuntimeError('original fixture identity/content did not survive')
        file_flags = {}
        for name in actual:
            fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=probe.directory)
            try:
                file_flags[name] = bool(flags(fd) & immutable)
            finally:
                os.close(fd)
        directory_flag = bool(flags(probe.directory) & immutable)
        expected = {'captured': (False, {'lock': False, 'owned.json': False}),
                    'directory-fenced': (True, {'lock': False, 'owned.json': False}),
                    'file-fenced:lock': (True, {'lock': True, 'owned.json': False}),
                    'complete': (True, {'lock': True, 'owned.json': True})}[boundary]
        if (directory_flag, file_flags) != expected:
            raise RuntimeError('fixture durable flags mismatch')
        complete = directory_flag and all(file_flags.values())
        print('\n' + json.dumps({'kind': 'persistence_recovered', 'nonce': nonce,
                          'boundary': boundary, 'original_identity_content_preserved': True,
                          'directory_flag': directory_flag, 'file_flags': file_flags,
                          'all_fixture_flags_set': complete, 'partial_or_unfenced': not complete,
                          'kernel': os.uname().release, 'uid': os.geteuid(),
                          'production_state_changed': False, 'grants_capacity': False}), flush=True)
    finally:
        # Close this boot's observation FD only. Do not clear recovered flags.
        os.close(probe.directory)


if __name__ == '__main__':
    main()
