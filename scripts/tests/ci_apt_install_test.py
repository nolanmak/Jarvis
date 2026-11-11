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

    def exercise(self, results):
        calls = []
        sources = []
        def run(args, **kwargs):
            calls.append(args)
            for arg in args:
                if arg.startswith('Dir::Etc::sourcelist='):
                    sources.append(Path(arg.split('=', 1)[1]).read_text())
            code, output = results[len(calls)-1]
            return subprocess.CompletedProcess(args, code, output)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'etc').mkdir()
            (root / 'etc/os-release').write_text('ID=ubuntu\nVERSION_CODENAME=noble\n')
            key = root / 'usr/share/keyrings/ubuntu-archive-keyring.gpg'
            key.parent.mkdir(parents=True)
            key.write_bytes(b'test-keyring')
            code = self.module.install(['poppler-utils'], run=run, root=root, architecture='amd64')
        return code, calls, sources

    def test_primary_success_never_changes_sources(self):
        code, calls, sources = self.exercise([(0, ''), (0, '')])
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 2)
        self.assertFalse(sources)

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


if __name__ == '__main__':
    unittest.main()
