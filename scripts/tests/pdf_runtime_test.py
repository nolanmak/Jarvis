"""Exercise the production installer, health check and startup without global deps."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class PdfRuntimeTests(unittest.TestCase):
    def test_install_detect_loss_repair_and_preserve_on_failed_upgrade(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo = root / 'repo'
            (repo / 'scripts').mkdir(parents=True)
            worker_dir = repo / 'crates/augmentagent-docs/python'
            worker_dir.mkdir(parents=True)
            shutil.copy(ROOT / 'scripts/pdf-runtime.py', repo / 'scripts')
            for name in ('render_pdf.py', 'requirements.txt'):
                shutil.copy(ROOT / 'crates/augmentagent-docs/python' / name, worker_dir)
            env = {**os.environ, 'XDG_DATA_HOME': str(root / 'data')}
            env.pop('AUGMENTAGENT_PDF_PYTHON', None)
            active = root / 'data/augmentagent/pdf-runtime'

            def run(*args, success=True, extra_env=None):
                result = subprocess.run([sys.executable, str(repo / 'scripts/pdf-runtime.py'), *args],
                                        cwd=root, env={**env, **(extra_env or {})},
                                        capture_output=True, text=True, timeout=180)
                self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
                return result

            self.assertIn('PDF runtime unavailable', run('--check', success=False).stderr)
            run()
            run('--check')
            first = active.resolve()
            run(extra_env={'PIP_NO_INDEX': '1'})  # Healthy startup/deploy needs no network.
            self.assertEqual(active.resolve(), first)
            # Reproduce the actual incident: metadata remains but the import is gone.
            markdown = next(active.glob('lib/python*/site-packages/markdown'))
            shutil.rmtree(markdown)
            self.assertIn('No module named', run('--check', success=False).stderr)
            run()
            run('--check')
            repaired = active.resolve()
            self.assertNotEqual(first, repaired)
            pins = worker_dir / 'requirements.txt'
            pins.write_text(pins.read_text().replace('Markdown==3.5.2', 'Markdown==0.0.0'))
            run(success=False, extra_env={'PIP_NO_INDEX': '1'})
            self.assertEqual(active.resolve(), repaired)
            # The previously published environment is intact after a failed deploy.
            pins.write_text((ROOT / 'crates/augmentagent-docs/python/requirements.txt').read_text())
            run('--check')

    def test_startup_checks_before_launching_daemon(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            (repo / 'scripts').mkdir()
            (repo / 'target/release').mkdir(parents=True)
            shutil.copy(ROOT / 'scripts/run-rs.sh', repo / 'scripts')
            (repo / 'scripts/pdf-runtime.py').write_text('raise SystemExit(1)\n')
            for path, script in [
                (repo / 'scripts/vault-mount.sh', '#!/bin/sh\nexit 0\n'),
                (repo / 'target/release/augmentagent', '#!/bin/sh\ntouch launched\n'),
            ]:
                path.write_text(script)
                path.chmod(0o755)
            result = subprocess.run(['bash', str(repo / 'scripts/run-rs.sh'), 'serve'],
                                    cwd=repo, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0)
            self.assertIn('PDF generation unavailable', result.stderr)
            self.assertTrue((repo / 'launched').exists())
            (repo / 'launched').unlink()
            (repo / 'scripts/pdf-runtime.py').write_text('import sys\nassert sys.argv[1:] == ["--check"]\n')
            subprocess.run(['bash', str(repo / 'scripts/run-rs.sh'), 'serve'], cwd=repo, check=True)
            self.assertTrue((repo / 'launched').exists())


if __name__ == '__main__':
    unittest.main()
