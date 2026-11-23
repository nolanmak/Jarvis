"""Mac checkout updater (#2417). Uses only temporary git repos."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import update


def git(cwd, *args):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


class UpdateTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="mac update ")
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        self.origin = root / "origin.git"
        self.author = root / "author"
        self.mac = root / "mac"
        self.state = root / "state"
        env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid"}
        os.environ.update(env)
        git(root, "init", "-q", "--bare", "-b", "main", str(self.origin))
        git(root, "clone", "-q", str(self.origin), str(self.author))
        self.commit("scripts/imessage/send.py", "v1")
        git(self.author, "push", "-q", "origin", "main")
        git(root, "clone", "-q", str(self.origin), str(self.mac))
        self.kicked = []

    def commit(self, path, text):
        p = self.author / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
        git(self.author, "add", path)
        git(self.author, "commit", "-q", "-m", f"change {path}")

    def publish(self, path, text):
        self.commit(path, text)
        git(self.author, "push", "-q", "origin", "main")

    def run_update(self):
        return update.run(self.mac, self.state, restart=self.kicked.append)

    def stamp(self):
        return json.loads((self.state / "mac-update.json").read_text())

    def test_fast_forwards_and_restarts_long_running_imessage_jobs(self):
        self.publish("scripts/imessage/send.py", "v2")
        self.assertEqual(self.run_update(), "updated")
        self.assertEqual((self.mac / "scripts/imessage/send.py").read_text(), "v2")
        self.assertEqual(self.kicked, list(update.RESTART_ON_IMESSAGE_CHANGE))
        stamp = self.stamp()
        self.assertEqual(stamp["commit"], git(self.mac, "rev-parse", "HEAD"))
        self.assertEqual(stamp["result"], "updated")

    def test_no_restart_when_only_other_paths_change(self):
        self.publish("crates/x/src/lib.rs", "rust")
        self.assertEqual(self.run_update(), "updated")
        self.assertEqual(self.kicked, [])

    def test_up_to_date_is_a_no_op(self):
        self.assertEqual(self.run_update(), "up-to-date")
        self.assertEqual(self.stamp()["result"], "up-to-date")

    def test_refuses_dirty_tracked_changes(self):
        self.publish("scripts/imessage/send.py", "v2")
        (self.mac / "scripts/imessage/send.py").write_text("local edit")
        self.assertEqual(self.run_update(), "skipped: local changes")
        self.assertEqual((self.mac / "scripts/imessage/send.py").read_text(), "local edit")
        self.assertEqual(self.kicked, [])

    def test_untracked_files_do_not_block(self):
        (self.mac / "notes.txt").write_text("scratch")
        self.publish("scripts/imessage/send.py", "v2")
        self.assertEqual(self.run_update(), "updated")

    def test_refuses_other_branch(self):
        git(self.mac, "checkout", "-q", "-b", "experiment")
        self.publish("scripts/imessage/send.py", "v2")
        self.assertEqual(self.run_update(), "skipped: not on main")

    def test_refuses_diverged_history_without_rewriting(self):
        (self.mac / "local.txt").write_text("x")
        git(self.mac, "add", "local.txt")
        git(self.mac, "commit", "-q", "-m", "local")
        self.publish("scripts/imessage/send.py", "v2")
        head = git(self.mac, "rev-parse", "HEAD")
        self.assertEqual(self.run_update(), "skipped: diverged from origin/main")
        self.assertEqual(git(self.mac, "rev-parse", "HEAD"), head)

    def test_fetch_failure_is_reported_not_raised(self):
        git(self.mac, "remote", "set-url", "origin", str(self.origin) + "-missing")
        self.assertTrue(self.run_update().startswith("failed: fetch"))
        self.assertTrue(self.stamp()["result"].startswith("failed: fetch"))

    def test_overlapping_run_is_skipped(self):
        self.state.mkdir(parents=True)
        with update.lock(self.state) as got:
            self.assertTrue(got)
            self.assertEqual(self.run_update(), "skipped: another update is running")


if __name__ == "__main__":
    unittest.main()
