#!/usr/bin/env python3
"""Disposable ACL/root-mode compatibility experiment, not a migration.

Only private temporary directories below supplied scratch parents are changed.
No application state, cache image, quota, live lock or production ACL is opened.
Use scratch parents on the desired filesystems, not live native/cache roots.
The actual old Python gate is exercised; Rust evidence is its pinned source
predicate, not a compiled Rust entry-point or complete cutover test.
"""
import argparse
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import struct
import sys
import tempfile

BRIDGE_SHA256 = '42bbc1aa57cbc2e750f9ee804132aee0799adedeb0233857974676b39e390c6f'
RETENTION_SHA256 = '4a3c91bcf069928038336e44f3718d99ee48a83e75b8fc69af5a56c7118a7a6e'
# Linux POSIX ACL xattr v2, with no named user/group entries.
ACL = struct.pack('<I', 2) + b''.join(struct.pack('<HHI', tag, permission, 0xffffffff)
    for tag, permission in ((1, 7), (4, 0), (16, 1), (32, 0)))


def probe(parent, index, bridge):
    with tempfile.TemporaryDirectory(prefix='autoship-acl-probe-', dir=parent) as name:
        root = Path(name).resolve()
        descriptor = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        try:
            for attribute in ('system.posix_acl_access', 'system.posix_acl_default'):
                try:
                    os.getxattr(descriptor, attribute)
                except OSError as error:
                    if error.errno != errno.ENODATA:
                        raise
                else:
                    raise RuntimeError('disposable root inherited an ACL; profile not exercised')
            # ACL first: the intermediate 0710 mode still gives the group no
            # effective access. Chmod first would expose a group-search window.
            os.setxattr(descriptor, 'system.posix_acl_access', ACL)
            if (stat.S_IMODE(os.fstat(descriptor).st_mode) != 0o710
                    or os.getxattr(descriptor, 'system.posix_acl_access') != ACL):
                raise RuntimeError('restrictive intermediate ACL state did not read back')
            os.fchmod(descriptor, 0o1710)
            os.fsync(descriptor)
            mode = stat.S_IMODE(os.fstat(descriptor).st_mode)
            scratch = bridge.BuildScratch(root)
            advanced = []

            def stop_before_allocation(*_):
                advanced.append(True)
                raise RuntimeError('old allocator passed its root gate')

            scratch._require_space = stop_before_allocation
            try:
                scratch.open()
            except bridge.Readiness as error:
                rejected = error.category == 'build_scratch_unavailable'
            else:
                rejected = False
            unchanged = not list(root.iterdir())
            writer = os.open('held-writer', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
                             0o600, dir_fd=descriptor)
            try:
                os.write(writer, b'owner access retained')
            finally:
                os.close(writer)
            facts = {'sample': index, 'mode_1710': mode == 0o1710,
                'intermediate_0710_acl_checked': True,
                'exact_acl_readback': os.getxattr(descriptor, 'system.posix_acl_access') == ACL,
                'legacy_python_rejected': rejected, 'allocation_boundary_not_reached': not advanced,
                'allocator_changed_nothing': unchanged,
                'legacy_rust_source_predicate_rejects': mode & 0o777 != 0o700,
                'owner_descriptor_still_writes': (root / 'held-writer').read_bytes() == b'owner access retained'}
            if not all(value for key, value in facts.items() if key != 'sample'):
                raise RuntimeError('ACL compatibility expectations failed')
            return facts
        finally:
            os.close(descriptor)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('scratch_parents', nargs='+', type=Path)
    args = parser.parse_args()
    if not sys.platform.startswith('linux') or os.getuid() == 0 or os.geteuid() != os.getuid():
        parser.error('requires the non-root Linux UID profile')
    repository = Path(__file__).resolve().parents[3]
    path = repository / 'scripts/codex-tool-bridge.py'
    source = path.read_bytes()
    retention = repository / 'crates/augmentagent-channel-core/src/code_mode/compute/retention.rs'
    if (hashlib.sha256(source).hexdigest() != BRIDGE_SHA256
            or hashlib.sha256(retention.read_bytes()).hexdigest() != RETENTION_SHA256):
        parser.error('legacy source changed; revalidate before changing pinned experiment inputs')
    spec = importlib.util.spec_from_file_location('legacy_acl_bridge_probe', path)
    bridge = importlib.util.module_from_spec(spec)
    sys.dont_write_bytecode = True
    exec(compile(source, str(path), 'exec'), bridge.__dict__)
    results = [probe(parent, index, bridge) for index, parent in enumerate(args.scratch_parents)]
    print(json.dumps({'experiment_only': True, 'rust_entrypoint_executed': False,
        'bridge_sha256': BRIDGE_SHA256, 'rust_retention_sha256': RETENTION_SHA256,
        'results': results}, indent=2))


if __name__ == '__main__':
    main()
