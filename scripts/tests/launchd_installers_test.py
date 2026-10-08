"""Contract tests for launchd installers; never touch the user's services."""

import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[2]
INSTALLERS = (
    "autostart",
    "dashboard",
    "autoupdate",
    "calendar",
    "digest",
    "research",
    "wix-sync",
)
LABELS = {
    "autostart": "com.nolanmak.augmentagent",
    "dashboard": "com.nolanmak.augmentagent-dashboard",
    "autoupdate": "com.nolanmak.augmentagent.updater",
    "calendar": "com.nolanmak.augmentagent.calendar",
    "digest": "com.nolanmak.augmentagent.digest",
    "research": "com.nolanmak.augmentagent.research",
    "wix-sync": "com.nolanmak.augmentagent.wix-sync",
}


class LaunchdInstallerTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="jarvis-launchd-test-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.repo = self.root / "Jarvis & <test> café"
        self.home = self.root / "home & <test> café"
        (self.repo / "scripts").mkdir(parents=True)
        (self.repo / "scripts/lib").mkdir()
        shutil.copy2(SOURCE / "scripts/lib/launchd-install.sh", self.repo / "scripts/lib")
        (self.repo / "target/release").mkdir(parents=True)
        (self.repo / "dist").mkdir()
        (self.home / ".local/bin").mkdir(parents=True)
        for name in INSTALLERS:
            shutil.copy2(SOURCE / "scripts" / f"install-{name}.sh", self.repo / "scripts")
        shutil.copy2(SOURCE / "scripts/uninstall-autostart.sh", self.repo / "scripts")
        for name in ("run-rs.sh", "run-dashboard.sh", "check-for-updates.sh", "calendar-poll.sh",
                     "daily-digest.sh", "daily-research.sh", "wix-events-sync.mjs"):
            (self.repo / "scripts" / name).write_text("#!/bin/sh\nexit 0\n")
            (self.repo / "scripts" / name).chmod(0o755)
        # Provisioning belongs to pdf_runtime_test; this fixture tests launchd.
        (self.repo / "scripts/pdf-runtime.py").write_text("raise SystemExit(0)\n")
        (self.repo / "target/release/augmentagent").write_text("#!/bin/sh\nexit 0\n")
        (self.repo / "target/release/augmentagent").chmod(0o755)
        (self.repo / "dist/dashboard-server.js").write_text("")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, body in {
            "uname": "echo Darwin\n",
            "launchctl": 'printf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\n'
                         'if [ "$1" = print ]; then [ -f "$LAUNCHCTL_LOADED" ]; exit; fi\n'
                         'if [ "$1" = bootout ]; then rm -f "$LAUNCHCTL_LOADED"; exit; fi\n'
                         'if [ "$1" = bootstrap ] && [ -f "$LAUNCHCTL_FAIL_ONCE" ]; then\n'
                         '  rm "$LAUNCHCTL_FAIL_ONCE"; exit 1\nfi\n'
                         '[ "$1" = bootstrap ] && touch "$LAUNCHCTL_LOADED"\nexit 0\n',
            "node": "exit 0\n",
            "deno": "exit 0\n",
            "jq": "exit 0\n",
            "codex": "exit 0\n",
            "claude": "exit 0\n",
            "cargo": "exit 0\n",
            "npm": "exit 0\n",
        }.items():
            path = self.bin / name
            path.write_text("#!/bin/sh\n" + body)
            path.chmod(0o755)
        self.env = {**os.environ, "HOME": str(self.home), "PATH": f"{self.bin}:/usr/bin:/bin",
                    "XDG_STATE_HOME": str(self.home / ".local/state"),
                    "AUGMENTAGENT_AUTOSTART_DRY_RUN": "true",
                    "AUGMENTAGENT_REASONER_CHAIN": "codex",
                    "LAUNCHCTL_LOG": str(self.root / "launchctl.log"),
                    "LAUNCHCTL_LOADED": str(self.root / "launchctl.loaded"),
                    "LAUNCHCTL_FAIL_ONCE": str(self.root / "fail-bootstrap-once")}

    def install(self, name):
        result = subprocess.run([str(self.repo / "scripts" / f"install-{name}.sh")],
                                cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return plistlib.loads((self.home / "Library/LaunchAgents" / f"{LABELS[name]}.plist").read_bytes())

    def test_all_installers_preserve_special_paths(self):
        for name in INSTALLERS:
            with self.subTest(installer=name):
                data = self.install(name)
                self.assertEqual(data["WorkingDirectory"], str(self.repo))
                self.assertTrue(data["ProgramArguments"][0].startswith(str(self.repo)) or name == "wix-sync")
                self.assertIn(str(self.home), data["StandardErrorPath"])
                if "EnvironmentVariables" in data:
                    self.assertIn(str(self.bin), data["EnvironmentVariables"]["PATH"])
                if name == "autostart":
                    self.assertEqual(data["ProgramArguments"][-1], "true")

    def test_missing_selected_provider_preserves_installed_job(self):
        old = self.install("autostart")
        path = self.home / "Library/LaunchAgents" / f"{LABELS['autostart']}.plist"
        before = path.read_bytes()
        self.env["AUGMENTAGENT_REASONER_CHAIN"] = "gemini"
        result = subprocess.run([str(self.repo / "scripts/install-autostart.sh")],
                                cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("gemini", result.stderr)
        self.assertEqual(path.read_bytes(), before)
        self.assertNotIn("bootout", (self.root / "launchctl.log").read_text())

    def test_selected_chain_from_dotenv_requires_only_selected_cli(self):
        self.env.pop("AUGMENTAGENT_REASONER_CHAIN")
        (self.repo / ".env").write_text("AUGMENTAGENT_REASONER_CHAIN='codex,cerebras'\n")
        (self.bin / "claude").unlink()
        data = self.install("autostart")
        self.assertEqual(data["ProgramArguments"][-1], "true")
        self.assertIn(str(self.bin), data["EnvironmentVariables"]["PATH"])

    def test_failed_bootstrap_restores_previous_plist(self):
        self.install("autostart")
        path = self.home / "Library/LaunchAgents" / f"{LABELS['autostart']}.plist"
        before = path.read_bytes()
        (self.root / "fail-bootstrap-once").touch()
        self.env["AUGMENTAGENT_AUTOSTART_DRY_RUN"] = "false"
        result = subprocess.run([str(self.repo / "scripts/install-autostart.sh")],
                                cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(plistlib.loads(path.read_bytes())["ProgramArguments"][-1], "true")
        self.assertTrue((self.root / "launchctl.loaded").exists())
        calls = (self.root / "launchctl.log").read_text()
        self.assertEqual(calls.count("bootstrap gui/"), 3)
        self.assertEqual(calls.count("bootout gui/"), 2)

    def test_invalid_candidate_does_not_unload_existing_service(self):
        self.install("autostart")
        path = self.home / "Library/LaunchAgents" / f"{LABELS['autostart']}.plist"
        before = path.read_bytes()
        bad = path.with_suffix(".new")
        bad.write_text("<plist><dict><invalid></dict></plist>")
        command = (f'source "$REPO/scripts/lib/launchd-install.sh"; '
                   f'launchd_install "{LABELS["autostart"]}" "$PLIST" "$BAD" true')
        env = {**self.env, "REPO": str(self.repo), "PLIST": str(path), "BAD": str(bad)}
        result = subprocess.run(["bash", "-c", command], cwd=self.repo,
                                env=env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(path.read_bytes(), before)
        self.assertTrue((self.root / "launchctl.loaded").exists())
        self.assertNotIn("bootout", (self.root / "launchctl.log").read_text())

    def test_reinstall_and_uninstall_touch_only_the_target_job(self):
        self.install("autostart")
        first = self.home / "Library/LaunchAgents" / f"{LABELS['autostart']}.plist"
        before = first.read_bytes()
        self.install("autostart")
        self.assertEqual(first.read_bytes(), before)
        result = subprocess.run([str(self.repo / "scripts/uninstall-autostart.sh")],
                                cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(first.exists())
        self.assertFalse((self.root / "launchctl.loaded").exists())
        for line in (self.root / "launchctl.log").read_text().splitlines():
            self.assertNotIn("augmentagent-dashboard", line)


if __name__ == "__main__":
    unittest.main()
