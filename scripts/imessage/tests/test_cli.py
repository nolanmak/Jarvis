"""Setup and transport tests use only temporary directories and synthetic data."""
import argparse
import fcntl
import json
import contextlib
import io
import os
from pathlib import Path
import plistlib
import subprocess
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import schedule
import sync as cli
from imessage_sync import slugify
from test_sync import make_fixture_db, add_message, ns


class CliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="imessage test ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.db = self.root / "chat ?#.db"
        con = make_fixture_db(self.db)
        add_message(con, 1, 1, "Synthetic message", ns(1782475200), 0)
        con.close()
        self.out = self.root / "private bundle"
        self.args = ["--db", str(self.db), "--out", str(self.out), "--no-contacts"]
        mask = os.umask(0o022)
        self.addCleanup(os.umask, mask)

    def test_export_is_private_incremental_and_read_only(self):
        original = self.db.read_bytes()
        self.assertEqual(cli.main(self.args), 0)
        index = json.loads((self.out / "conversations/index.json").read_text())
        self.assertTrue(index)
        before = {p: p.read_bytes() for p in self.out.rglob("messages.md")}
        self.assertEqual(cli.main(self.args), 0)
        self.assertEqual(before, {p: p.read_bytes() for p in before})
        self.assertEqual(original, self.db.read_bytes())
        for path in self.out.rglob("*"):
            self.assertEqual(path.stat().st_mode & 0o077, 0, str(path))

    def test_reject_source_and_other_git_output(self):
        for path in (cli.SOURCE_ROOT, cli.SOURCE_ROOT / "exports"):
            with self.assertRaises(argparse.ArgumentTypeError):
                cli.output_path(str(path))
        (self.root / ".git").write_text("gitdir: elsewhere")
        with self.assertRaises(argparse.ArgumentTypeError):
            cli.output_path(str(self.out))

    def test_remote_validation(self):
        self.assertEqual(cli.remote_path("agent@host:/private/bundle"), "agent@host:/private/bundle/")
        for value in ("-host:/data", "host:/", "host:/data/../src", "host:/data;id", "host:relative", "host:/a b"):
            with self.assertRaises(argparse.ArgumentTypeError):
                cli.remote_path(value)

    def test_empty_and_dot_handles_are_safe_directory_names(self):
        for value in ("", ".", ".."):
            self.assertTrue(slugify(value).startswith("chat-"))
            self.assertNotIn("/", slugify(value))

    def test_failed_transfer_retries_without_new_messages(self):
        args = self.args + ["--remote", "agent@host:/private/bundle"]
        with patch.object(cli.shutil, "which", return_value="/usr/bin/rsync"), patch.object(cli, "mirror") as mirror:
            mirror.side_effect = [subprocess.CalledProcessError(1, "rsync"), None]
            self.assertEqual(cli.main(args), 1)
            before = {p: p.read_bytes() for p in self.out.rglob("messages.md")}
            self.assertEqual(cli.main(args), 0)
            self.assertEqual(mirror.call_count, 2)
            self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_transport_only_sends_bundle_files(self):
        with patch.object(cli.subprocess, "run") as run:
            cli.mirror(self.out, "host:/private/bundle/")
        command = run.call_args.args[0]
        self.assertEqual(command[-3:], [str(self.out / "conversations"), str(self.out / "index.md"), "host:/private/bundle/"])
        self.assertNotIn("--delete", command)
        self.assertIn("--chmod=D700,F600", command)

    def test_lock_prevents_duplicate_export(self):
        self.out.mkdir()
        with (self.out / ".sync.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(cli, "sync") as exporter:
                self.assertEqual(cli.main(self.args), 0)
                exporter.assert_not_called()

    def test_missing_database_does_not_create_one(self):
        self.db.unlink()
        self.assertEqual(cli.main(self.args), 1)
        self.assertFalse(self.db.exists())

    def test_disk_access_denial_reports_actionable_error(self):
        stderr = io.StringIO()
        with patch.object(cli, "sync", side_effect=sqlite3.DatabaseError("authorization denied")), contextlib.redirect_stderr(stderr):
            self.assertEqual(cli.main(self.args), 1)
        self.assertIn("Full Disk Access", stderr.getvalue())

    def test_plist_roundtrip_preserves_paths_and_arguments(self):
        args = ["--out", "/private/path with spaces & chars", "--no-contacts"]
        value = plistlib.loads(plistlib.dumps(schedule.make_plist(args, 15, self.root)))
        self.assertEqual(value["ProgramArguments"][2:], args)
        self.assertEqual(value["StartInterval"], 900)
        self.assertTrue(value["RunAtLoad"])
        self.assertEqual(value["Umask"], 0o077)

    def test_uninstall_only_removes_own_schedule_without_validating_output(self):
        plist = self.root / f"Library/LaunchAgents/{schedule.LABEL}.plist"
        plist.parent.mkdir(parents=True)
        plist.write_text("placeholder")
        unrelated = plist.with_name("unrelated.plist")
        unrelated.write_text("keep")
        with patch.object(schedule.sys, "platform", "darwin"), patch.object(schedule.sys, "argv", ["schedule.py", "--uninstall"]), patch.object(schedule.Path, "home", return_value=self.root), patch.object(schedule.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run, patch.object(schedule, "sync_parser") as parser:
            schedule.main()
        parser.assert_not_called()
        self.assertFalse(plist.exists())
        self.assertTrue(unrelated.exists())
        self.assertEqual(run.call_args.args[0], ["launchctl", "bootout", f"gui/{os.getuid()}/{schedule.LABEL}"])


if __name__ == "__main__":
    unittest.main()
