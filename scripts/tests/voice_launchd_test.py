"""macOS voice jobs must keep credentials and runtime paths private."""

import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / 'scripts/install-sidecar.py'


class VoiceLaunchdTests(unittest.TestCase):
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
