#!/usr/bin/env python3
"""Attest offline Cargo registry sources against locked package archive hashes.

Missing optional packages are left for Cargo's offline resolution. Every present
source tree must match a cached .crate whose SHA256 matches Cargo.lock. This does
not attest arbitrary git dependencies or an entire operating-system image.
"""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import sys
import tarfile
import tomllib


def digest_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        while chunk := stream.read(262144):
            digest.update(chunk)
    return digest.hexdigest()


def attest(lock_path, registry):
    locked = tomllib.loads(Path(lock_path).read_text())
    registry = Path(registry)
    verified = []
    for package in locked.get('package', []):
        source = package.get('source')
        if source is None:
            continue  # workspace/path inputs are the reviewed candidate itself
        if source != 'registry+https://github.com/rust-lang/crates.io-index':
            raise RuntimeError('unsupported dependency source requires independent provenance')
        name, version, expected = package['name'], package['version'], package.get('checksum', '')
        if any('/' in value or '\\' in value or value in ('.', '..') for value in (name, version)):
            raise RuntimeError('invalid package identity')
        if len(expected) != 64 or any(c not in '0123456789abcdef' for c in expected):
            raise RuntimeError('locked registry package lacks SHA256')
        identity = f'{name}-{version}'
        for index in sorted((registry / 'src').glob('*')):
            tree = index / identity
            if not tree.exists():
                continue
            if index.is_symlink() or tree.is_symlink():
                raise RuntimeError('symlinked dependency source refused')
            archive = registry / 'cache' / index.name / f'{identity}.crate'
            if not archive.is_file() or archive.is_symlink() or digest_file(archive) != expected:
                raise RuntimeError(f'archive missing or checksum mismatch: {identity}')
            members = set()
            with tarfile.open(archive, 'r:gz') as source_tar:
                for member in source_tar:
                    parts = PurePosixPath(member.name).parts
                    if not parts or parts[0] != identity or '..' in parts:
                        raise RuntimeError('unsafe package archive path')
                    relative = Path(*parts[1:])
                    if member.isdir():
                        continue
                    if not member.isfile() or not parts[1:]:
                        raise RuntimeError('non-regular package archive entry')
                    target = tree / relative
                    if any((tree / Path(*relative.parts[:i])).is_symlink() for i in range(1, len(relative.parts)+1)):
                        raise RuntimeError('dependency source symlink refused')
                    if not target.is_file():
                        raise RuntimeError(f'dependency source missing: {identity}')
                    extracted = source_tar.extractfile(member)
                    digest = hashlib.sha256()
                    while chunk := extracted.read(262144):
                        digest.update(chunk)
                    if digest_file(target) != digest.hexdigest():
                        raise RuntimeError(f'dependency source modified: {identity}')
                    members.add(relative.as_posix())
            for current, directories, files in os.walk(tree, followlinks=False):
                for entry in directories + files:
                    path = Path(current) / entry
                    if path.is_symlink():
                        raise RuntimeError('dependency source symlink refused')
                for entry in files:
                    relative = (Path(current) / entry).relative_to(tree).as_posix()
                    if relative not in members and relative not in ('.cargo-ok', '.cargo-checksum.json'):
                        raise RuntimeError(f'unregistered dependency source: {identity}')
            verified.append((source, identity, expected))
    encoded = json.dumps(sorted(set(verified)), separators=(',', ':')).encode()
    return dict(schema_version=1, packages=len(verified), sha256=hashlib.sha256(encoded).hexdigest())


if __name__ == '__main__':
    try:
        if len(sys.argv) != 3:
            raise RuntimeError('expected Cargo.lock and registry root')
        print(json.dumps(attest(sys.argv[1], sys.argv[2])))
    except Exception as error:
        print(f'Verifier dependency integrity refused: {error}', file=sys.stderr)
        raise SystemExit(126)
