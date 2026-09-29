"""Native adapter service installation never uses live Runpod credentials."""
import json
import importlib.util
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / 'scripts/install-runpod-adapter.py'
LAUNCHER = ROOT / 'scripts/start-runpod-adapter.py'


class AdapterInstall(unittest.TestCase):
    def test_private_launcher_refuses_a_fifo_without_blocking(self):
        with tempfile.TemporaryDirectory() as directory:
            pipe = Path(directory) / 'runpod-adapter.env'
            os.mkfifo(pipe, 0o600)
            result = subprocess.run([sys.executable, str(LAUNCHER), str(pipe),
                                     str(Path(directory) / 'server.py')],
                                    capture_output=True, text=True, timeout=2)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('owner-private', result.stderr)

    def test_private_launcher_real_exec_keeps_keys_out_of_argv(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            credential = root / 'runpod-adapter.env'
            credential.write_text('RUNPOD_API_KEY=synthetic-upstream\nADAPTER_API_KEY=synthetic-client\n')
            credential.chmod(0o600)
            receipt = root / 'receipt.json'
            server = root / 'server.py'
            server.write_text("import json,os,sys\nfrom pathlib import Path\n"
                              "Path(os.environ['TEST_RECEIPT']).write_text(json.dumps({'argv':sys.argv,'upstream':os.environ['RUNPOD_API_KEY'],'client':os.environ['ADAPTER_API_KEY']}))\n")
            result = subprocess.run([sys.executable, str(LAUNCHER), str(credential), str(server)],
                                    env={**os.environ, 'TEST_RECEIPT': str(receipt)},
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            content = json.loads(receipt.read_text())
            self.assertEqual(content['argv'], [str(server)])
            self.assertEqual(content['upstream'], 'synthetic-upstream')
            self.assertEqual(content['client'], 'synthetic-client')

    def test_darwin_install_loads_private_job_and_retains_journal_on_reinstall(self):
        spec = importlib.util.spec_from_file_location('runpod_installer', INSTALLER)
        installer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(installer)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home, config, data = (root / name for name in ('home with spaces Ω', 'config & routes', 'data Ω'))
            home.mkdir()
            private = config / 'augmentagent'
            private.mkdir(parents=True)
            credential = private / 'runpod-adapter.env'
            credential.write_text('RUNPOD_API_KEY=synthetic-upstream\nADAPTER_API_KEY=synthetic-client\n')
            credential.chmod(0o600)
            routes = private / 'runpod-routes.json'
            routes.write_text('{}')
            routes.chmod(0o600)
            env = {'HOME': str(home), 'XDG_CONFIG_HOME': str(config), 'XDG_DATA_HOME': str(data)}
            calls = []

            def run(argv, **_kwargs):
                calls.append(argv)
                Path(argv[4]).replace(Path(argv[3]))
                return subprocess.CompletedProcess(argv, 0)

            with mock.patch.dict(os.environ, env), \
                 mock.patch.object(installer.sys, 'platform', 'darwin'), \
                 mock.patch.object(installer.sys, 'argv', ['installer']), \
                 mock.patch.object(installer.subprocess, 'run', side_effect=run):
                installer.main()
                journal = data / 'augmentagent/runpod-adapter/state/jobs.sqlite3'
                journal.write_text('retained job')
                installer.main()
                credential.chmod(0o644)
                with self.assertRaisesRegex(SystemExit, 'owner-private'):
                    installer.main()
            plist = home / 'Library/LaunchAgents/com.nolanmak.augmentagent.runpod-adapter.plist'
            self.assertEqual(plist.stat().st_mode & 0o777, 0o600)
            self.assertEqual((data / 'augmentagent/runpod-adapter/state').stat().st_mode & 0o777, 0o700)
            self.assertTrue((data / 'augmentagent/runpod-adapter/server.py').is_file())
            self.assertEqual(journal.read_text(), 'retained job')
            self.assertEqual(len(calls), 2)
            self.assertTrue(all(args[:2] == ['/bin/bash', str(ROOT / 'scripts/lib/install-launchd-plist.sh')] for args in calls))

    def test_private_launcher_passes_secrets_only_in_environment(self):
        spec = importlib.util.spec_from_file_location('runpod_launcher', LAUNCHER)
        launcher = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(launcher)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            credential = root / 'runpod-adapter.env'
            credential.write_text('RUNPOD_API_KEY=synthetic-upstream\nADAPTER_API_KEY=synthetic-client\n')
            credential.chmod(0o600)
            server = root / 'server.py'
            with mock.patch.object(launcher.sys, 'argv', ['launcher', str(credential), str(server)]), \
                 mock.patch.object(launcher.os, 'execve') as execute:
                launcher.main()
            argv, environment = execute.call_args.args[1:]
            self.assertEqual(argv, [sys.executable, '-u', str(server)])
            self.assertEqual(environment['RUNPOD_API_KEY'], 'synthetic-upstream')
            self.assertEqual(environment['ADAPTER_API_KEY'], 'synthetic-client')
            self.assertNotIn('synthetic-upstream', str(argv))
            credential.chmod(0o644)
            with mock.patch.object(launcher.sys, 'argv', ['launcher', str(credential), str(server)]):
                with self.assertRaisesRegex(SystemExit, 'owner-private'):
                    launcher.main()

    def test_darwin_render_and_rerender_keep_private_account_and_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home, config, data = (root / name for name in ('home with spaces Ω', 'config & routes', 'data Ω'))
            private = config / 'augmentagent'
            private.mkdir(parents=True, mode=0o700)
            home.mkdir()
            credential = private / 'runpod-adapter.env'
            credential.write_text('RUNPOD_API_KEY=synthetic-upstream\nADAPTER_API_KEY=synthetic-client\n')
            credential.chmod(0o600)
            routes = private / 'runpod-routes.json'
            routes.write_text(json.dumps({'routes': []}))
            routes.chmod(0o600)
            env = {**os.environ, 'HOME': str(home), 'XDG_CONFIG_HOME': str(config),
                   'XDG_DATA_HOME': str(data)}
            rendered = root / 'rendered.plist'
            result = subprocess.run([sys.executable, str(INSTALLER), '--render-only', str(rendered)],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            plist_path = home / 'Library/LaunchAgents/com.nolanmak.augmentagent.runpod-adapter.plist'
            self.assertFalse(plist_path.exists())
            plist = plistlib.loads(rendered.read_bytes())
            self.assertEqual(plist['EnvironmentVariables']['RUNPOD_ADAPTER_HOST'], '127.0.0.1')
            self.assertEqual(plist['EnvironmentVariables']['RUNPOD_ADAPTER_PORT'], '20129')
            self.assertEqual(plist['EnvironmentVariables']['RUNPOD_ADAPTER_ROUTES'], str(routes))
            self.assertNotIn('synthetic-upstream', rendered.read_text())
            self.assertEqual(rendered.stat().st_mode & 0o777, 0o600)
            journal = data / 'augmentagent/runpod-adapter/state/jobs.sqlite3'
            journal.parent.mkdir(parents=True, exist_ok=True)
            journal.write_text('synthetic journal')
            result = subprocess.run([sys.executable, str(INSTALLER), '--render-only', str(rendered)],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(journal.read_text(), 'synthetic journal')
            self.assertEqual(credential.read_text(), 'RUNPOD_API_KEY=synthetic-upstream\nADAPTER_API_KEY=synthetic-client\n')

    def test_missing_private_credentials_fail_before_install(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / 'home'
            home.mkdir()
            env = {**os.environ, 'HOME': str(home), 'XDG_CONFIG_HOME': str(root / 'config'),
                   'XDG_DATA_HOME': str(root / 'data')}
            result = subprocess.run([sys.executable, str(INSTALLER), '--render-only', str(root / 'rendered.plist')],
                                    env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('runpod-adapter.env', result.stderr)
            self.assertFalse((home / 'Library/LaunchAgents/com.nolanmak.augmentagent.runpod-adapter.plist').exists())


if __name__ == '__main__':
    unittest.main()
