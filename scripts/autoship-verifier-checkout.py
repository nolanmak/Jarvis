#!/usr/bin/env python3
"""Export literal committed blobs for verifier execution, without checkout state."""
import hashlib
import json
import os
import pathlib
import subprocess
import sys


def export(repository, head, destination):
    if len(head) not in (40, 64) or any(c not in "0123456789abcdefABCDEF" for c in head):
        raise ValueError("immutable commit required")
    destination = pathlib.Path(destination).resolve(strict=True)
    if any(destination.iterdir()):
        raise ValueError("verifier export must start empty")
    command = ["/usr/bin/git", "--no-replace-objects", "-C", str(repository)]
    entries = subprocess.check_output(command + ["ls-tree", "-r", "-z", "--full-tree", head])
    rows = [row for row in entries.split(b"\0") if row]
    if len(rows) > 100000:
        raise ValueError("verifier source exceeds file bound")
    digest = hashlib.sha256()
    total = 0
    with subprocess.Popen(command + ["cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE) as objects:
        try:
            for row in rows:
                metadata, raw_path = row.split(b"\t", 1)
                mode, kind, oid = metadata.split()
                if kind != b"blob" or mode not in (b"100644", b"100755"):
                    raise ValueError("verifier source requires regular committed blobs; symlinks/submodules need explicit admission")
                relative = pathlib.PurePosixPath(os.fsdecode(raw_path))
                if relative.is_absolute() or any(p in ("..", ".git") for p in relative.parts):
                    raise ValueError("invalid committed source path")
                objects.stdin.write(oid + b"\n")
                objects.stdin.flush()
                actual_oid, actual_kind, raw_size = objects.stdout.readline().split()
                size = int(raw_size)
                total += size
                if actual_oid != oid or actual_kind != b"blob" or size > 64 * 1024 * 1024 or total > 512 * 1024 * 1024:
                    raise ValueError("invalid or oversized committed blob")
                target = destination.joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                remaining = size
                with target.open("xb") as output:
                    while remaining:
                        chunk = objects.stdout.read(min(remaining, 1024 * 1024))
                        if not chunk:
                            raise ValueError("incomplete committed blob")
                        output.write(chunk)
                        remaining -= len(chunk)
                if objects.stdout.read(1) != b"\n":
                    raise ValueError("invalid object stream terminator")
                target.chmod(0o755 if mode == b"100755" else 0o644)
                digest.update(row + b"\0")
            objects.stdin.close()
            if objects.wait() != 0:
                raise ValueError("committed source export failed")
        except BaseException:
            objects.kill()
            objects.wait()
            raise
    return {"schema_version": 1, "head": head, "files": len(rows), "bytes": total, "tree_manifest_sha256": digest.hexdigest()}


if __name__ == "__main__":
    print(json.dumps(export(*sys.argv[1:])))
