#!/usr/bin/env python3
"""Disposable Linux permission/flock experiment for the capacity C0 contract.

This is not a migration or a production admission test. It never opens the
application's real locks, state, registry, snapshots or provider credentials.
"""
import errno
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import tempfile

LEGACY_BRIDGE_SHA256 = '42bbc1aa57cbc2e750f9ee804132aee0799adedeb0233857974676b39e390c6f'


def legacy_scratch_probe(parent, label, bridge):
    """Exercise the actual legacy allocator's root gate without creating images."""
    with tempfile.TemporaryDirectory(prefix='autoship-scratch-compat-', dir=parent) as name:
        root = Path(name)
        scratch = bridge.BuildScratch(root)
        original = scratch._open_root()
        try:
            os.fchmod(original, 0o1700)
            called = []

            def stop_before_allocation(*_):
                called.append(True)
                raise RuntimeError('legacy allocator advanced past its root fence')

            scratch._require_space = stop_before_allocation
            try:
                scratch.open()
            except bridge.Readiness as error:
                rejected = error.category == 'build_scratch_unavailable'
            else:
                rejected = False
            unchanged = not list(root.iterdir())
            fd = os.open('held-reference', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
                         0o600, dir_fd=original)
            try:
                os.write(fd, b'owner directory writes remain possible')
            finally:
                os.close(fd)
            result = {'location': label, 'mode_1700': stat.S_IMODE(root.stat().st_mode) == 0o1700,
                      'legacy_allocator_rejected': rejected, 'allocation_boundary_not_reached': not called,
                      'allocator_changed_nothing': unchanged, 'preopened_directory_remains_usable': True}
            if not all(value for key, value in result.items() if key != 'location'):
                raise RuntimeError('legacy scratch root expectations failed: ' + json.dumps(result))
            return result
        finally:
            os.close(original)


def probe(parent, label):
    """Check negative-open fencing and its preopened-descriptor limitation."""
    with tempfile.TemporaryDirectory(prefix='autoship-compat-probe-', dir=parent) as name:
        root = Path(name)
        path = root / 'admission.lock'
        old = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600)
        try:
            fcntl.flock(old, fcntl.LOCK_EX | fcntl.LOCK_NB)
            os.fchmod(old, 0o400)
            os.fsync(old)
            result = {'location': label, 'read_only_mode': stat.S_IMODE(os.fstat(old).st_mode) == 0o400}
            try:
                opened = os.open(path, os.O_RDWR | os.O_CLOEXEC)
            except OSError as error:
                result['fresh_legacy_open_rejected'] = error.errno == errno.EACCES
            else:
                os.close(opened)
                result['fresh_legacy_open_rejected'] = False

            current = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            try:
                try:
                    fcntl.flock(current, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except OSError as error:
                    result['readonly_respects_existing_lock'] = error.errno in (errno.EAGAIN, errno.EACCES)
                else:
                    result['readonly_respects_existing_lock'] = False
                    fcntl.flock(current, fcntl.LOCK_UN)
                fcntl.flock(old, fcntl.LOCK_UN)
                fcntl.flock(current, fcntl.LOCK_EX | fcntl.LOCK_NB)
                result['readonly_exclusive_lock_works'] = True
                fcntl.flock(current, fcntl.LOCK_UN)
            finally:
                os.close(current)

            payload = b'preopened descriptor remains writable'
            os.write(old, payload)
            fcntl.flock(old, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result['preopened_legacy_descriptor_survives'] = path.read_bytes() == payload

            marker = root / '00000000-0000-4000-8000-000000000000.reservation'
            marker.write_bytes(b'{}')
            marker.chmod(0o400)
            try:
                opened = os.open(marker, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o600)
            except OSError as error:
                result['legacy_claim_open_rejected'] = error.errno == errno.EACCES and marker.exists()
            else:
                os.close(opened)
                result['legacy_claim_open_rejected'] = False
            if not all(value for key, value in result.items() if key != 'location'):
                raise RuntimeError('permission/flock expectations failed: ' + json.dumps(result))
            return result
        finally:
            os.close(old)


def main():
    if not sys.platform.startswith('linux') or os.getuid() == 0 or os.geteuid() != os.getuid():
        raise SystemExit('requires the supported non-root Linux UID profile')
    runtime = Path('/run/user') / str(os.geteuid())
    info = runtime.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise SystemExit('requires an owned private systemd-user runtime directory')
    script = Path(__file__).resolve().parents[3] / 'scripts/codex-tool-bridge.py'
    source = script.read_bytes()
    source_digest = hashlib.sha256(source).hexdigest()
    if source_digest != LEGACY_BRIDGE_SHA256:
        raise SystemExit('legacy bridge changed; revalidate its root gate before updating probe binding')
    results = [probe(tempfile.gettempdir(), 'temporary-root'), probe(runtime, 'user-runtime')]
    spec = importlib.util.spec_from_file_location('legacy_storage_bridge_probe', script)
    bridge = importlib.util.module_from_spec(spec)
    sys.dont_write_bytecode = True
    exec(compile(source, str(script), 'exec'), bridge.__dict__)
    scratch = [legacy_scratch_probe(tempfile.gettempdir(), 'temporary-root', bridge),
               legacy_scratch_probe(runtime, 'user-runtime', bridge)]
    print(json.dumps({'experiment_only': True,
                      'bridge_sha256': source_digest,
                      'lock_results': results, 'scratch_results': scratch}, indent=2))


if __name__ == '__main__':
    main()
