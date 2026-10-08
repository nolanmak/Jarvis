#!/usr/bin/python3
"""Guest-only wheel resolution and installation, always as the worker uid."""
import email.parser
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import zipfile


def pip_main(arguments):
    # The pinned archive is operator supplied and hash checked by the host.
    # Do not let pip settings, credential stores, or arbitrary origins widen
    # the package broker's policy. Each CLI invocation gets a fresh process.
    sys.path.insert(0, '/pip.whl')
    from pip._internal.cli.main import main
    from pip._internal.network.session import PipSession
    from pip._internal.exceptions import InstallationError
    from pip._internal.req import constructors
    from pip._vendor.packaging.requirements import Requirement
    from urllib.parse import urlsplit
    original = PipSession.request

    def guarded(self, method, url, *args, **kwargs):
        parsed = urlsplit(url)
        if (method.upper() not in ('GET', 'HEAD') or parsed.scheme != 'https'
                or parsed.netloc not in ('pypi.org', 'files.pythonhosted.org')
                or parsed.query or parsed.username or parsed.password):
            raise InstallationError('JARVIS_DEPENDENCY_POLICY: registry URL denied')
        return original(self, method, url, *args, **kwargs)

    PipSession.request = guarded
    original_requirement = constructors.install_req_from_req_string

    def registry_requirement(value, *args, **kwargs):
        # Reject every direct URL in dependency metadata before pip creates a
        # link candidate (including URLs at otherwise approved registries).
        # The operator-pinned pip is loaded before its resolver imports this
        # constructor; the real-VM fixture verifies that integration.
        if Requirement(value).url:
            raise InstallationError('JARVIS_DEPENDENCY_POLICY: transitive URL requirement')
        return original_requirement(value, *args, **kwargs)

    constructors.install_req_from_req_string = registry_requirement
    raise SystemExit(main(['--isolated', '--disable-pip-version-check', '--no-input', *arguments]))


def prepare():
    job = json.loads(Path('/prepare.json').read_text())
    root = Path('/cache/envs') / job['environmentId']
    wheels = root / 'wheels'; wheels.mkdir()
    staging = root / 'tmp'; staging.mkdir()
    # Isolated pip ignores configuration; TMPDIR remains a normal OS setting.
    os.environ['TMPDIR'] = str(staging)
    command = ['/usr/bin/python3', '-I', '-B', '/prepare.py', '--pip']
    common = ['--only-binary=:all:', '--no-cache-dir', '--retries', '0', '--timeout', '15',
              '--cert', '/etc/jarvis-registry-ca.pem']
    downloaded = subprocess.run([*command, 'download', *common, '--dest', str(wheels),
                                '--index-url', 'https://pypi.org/simple/', *job['requirements']])
    if downloaded.returncode:
        raise SystemExit(downloaded.returncode)
    sys.path.insert(0, '/pip.whl')
    from pip._vendor.packaging.requirements import Requirement
    from pip._vendor.packaging.utils import canonicalize_name, parse_wheel_filename
    locked, names = [], set()
    paths = sorted(wheels.iterdir())
    if not 1 <= len(paths) <= 256:
        raise ValueError('JARVIS_DEPENDENCY_POLICY: resolved package count')
    for path in paths:
        name, version, _, _ = parse_wheel_filename(path.name)
        name = canonicalize_name(name)
        if name in names or path.is_symlink() or not path.is_file():
            raise ValueError('JARVIS_DEPENDENCY_POLICY: invalid wheel set')
        names.add(name)
        with zipfile.ZipFile(path) as archive:
            metadata = [n for n in archive.namelist() if n.endswith('.dist-info/METADATA')]
            if len(metadata) != 1 or archive.getinfo(metadata[0]).file_size > 1024 * 1024:
                raise ValueError('JARVIS_DEPENDENCY_POLICY: wheel metadata')
            parsed = email.parser.BytesParser().parsebytes(archive.read(metadata[0]))
            if canonicalize_name(parsed['Name']) != name or parsed['Version'] != str(version):
                raise ValueError('JARVIS_DEPENDENCY_POLICY: wheel identity mismatch')
            for dependency in parsed.get_all('Requires-Dist', []):
                if Requirement(dependency).url:
                    raise ValueError('JARVIS_DEPENDENCY_POLICY: transitive URL requirement')
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        locked.append({'name': name, 'version': str(version), 'sha256': digest})
    locked.sort(key=lambda entry: entry['name'])
    lockfile = root / 'requirements.lock'
    lockfile.write_text(''.join(f"{p['name']}=={p['version']} --hash=sha256:{p['sha256']}\n" for p in locked))
    installed = subprocess.run([*command, 'install', *common, '--no-index', '--find-links', str(wheels),
                               '--require-hashes', '--no-deps', '--no-compile', '--target', str(root / 'site'),
                               '-r', str(lockfile)])
    if installed.returncode:
        raise SystemExit(installed.returncode)
    (root / 'lock.json').write_text(json.dumps(locked, separators=(',', ':')))


if __name__ == '__main__':
    if sys.argv[1:2] == ['--pip']:
        pip_main(sys.argv[2:])
    else:
        prepare()
