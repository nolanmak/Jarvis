import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest


class AptInstallTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        path = Path(__file__).resolve().parents[1] / 'ci-apt-install.py'
        spec = importlib.util.spec_from_file_location('ci_apt_install', path)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def exercise(self, results, packages=None, installed=None, suite="noble",
                 os_id="ubuntu", architecture="amd64", keyring=True):
        calls = []
        sources = []
        source_paths = []
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
            (root / 'etc/os-release').write_text(f'ID={os_id}\nVERSION_CODENAME={suite}\n')
            key = root / 'usr/share/keyrings/ubuntu-archive-keyring.gpg'
            key.parent.mkdir(parents=True)
            if keyring:
                key.write_bytes(b'test-keyring')
            parts = root / 'etc/apt/sources.list.d'
            parts.mkdir(parents=True)
            (parts / 'microsoft.list').write_text('deb https://packages.microsoft.com/repos/code stable main\n')
            original = {p.relative_to(root): p.read_bytes() for p in root.rglob('*') if p.is_file()}
            code = self.module.install(packages or ['poppler-utils'], run=run, root=root, architecture=architecture, installed=installed or set())
            self.assertEqual(original, {p.relative_to(root): p.read_bytes() for p in root.rglob('*') if p.is_file()})
            self.assertTrue(all(not path.exists() for path in source_paths), 'temporary APT source survived installation')
        return code, calls, sources

    def test_unrelated_microsoft_429_recovers_with_signed_ubuntu_sources(self):
        output = ('E: Failed to fetch https://packages.microsoft.com/ubuntu/26.04/prod/dists/resolute/InRelease '
                  '429 Too Many Requests [IP: 13.107.246.40 443]\n'
                  'E: Some index files failed to download. They have been ignored, or old ones used instead.\n')
        code, calls, sources = self.exercise([(100, output), (0, ''), (0, '')],
                                              packages=['poppler-utils', 'ripgrep'], suite='resolute')
        self.assertEqual(code, 0, 'an unrelated Microsoft feed blocked required Ubuntu packages')
        self.assertEqual(len(calls), 3)
        self.assertIn('install', calls[-1])
        self.assertIn('poppler-utils', calls[-1])
        self.assertIn('ripgrep', calls[-1])
        self.assertIn('https://archive.ubuntu.com/ubuntu/', sources[0])
        self.assertIn('https://security.ubuntu.com/ubuntu/', sources[0])
        self.assertIn('Signed-By:', sources[0])
        self.assertNotIn('packages.microsoft.com', sources[0])
        self.assertEqual(sources[0], sources[1])
        for call in calls:
            self.assertIn('Acquire::Retries=2', call)
            self.assertIn('APT::Get::AllowUnauthenticated=false', call)

    def test_microsoft_429_fallback_failure_is_terminal(self):
        microsoft = 'E: Failed to fetch https://packages.microsoft.com/ubuntu/26.04/prod/dists/resolute/InRelease 429 Too Many Requests\n'
        for results, count in [([(100, microsoft), (100, 'E: The repository is not signed.')], 2),
                               ([(100, microsoft), (0, ''), (100, 'E: Unable to locate package poppler-utils')], 3)]:
            with self.subTest(results=results):
                code, calls, _ = self.exercise(results, suite='resolute')
                self.assertEqual((code, len(calls)), (100, count))

    def test_microsoft_429_mixed_failures_never_hide_required_source_or_security_errors(self):
        microsoft = 'E: Failed to fetch https://packages.microsoft.com/ubuntu/26.04/prod/dists/resolute/InRelease 429 Too Many Requests\n'
        for other in ['E: Failed to fetch https://archive.ubuntu.com/ubuntu/dists/resolute/InRelease 429 Too Many Requests',
                      'E: The repository is not signed.', 'W: GPG error NO_PUBKEY',
                      'E: Could not get lock /var/lib/dpkg/lock',
                      'E: Failed to fetch https://packages.microsoft.com.example.invalid/InRelease 429 Too Many Requests']:
            with self.subTest(other=other):
                code, calls, sources = self.exercise([(100, microsoft + other)])
                self.assertEqual((code, len(calls), sources), (100, 1, []))

    def test_required_package_download_429_is_not_an_unrelated_source_failure(self):
        code, calls, sources = self.exercise([(0, ''), (100,
            'E: Failed to fetch https://packages.microsoft.com/pkg.deb 429 Too Many Requests')])
        self.assertEqual((code, len(calls), sources), (100, 2, []))

    def test_primary_success_never_changes_sources(self):
        code, calls, sources = self.exercise([(0, ''), (0, '')])
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 2)
        self.assertFalse(sources)

    def test_installed_packages_are_preserved_without_update_or_downgrade(self):
        code, calls, _ = self.exercise([], installed={'poppler-utils'})
        self.assertEqual((code, calls), (0, []))
        code, calls, _ = self.exercise([(0, ''), (0, '')],
                                      packages=['poppler-utils', 'python3-yaml'], installed={'python3-yaml'})
        self.assertEqual(code, 0)
        self.assertIn('poppler-utils', calls[-1])
        self.assertNotIn('python3-yaml', calls[-1])

    def test_dns_failure_uses_one_signed_isolated_fallback(self):
        code, calls, sources = self.exercise([
            (100, "E: Failed to fetch https://archive.ubuntu.com Temporary failure resolving 'archive.ubuntu.com'"),
            (0, ''), (0, '')])
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 3)
        self.assertIn('https://mirrors.edge.kernel.org/ubuntu/', sources[0])
        self.assertIn('Signed-By:', sources[0])
        self.assertIn('noble noble-updates noble-security noble-backports', sources[0])
        self.assertIn('Architectures: amd64', sources[0])
        self.assertNotIn('Trusted:', sources[0])
        self.assertEqual(sources[0], sources[1])
        for call in calls:
            self.assertIn('APT::Get::AllowUnauthenticated=false', call)

    def test_dns_failure_with_unrouted_ipv6_recovers_without_weakening_mixed_error_gate(self):
        dns = "E: Failed to fetch https://archive.ubuntu.com/dists/resolute/InRelease Temporary failure resolving 'archive.ubuntu.com'\n"
        ipv6 = "E: Failed to fetch https://security.ubuntu.com/dists/resolute-security/InRelease Cannot initiate the connection to security.ubuntu.com:443 (2620:2d:4000:1::102). - connect (101: Network is unreachable)\n"
        # Real hosted APT output mixed DNS and an unavailable IPv6 route. Keep
        # refusing that mixed output; request the working IP family instead.
        self.assertFalse(self.module.dns_failure(dns + ipv6))
        def primary(args):
            return 100, dns if 'Acquire::ForceIPv4=true' in args else dns + ipv6
        code, calls, sources = self.exercise([primary, (0, ''), (0, '')])
        self.assertEqual(code, 0, 'unrouted IPv6 prevented the bounded signed DNS fallback')
        self.assertEqual(len(calls), 3)
        self.assertIn('Signed-By:', sources[0])

    def test_install_dns_failure_also_recovers(self):
        code, calls, _ = self.exercise([(0, ''), (100, "Temporary failure resolving 'security.ubuntu.com'"), (0, ''), (0, '')])
        self.assertEqual((code, len(calls)), (0, 4))

    def test_non_dns_and_mixed_security_errors_do_not_fallback(self):
        for output in ['E: Unable to locate package bogus', "Temporary failure resolving 'archive.ubuntu.com'\nE: The repository is not signed.", "Temporary failure resolving 'archive.ubuntu.com'\nE: Could not get lock /var/lib/dpkg/lock", "Temporary failure resolving 'archive.ubuntu.com'\nE: Failed to fetch https://example.invalid/package 404 Not Found"]:
            with self.subTest(output=output):
                code, calls, sources = self.exercise([(100, output)])
                self.assertEqual((code, len(calls), sources), (100, 1, []))

    def test_fallback_failure_is_terminal(self):
        code, calls, _ = self.exercise([(100, "Temporary failure resolving 'archive.ubuntu.com'"), (100, 'E: bad mirror signature')])
        self.assertEqual((code, len(calls)), (100, 2))

    def test_microsoft_only_update_recovers_missing_ubuntu_archives(self):
        for suite in ('noble', 'resolute'):
            with self.subTest(suite=suite):
                code, calls, sources = self.exercise([
                    (0, f'Hit:1 https://packages.microsoft.com/ubuntu/26.04/prod {suite} InRelease\n'),
                    (100, 'E: Unable to locate package poppler-utils\nE: Unable to locate package ripgrep\n'),
                    (0, f'microsoft-ubuntu-{suite}-prod\t{suite}\n'),
                    (0, ''), (0, '')], packages=['poppler-utils', 'ripgrep'], suite=suite)
                self.assertEqual(code, 0, 'missing Ubuntu archives were not recovered after a successful Microsoft-only update')
                self.assertEqual(len(calls), 5)
                self.assertIn('indextargets', calls[2])
                self.assertIn('https://archive.ubuntu.com/ubuntu/', sources[0])
                self.assertIn('https://security.ubuntu.com/ubuntu/', sources[0])
                self.assertIn(f'Suites: {suite} {suite}-updates {suite}-backports', sources[0])
                self.assertIn(f'Suites: {suite}-security', sources[0])
                self.assertIn('Signed-By:', sources[0])
                self.assertEqual(sources[0], sources[1])
                self.assertNotIn('Trusted:', sources[0])
                for call in calls:
                    self.assertIn('APT::Get::AllowUnauthenticated=false', call)
                    self.assertIn('Acquire::AllowInsecureRepositories=false', call)

    def test_no_binary_index_targets_also_recovers_missing_ubuntu_archives(self):
        code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package poppler-utils'),
                                            (0, ''), (0, ''), (0, '')], suite='resolute')
        self.assertEqual(code, 0, 'an empty binary-source inventory lost the signed Ubuntu fallback')
        self.assertEqual(len(calls), 5)
        self.assertTrue(sources)

    def test_existing_ubuntu_binary_source_keeps_unknown_packages_terminal(self):
        code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package poppler-utils'),
                                            (0, 'Microsoft\tnoble\nUbuntu\tnoble\n')])
        self.assertEqual((code, len(calls), sources), (100, 3, []))
        self.assertIn('Identifier: Packages', calls[2])
        self.assertIn('$(ORIGIN)\t$(CODENAME)', calls[2])

    def test_old_ubuntu_release_does_not_supply_running_release(self):
        code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package poppler-utils'),
                                            (0, 'Ubuntu\tjammy\n'), (0, ''), (0, '')], suite='resolute')
        self.assertEqual((code, len(calls)), (0, 5))
        self.assertIn('resolute', sources[0])
        self.assertNotIn('jammy', sources[0])

    def test_unknown_or_failed_source_inventory_is_terminal(self):
        def timeout(args):
            raise subprocess.TimeoutExpired(args, 30)
        for response in [(100, 'E: inventory unavailable'), (0, 'unsupported output'),
                         (0, '$(ORIGIN)\t$(CODENAME)\n'), timeout]:
            with self.subTest(response=response):
                code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package poppler-utils'), response])
                self.assertEqual((code, len(calls), sources), (100, 3, []))

    def test_lookup_mixed_with_security_lock_or_dns_errors_never_retries(self):
        for other in ['W: GPG error NO_PUBKEY', 'E: Could not get lock /var/lib/dpkg/lock',
                      'W: Hash Sum mismatch', 'W: Certificate verification failed',
                      "W: Temporary failure resolving 'archive.ubuntu.com'",
                      'E: The repository is not signed.', 'E: Failed to fetch https://example.invalid/pkg 404 Not Found']:
            with self.subTest(other=other):
                code, calls, sources = self.exercise([(0, ''), (100, f'E: Unable to locate package poppler-utils\n{other}')])
                self.assertEqual((code, len(calls), sources), (100, 2, []))

    def test_missing_sources_retry_signature_failure_is_terminal(self):
        code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package poppler-utils'),
                                            (0, ''), (100, 'E: The repository is not signed.')])
        self.assertEqual((code, len(calls), len(sources)), (100, 4, 1))

    def test_missing_sources_retry_install_failure_is_terminal(self):
        code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package poppler-utils'),
                                            (0, ''), (0, ''), (100, 'E: Unable to locate package poppler-utils')])
        self.assertEqual((code, len(calls), len(sources)), (100, 5, 2))

    def test_missing_sources_requires_supported_ubuntu_and_system_keyring(self):
        for options in [dict(os_id='debian'), dict(suite='jammy'), dict(architecture='arm64'), dict(keyring=False)]:
            with self.subTest(options=options):
                code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package poppler-utils')], **options)
                self.assertEqual((code, len(calls), sources), (100, 2, []))

    def test_unrequested_package_error_does_not_retry(self):
        code, calls, sources = self.exercise([(0, ''), (100, 'E: Unable to locate package bogus')])
        self.assertEqual((code, len(calls), sources), (100, 2, []))

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


if __name__ == '__main__':
    unittest.main()
