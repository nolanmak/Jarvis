"""Exercise the real CI entrypoint with synthetic command outcomes, no builds."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class ComputeCiTests(unittest.TestCase):
    def run_ci(self, mode):
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = Path(tmp)
            for name in ('cargo', 'deno'):
                command = bin_dir / name
                command.write_text('''#!/bin/sh
if [ "$CI_FIXTURE_MODE" = failed ]; then
  echo 'test result: ok. 1 passed; 0 failed;'
  exit 1
fi
if [ "$CI_FIXTURE_MODE" = empty ]; then
  echo 'test result: ok. 0 passed; 0 failed;'
  exit 0
fi
case "$0" in
  */cargo) echo 'test result: ok. 1 passed; 0 failed;' ;;
  */deno) echo 'ok | 1 passed | 0 failed (10ms)' ;;
esac
''')
                command.chmod(0o700)
            env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ.get('PATH', ''),
                       CI_FIXTURE_MODE=mode)
            return subprocess.run(['bash', str(ROOT / 'scripts/ci-code-mode.sh')],
                                  env=env, cwd=ROOT, capture_output=True, text=True, timeout=15)

    def test_zero_test_success_is_rejected(self):
        result = self.run_ci('empty')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('did not report any passing tests', result.stderr)

    def test_failed_command_cannot_hide_behind_success_text_or_tee(self):
        self.assertNotEqual(self.run_ci('failed').returncode, 0)

    def test_all_required_commands_must_finish(self):
        result = self.run_ci('passed')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count('test result: ok.'), 6)
        self.assertIn('ok | 1 passed | 0 failed', result.stdout)
