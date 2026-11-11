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

    def exercise(self, results, packages=None, installed=None):
        calls = []
        sources = []
        def run(args, **kwargs):
            calls.append(args)
            for arg in args:
                if arg.startswith('Dir::Etc::sourcelist='):
                    sources.append(Path(arg.split('=', 1)[1]).read_text())
            response = results[len(calls)-1]
            code, output = response(args) if callable(response) else response
            return subprocess.CompletedProcess(args, code, output)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'etc').mkdir()
            (root / 'etc/os-release').write_text('ID=ubuntu\nVERSION_CODENAME=noble\n')
            key = root / 'usr/share/keyrings/ubuntu-archive-keyring.gpg'
            key.parent.mkdir(parents=True)
            key.write_bytes(b'test-keyring')
            code = self.module.install(packages or ['poppler-utils'], run=run, root=root, architecture='amd64', installed=installed or set())
        return code, calls, sources

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
