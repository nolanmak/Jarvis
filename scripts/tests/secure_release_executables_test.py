"""secure-release-executables.py must run on the oldest supported python3.

macOS runners (and stock macOS) ship Python 3.9, where pathlib.Path.stat()
has no `follow_symlinks` argument; using it broke every launchd installer on
macOS. This test simulates that pathlib so the regression shows up on any host.
"""
import importlib.util
import os
import pathlib
import sys
import stat
import tempfile
import unittest
from unittest import mock

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "secure-release-executables.py"


def load():
    spec = importlib.util.spec_from_file_location("secure_release_executables", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


REAL_STAT = pathlib.Path.stat


def py39_stat(self, **kwargs):
    """Path.stat() as the script sees it on Python 3.9: no keywords.

    Newer stdlib internals (Path.exists) pass follow_symlinks themselves, so
    only calls made directly from the script under test are rejected.
    """
    if kwargs and sys._getframe(1).f_code.co_filename == str(SCRIPT):
        raise TypeError("stat() got an unexpected keyword argument 'follow_symlinks'")
    return REAL_STAT(self, **kwargs)


class SecureReleaseExecutablesTest(unittest.TestCase):
    def test_runs_without_python_310_pathlib_and_strips_group_other_write(self):
        module = load()
        with tempfile.TemporaryDirectory() as d:
            for name in ("augmentagent", "augmentagent-mcp-memory"):
                p = pathlib.Path(d) / name
                p.write_bytes(b"\x7fELF binary")
                os.chmod(p, 0o777)
            with mock.patch.object(pathlib.Path, "stat", py39_stat):
                module.secure_release_executables(d)
            for name in ("augmentagent", "augmentagent-mcp-memory"):
                self.assertEqual(stat.S_IMODE(os.stat(pathlib.Path(d) / name).st_mode), 0o755)

    def test_a_swapped_executable_is_still_refused(self):
        module = load()
        with tempfile.TemporaryDirectory() as d:
            for name in ("augmentagent", "augmentagent-mcp-memory"):
                p = pathlib.Path(d) / name
                p.write_bytes(b"x")
                os.chmod(p, 0o755)
            real_fstat = os.fstat
            def moved(fd):
                info = real_fstat(fd)
                return os.stat_result((info.st_mode, info.st_ino + 1) + tuple(info)[2:])
            with mock.patch.object(module.os, "fstat", moved), self.assertRaises(RuntimeError):
                module.secure_release_executables(d)


if __name__ == "__main__":
    unittest.main()
