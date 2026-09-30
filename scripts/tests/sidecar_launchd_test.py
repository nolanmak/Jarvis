"""macOS sidecar service contracts with synthetic binaries and private state."""

import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import importlib.util


ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts/install-sidecar.py"


class SidecarLaunchdTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="jarvis-sidecar-test-", dir='/tmp')
        self.addCleanup(self.scratch.cleanup)
        self.home = Path(self.scratch.name) / "owner home café"
        self.home.mkdir()

    def render(self, name):
        output = Path(self.scratch.name) / f"{name}.plist"
        proc = subprocess.run(
            ["python3", str(INSTALLER), name, "--render-only", str(output)],
            env={**os.environ, "HOME": str(self.home),
                 "XDG_CONFIG_HOME": str(self.home / '.config'),
                 "XDG_STATE_HOME": str(self.home / '.local/state')},
            text=True, capture_output=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return plistlib.loads(output.read_bytes())

    def test_renderer_fetch_and_whatsapp_have_private_managed_jobs(self):
        for name in ("renderer", "fetch", "wa-sidecar"):
            with self.subTest(name=name):
                plist = self.render(name)
                self.assertEqual(plist["Label"], f"com.nolanmak.augmentagent.{name}")
                self.assertTrue(plist["RunAtLoad"])
                self.assertTrue(plist["KeepAlive"])
                self.assertEqual(plist["Umask"], 0o077)
                self.assertIn(str(self.home), plist["StandardErrorPath"])
                self.assertNotIn("API_KEY", str(plist))

    def test_socket_path_is_short_even_for_a_long_unicode_home(self):
        self.home = self.home / ("long café " * 20)
        self.home.mkdir(parents=True)
        for name, key in (("renderer", "AUGMENTAGENT_RENDERER_SOCK"),
                          ("fetch", "FETCH_SOCKET"), ("wa-sidecar", "AUGMENTAGENT_WA_SOCK")):
            with self.subTest(name=name):
                value = self.render(name)["EnvironmentVariables"][key]
                self.assertLess(len(os.fsencode(value)), 100)
                self.assertIn(str(os.getuid()), value)

    def test_uninstall_removes_only_selected_job_and_retains_session_state(self):
        agents = self.home / 'Library/LaunchAgents'
        agents.mkdir(parents=True)
        state = self.home / '.local/state/augmentagent'
        state.mkdir(parents=True)
        session = state / 'whatsmeow.db'
        session.write_text('synthetic session')
        for name in ('renderer', 'fetch', 'wa-sidecar'):
            (agents / f'com.nolanmak.augmentagent.{name}.plist').write_text('fixture')
        fake_bin = Path(self.scratch.name) / 'bin'
        fake_bin.mkdir()
        (fake_bin / 'uname').write_text('#!/bin/sh\necho Darwin\n')
        (fake_bin / 'launchctl').write_text(
            '#!/bin/sh\nprintf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\nexit 0\n')
        for name in ('uname', 'launchctl'):
            (fake_bin / name).chmod(0o755)
        log = Path(self.scratch.name) / 'calls.log'
        result = subprocess.run(
            ['bash', str(ROOT / 'scripts/uninstall-sidecar.sh'), 'wa-sidecar'],
            env={**os.environ, 'HOME': str(self.home),
                 'PATH': f'{fake_bin}:/usr/bin:/bin', 'LAUNCHCTL_LOG': str(log)},
            text=True, capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((agents / 'com.nolanmak.augmentagent.wa-sidecar.plist').exists())
        self.assertTrue((agents / 'com.nolanmak.augmentagent.renderer.plist').exists())
        self.assertTrue((agents / 'com.nolanmak.augmentagent.fetch.plist').exists())
        self.assertEqual(session.read_text(), 'synthetic session')
        self.assertIn('bootout gui/', log.read_text())

    def test_installer_bootstraps_only_built_sidecars_and_retains_other_jobs(self):
        repo = Path(self.scratch.name) / 'checkout café'
        (repo / 'scripts/lib').mkdir(parents=True)
        for name in ('install-launchd-plist.sh', 'launchd-install.sh'):
            shutil.copy2(ROOT / 'scripts/lib' / name, repo / 'scripts/lib' / name)
        (repo / 'sidecars/renderer/node_modules/@remotion/renderer').mkdir(parents=True)
        (repo / 'sidecars/fetch/node_modules').mkdir(parents=True)
        (repo / 'sidecars/fetch/dist').mkdir(parents=True)
        (repo / 'sidecars/fetch/dist/index.js').write_text('')
        binary = repo / 'sidecars/wa-sidecar/wa-sidecar'
        binary.parent.mkdir(parents=True)
        binary.write_text('#!/bin/sh\n')
        binary.chmod(0o755)
        fake_bin = Path(self.scratch.name) / 'bin'
        fake_bin.mkdir()
        (fake_bin / 'node').write_text('#!/bin/sh\nexit 0\n')
        (fake_bin / 'node').chmod(0o755)
        calls = Path(self.scratch.name) / 'launchctl.log'
        (fake_bin / 'launchctl').write_text(
            '#!/bin/sh\nprintf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\n'
            'if [ "$1" = print ]; then exit 1; fi\nexit 0\n')
        (fake_bin / 'launchctl').chmod(0o755)
        spec = importlib.util.spec_from_file_location('sidecar_installer', INSTALLER)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        env = {**os.environ, 'HOME': str(self.home),
               'XDG_CONFIG_HOME': str(self.home / '.config'),
               'XDG_STATE_HOME': str(self.home / '.local/state'),
               'PATH': f'{fake_bin}:/usr/bin:/bin', 'LAUNCHCTL_LOG': str(calls)}
        runtime = Path(self.scratch.name) / 'runtime'
        with mock.patch.object(module, 'ROOT', repo), \
             mock.patch.object(module, 'runtime_directory', return_value=runtime), \
             mock.patch.object(sys, 'platform', 'darwin'), \
             mock.patch.dict(os.environ, env, clear=True):
            for name in ('renderer', 'fetch', 'wa-sidecar'):
                with mock.patch.object(sys, 'argv', ['install-sidecar.py', name]):
                    module.main()
            renderer_plist = self.home / 'Library/LaunchAgents/com.nolanmak.augmentagent.renderer.plist'
            installed = renderer_plist.read_bytes()
            shutil.rmtree(repo / 'sidecars/renderer/node_modules')
            previous_calls = calls.read_text()
            with mock.patch.object(sys, 'argv', ['install-sidecar.py', 'renderer']):
                with self.assertRaises(SystemExit):
                    module.main()
            self.assertEqual(renderer_plist.read_bytes(), installed)
            self.assertEqual(calls.read_text(), previous_calls)
            fetch_plist = self.home / 'Library/LaunchAgents/com.nolanmak.augmentagent.fetch.plist'
            installed_fetch = fetch_plist.read_bytes()
            credential = self.home / '.config/augmentagent/fetch.env'
            credential.parent.mkdir(parents=True, exist_ok=True)
            credential.write_text('FIRECRAWL_API_KEY=synthetic\n')
            credential.chmod(0o644)
            with mock.patch.object(sys, 'argv', ['install-sidecar.py', 'fetch']):
                with self.assertRaises(SystemExit):
                    module.main()
            self.assertEqual(fetch_plist.read_bytes(), installed_fetch)
            self.assertEqual(calls.read_text(), previous_calls)
        content = calls.read_text()
        for name in ('renderer', 'fetch', 'wa-sidecar'):
            label = f'com.nolanmak.augmentagent.{name}'
            self.assertIn(f'bootstrap gui/{os.getuid()}', content)
            self.assertTrue((self.home / 'Library/LaunchAgents' / f'{label}.plist').is_file())
        self.assertEqual((self.home / 'Library/LaunchAgents').stat().st_mode & 0o777, 0o700)


if __name__ == "__main__":
    unittest.main()
