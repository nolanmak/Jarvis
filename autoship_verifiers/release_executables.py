import importlib.util
import json
import os
import pathlib
import stat
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path.cwd()))
spec = importlib.util.spec_from_file_location("release_secure", Path.cwd() / "scripts" / "secure-release-executables.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

names = ("augmentagent", "augmentagent-mcp-memory")

def write_pair(directory):
    for name in names:
        path = Path(directory) / name
        path.write_bytes(b"#!/bin/sh\nexit 0\n")
        path.chmod(0o775)

def run(directory):
    try:
        module.secure_release_executables(directory)
        return True
    except Exception:
        return False

with tempfile.TemporaryDirectory() as root:
    write_pair(root)
    original_stat = pathlib.Path.stat
    def old_stat(self, *args, **kwargs):
        if kwargs:
            raise TypeError("stat() got an unexpected keyword argument")
        return original_stat(self, *args, **kwargs)
    pathlib.Path.stat = old_stat
    compatible = run(root)
    pathlib.Path.stat = original_stat

with tempfile.TemporaryDirectory() as root:
    write_pair(root)
    second = Path(root) / names[1]
    second.unlink()
    second.symlink_to(Path(root) / names[0])
    symlink_rejected = not run(root)
    first_mode_after_rejection = stat.S_IMODE((Path(root) / names[0]).stat().st_mode)

with tempfile.TemporaryDirectory() as root:
    write_pair(root)
    target = Path(root) / names[0]
    original_lstat = pathlib.Path.lstat
    def replaced_lstat(self):
        result = original_lstat(self)
        if self == target:
            values = list(result)
            values[1] += 1
            return os.stat_result(values)
        return result
    pathlib.Path.lstat = replaced_lstat
    replacement_rejected = not run(root)
    pathlib.Path.lstat = original_lstat

with tempfile.TemporaryDirectory() as root:
    write_pair(root)
    normal = run(root)
    first_mode = stat.S_IMODE((Path(root) / names[0]).stat().st_mode)
    second_mode = stat.S_IMODE((Path(root) / names[1]).stat().st_mode)

print(json.dumps([compatible, symlink_rejected, replacement_rejected, first_mode_after_rejection, first_mode, second_mode]))
