#!/usr/bin/env python3
import importlib.util
import os
import pathlib
import py_compile
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("checkout", pathlib.Path(__file__).with_name("autoship-verifier-checkout.py"))
checkout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checkout)


class CommittedSource(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")

    def git(self, *args):
        return subprocess.check_output(["/usr/bin/git", "-C", str(self.repo), *args], stderr=subprocess.PIPE).decode().strip()

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def exported(self, head):
        target = self.root / "export"
        target.mkdir()
        checkout.export(self.repo, head, target)
        return target

    def behavior(self, root):
        return subprocess.check_output(["/usr/bin/python3", "-c", "import arithmetic; print(arithmetic.result())"], cwd=root).decode().strip()

    def test_ignored_green_bytecode_cannot_override_red_committed_source(self):
        red = "def result(): return 'red!'\n"
        green = "def result(): return 'good'\n"
        self.assertEqual(len(red), len(green))
        path = self.repo / "arithmetic.py"
        path.write_text(red)
        (self.repo / ".gitignore").write_text("__pycache__/\n")
        head = self.commit()
        path.write_text(green)
        os.utime(path, (1700000000, 1700000000))
        py_compile.compile(str(path), doraise=True)
        path.write_text(red)
        os.utime(path, (1700000000, 1700000000))
        self.assertEqual(self.git("status", "--porcelain", "--untracked-files=all"), "")
        self.assertEqual(self.behavior(self.repo), "good", "prove the poisoned ignored cache is executable")
        exported = self.exported(head)
        self.assertFalse((exported / "__pycache__").exists())
        self.assertEqual(self.behavior(exported), "red!")

    def test_export_uses_head_not_staged_bytes_or_export_ignore(self):
        (self.repo / "arithmetic.py").write_text("def result(): return 'red!'\n")
        (self.repo / ".gitattributes").write_text("arithmetic.py export-ignore\n")
        head = self.commit()
        (self.repo / "arithmetic.py").write_text("def result(): return 'good'\n")
        self.git("add", "arithmetic.py")
        self.assertEqual(self.behavior(self.exported(head)), "red!")

    def test_unadmitted_git_symlink_is_rejected(self):
        (self.repo / "link").symlink_to("/etc/passwd")
        head = self.commit()
        with self.assertRaisesRegex(ValueError, "regular committed blobs"):
            self.exported(head)


if __name__ == "__main__":
    unittest.main()
