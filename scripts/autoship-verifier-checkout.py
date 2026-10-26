#!/usr/bin/env python3
"""Export literal committed blobs for verifier execution, without checkout state."""
import hashlib
import json
import os
import pathlib
import subprocess
import shutil
import sys


MAX_FILES = 100000
MAX_MANIFEST_BYTES = 64 * 1024 * 1024
MAX_RECORD_BYTES = 8192


def manifest(command):
    # Bound both the total stream and individual records before buffering them.
    with subprocess.Popen(command, stdout=subprocess.PIPE) as process:
        pending = b""
        total = count = 0
        try:
            while True:
                chunk = process.stdout.read(4096)
                if not chunk:
                    break
                total += len(chunk)
                if total > MAX_MANIFEST_BYTES:
                    raise ValueError("verifier source exceeds manifest byte bound")
                pending += chunk
                while b"\0" in pending:
                    row, pending = pending.split(b"\0", 1)
                    count += 1
                    if count > MAX_FILES:
                        raise ValueError("verifier source exceeds file bound")
                    if not row or len(row) > MAX_RECORD_BYTES:
                        raise ValueError("verifier source exceeds record bound")
                    yield row
                if len(pending) > MAX_RECORD_BYTES:
                    raise ValueError("verifier source exceeds record bound")
            if pending or process.wait() != 0:
                raise ValueError("incomplete committed tree manifest")
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()


def require_space(destination, reserve_bytes, next_bytes=0):
    if shutil.disk_usage(destination).free < reserve_bytes + next_bytes:
        raise ValueError("verifier export disk reserve exhausted")


def export(repository, head, destination, reserve_bytes=0):
    reserve_bytes = int(reserve_bytes)
    if reserve_bytes < 0:
        raise ValueError("invalid disk reserve")
    if len(head) not in (40, 64) or any(c not in "0123456789abcdefABCDEF" for c in head):
        raise ValueError("immutable commit required")
    destination = pathlib.Path(destination).resolve(strict=True)
    if any(destination.iterdir()):
        raise ValueError("verifier export must start empty")
    command = ["/usr/bin/git", "--no-replace-objects", "-C", str(repository)]
    source_mtime = int(subprocess.check_output(command + ["show", "-s", "--format=%ct", head]).strip())
    require_space(destination, reserve_bytes)
    rows = manifest(command + ["ls-tree", "-r", "-z", "--full-tree", head])
    count = 0
    digest = hashlib.sha256()
    total = 0
    with subprocess.Popen(command + ["cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE) as objects:
        try:
            for row in rows:
                count += 1
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
                require_space(destination, reserve_bytes, size + 4096)
                target = destination.joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                remaining = size
                with target.open("xb") as output:
                    while remaining:
                        chunk = objects.stdout.read(min(remaining, 1024 * 1024))
                        if not chunk:
                            raise ValueError("incomplete committed blob")
                        require_space(destination, reserve_bytes, len(chunk) + 4096)
                        output.write(chunk)
                        remaining -= len(chunk)
                if objects.stdout.read(1) != b"\n":
                    raise ValueError("invalid object stream terminator")
                target.chmod(0o755 if mode == b"100755" else 0o644)
                os.utime(target, (source_mtime, source_mtime))
                digest.update(row + b"\0")
            objects.stdin.close()
            if objects.wait() != 0:
                raise ValueError("committed source export failed")
        except BaseException:
            objects.kill()
            objects.wait()
            raise
        finally:
            rows.close()
    require_space(destination, reserve_bytes)
    return {"schema_version": 1, "head": head, "files": count, "bytes": total, "tree_manifest_sha256": digest.hexdigest(), "source_mtime": source_mtime}


if __name__ == "__main__":
    print(json.dumps(export(*sys.argv[1:])))
