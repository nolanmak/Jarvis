#!/usr/bin/env python3
"""Exercise the privacy gate against disposable Git repositories."""
import pathlib
import shutil
import subprocess
import tempfile
import unittest

SCANNER = pathlib.Path(__file__).resolve().parents[1] / "check-no-personal-data.sh"


class PrivacyGateTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        shutil.copyfile(SCANNER, self.root / "scan.sh")

    def write(self, name, body):
        p = self.root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body)
        return p

    def scan(self, *args):
        return subprocess.run(
            ["bash", "scan.sh", *args], cwd=self.root,
            capture_output=True, text=True,
        )

    def test_reserved_domains_and_subdomains(self):
        for domain in ["example.com", "mail.example.org", "EXAMPLE.NET", "a.test"]:
            with self.subTest(domain=domain):
                self.write("fixture.txt", "person" + "@" + domain)
                self.assertEqual(self.scan("fixture.txt").returncode, 0)

    def test_real_domains_and_suffix_lookalikes_are_blocked(self):
        for domain in ["gmail.com", "example.com.attacker.org", "localhost.attacker.org"]:
            with self.subTest(domain=domain):
                self.write("fixture.txt", "person" + "@" + domain)
                self.assertEqual(self.scan("fixture.txt").returncode, 1)

    def test_personal_mailbox_cannot_be_exempted_by_a_fixture_marker(self):
        for domain in ["gmail.com", "GMAIL.COM", "outlook.com", "proton.me"]:
            with self.subTest(domain=domain):
                self.write("fixture.txt", "person" + "@" + domain + " // pii-ok: synthetic")
                self.assertEqual(self.scan("fixture.txt").returncode, 1)

    def test_security_documentation_is_scanned(self):
        self.write("docs/SECURITY.md", "person" + "@" + "gmail.com")
        self.assertEqual(self.scan("docs/SECURITY.md").returncode, 1)

    def test_templates_are_scanned_and_values_are_not_printed(self):
        token = "github_pat_" + "A1b2" * 12
        self.write(".env.example", "GITHUB_TOKEN=" + token)
        result = self.scan(".env.example")
        self.assertEqual(result.returncode, 1)
        self.assertNotIn(token, result.stdout + result.stderr)

    def test_private_key_header_is_not_parsed_as_a_grep_option(self):
        self.write("fixture.txt", "-----BEGIN " + "PRIVATE KEY-----")
        self.assertEqual(self.scan("fixture.txt").returncode, 1)

    def test_current_provider_key_formats(self):
        for prefix in ["sk-proj-", "sk-ant-", "ghp_", "github_pat_"]:
            with self.subTest(prefix=prefix):
                self.write("fixture.txt", prefix + "Ab9_" * 12 if prefix != "ghp_" else prefix + "Ab9" * 16)
                self.assertEqual(self.scan("fixture.txt").returncode, 1)

    def test_secret_paths_are_blocked_even_when_empty(self):
        for name in [".env", ".env.export", ".env.example.backup", "data.db",
                     "linkedin-auth.json", "twitter-session.json",
                     "sidecar/chrome-profile/Preferences"]:
            with self.subTest(name=name):
                self.write(name, "")
                self.assertEqual(self.scan(name).returncode, 1)

    def test_staged_blob_cannot_be_hidden_by_unstaged_cleanup(self):
        p = self.write("fixture.txt", "person" + "@" + "gmail.com")
        subprocess.run(["git", "add", "fixture.txt"], cwd=self.root, check=True)
        p.write_text("person" + "@" + "example.com")
        self.assertEqual(self.scan().returncode, 1)

    def test_renamed_sensitive_file_is_scanned(self):
        self.write("before.txt", "person" + "@" + "gmail.com")
        subprocess.run(["git", "add", "before.txt"], cwd=self.root, check=True)
        subprocess.run(["git", "-c", "user.name=Fixture", "-c",
                        "user.email=fixture@example.com", "commit", "-qm", "fixture"],
                       cwd=self.root, check=True)
        subprocess.run(["git", "mv", "before.txt", "after.txt"], cwd=self.root, check=True)
        self.assertEqual(self.scan().returncode, 1)


if __name__ == "__main__":
    unittest.main()
