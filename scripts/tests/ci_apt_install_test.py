import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HELPER = Path(__file__).resolve().parents[1] / 'ci-apt-install.py'

# Real hosted output from 2026-11-19 (runs 44524724677, 44524724530, ...):
# one unrelated pocket was unreachable and APT's strict update mode exited 100.
SECURITY_DOWN = ('Err:15 http://security.ubuntu.com/ubuntu noble-security InRelease\n'
                 '  Connection failed [IP: 91.189.92.23 80]\n'
                 'E: Failed to fetch http://security.ubuntu.com/ubuntu/dists/noble-security/InRelease  '
                 'Connection failed [IP: 91.189.92.23 80]\n'
                 'E: Some index files failed to download. They have been ignored, or old ones used instead.\n')
MICROSOFT_429 = ('E: Failed to fetch https://packages.microsoft.com/ubuntu/26.04/prod/dists/resolute/InRelease '
                 '429 Too Many Requests [IP: 13.107.246.40 443]\n'
                 'E: Some index files failed to download. They have been ignored, or old ones used instead.\n')
ARCHIVE_DOWN = ('E: Failed to fetch http://archive.ubuntu.com/ubuntu/dists/noble/InRelease  '
                'Could not connect to archive.ubuntu.com:80 (185.125.190.36), connection timed out\n'
                'E: Some index files failed to download. They have been ignored, or old ones used instead.\n')
DEB_DOWN = ('E: Failed to fetch http://archive.ubuntu.com/ubuntu/pool/main/p/poppler/poppler-utils_24.02.0_amd64.deb  '
            '503  Service Unavailable [IP: 185.125.190.36 80]\n'
            'E: Unable to fetch some archives, maybe run apt-get update or try with --fix-missing?\n')
LOOKUP = 'E: Unable to locate package poppler-utils\n'


class AptInstallTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('ci_apt_install', HELPER)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def exercise(self, results, packages=None, installed=None, suite="noble",
                 os_id="ubuntu", architecture="amd64", keyring=True, os_release=True,
                 clock=None):
        calls = []
        sources = []
        source_paths = []
        self.sleeps = []
        def run(args, **kwargs):
            calls.append(args)
            for arg in args:
                if arg.startswith('Dir::Etc::sourcelist='):
                    source = Path(arg.split('=', 1)[1])
                    source_paths.append(source)
                    sources.append(source.read_text())
                    self.assertEqual(source.stat().st_mode & 0o777, 0o644)
                    self.assertEqual(source.parent.stat().st_mode & 0o777, 0o755)
            response = results[len(calls)-1]
            code, output = response(args) if callable(response) else response
            return subprocess.CompletedProcess(args, code, output)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'etc').mkdir()
            if os_release:
                (root / 'etc/os-release').write_text(f'ID={os_id}\nVERSION_CODENAME={suite}\n')
            key = root / 'usr/share/keyrings/ubuntu-archive-keyring.gpg'
            key.parent.mkdir(parents=True)
            if keyring:
                key.write_bytes(b'test-keyring')
            parts = root / 'etc/apt/sources.list.d'
            parts.mkdir(parents=True)
            (parts / 'microsoft.list').write_text('deb https://packages.microsoft.com/repos/code stable main\n')
            original = {p.relative_to(root): p.read_bytes() for p in root.rglob('*') if p.is_file()}
            code = self.module.install(packages or ['poppler-utils'], run=run, root=root, architecture=architecture,
                                       installed=installed or set(), sleep=self.sleeps.append,
                                       backoff=(10, 30), clock=clock or (lambda: 0.0))
            self.assertEqual(original, {p.relative_to(root): p.read_bytes() for p in root.rglob('*') if p.is_file()})
            self.assertTrue(all(not path.exists() for path in source_paths), 'temporary APT source survived installation')
        self.assertEqual(len(calls), len(results), 'unexpected number of APT invocations')
        for call in calls:
            self.assertIn('APT::Get::AllowUnauthenticated=false', call)
            self.assertIn('Acquire::AllowInsecureRepositories=false', call)
        return code, calls, sources

    def assert_signed_mirror(self, source, mirror, suite='noble'):
        self.assertIn(f'URIs: {mirror}', source)
        self.assertIn(f'Suites: {suite} {suite}-updates {suite}-security {suite}-backports', source)
        self.assertIn('Signed-By:', source)
        self.assertIn('Architectures: amd64', source)
        self.assertNotIn('Trusted:', source)
        self.assertNotIn('packages.microsoft.com', source)
        self.assertNotIn('security.ubuntu.com', source)

    # -- (b) unrelated or secondary source failures are tolerated ------------

    def test_unreachable_security_pocket_does_not_block_installable_packages(self):
        code, calls, sources = self.exercise([(100, SECURITY_DOWN), (0, '')])
        self.assertEqual(code, 0, 'an unreachable security.ubuntu.com failed a required check')
        self.assertIn('update', calls[0])
        self.assertIn('install', calls[1])
        self.assertEqual((sources, self.sleeps), ([], []))

    def test_unrelated_microsoft_429_does_not_block_installable_packages(self):
        code, calls, sources = self.exercise([(100, MICROSOFT_429), (0, '')],
                                             packages=['poppler-utils', 'ripgrep'], suite='resolute')
        self.assertEqual(code, 0, 'an unrelated Microsoft feed blocked required Ubuntu packages')
        self.assertIn('poppler-utils', calls[-1])
        self.assertIn('ripgrep', calls[-1])
        self.assertEqual((sources, self.sleeps), ([], []))

    def test_dns_and_unrouted_ipv6_update_failures_are_tolerated(self):
        dns = "E: Failed to fetch https://archive.ubuntu.com/dists/resolute/InRelease Temporary failure resolving 'archive.ubuntu.com'\n"
        ipv6 = ("E: Failed to fetch https://security.ubuntu.com/dists/resolute-security/InRelease Cannot initiate the "
                "connection to security.ubuntu.com:443 (2620:2d:4000:1::102). - connect (101: Network is unreachable)\n")
        code, calls, _ = self.exercise([(100, dns + ipv6), (0, '')])
        self.assertEqual(code, 0)
        self.assertIn('Acquire::ForceIPv4=true', calls[0])

    # -- (a) bounded retries with backoff ------------------------------------

    def test_transient_package_download_failure_retries_with_backoff(self):
        code, calls, sources = self.exercise([(0, ''), (100, DEB_DOWN), (0, ''), (0, '')])
        self.assertEqual(code, 0)
        self.assertEqual(self.sleeps, [10])
        self.assertEqual(sources, [])

    def test_required_source_outage_retries_then_recovers_on_primary(self):
        code, calls, sources = self.exercise([(100, ARCHIVE_DOWN), (100, LOOKUP),
                                              (100, ARCHIVE_DOWN), (100, LOOKUP),
                                              (0, ''), (0, '')])
        self.assertEqual(code, 0)
        self.assertEqual(self.sleeps, [10, 30])
        self.assertEqual(sources, [])

    def test_retry_budget_stops_before_exceeding_wall_clock(self):
        times = iter([0.0] + [485.0] * 10)
        code, calls, sources = self.exercise([(0, ''), (100, DEB_DOWN)], clock=lambda: next(times))
        self.assertEqual(code, 100, 'retries ran past the APT time budget')
        self.assertEqual((self.sleeps, sources), ([], []))

    # -- (c) fallback to other signed archive mirrors -------------------------

    def test_persistent_primary_outage_falls_back_to_azure_mirror(self):
        code, calls, sources = self.exercise([(100, ARCHIVE_DOWN), (100, LOOKUP)] * 3 + [(0, ''), (0, '')])
        self.assertEqual(code, 0)
        self.assertEqual(self.sleeps, [10, 30])
        self.assertEqual(len(sources), 2)
        self.assert_signed_mirror(sources[0], 'http://azure.archive.ubuntu.com/ubuntu/')
        self.assertEqual(sources[0], sources[1])
        self.assertIn('Dir::Etc::sourceparts=-', calls[-1])

    def test_second_fallback_mirror_is_used_when_azure_also_fails(self):
        code, calls, sources = self.exercise([(0, ''), (100, DEB_DOWN)] * 3 +
                                             [(100, ARCHIVE_DOWN), (100, DEB_DOWN), (0, ''), (0, '')])
        self.assertEqual(code, 0)
        self.assert_signed_mirror(sources[0], 'http://azure.archive.ubuntu.com/ubuntu/')
        self.assert_signed_mirror(sources[-1], 'https://mirrors.edge.kernel.org/ubuntu/')

    def test_microsoft_only_update_recovers_missing_ubuntu_archives(self):
        for suite in ('noble', 'resolute'):
            with self.subTest(suite=suite):
                code, calls, sources = self.exercise([
                    (0, f'Hit:1 https://packages.microsoft.com/ubuntu/26.04/prod {suite} InRelease\n'),
                    (100, 'E: Unable to locate package poppler-utils\nE: Unable to locate package ripgrep\n'),
                    (0, f'microsoft-ubuntu-{suite}-prod\t{suite}\n'),
                    (0, ''), (0, '')], packages=['poppler-utils', 'ripgrep'], suite=suite)
                self.assertEqual(code, 0, 'missing Ubuntu archives were not recovered after a successful Microsoft-only update')
                self.assertIn('indextargets', calls[2])
                self.assert_signed_mirror(sources[0], 'http://azure.archive.ubuntu.com/ubuntu/', suite)
                self.assertEqual(sources[0], sources[1])

    def test_no_binary_index_targets_also_recovers_missing_ubuntu_archives(self):
        code, calls, sources = self.exercise([(0, ''), (100, LOOKUP), (0, ''), (0, ''), (0, '')], suite='resolute')
        self.assertEqual(code, 0, 'an empty binary-source inventory lost the signed Ubuntu fallback')
        self.assertTrue(sources)

    def test_old_ubuntu_release_does_not_supply_running_release(self):
        code, calls, sources = self.exercise([(0, ''), (100, LOOKUP), (0, 'Ubuntu\tjammy\n'), (0, ''), (0, '')],
                                             suite='resolute')
        self.assertEqual(code, 0)
        self.assertIn('resolute', sources[0])
        self.assertNotIn('jammy', sources[0])

    # -- (d) still fail when required packages cannot be installed ------------

    def test_every_source_failing_is_terminal_after_bounded_attempts(self):
        code, calls, sources = self.exercise([(100, ARCHIVE_DOWN), (100, LOOKUP)] * 5)
        self.assertEqual(code, 100, 'an unrecoverable APT outage reported success')
        self.assertEqual(self.sleeps, [10, 30])
        self.assertEqual(len(sources), 4)

    def test_package_missing_from_healthy_fallback_mirror_is_terminal(self):
        code, calls, sources = self.exercise([(100, ARCHIVE_DOWN), (100, LOOKUP)] * 3 + [(0, ''), (100, LOOKUP)])
        self.assertEqual(code, 100)
        self.assertEqual(len(sources), 2, 'a healthy mirror without the package still tried another mirror')

    def test_fallback_signature_failure_is_terminal(self):
        code, calls, sources = self.exercise([(0, ''), (100, LOOKUP), (0, ''), (100, 'E: The repository is not signed.')])
        self.assertEqual((code, len(sources)), (100, 1))

    def test_unknown_package_with_healthy_ubuntu_sources_fails_without_retry(self):
        code, calls, sources = self.exercise([(0, ''), (100, LOOKUP), (0, 'Microsoft\tnoble\nUbuntu\tnoble\n')])
        self.assertEqual((code, sources, self.sleeps), (100, [], []))
        self.assertIn('Identifier: Packages', calls[2])
        self.assertIn('$(ORIGIN)\t$(CODENAME)', calls[2])

    def test_unrequested_package_error_does_not_retry(self):
        code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package bogus')])
        self.assertEqual((code, sources, self.sleeps), (100, [], []))

    def test_unknown_or_failed_source_inventory_is_terminal(self):
        def timeout(args):
            raise subprocess.TimeoutExpired(args, 30)
        for response in [(100, 'E: inventory unavailable'), (0, 'unsupported output'),
                         (0, '$(ORIGIN)\t$(CODENAME)\n'), timeout]:
            with self.subTest(response=response):
                code, calls, sources = self.exercise([(0, ''), (100, LOOKUP), response])
                self.assertEqual((code, sources), (100, []))

    def test_trust_lock_and_not_found_errors_never_retry_or_fall_back(self):
        for other in ['E: The repository is not signed.', 'W: GPG error NO_PUBKEY 871920D1991BC93C',
                      'E: Could not get lock /var/lib/dpkg/lock', 'W: Hash Sum mismatch',
                      'W: Certificate verification failed: The certificate is NOT trusted.',
                      'E: Failed to fetch https://example.invalid/package 404 Not Found']:
            for phase in ('update', 'install'):
                with self.subTest(other=other, phase=phase):
                    mixed = SECURITY_DOWN + other
                    results = [(100, mixed)] if phase == 'update' else [(0, ''), (100, mixed)]
                    code, calls, sources = self.exercise(results)
                    self.assertEqual((code, sources, self.sleeps), (100, [], []))

    def test_lookup_mixed_with_other_errors_never_retries(self):
        for other in ['W: GPG error NO_PUBKEY', 'E: Could not get lock /var/lib/dpkg/lock',
                      "W: Temporary failure resolving 'archive.ubuntu.com'",
                      'E: Failed to fetch https://example.invalid/pkg 404 Not Found']:
            with self.subTest(other=other):
                code, calls, sources = self.exercise([(0, ''), (100, LOOKUP + other)])
                self.assertEqual((code, sources, self.sleeps), (100, [], []))

    def test_fallback_requires_supported_ubuntu_and_system_keyring(self):
        for options in [dict(os_id='debian'), dict(suite='jammy'), dict(architecture='arm64'),
                        dict(keyring=False), dict(os_release=False)]:
            with self.subTest(options=options):
                code, calls, sources = self.exercise([(100, ARCHIVE_DOWN), (100, LOOKUP)] * 3, **options)
                self.assertEqual((code, sources), (100, []))

    # -- unchanged behavior ----------------------------------------------------

    def test_primary_success_never_changes_sources(self):
        code, calls, sources = self.exercise([(0, ''), (0, '')])
        self.assertEqual((code, sources, self.sleeps), (0, [], []))
        self.assertIn('Acquire::Retries=2', calls[0])

    def test_installed_packages_are_preserved_without_update_or_downgrade(self):
        code, calls, _ = self.exercise([], installed={'poppler-utils'})
        self.assertEqual((code, calls), (0, []))
        code, calls, _ = self.exercise([(0, ''), (0, '')],
                                      packages=['poppler-utils', 'python3-yaml'], installed={'python3-yaml'})
        self.assertEqual(code, 0)
        self.assertIn('poppler-utils', calls[-1])
        self.assertNotIn('python3-yaml', calls[-1])

    def test_backoff_configuration_is_bounded(self):
        parse = self.module.backoff_from_env
        self.assertEqual(parse(None), (10, 30))
        self.assertEqual(parse('0,0'), (0, 0))
        self.assertEqual(parse('1,2,3,4,5,6,7'), (1, 2, 3, 4), 'more than five attempts')
        self.assertEqual(parse('999'), (120,))
        for bad in ['', 'x', '-1', '1,,2']:
            self.assertEqual(parse(bad), (10, 30), bad)

    def test_playwright_dependency_plan_is_data_not_a_shell_command(self):
        cases = [(0, 'All system dependencies are installed.\n', []),
                 (1, 'Missing system dependencies (2):\n  fonts-ipafont-gothic\n  libnss3\n', ['fonts-ipafont-gothic', 'libnss3'])]
        for code, output, expected in cases:
            calls = []
            def run(args, **kwargs):
                calls.append(args)
                return subprocess.CompletedProcess(args, code, output)
            self.assertEqual(self.module.playwright_dependencies('chrome', run=run), expected)
            self.assertEqual(calls, [['npx', '--no-install', 'playwright', 'install-deps', '--dry-run', 'chrome']])
        for output in ['Missing system dependencies (2):\n  libnss3\n',
                       'Missing system dependencies (1):\n  libnss3; touch /tmp/unsafe\n',
                       'E: package inventory unavailable\n',
                       'Missing system dependencies (2):\n  libnss3\n  libnss3\n']:
            def run(args, **kwargs):
                return subprocess.CompletedProcess(args, 1, output)
            with self.subTest(output=output), self.assertRaises(ValueError):
                self.module.playwright_dependencies('chrome', run=run)


