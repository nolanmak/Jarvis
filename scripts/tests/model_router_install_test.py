"""Private, atomic installer writes; no external services."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import plistlib
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location('model_router_installer', Path(__file__).resolve().parents[1] / 'install-model-router.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
launcher_spec = importlib.util.spec_from_file_location('model_router_launcher', Path(__file__).resolve().parents[1] / 'start-model-router.py')
launcher = importlib.util.module_from_spec(launcher_spec)
launcher_spec.loader.exec_module(launcher)


class PrivateConfiguration(unittest.TestCase):
    def test_credential_launcher_refuses_a_fifo_without_blocking(self):
        with tempfile.TemporaryDirectory() as directory:
            pipe = Path(directory) / '9router.env'
            os.mkfifo(pipe, 0o600)
            result = subprocess.run([sys.executable, str(installer.ROOT / 'scripts/start-model-router.py'),
                                     str(pipe), str(Path(directory) / 'node'), str(Path(directory) / 'server.js')],
                                    capture_output=True, text=True, timeout=2)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('owner-private', result.stderr)

    def test_credential_launcher_real_exec_keeps_secrets_out_of_argv(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            credential = root / '9router.env'
            credential.write_text('JWT_SECRET=synthetic-jwt\nINITIAL_PASSWORD=synthetic-password\n')
            credential.chmod(0o600)
            receipt = root / 'receipt.json'
            fake_node = root / 'node'
            fake_node.write_text(f'#!{sys.executable}\nimport json,os,sys\nfrom pathlib import Path\n'
                                 "Path(os.environ['TEST_RECEIPT']).write_text(json.dumps({'argv':sys.argv,'jwt':os.environ['JWT_SECRET'],'password':os.environ['INITIAL_PASSWORD']}))\n")
            fake_node.chmod(0o700)
            server = root / 'server.js'
            result = subprocess.run([sys.executable, str(installer.ROOT / 'scripts/start-model-router.py'),
                                     str(credential), str(fake_node), str(server)],
                                    env={**os.environ, 'TEST_RECEIPT': str(receipt)},
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            content = json.loads(receipt.read_text())
            self.assertEqual(content['argv'], [str(fake_node), str(server)])
            self.assertEqual(content['jwt'], 'synthetic-jwt')
            self.assertEqual(content['password'], 'synthetic-password')

    def test_credential_launcher_execs_without_putting_secrets_in_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            credential = root / '9router.env'
            credential.write_text('JWT_SECRET=synthetic-jwt\nINITIAL_PASSWORD=synthetic-password\n')
            credential.chmod(0o600)
            node, server = root / 'node', root / 'server.js'
            with mock.patch.object(launcher.sys, 'argv', ['launcher', str(credential), str(node), str(server)]), \
                 mock.patch.object(launcher.os, 'execve') as execute:
                launcher.main()
            argv = execute.call_args.args[1]
            environment = execute.call_args.args[2]
            self.assertEqual(argv, [str(node), str(server)])
            self.assertEqual(environment['JWT_SECRET'], 'synthetic-jwt')
            self.assertEqual(environment['INITIAL_PASSWORD'], 'synthetic-password')
            credential.chmod(0o644)
            with mock.patch.object(launcher.sys, 'argv', ['launcher', str(credential), str(node), str(server)]):
                with self.assertRaisesRegex(SystemExit, 'owner-private'):
                    launcher.main()

    def test_replace_is_private_and_does_not_follow_an_existing_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = root / 'unrelated'
            original.write_text('leave me alone')
            config = root / 'config'
            config.symlink_to(original)
            installer.private_write(config, 'new secret')
            self.assertEqual(original.read_text(), 'leave me alone')
            self.assertFalse(config.is_symlink())
            self.assertEqual(config.read_text(), 'new secret')
            self.assertEqual(os.stat(config).st_mode & 0o777, 0o600)
            installer.private_write(config, 'replacement secret')
            self.assertEqual(config.read_text(), 'replacement secret')
            self.assertEqual(len(list(root.iterdir())), 2)

    def test_darwin_install_renders_private_launch_agent_and_preserves_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'pinned source'
            built = source / '.next' / 'standalone'
            built.mkdir(parents=True)
            (built / 'custom-server.js').write_text('/* synthetic */')
            home = root / 'home with spaces Ω'
            home.mkdir()
            config = root / 'config & routes'
            data = root / 'data Ω'
            env = {'HOME': str(home), 'XDG_CONFIG_HOME': str(config), 'XDG_DATA_HOME': str(data)}
            calls = []

            def run(argv, **_kwargs):
                calls.append(argv)
                if argv[:2] == ['/bin/bash', str(installer.ROOT / 'scripts/lib/install-launchd-plist.sh')]:
                    Path(argv[4]).replace(Path(argv[3]))
                return SimpleNamespace(returncode=0, stdout='v22.23.2' if argv == ['/usr/bin/node', '--version'] else '')

            def api(endpoint, body=None, cookie=None):
                if endpoint == '/api/auth/login':
                    return {}, 'auth_token=synthetic'
                if endpoint == '/api/keys':
                    return {'key': 'synthetic-key'}, ''
                return {}, ''

            response = mock.MagicMock()
            response.__enter__.return_value.read.return_value = b''
            with mock.patch.dict(os.environ, env), \
                 mock.patch.object(installer.sys, 'platform', 'darwin'), \
                 mock.patch.object(installer.sys, 'argv', ['installer', '--built-source', str(source)]), \
                 mock.patch.object(installer.subprocess, 'check_output', return_value=installer.REVISION), \
                 mock.patch.object(installer.subprocess, 'run', side_effect=run), \
                 mock.patch.object(installer.shutil, 'which', side_effect=lambda name: '/usr/bin/node' if name == 'node' else '/usr/bin/python3'), \
                 mock.patch.object(installer, 'api', side_effect=api), \
                 mock.patch.object(installer.urllib.request, 'urlopen', return_value=response):
                installer.main()
                credential = (config / 'augmentagent' / '9router.env').read_bytes()
                router_config = config / 'augmentagent' / 'model-router.json'
                router_config.write_text('{"mode":"selected-by-owner"}')
                account_state = data / 'augmentagent' / '9router' / 'data' / 'accounts.json'
                account_state.parent.mkdir(parents=True, exist_ok=True)
                account_state.write_text('{"account":"synthetic"}')
                installer.main()
                self.assertEqual((config / 'augmentagent' / '9router.env').read_bytes(), credential)
                self.assertEqual(router_config.read_text(), '{"mode":"selected-by-owner"}')
                self.assertEqual(account_state.read_text(), '{"account":"synthetic"}')
                with mock.patch.object(installer.subprocess, 'run', return_value=SimpleNamespace(returncode=0, stdout='v20.0.0')):
                    with self.assertRaisesRegex(SystemExit, 'Node.js 22'):
                        installer.main()
                (config / 'augmentagent' / '9router.env').chmod(0o644)
                with self.assertRaisesRegex(SystemExit, 'owner-private'):
                    installer.main()
                (config / 'augmentagent' / '9router.env').chmod(0o600)

            plist_path = home / 'Library' / 'LaunchAgents' / 'com.nolanmak.augmentagent.model-router.plist'
            plist = plistlib.loads(plist_path.read_bytes())
            self.assertEqual(plist['ProgramArguments'], [
                installer.sys.executable, str(installer.ROOT / 'scripts/start-model-router.py'),
                str(config / 'augmentagent' / '9router.env'), '/usr/bin/node',
                str(data / 'augmentagent' / '9router' / (installer.REVISION + '-runpod-reconciliation-5') / 'custom-server.js'),
            ])
            self.assertEqual(plist['EnvironmentVariables']['HOSTNAME'], '127.0.0.1')
            self.assertEqual(plist['EnvironmentVariables']['PORT'], '20128')
            self.assertNotIn('synthetic-key', plist_path.read_text())
            self.assertEqual(plist_path.stat().st_mode & 0o777, 0o600)
            self.assertEqual((config / 'augmentagent' / '9router.env').stat().st_mode & 0o777, 0o600)
            self.assertTrue(any(args[:2] == ['/bin/bash', str(installer.ROOT / 'scripts/lib/install-launchd-plist.sh')] for args in calls))


if __name__ == '__main__':
    unittest.main()
