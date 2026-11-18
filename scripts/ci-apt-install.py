#!/usr/bin/env python3
"""Install CI packages, with signed retries for DNS or missing Ubuntu sources.

Temporary APT source options apply only to the fallback commands. Host source
files, signature keys and system DNS configuration are never changed.
"""
import argparse
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile


def dns_failure(output):
    if not re.search(r'Temporary failure resolving|Could not resolve', output):
        return False
    # A mixed error must not turn a signature, lock or package failure into a
    # mirror retry. Only APT's DNS/fetch aggregation diagnostics are eligible.
    for line in output.splitlines():
        if line.startswith('E: Failed to fetch ') and not re.search(r'Temporary failure resolving|Could not resolve', line):
            return False
        if line.startswith('E:') and not any(text in line for text in (
            'Failed to fetch ', 'Some index files failed to download',
            'Unable to fetch some archives')):
            return False
    return not re.search(r'NO_PUBKEY|not signed|signatures? (?:could|couldn|were|could not)|Hash Sum mismatch|Certificate verification failed', output, re.I)


def package_lookup_failure(output, packages):
    errors = [line for line in output.splitlines() if line.startswith('E:')]
    expected = {f'E: Unable to locate package {package}' for package in packages}
    return bool(errors) and all(line in expected for line in errors) and not re.search(
        r'Temporary failure resolving|Could not resolve|NO_PUBKEY|not signed|signatures? (?:could|couldn|were|could not)|Hash Sum mismatch|Certificate verification failed', output, re.I)