FAKE_APT_GET = r'''#!/usr/bin/env bash
# Scripted apt-get: SCENARIO selects which sources are "down".
set -u
echo "$*" >> "$FAKE_APT_LOG"
case " $* " in *" update "*) action=update ;; *" install "*) action=install ;; *) action=other ;; esac
case " $* " in *Dir::Etc::sourcelist=*) source=fallback ;; *) source=primary ;; esac
n=$(grep -c " $action" "$FAKE_APT_LOG" || true)
security_down='E: Failed to fetch http://security.ubuntu.com/ubuntu/dists/noble-security/InRelease  Connection failed [IP: 91.189.91.82 80]
E: Some index files failed to download. They have been ignored, or old ones used instead.'
archive_down='E: Failed to fetch http://archive.ubuntu.com/ubuntu/dists/noble/InRelease  Connection failed [IP: 185.125.190.36 80]
E: Some index files failed to download. They have been ignored, or old ones used instead.'
case "$SCENARIO/$source/$action" in
  security-down/primary/update) echo "$security_down"; exit 100 ;;
  flaky-primary/primary/update) if [ "$n" -lt 3 ]; then echo "$archive_down"; exit 100; fi ;;
  flaky-primary/primary/install) if [ "$n" -lt 3 ]; then echo 'E: Unable to locate package poppler-utils'; exit 100; fi ;;
  primary-down/primary/update|all-down/*/update) echo "$archive_down"; exit 100 ;;
  primary-down/primary/install|all-down/*/install) echo 'E: Unable to locate package poppler-utils'; exit 100 ;;
esac
exit 0
'''


