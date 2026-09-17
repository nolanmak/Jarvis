"""sync.py / schedule.py tests: lock, output guards, git commit flow, scrub
gate, launchd plist. Temporary directories and synthetic data only."""
import argparse
import contextlib
import fcntl
import io
import json
import os
import plistlib
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import schedule  # noqa: E402
import sync as cli  # noqa: E402
from test_sync import make_fixture_db, add_note, set_body  # noqa: E402

GIT_ID = ["-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false"]


def git(repo, *args, **kw):
    return subprocess.run(["git", "-C", str(repo), *GIT_ID, *args], check=True, capture_output=True, text=True, **kw).stdout


class CliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="apple notes cli ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.db = self.root / "Note Store.sqlite"
        self.con = make_fixture_db(self.db)
        self.addCleanup(self.con.close)
        add_note(self.con, 10, "First note", "hello")
        self.out = self.root / "private notes"
        self.args = ["--db", str(self.db), "--out", str(self.out)]
        mask = os.umask(0o022)
        self.addCleanup(os.umask, mask)

    def main(self, *extra):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = cli.main(self.args + list(extra))
        return code, out.getvalue(), err.getvalue()

    # -- plain export --

    def test_export_writes_bundle_read_only_and_private(self):
        original = self.db.read_bytes()
        code, out, _ = self.main()
        self.assertEqual(code, 0)
        self.assertIn("1 new", out)
        self.assertEqual(self.db.read_bytes(), original)
        self.assertTrue((self.out / "notes/notes/first-note.md").exists())
        self.assertEqual(self.out.stat().st_mode & 0o777, 0o700)

    def test_reject_output_inside_source_checkout(self):
        with self.assertRaises(argparse.ArgumentTypeError):
            cli.output_path(str(Path(__file__).resolve().parents[3] / "private"))

    def test_git_output_is_allowed(self):
        git(self.root, "init", "-q", str(self.out))
        self.assertEqual(cli.output_path(str(self.out)), self.out.resolve())

    def test_config_file_loaded_from_bundle(self):
        self.out.mkdir()
        (self.out / "config.json").write_text(json.dumps({"skip_notes": ["00000000-0000-0000-0000-000000000010"]}))
        code, out, _ = self.main()
        self.assertEqual(code, 0)
        self.assertIn("1 skipped", out)
        self.assertEqual(list(self.out.glob("notes/*/*.md")), [])

    def test_lock_prevents_duplicate_export(self):
        self.out.mkdir()
        with (self.out / ".sync.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(cli, "sync") as exporter:
                code, out, _ = self.main()
        self.assertEqual(code, 0)
        self.assertIn("another sync is running; skipped", out)
        exporter.assert_not_called()

    def test_missing_database_does_not_create_one(self):
        self.con.close()
        self.db.unlink()
        self.assertEqual(self.main()[0], 1)
        self.assertFalse(self.db.exists())

    def test_disk_access_denial_reports_actionable_error(self):
        with patch.object(cli, "sync", side_effect=sqlite3.DatabaseError("authorization denied")):
            code, _, err = self.main()
        self.assertEqual(code, 1)
        self.assertIn("Full Disk Access", err)

    # -- git commit flow --

    def init_repo(self):
        git(self.root, "init", "-q", "-b", "main", str(self.out))

    def test_commit_creates_descriptive_commit(self):
        self.init_repo()
        code, out, _ = self.main("--commit")
        self.assertEqual(code, 0)
        subject = git(self.out, "log", "-1", "--format=%s").strip()
        self.assertRegex(subject, r"^Sync notes \d{4}-\d{2}-\d{2} \d{2}:\d{2}: 1 new, 0 updated, 0 renamed, 0 deleted$")
        self.assertIn("First note", git(self.out, "log", "-1", "--format=%b"))
        self.assertEqual(git(self.out, "status", "--porcelain").strip(), "")
        self.assertIn("committed", out)

    def test_commit_noop_when_nothing_changed(self):
        self.init_repo()
        self.main("--commit")
        code, out, _ = self.main("--commit")
        self.assertEqual(code, 0)
        self.assertIn("nothing to commit", out)
        self.assertEqual(git(self.out, "rev-list", "--count", "HEAD").strip(), "1")

    def test_commit_body_lists_scrubbed_titles_and_pushes_when_remote_exists(self):
        self.init_repo()
        remote = self.root / "remote.git"
        git(self.root, "init", "-q", "--bare", str(remote))
        git(self.out, "remote", "add", "origin", str(remote))
        code, out, _ = self.main("--commit")
        self.assertEqual(code, 0)
        self.assertIn("pushed", out)
        self.assertEqual(git(remote, "rev-list", "--count", "main").strip(), "1")

    def test_commit_without_remote_keeps_commit_and_says_so(self):
        self.init_repo()
        code, out, _ = self.main("--commit")
        self.assertEqual(code, 0)
        self.assertIn("no remote", out)

    def test_commit_refuses_when_scrub_check_finds_a_secret(self):
        self.init_repo()
        # Bypass the sync-time scrub to plant a secret the way a scrubber gap would.
        self.main()
        (self.out / "notes/notes/first-note.md").write_text("---\ntitle: x\n---\nAKIAIOSFODNN7EXAMPLE\n")
        code, _, err = self.main("--commit")
        self.assertEqual(code, 1)
        self.assertIn("notes/notes/first-note.md:4 aws-access-key", err)
        self.assertIn("refusing to commit", err)
        self.assertEqual(git(self.out, "rev-list", "--all", "--count").strip(), "0")

    def test_commit_requires_git_repo(self):
        code, _, err = self.main("--commit")
        self.assertEqual(code, 1)
        self.assertIn("not a git repository", err)

    def test_edit_shows_as_diff_on_same_path(self):
        self.init_repo()
        self.main("--commit")
        set_body(self.con, 10, "hello again", modified=1782475400)
        self.main("--commit")
        diff = git(self.out, "diff", "HEAD~1", "HEAD", "--", "notes/notes/first-note.md")
        self.assertIn("-hello\n", diff)
        self.assertIn("+hello again\n", diff)

    # -- launchd --

    def test_plist_roundtrip_preserves_paths_and_arguments(self):
        args = ["--out", "/private/path with spaces & chars", "--commit"]
        value = plistlib.loads(plistlib.dumps(schedule.make_plist(args, 2, self.root)))
        self.assertEqual(value["ProgramArguments"][2:], args)
        self.assertEqual(value["StartInterval"], 120)
        self.assertTrue(value["RunAtLoad"])
        self.assertEqual(value["Umask"], 0o077)
        self.assertEqual(value["Label"], "org.augmentagent.apple-notes-sync")
        self.assertTrue(value["StandardOutPath"].endswith("apple-notes-sync.log"))

    def test_uninstall_only_removes_own_schedule(self):
        plist = self.root / f"Library/LaunchAgents/{schedule.LABEL}.plist"
        plist.parent.mkdir(parents=True)
        plist.write_text("placeholder")
        unrelated = plist.with_name("unrelated.plist")
        unrelated.write_text("keep")
        with patch.object(schedule.sys, "platform", "darwin"), \
             patch.object(schedule.sys, "argv", ["schedule.py", "--uninstall"]), \
             patch.object(schedule.Path, "home", return_value=self.root), \
             patch.object(schedule.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
            schedule.main()
        self.assertFalse(plist.exists())
        self.assertTrue(unrelated.exists())
        self.assertEqual(run.call_args.args[0], ["launchctl", "bootout", f"gui/{os.getuid()}/{schedule.LABEL}"])


if __name__ == "__main__":
    unittest.main()