def missing_ubuntu_sources(run, common, suite):
    # APT resolves both legacy .list and deb822 .sources entries, including
    # Enabled/Types filtering. Origin/Codename come from the Release metadata;
    # a Microsoft source can use the Ubuntu codename without being Ubuntu.
    try:
        inventory = run([*common, 'indextargets', '--format', '$(ORIGIN)\t$(CODENAME)',
                         'Identifier: Packages'], stdout=subprocess.PIPE,
                        stderr=subprocess.STDOUT, text=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        return False
    if inventory.returncode:
        return False
    targets = []
    for line in inventory.stdout.splitlines():
        if not line:
            continue
        fields = line.split('\t')
        if len(fields) != 2 or '$(' in line:
            return False  # Unknown inventory must not change source selection.
        targets.append(fields)
    return ['Ubuntu', suite] not in targets


def playwright_dependencies(browser, *, run=subprocess.run):
    if browser != 'chrome':
        raise ValueError('unsupported browser dependency plan')
    result = run(['npx', '--no-install', 'playwright', 'install-deps', '--dry-run', browser],
                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=60)
    output = result.stdout
    if len(output) > 65536:
        raise ValueError('Playwright dependency plan exceeds bound')
    if result.returncode == 0 and output.strip() == 'All system dependencies are installed.':
        return []
    match = re.search(r'^Missing system dependencies \(([0-9]+)\):\n((?:  [a-z0-9][a-z0-9+.:-]*\n?)+)\Z', output, re.M)
    if result.returncode != 1 or not match:
        raise ValueError('Playwright dependency inventory unavailable or unsupported')
    packages = match[2].split()
    if not 0 < len(packages) <= 256 or len(packages) != int(match[1]) or len(set(packages)) != len(packages):
        raise ValueError('Playwright dependency inventory is inconsistent')
    return packages


def install(packages, *, run=subprocess.run, root=Path('/'), architecture=None, installed=None):
    if not packages or any(not re.fullmatch(r'[a-z0-9][a-z0-9+.\-]*(?::[a-z0-9]+)?', p) for p in packages):
        raise ValueError('expected explicit APT package names')
    if installed is None:
        inventory = subprocess.run(['dpkg-query', '-W', '-f=${binary:Package}\t${Status}\n', *packages],
                                   capture_output=True, text=True, timeout=30)
        # dpkg-query exits 1 when some packages are missing; valid installed
        # rows still describe the others. Errors never imply presence.
        installed = set()
        for line in inventory.stdout.splitlines():
            package, _, status = line.partition('\t')
            if status == 'install ok installed':
                installed.add(package)
                installed.add(package.split(':', 1)[0])
    packages = [package for package in packages if package not in installed]
    if not packages:
        print('All required CI APT packages are already installed', flush=True)
        return 0
    # Hosted runners have IPv4 egress; avoid an unavailable IPv6 route turning
    # a DNS-only mirror outage into a mixed error that must fail closed.
    common = ['sudo', 'apt-get', '-o', 'Acquire::ForceIPv4=true',
              '-o', 'Acquire::Retries=0',
              '-o', 'Acquire::http::Timeout=20', '-o', 'Acquire::https::Timeout=20',
              '-o', 'APT::Update::Error-Mode=any',
              '-o', 'Acquire::AllowInsecureRepositories=false',
              '-o', 'Acquire::AllowDowngradeToInsecureRepositories=false',
              '-o', 'APT::Get::AllowUnauthenticated=false']

    def execute(options):
        for action in [['update'], ['install', '-y', '--no-install-recommends', *packages]]:
            result = run([*common, *options, *action], stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, text=True, timeout=600)
            print(result.stdout, end='', flush=True)
            if result.returncode:
                return result, action[0]
        return result, "install"

    result, action = execute([])
    dns = dns_failure(result.stdout)
    lookup = action == "install" and package_lookup_failure(result.stdout, packages)
    if not result.returncode or not (dns or lookup):
        return result.returncode

    release = {}
    for line in (root / 'etc/os-release').read_text().splitlines():
        if '=' in line:
            key, value = line.split('=', 1)
            values = shlex.split(value)
            release[key] = values[0] if len(values) == 1 else ''
    suite = release.get('VERSION_CODENAME')
    keyring = root / 'usr/share/keyrings/ubuntu-archive-keyring.gpg'
    if architecture is None:
        architecture = subprocess.check_output(['dpkg', '--print-architecture'], text=True).strip()
    if release.get('ID') != 'ubuntu' or suite not in ('noble', 'resolute') or architecture not in ('amd64', 'amd64v3') or not keyring.is_file():
        print('Signed APT fallback unavailable: unsupported Ubuntu suite/architecture or missing archive keyring', flush=True)
        return result.returncode

    if dns:
        print('APT DNS failure: retrying once against kernel.org with Ubuntu archive signatures required', flush=True)
        archives = [('https://mirrors.edge.kernel.org/ubuntu/',
                     f'{suite} {suite}-updates {suite}-security {suite}-backports')]
    else:
        if not missing_ubuntu_sources(run, common, suite):
            return result.returncode
        print(f'APT has no Ubuntu binary sources for {suite}: retrying once against signed Ubuntu archives', flush=True)
        archives = [('https://archive.ubuntu.com/ubuntu/', f'{suite} {suite}-updates {suite}-backports'),
                    ('https://security.ubuntu.com/ubuntu/', f'{suite}-security')]

    with tempfile.TemporaryDirectory(prefix='jarvis-ci-apt-') as directory:
        # The unprivileged _apt downloader needs read/traverse permission.
        os.chmod(directory, 0o755)
        source = Path(directory) / 'ubuntu.sources'
        source.write_text('\n'.join(
            f'Types: deb\nURIs: {uri}\nSuites: {suites}\n'
            f'Components: main restricted universe multiverse\n'
            f'Architectures: {architecture}\nSigned-By: {keyring}\n'
            for uri, suites in archives))
        source.chmod(0o644)
        return execute(['-o', f'Dir::Etc::sourcelist={source}',
                        '-o', 'Dir::Etc::sourceparts=-'])[0].returncode


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('packages', nargs='*')
    parser.add_argument('--playwright-deps', choices=['chrome'])
    args = parser.parse_args()
    if args.playwright_deps:
        if args.packages:
            parser.error('choose explicit packages or a Playwright dependency plan')
        packages = playwright_dependencies(args.playwright_deps)
        raise SystemExit(install(packages) if packages else 0)
    raise SystemExit(install(args.packages))