class FakeAptOnPathTests(unittest.TestCase):
    """Run the real CLI against a fake sudo/apt-get/dpkg on PATH."""

    def run_helper(self, scenario):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            bin_dir = work / 'bin'
            bin_dir.mkdir()
            stubs = {
                'apt-get': FAKE_APT_GET,
                'sudo': '#!/usr/bin/env bash\nexec "$@"\n',
                'dpkg-query': '#!/usr/bin/env bash\nexit 1\n',
                'dpkg': '#!/usr/bin/env bash\necho amd64\n',
            }
            for name, body in stubs.items():
                (bin_dir / name).write_text(body)
                (bin_dir / name).chmod(0o755)
            root = work / 'root'
            (root / 'etc').mkdir(parents=True)
            (root / 'etc/os-release').write_text('ID=ubuntu\nVERSION_CODENAME=noble\n')
            (root / 'usr/share/keyrings').mkdir(parents=True)
            (root / 'usr/share/keyrings/ubuntu-archive-keyring.gpg').write_bytes(b'test-keyring')
            log = work / 'apt.log'
            log.touch()
            env = dict(os.environ, PATH=f'{bin_dir}:{os.environ["PATH"]}', SCENARIO=scenario,
                       FAKE_APT_LOG=str(log), CI_APT_BACKOFF_SECONDS='0,0')
            done = subprocess.run([sys.executable, str(HELPER), '--root', str(root), 'poppler-utils'],
                                  env=env, capture_output=True, text=True, timeout=60)
            calls = log.read_text().splitlines()
        return done, calls

    def test_secondary_source_down_still_installs(self):
        done, calls = self.run_helper('security-down')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(len(calls), 2)

    def test_flaky_primary_retries_until_it_recovers(self):
        done, calls = self.run_helper('flaky-primary')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(len(calls), 6)
        self.assertFalse(any('sourcelist' in call for call in calls))

    def test_dead_primary_falls_back_to_signed_mirror(self):
        done, calls = self.run_helper('primary-down')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(len(calls), 8)
        self.assertTrue(all('sourcelist' in call for call in calls[6:]))
        self.assertIn('azure.archive.ubuntu.com', done.stdout)

    def test_uninstallable_packages_still_fail(self):
        done, calls = self.run_helper('all-down')
        self.assertEqual(done.returncode, 100, done.stdout + done.stderr)
        self.assertEqual(len(calls), 10, 'retries and fallbacks were not bounded')


if __name__ == '__main__':
    unittest.main()
