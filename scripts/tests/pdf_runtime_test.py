"""Exercise the production installer, health check and startup without global deps."""
import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[2]
# The real installer test already needs PyPI for renderer dependencies. Supply
# its installer in the fixture too: isolated HOME must not rely on user pip.
# Pin and verify the official wheel before loading any of its code.
PIP_URL = ('https://files.pythonhosted.org/packages/44/3c/'
           'd717024885424591d5376220b5e836c2d5293ce2011523c9de23ff7bf068/'
           'pip-25.3-py3-none-any.whl')
PIP_SHA256 = '9655943313a94722b7774661c21049070f6bbb0a1516bf02f7c8d5d9201514cd'


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
            with urllib.request.urlopen(PIP_URL, timeout=30) as response:
                wheel = response.read(4 * 1024 * 1024 + 1)
            self.assertLessEqual(len(wheel), 4 * 1024 * 1024)
            self.assertEqual(hashlib.sha256(wheel).hexdigest(), PIP_SHA256)
            bootstrap = root / 'pip-bootstrap'
            with zipfile.ZipFile(io.BytesIO(wheel)) as archive:
                archive.extractall(bootstrap)
            env['PYTHONPATH'] = str(bootstrap)
            env['PYTHONNOUSERSITE'] = '1'
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
