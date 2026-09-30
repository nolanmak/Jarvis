"""Disposable launchd contract tests for the wiki and finance schedules."""

import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[2]
LABEL = "com.nolanmak.augmentagent"


class ScheduledSyncInstallersTest(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="jarvis-schedules-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.repo = self.root / "checkout & <private>"
        self.home = self.root / "home & <private>"
        (self.repo / "scripts/lib").mkdir(parents=True)
        (self.repo / "target/release").mkdir(parents=True)
        (self.repo / "wiki").mkdir()
        self.home.mkdir()
        shutil.copy2(SOURCE / "scripts/lib/launchd-install.sh", self.repo / "scripts/lib")
        if (SOURCE / "scripts/lib/scheduled-sync.sh").exists():
            shutil.copy2(SOURCE / "scripts/lib/scheduled-sync.sh", self.repo / "scripts/lib")
        binary = self.repo / "target/release/augmentagent"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
        for name in ("wiki", "finance"):
            for action in ("install", "uninstall"):
                source = SOURCE / f"scripts/{action}-{name}-sync.sh"
                if source.exists():
                    shutil.copy2(source, self.repo / "scripts")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, body in {
            "uname": "echo Darwin\n",
            "launchctl": 'printf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\n'
            'if [ "$1" = print ]; then [ -f "$LAUNCHCTL_LOADED" ]; exit; fi\n'
            'if [ "$1" = bootout ]; then rm -f "$LAUNCHCTL_LOADED"; exit; fi\n'
            'if [ "$1" = bootstrap ]; then touch "$LAUNCHCTL_LOADED"; exit; fi\n'
            'exit 0\n',
            "git": 'printf "%s\\n" "${TEST_WIKI_REMOTE:-' +
                   'git@github.com:owner/private-wiki.git}"\n',
            "gh": 'printf "%s\\n" "$*" >> "$GH_LOG"\n'
                  'if [ -n "${TEST_WIKI_PRIVATE:-}" ]; then printf "%s\\n" "$TEST_WIKI_PRIVATE";\n'
                  'elif [ "$3" = "${TEST_WIKI_PUBLIC_SLUG:-}" ]; then echo false;\n'
                  'else echo true; fi\n',
        }.items():
            path = self.bin / name
            path.write_text("#!/bin/sh\n" + body)
            path.chmod(0o755)
        self.env = {**os.environ, "HOME": str(self.home),
                    "PATH": f"{self.bin}:/usr/bin:/bin",
                    "XDG_STATE_HOME": str(self.home / ".local/state"),
                    "LAUNCHCTL_LOG": str(self.root / "launchctl.log"),
                    "LAUNCHCTL_LOADED": str(self.root / "loaded"),
                    "GH_LOG": str(self.root / "gh.log")}

    def run_script(self, action, name):
        return subprocess.run([str(self.repo / f"scripts/{action}-{name}-sync.sh")],
                              cwd=self.repo, env=self.env, text=True, capture_output=True)

    def plist(self, name):
        return self.home / "Library/LaunchAgents" / f"{LABEL}.{name}-sync.plist"

    def test_wiki_schedule_uses_exact_command_and_ten_minute_calendar(self):
        result = self.run_script("install", "wiki")
        self.assertEqual(result.returncode, 0, result.stderr)
        data = plistlib.loads(self.plist("wiki").read_bytes())
        self.assertEqual(data["WorkingDirectory"], str(self.repo))
        self.assertEqual(data["ProgramArguments"],
                         [str(self.repo / "target/release/augmentagent"),
                          "--wiki-dir", "./wiki", "wiki", "sync"])
        self.assertEqual(data["StartCalendarInterval"],
                         [{"Minute": minute} for minute in range(0, 60, 10)])
        self.assertNotIn("RunAtLoad", data)
        self.assertEqual(data["EnvironmentVariables"]["HOME"], str(self.home))
        self.assertIn(str(self.bin), data["EnvironmentVariables"]["PATH"])
        self.assertIn(str(self.home), data["StandardErrorPath"])
        self.assertNotIn("KeepAlive", data)
        self.assertIn("repo view owner/private-wiki --json isPrivate --jq .isPrivate",
                      (self.root / "gh.log").read_text())

    def test_custom_wiki_path_is_absolute_and_xml_escaped(self):
        custom = self.root / "another & <wiki>"
        custom.mkdir()
        self.env["AUGMENTAGENT_WIKI_DIR"] = str(custom)
        result = self.run_script("install", "wiki")
        self.assertEqual(result.returncode, 0, result.stderr)
        data = plistlib.loads(self.plist("wiki").read_bytes())
        self.assertEqual(data["ProgramArguments"][2], str(custom))

    def test_relative_custom_wiki_path_is_rejected(self):
        self.env["AUGMENTAGENT_WIKI_DIR"] = "wiki"
        result = self.run_script("install", "wiki")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute", result.stderr)
        self.assertFalse(self.plist("wiki").exists())

    def test_finance_schedule_uses_six_hour_calendar_and_private_files(self):
        result = self.run_script("install", "finance")
        self.assertEqual(result.returncode, 0, result.stderr)
        data = plistlib.loads(self.plist("finance").read_bytes())
        self.assertEqual(data["ProgramArguments"],
                         [str(self.repo / "target/release/augmentagent"),
                          "--wiki-dir", "./wiki", "finance", "sync"])
        self.assertEqual(data["StartCalendarInterval"],
                         [{"Hour": hour, "Minute": 17} for hour in (0, 6, 12, 18)])
        self.assertEqual(data["Umask"], 0o077)
        self.assertEqual(data["EnvironmentVariables"]["HOME"], str(self.home))
        self.assertEqual((self.home / ".local/state/augmentagent").stat().st_mode & 0o777, 0o700)

    def test_reinstall_and_uninstall_are_idempotent_and_targeted(self):
        for name in ("wiki", "finance"):
            with self.subTest(name=name):
                log = self.root / "launchctl.log"
                before_calls = log.read_text() if log.exists() else ""
                self.assertEqual(self.run_script("install", name).returncode, 0)
                before = self.plist(name).read_bytes()
                self.assertEqual(self.run_script("install", name).returncode, 0)
                self.assertEqual(self.plist(name).read_bytes(), before)
                other = "finance" if name == "wiki" else "wiki"
                self.assertEqual(self.run_script("uninstall", name).returncode, 0)
                self.assertEqual(self.run_script("uninstall", name).returncode, 0)
                self.assertFalse(self.plist(name).exists())
                self.assertNotIn(f"{LABEL}.{other}-sync", log.read_text()[len(before_calls):])

    def test_missing_wiki_or_binary_does_not_touch_live_service(self):
        self.assertEqual(self.run_script("install", "wiki").returncode, 0)
        self.repo.joinpath("wiki").rmdir()
        before = self.plist("wiki").read_bytes()
        calls = (self.root / "launchctl.log").read_text()
        result = self.run_script("install", "wiki")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.plist("wiki").read_bytes(), before)
        self.assertEqual((self.root / "launchctl.log").read_text(), calls)

    def test_public_github_origin_is_rejected_before_service_mutation(self):
        self.assertEqual(self.run_script("install", "wiki").returncode, 0)
        before = self.plist("wiki").read_bytes()
        calls = (self.root / "launchctl.log").read_text()
        self.env["TEST_WIKI_PRIVATE"] = "false"
        result = self.run_script("install", "wiki")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("private", result.stderr)
        self.assertEqual(self.plist("wiki").read_bytes(), before)
        self.assertEqual((self.root / "launchctl.log").read_text(), calls)

    def test_second_public_push_target_is_rejected(self):
        self.env["TEST_WIKI_REMOTE"] = (
            "git@github.com:owner/private-wiki.git\n"
            "https://github.com/owner/public-wiki.git"
        )
        self.env["TEST_WIKI_PUBLIC_SLUG"] = "owner/public-wiki"
        result = self.run_script("install", "wiki")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.plist("wiki").exists())
        calls = (self.root / "gh.log").read_text().splitlines()
        self.assertEqual(len(calls), 2)
        self.assertIn("owner/private-wiki", calls[0])
        self.assertIn("owner/public-wiki", calls[1])


if __name__ == "__main__":
    unittest.main()
