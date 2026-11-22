#!/usr/bin/env python3
"""Install CI packages, riding out transient APT mirror and network failures.

Transient fetch failures (connection, DNS, timeouts, 429/5xx) are retried with
bounded backoff. An update that fails only for such reasons is tolerated: the
install step still has to find and download every requested package. When the
primary sources keep failing, update and install are retried against signed
Ubuntu archive mirrors. Signature, hash, lock and 404 failures are never
retried, and the helper exits non-zero if any requested package cannot be
installed.

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
import time


TRUST_FAILURE = re.compile(
    r'NO_PUBKEY|EXPKEYSIG|BADSIG|not signed|signatures? (?:could|couldn|were|could not)'
    r'|Hash Sum mismatch|Certificate verification failed', re.I)
# Network and server-side reasons APT appends to "E: Failed to fetch <uri>".
# 404, signature and hash failures are deliberately absent: they are not
# outages a retry or another signed mirror should paper over.
TRANSIENT_REASON = re.compile(
    r'Connection failed|Could not connect|Cannot initiate the connection|connection timed out'
    r'|Temporary failure resolving|Could not resolve|Unable to connect|Failed to connect'
    r'|Operation too slow|Network is unreachable|No route to host|Connection reset'
    r'|Undetermined Error|\s(?:429|50[0-4])\s', re.I)
AGGREGATE_ERRORS = ('E: Some index files failed to download', 'E: Unable to fetch some archives')

ATTEMPTS_MAX = 5
DEFAULT_BACKOFF = (10, 30)
BACKOFF_MAX = 120
# Wall-clock budget for retries and fallbacks. A dead host costs APT ~2.5
# minutes per update, and the slowest consumer job has a 20-minute timeout.
TIME_BUDGET = 480
FALLBACK_MIRRORS = ('http://azure.archive.ubuntu.com/ubuntu/',
                    'https://mirrors.edge.kernel.org/ubuntu/')


def transient_failure(output):
    """True when every APT error is a network/server fetch failure."""
    if TRUST_FAILURE.search(output):
        return False
    for line in output.splitlines():
        if line.startswith('E: Failed to fetch '):
            if not TRANSIENT_REASON.search(line):
                return False
        elif line.startswith('E:') and not line.startswith(AGGREGATE_ERRORS):
            return False
    return bool(TRANSIENT_REASON.search(output))


def backoff_from_env(value):
    """Seconds to wait before each retry; attempts = len(backoff) + 1."""
    if value is None:
        return DEFAULT_BACKOFF
    try:
        delays = tuple(int(part) for part in value.split(','))
    except ValueError:
        return DEFAULT_BACKOFF
    if any(delay < 0 for delay in delays):
        return DEFAULT_BACKOFF
    return tuple(min(delay, BACKOFF_MAX) for delay in delays[:ATTEMPTS_MAX - 1])


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


def install(packages, *, run=subprocess.run, root=Path('/'), architecture=None, installed=None,
            sleep=time.sleep, backoff=None, clock=time.monotonic):
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
    if backoff is None:
        backoff = backoff_from_env(os.environ.get('CI_APT_BACKOFF_SECONDS'))
    started = clock()
    # Hosted runners have IPv4 egress; avoid waiting on an unavailable IPv6 route.
    common = ['sudo', 'apt-get', '-o', 'Acquire::ForceIPv4=true',
              '-o', 'Acquire::Retries=2',
              '-o', 'Acquire::http::Timeout=20', '-o', 'Acquire::https::Timeout=20',
              '-o', 'APT::Update::Error-Mode=any',
              '-o', 'Acquire::AllowInsecureRepositories=false',
              '-o', 'Acquire::AllowDowngradeToInsecureRepositories=false',
              '-o', 'APT::Get::AllowUnauthenticated=false']

    def apt(options, *action):
        result = run([*common, *options, *action], stdout=subprocess.PIPE,
                     stderr=subprocess.STDOUT, text=True, timeout=600)
        print(result.stdout, end='', flush=True)
        return result

    def within_budget(delay=0):
        if clock() - started + delay > TIME_BUDGET:
            print(f'APT recovery budget of {TIME_BUDGET}s exhausted; not retrying', flush=True)
            return False
        return True

    def attempt(options, delays):
        """Update then install, retrying transient failures.

        Returns (returncode, reason): reason is 'ok', 'fatal' (never retried
        or recovered), 'lookup' (packages absent after a clean update) or
        'transient' (network failures persisted through every attempt).
        """
        for number in range(len(delays) + 1):
            if number:
                if not within_budget(delays[number - 1]):
                    break
                print(f'APT attempt {number + 1}/{len(delays) + 1} in {delays[number - 1]}s', flush=True)
                sleep(delays[number - 1])
            update = apt(options, 'update')
            degraded = bool(update.returncode)
            if degraded:
                if not transient_failure(update.stdout):
                    return update.returncode, 'fatal'
                # Some source (often security.ubuntu.com or Microsoft) was
                # unreachable. Lists from the healthy sources were refreshed,
                # so let install decide: it fails if a required package is
                # unavailable.
                print('APT update hit transient fetch failures; installing from the sources that responded', flush=True)
            result = apt(options, 'install', '-y', '--no-install-recommends', *packages)
            if not result.returncode:
                return 0, 'ok'
            if transient_failure(result.stdout):
                continue
            if package_lookup_failure(result.stdout, packages):
                if degraded:
                    continue
                return result.returncode, 'lookup'
            return result.returncode, 'fatal'
        return result.returncode, 'transient'

    code, reason = attempt([], backoff)
    if reason in ('ok', 'fatal'):
        return code
    if reason == 'lookup' and not missing_ubuntu_sources(run, common, suite_of(root)):
        return code

    release = os_release(root)
    suite = release.get('VERSION_CODENAME')
    keyring = root / 'usr/share/keyrings/ubuntu-archive-keyring.gpg'
    if architecture is None:
        architecture = subprocess.check_output(['dpkg', '--print-architecture'], text=True).strip()
    if release.get('ID') != 'ubuntu' or suite not in ('noble', 'resolute') or architecture not in ('amd64', 'amd64v3') or not keyring.is_file():
        print('Signed APT fallback unavailable: unsupported Ubuntu suite/architecture or missing archive keyring', flush=True)
        return code

    for mirror in FALLBACK_MIRRORS:
        if not within_budget():
            return code
        print(f'Primary APT sources failed: retrying against {mirror} with Ubuntu archive signatures required', flush=True)
        with tempfile.TemporaryDirectory(prefix='jarvis-ci-apt-') as directory:
            # The unprivileged _apt downloader needs read/traverse permission.
            os.chmod(directory, 0o755)
            source = Path(directory) / 'ubuntu.sources'
            source.write_text(
                f'Types: deb\nURIs: {mirror}\n'
                f'Suites: {suite} {suite}-updates {suite}-security {suite}-backports\n'
                f'Components: main restricted universe multiverse\n'
                f'Architectures: {architecture}\nSigned-By: {keyring}\n')
            source.chmod(0o644)
            code, reason = attempt(['-o', f'Dir::Etc::sourcelist={source}',
                                    '-o', 'Dir::Etc::sourceparts=-'], ())
        if reason != 'transient':
            # Success, or a healthy signed mirror that refused or lacks the
            # packages: another mirror would not change the answer.
            return code
    return code


def os_release(root):
    try:
        text = (root / 'etc/os-release').read_text()
    except OSError:
        return {}
    release = {}
    for line in text.splitlines():
        if '=' in line:
            key, value = line.split('=', 1)
            values = shlex.split(value)
            release[key] = values[0] if len(values) == 1 else ''
    return release


def suite_of(root):
    return os_release(root).get('VERSION_CODENAME')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('packages', nargs='*')
    parser.add_argument('--playwright-deps', choices=['chrome'])
    parser.add_argument('--root', type=Path, default=Path('/'), help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.playwright_deps:
        if args.packages:
            parser.error('choose explicit packages or a Playwright dependency plan')
        packages = playwright_dependencies(args.playwright_deps)
        raise SystemExit(install(packages, root=args.root) if packages else 0)
    raise SystemExit(install(args.packages, root=args.root))
