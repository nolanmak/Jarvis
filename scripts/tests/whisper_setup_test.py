"""Local whisper provisioning must reject unverified runtime artifacts."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[1] / 'build-whisper.sh'


class WhisperSetupTests(unittest.TestCase):
    def test_verify_only_rejects_unpinned_or_corrupt_runtime_without_network(self):
        with tempfile.TemporaryDirectory(prefix='jarvis-whisper-setup-') as scratch:
            repo = Path(scratch)
            script = repo / 'scripts/build-whisper.sh'
            script.parent.mkdir()
            shutil.copy2(SOURCE, script)
            vendor = repo / 'vendor/whisper'
            (vendor / 'models').mkdir(parents=True)
            binary = vendor / 'main'
            binary.write_text('#!/bin/sh\nexit 0\n')
            binary.chmod(0o755)
            (vendor / 'models/ggml-medium.en.bin').write_text('synthetic-corrupt-model')
            result = subprocess.run(['bash', str(script), '--verify-only'],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0,
                                'unverified artifacts were accepted as a pinned voice runtime')
            self.assertIn('verification', result.stderr.lower())


if __name__ == '__main__':
    unittest.main()
