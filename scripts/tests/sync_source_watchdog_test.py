"""Contract tests for the data-source watchdog.

The exporters can only export what their source app has written. Notes.app and
WhatsApp both stopped running on this Mac — Notes for ~8 weeks — so their
SQLite stores went stale and the sync jobs dutifully reported "0 changed"
forever. The watchdog relaunches a source app when its database has gone cold,
so a closed app cannot silently stop data collection.

Synthetic: no real app is launched and no real launchctl is called.
"""

import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import time
import unittest

SOURCE = Path(__file__).resolve().parents[2]
LABEL = "com.nolanmak.augmentagent"


class SyncSourceWatchdogTest(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="jarvis-watchdog-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.repo = self.root / "checkout"
        self.home = self.root / "home"
        (self.repo / "scripts/lib").mkdir(parents=True)
        self.home.mkdir()
        for name in ("launchd-install.sh", "notify-owner.sh"):
            src = SOURCE / "scripts/lib" / name
            if src.exists():
                shutil.copy2(src, self.repo / "scripts/lib")
        for name in ("sync-source-watchdog.sh", "install-sync-watchdog.sh",
                     "uninstall-sync-watchdog.sh"):
            src = SOURCE / "scripts" / name
            if src.exists():
                shutil.copy2(src, self.repo / "scripts")

        # Stand in for the two source databases.
        self.notes_db = self.root / "notes/NoteStore.sqlite"
        self.wa_db = self.root / "wa/ChatStorage.sqlite"
        for db in (self.notes_db, self.wa_db):
            db.parent.mkdir(parents=True)
            db.write_text("x")

        self.bin = self.root / "bin"
        self.bin.mkdir()
        stubs = {
            "uname": "echo Darwin\n",
            "launchctl": 'printf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\n'
                         'if [ "$1" = print ]; then [ -f "$LAUNCHCTL_LOADED" ]; exit; fi\n'
                         'if [ "$1" = bootout ]; then rm -f "$LAUNCHCTL_LOADED"; exit; fi\n'
                         'if [ "$1" = bootstrap ]; then touch "$LAUNCHCTL_LOADED"; exit; fi\n'
                         'exit 0\n',
            # Records every launch request so a test can assert which app was opened.
            "open": 'printf "%s\\n" "$*" >> "$OPEN_LOG"\n',
            # Reports an app as running only when the test says so.
            "pgrep": 'case "$*" in *Notes*) [ -n "${NOTES_RUNNING:-}" ] ;; '
                     '*WhatsApp*) [ -n "${WA_RUNNING:-}" ] ;; *) false ;; esac\n'
                     'exit $?\n',
            # Stands in for the HTTP client: records the arguments and the
            # posted body so a test can assert what reached the owner channel
            # without any network traffic.
            "curl": 'printf "%s\\n" "$*" >> "$CURL_LOG"\n'
                    'exit "${CURL_EXIT:-0}"\n',
        }
        for name, body in stubs.items():
            path = self.bin / name
            path.write_text("#!/bin/sh\n" + body)
            path.chmod(0o755)

        self.env = {
            **os.environ,
            "HOME": str(self.home),
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "XDG_STATE_HOME": str(self.home / ".local/state"),
            "LAUNCHCTL_LOG": str(self.root / "launchctl.log"),
            "LAUNCHCTL_LOADED": str(self.root / "loaded"),
            "OPEN_LOG": str(self.root / "open.log"),
            "CURL_LOG": str(self.root / "curl.log"),
            "DISCORD_WEBHOOK_URL": "https://hooks.example.invalid/watchdog-stub",
            "WATCHDOG_NOTES_DB": str(self.notes_db),
            "WATCHDOG_WHATSAPP_DB": str(self.wa_db),
            "WATCHDOG_MAX_AGE_S": "3600",
        }

    # --- helpers ---------------------------------------------------------
    def age(self, path, seconds):
        """Backdate a file so the watchdog sees it as cold."""
        when = time.time() - seconds
        os.utime(path, (when, when))

    def run_watchdog(self, **overrides):
        # An override of None unsets the variable, so a test can run with no
        # webhook configured at all.
        env = {**self.env, **overrides}
        env = {k: v for k, v in env.items() if v is not None}
        return subprocess.run([str(self.repo / "scripts/sync-source-watchdog.sh")],
                              cwd=self.repo, env=env,
                              text=True, capture_output=True)

    def opened(self):
        log = self.root / "open.log"
        return log.read_text() if log.exists() else ""

    def posted(self):
        log = self.root / "curl.log"
        return log.read_text() if log.exists() else ""

    # --- the contract ----------------------------------------------------
    def test_mtime_is_read_on_bsd_and_gnu_stat(self):
        # `stat -f %m` is BSD-only; on GNU coreutils -f means --file-system and
        # the mtime read fails, so nothing is ever judged cold. CI runs Linux,
        # so the freshness logic has to work with either stat.
        self.age(self.notes_db, 7 * 86400)
        result = self.run_watchdog()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cold", result.stdout + result.stderr,
                      "freshness could not be computed with this platform's stat")

    def test_cold_database_relaunches_its_app(self):
        self.age(self.notes_db, 7 * 86400)
        result = self.run_watchdog()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Notes", self.opened())

    def test_fresh_database_is_left_alone(self):
        # Both written just now: nothing is wrong, so nothing is launched.
        result = self.run_watchdog()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.opened().strip(), "")

    def test_each_app_is_judged_on_its_own_database(self):
        # Only WhatsApp is cold. Notes must not be relaunched for it.
        self.age(self.wa_db, 7 * 86400)
        self.run_watchdog()
        opened = self.opened()
        self.assertIn("WhatsApp", opened)
        self.assertNotIn("Notes", opened)

    def test_a_running_app_with_a_cold_database_is_still_relaunched(self):
        # The exact failure seen in the field: WhatsApp's helper process was
        # alive while the main app was gone and the store was 13 days cold.
        # "Is the process up?" is the wrong question; freshness is the signal.
        self.age(self.wa_db, 13 * 86400)
        self.run_watchdog(WA_RUNNING="1")
        self.assertIn("WhatsApp", self.opened())

    def test_a_missing_database_is_reported_not_silently_skipped(self):
        self.notes_db.unlink()
        result = self.run_watchdog()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NoteStore.sqlite", result.stdout + result.stderr)

    def test_threshold_is_configurable_and_respected(self):
        # 2 h old with a 1 h threshold is cold; with a 24 h threshold it is not.
        self.age(self.notes_db, 2 * 3600)
        self.run_watchdog(WATCHDOG_MAX_AGE_S="3600")
        self.assertIn("Notes", self.opened())
        (self.root / "open.log").unlink()
        self.run_watchdog(WATCHDOG_MAX_AGE_S="86400")
        self.assertEqual(self.opened().strip(), "")

    def test_repeated_runs_do_not_relaunch_a_still_cold_app_every_tick(self):
        # Relaunching is not instant: a store can stay cold for a few ticks
        # while the app starts. Hammering `open` every minute is the bug a
        # naive watchdog introduces, so a backoff stamp must suppress it.
        self.age(self.notes_db, 7 * 86400)
        self.run_watchdog()
        first = self.opened().count("Notes")
        self.run_watchdog()
        self.assertEqual(self.opened().count("Notes"), first,
                         "watchdog relaunched the app again inside the backoff window")

    def test_a_store_cold_past_the_alarm_threshold_is_escalated(self):
        # Relaunching can fail for reasons the watchdog cannot fix: the app is
        # uninstalled, signed out, or needs a device re-pair. After a long cold
        # spell the operator has to be told, or this becomes the same silent
        # outage with extra steps.
        self.age(self.notes_db, 3 * 86400)
        result = self.run_watchdog(WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ALARM", result.stdout + result.stderr)

    def test_a_brief_cold_spell_does_not_escalate(self):
        # Cold enough to relaunch, nowhere near cold enough to alarm.
        self.age(self.notes_db, 7 * 3600)
        result = self.run_watchdog(WATCHDOG_MAX_AGE_S="21600",
                                   WATCHDOG_ALARM_AGE_S="86400")
        self.assertIn("Notes", self.opened())
        self.assertNotIn("ALARM", result.stdout + result.stderr)

    def test_an_alarm_escalation_reaches_the_owner_channel(self):
        # A log line nobody reads is not an alert. The ALARM is the one state
        # the watchdog cannot fix itself, so it has to leave the log file and
        # reach the owner the way the updater's stall alerts do.
        self.age(self.notes_db, 3 * 86400)
        result = self.run_watchdog(WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ALARM", result.stdout + result.stderr)
        posted = self.posted()
        self.assertIn("hooks.example.invalid/watchdog-stub", posted)
        self.assertIn("Notes", posted)

    def test_a_cold_store_below_the_alarm_threshold_does_not_notify(self):
        # Cold enough to relaunch is the routine case the watchdog handles on
        # its own; paging the owner for it would train him to ignore the page.
        self.age(self.notes_db, 7 * 3600)
        result = self.run_watchdog(WATCHDOG_MAX_AGE_S="21600",
                                   WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Notes", self.opened())
        self.assertEqual(self.posted().strip(), "")

    def test_repeated_alarms_do_not_notify_every_tick(self):
        # The alarm state persists for as long as the app stays uninstalled.
        # At a 15-minute interval an unthrottled post is ~96 messages a day.
        self.age(self.notes_db, 3 * 86400)
        self.run_watchdog(WATCHDOG_ALARM_AGE_S="86400")
        first = self.posted().count("watchdog-stub")
        self.assertEqual(first, 1, self.posted())
        self.run_watchdog(WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(self.posted().count("watchdog-stub"), first,
                         "watchdog posted again inside the throttle window")

    def test_an_unconfigured_webhook_is_not_fatal(self):
        self.age(self.notes_db, 3 * 86400)
        result = self.run_watchdog(DISCORD_WEBHOOK_URL=None,
                                   WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ALARM", result.stdout + result.stderr)
        self.assertEqual(self.posted().strip(), "")

    def test_a_failing_webhook_does_not_fail_the_tick(self):
        # Same invariant as a failed `open`: one undeliverable alert must not
        # stop the apps after it from being judged.
        self.age(self.notes_db, 3 * 86400)
        self.age(self.wa_db, 3 * 86400)
        result = self.run_watchdog(CURL_EXIT="1", WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ALARM", result.stdout + result.stderr)
        self.assertIn("WhatsApp", self.opened())

    def test_a_failed_post_is_retried_on_the_next_tick(self):
        # A Discord outage must not consume the throttle window: nobody was
        # told, so the next tick still owes the owner an alert.
        self.age(self.notes_db, 3 * 86400)
        self.run_watchdog(CURL_EXIT="1", WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(self.posted().count("watchdog-stub"), 1, self.posted())
        self.run_watchdog(WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(self.posted().count("watchdog-stub"), 2,
                         "watchdog did not retry after a failed delivery")
        # Once delivery succeeds the window applies again.
        self.run_watchdog(WATCHDOG_ALARM_AGE_S="86400")
        self.assertEqual(self.posted().count("watchdog-stub"), 2, self.posted())

    def test_install_writes_a_loaded_launchd_job(self):
        result = subprocess.run([str(self.repo / "scripts/install-sync-watchdog.sh")],
                                cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        plist = self.home / "Library/LaunchAgents" / f"{LABEL}.sync-watchdog.plist"
        self.assertTrue(plist.exists(), "no launchd plist written")
        data = plistlib.loads(plist.read_bytes())
        self.assertEqual(data["ProgramArguments"],
                         [str(self.repo / "scripts/sync-source-watchdog.sh")])
        self.assertTrue(data["RunAtLoad"])
        self.assertGreaterEqual(data["StartInterval"], 300)

    def test_install_is_idempotent(self):
        script = str(self.repo / "scripts/install-sync-watchdog.sh")
        for _ in range(2):
            self.assertEqual(subprocess.run([script], cwd=self.repo, env=self.env,
                                            text=True, capture_output=True).returncode, 0)
        plists = list((self.home / "Library/LaunchAgents").glob("*sync-watchdog*"))
        self.assertEqual(len(plists), 1, plists)

    def test_uninstall_removes_the_job_and_keeps_data(self):
        subprocess.run([str(self.repo / "scripts/install-sync-watchdog.sh")],
                       cwd=self.repo, env=self.env, capture_output=True)
        result = subprocess.run([str(self.repo / "scripts/uninstall-sync-watchdog.sh")],
                                cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        plist = self.home / "Library/LaunchAgents" / f"{LABEL}.sync-watchdog.plist"
        self.assertFalse(plist.exists())
        self.assertTrue(self.notes_db.exists(), "uninstall must not touch source data")


if __name__ == "__main__":
    unittest.main()
