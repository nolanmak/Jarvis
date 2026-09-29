"""macOS voice jobs must keep credentials and runtime paths private."""

import os
import importlib.util
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / 'scripts/install-sidecar.py'
WRAPPER = ROOT / 'scripts/start-sidecar.py'


class VoiceLaunchdTests(unittest.TestCase):
    def test_voice_install_and_uninstall_keep_jobs_and_state_separate(self):
        with tempfile.TemporaryDirectory(prefix='jarvis-voice-install-', dir='/tmp') as scratch:
            root = Path(scratch)
            repo = root / 'checkout café'
            (repo / 'scripts/lib').mkdir(parents=True)
            for name in ('install-launchd-plist.sh', 'launchd-install.sh'):
                shutil.copy2(ROOT / 'scripts/lib' / name, repo / 'scripts/lib' / name)
            for relative in ('target/release/augmentagent', 'vendor/whisper/main',
                             'sidecars/discord-voice/node_modules/node/bin/node'):
                binary = repo / relative
                binary.parent.mkdir(parents=True, exist_ok=True)
                binary.write_text('#!/bin/sh\nexit 0\n')
                binary.chmod(0o755)
            for relative in ('vendor/whisper/models/ggml-medium.en.bin',
                             'sidecars/discord-voice/dist/src/main.js'):
                artifact = repo / relative
                artifact.parent.mkdir(parents=True, exist_ok=True)
                artifact.write_text('synthetic')
            home = root / 'owner home'
            config = home / '.config/augmentagent'
            config.mkdir(parents=True, mode=0o700)
            credential = config / 'discord-voice.env'
            credential.write_text('DEEPGRAM_API_KEY=synthetic\n')
            credential.chmod(0o600)
            fake_bin = root / 'bin'
            fake_bin.mkdir()
            (fake_bin / 'launchctl').write_text(
                '#!/bin/sh\nprintf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\n'
                'if [ "$1" = print ]; then exit 1; fi\nexit 0\n')
            (fake_bin / 'uname').write_text('#!/bin/sh\necho Darwin\n')
            for name in ('launchctl', 'uname'):
                (fake_bin / name).chmod(0o755)
            calls = root / 'launchctl.log'
            env = {**os.environ, 'HOME': str(home),
                   'XDG_CONFIG_HOME': str(home / '.config'),
                   'XDG_STATE_HOME': str(home / '.local/state'),
                   'PATH': f'{fake_bin}:/usr/bin:/bin', 'LAUNCHCTL_LOG': str(calls)}
            spec = importlib.util.spec_from_file_location('sidecar_installer', INSTALLER)
            installer = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(installer)
            with mock.patch.object(installer, 'ROOT', repo), \
                 mock.patch.object(installer, 'runtime_directory', return_value=root / 'runtime'), \
                 mock.patch.object(sys, 'platform', 'darwin'), \
                 mock.patch.dict(os.environ, env, clear=True):
                for name in ('discord-voice', 'telegram-capture'):
                    with mock.patch.object(sys, 'argv', ['install-sidecar.py', name]):
                        installer.main()
                agents = home / 'Library/LaunchAgents'
                voice_plist = agents / 'com.nolanmak.augmentagent.discord-voice.plist'
                capture_plist = agents / 'com.nolanmak.augmentagent.telegram-capture.plist'
                installed = voice_plist.read_bytes()
                credential.chmod(0o644)
                with mock.patch.object(sys, 'argv', ['install-sidecar.py', 'discord-voice']):
                    with self.assertRaises(SystemExit):
                        installer.main()
                self.assertEqual(voice_plist.read_bytes(), installed)
                self.assertTrue(capture_plist.is_file())
            session = home / '.local/state/augmentagent/voice-session.json'
            session.write_text('synthetic session')
            result = subprocess.run(
                ['bash', str(ROOT / 'scripts/uninstall-sidecar.sh'), 'discord-voice'],
                env=env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(voice_plist.exists())
            self.assertTrue(capture_plist.exists())
            self.assertEqual(session.read_text(), 'synthetic session')

    def test_voice_wrapper_loads_only_private_provider_keys_before_exec(self):
        with tempfile.TemporaryDirectory(prefix='jarvis-voice-wrapper-', dir='/tmp') as scratch:
            root = Path(scratch)
            config = root / 'discord-voice.env'
            config.write_text('DEEPGRAM_API_KEY=synthetic\n')
            config.chmod(0o600)
            socket = root / 'voice.sock'
            marker = root / 'executed'
            child = ('import os, pathlib, sys; '
                     'assert os.environ["DEEPGRAM_API_KEY"] == "synthetic"; '
                     'assert "NODE_OPTIONS" not in os.environ; '
                     'pathlib.Path(sys.argv[1]).write_text("ran")')
            env = {**os.environ, 'AUGMENTAGENT_VOICE_CREDENTIALS': str(config),
                   'NODE_OPTIONS': '--require /nonexistent'}
            command = [sys.executable, str(WRAPPER), 'discord-voice', str(socket),
                       sys.executable, '-c', child, str(marker)]
            result = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(marker.read_text(), 'ran')
            marker.unlink()
            config.write_text('DISCORD_BOT_TOKEN=synthetic\n')
            result = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())

    def test_voice_credentials_reject_public_mode_and_daemon_token(self):
        spec = importlib.util.spec_from_file_location('sidecar_installer', INSTALLER)
        installer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(installer)
        with tempfile.TemporaryDirectory(prefix='jarvis-voice-config-') as scratch:
            credential = Path(scratch) / 'discord-voice.env'
            credential.write_text('DEEPGRAM_API_KEY=synthetic\n')
            credential.chmod(0o644)
            with self.assertRaisesRegex(SystemExit, 'owner-private'):
                installer.validate_voice_credentials(credential)
            credential.chmod(0o600)
            installer.validate_voice_credentials(credential)
            credential.write_text('DISCORD_BOT_TOKEN=synthetic\n')
            with self.assertRaisesRegex(SystemExit, 'invalid or duplicate key'):
                installer.validate_voice_credentials(credential)
            credential.unlink()
            target = Path(scratch) / 'outside.env'
            target.write_text('DEEPGRAM_API_KEY=synthetic\n')
            target.chmod(0o600)
            credential.symlink_to(target)
            with self.assertRaisesRegex(SystemExit, 'safely'):
                installer.validate_voice_credentials(credential)

    def test_voice_jobs_render_with_private_separate_configuration(self):
        with tempfile.TemporaryDirectory(prefix='jarvis-voice-launchd-') as scratch:
            home = Path(scratch) / 'home with spaces café'
            home.mkdir()
            for name in ('discord-voice', 'telegram-capture'):
                with self.subTest(name=name):
                    output = Path(scratch) / f'{name}.plist'
                    env = {**os.environ, 'HOME': str(home),
                           'XDG_CONFIG_HOME': str(home / '.config'),
                           'XDG_STATE_HOME': str(home / '.local/state')}
                    result = subprocess.run(
                        [sys.executable, str(INSTALLER), name, '--render-only', str(output)],
                        env=env, text=True, capture_output=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    job = plistlib.loads(output.read_bytes())
                    self.assertEqual(job['Label'], f'com.nolanmak.augmentagent.{name}')
                    self.assertEqual(job['Umask'], 0o077)
                    self.assertIn(str(home), job['StandardErrorPath'])
                    self.assertNotIn('API_KEY', str(job))
                    self.assertNotIn('DISCORD_BOT_TOKEN', str(job))
                    self.assertNotIn('telegram-capture.env', str(job))
                    if name == 'discord-voice':
                        self.assertEqual(job['EnvironmentVariables']['AUGMENTAGENT_DISCORD_VOICE_SOCKET'],
                                         f'/tmp/augmentagent-{os.getuid()}/discord-voice.sock')
                        self.assertIn('sidecars/discord-voice/node_modules/node/bin/node',
                                      ' '.join(job['ProgramArguments']))
                    else:
                        self.assertIn('voice serve --dry-run false',
                                      ' '.join(job['ProgramArguments']))


if __name__ == '__main__':
    unittest.main()
