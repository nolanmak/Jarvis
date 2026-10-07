#!/usr/bin/env python3
"""Install/check the isolated PDF runtime used by the daemon and CLI (#1433)."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import venv

ROOT = Path(__file__).resolve().parent.parent
WORKER = ROOT / 'crates/augmentagent-docs/python/render_pdf.py'
REQUIREMENTS = WORKER.with_name('requirements.txt')


def runtime_path():
    data = Path(os.environ.get('XDG_DATA_HOME') or Path.home() / '.local/share')
    return data / 'augmentagent/pdf-runtime'


def check(python):
    # Check actual versions and render a document: imports alone miss missing
    # fonts and incompatible ReportLab installations. Ignore cwd/PYTHON*.
    pins = [line for line in REQUIREMENTS.read_text().splitlines()
            if line and not line.startswith('#')]
    code = ('from importlib.metadata import version\n'
            f'for pin in {pins!r}:\n'
            '    name, expected = pin.split("==")\n'
            '    assert version(name) == expected, f"{name}: expected {expected}"\n')
    subprocess.run([str(python), '-I', '-c', code], check=True, timeout=30)
    result = subprocess.run(
        [str(python), '-I', '-c', WORKER.read_text()],
        input=b'# PDF health check\n\n| Status |\n| --- |\n| **Ready** |\n\n[Link](https://example.com)',
        capture_output=True, timeout=30)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors='replace'))
    if not result.stdout.startswith(b'%PDF-') or not result.stdout.endswith(b'%%EOF\n'):
        raise RuntimeError('renderer did not produce a complete PDF')


def install():
    active = runtime_path()
    if os.environ.get('AUGMENTAGENT_PDF_PYTHON'):
        check(os.environ['AUGMENTAGENT_PDF_PYTHON'])
        return
    try:
        check(active / 'bin/python3')
        return
    except (OSError, subprocess.SubprocessError, RuntimeError):
        pass
    # Build separately so a failed install never replaces the working runtime.
    # --without-pip works on Debian hosts without the optional ensurepip package;
    # the host pip (>=22.3) can install into that interpreter with --python.
    active.parent.mkdir(parents=True, exist_ok=True)
    candidate = Path(tempfile.mkdtemp(prefix='pdf-runtime-', dir=active.parent))
    link = candidate.with_name(candidate.name + '.link')
    try:
        venv.EnvBuilder(with_pip=False).create(candidate)
        python = candidate / 'bin/python3'
        subprocess.run([sys.executable, '-m', 'pip', '--python', str(python),
                        'install', '--disable-pip-version-check', '-r', str(REQUIREMENTS)],
                       check=True, timeout=300)
        check(python)
        link.symlink_to(candidate.name, target_is_directory=True)
        link.replace(active)
    except BaseException:
        link.unlink(missing_ok=True)
        shutil.rmtree(candidate)
        raise
    print(f'PDF runtime ready: {active}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='offline health check; never install')
    args = parser.parse_args()
    try:
        if args.check:
            check(os.environ.get('AUGMENTAGENT_PDF_PYTHON') or runtime_path() / 'bin/python3')
        else:
            install()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        print(f'PDF runtime unavailable: {error}\n'
              'Run python3 scripts/pdf-runtime.py; requires python3-pip and fonts-liberation.',
              file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
