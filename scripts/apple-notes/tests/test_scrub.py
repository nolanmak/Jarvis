"""Secret scrubber tests (#1056). Every fixture secret here is synthetic."""
import base64
import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scrub import scrub, Finding  # noqa: E402


def kinds(findings):
    return sorted({f.kind for f in findings})


class KindTests(unittest.TestCase):
    """One test per kind: replaced in place, surroundings preserved, line reported."""

    def check(self, text, kind, expect_line=1):
        out, findings = scrub(text)
        self.assertIn(f"[REDACTED:{kind}]", out)
        self.assertEqual(kinds(findings), [kind])
        self.assertEqual(findings[0].line, expect_line)
        return out

    def test_aws_access_key(self):
        out = self.check("key AKIAIOSFODNN7EXAMPLE end", "aws-access-key")
        self.assertEqual(out, "key [REDACTED:aws-access-key] end")

    def test_aws_secret_same_line(self):
        out = self.check("aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "aws-secret-key")
        self.assertTrue(out.startswith("aws_secret_access_key = "))
        self.assertNotIn("wJalrXUtnFEMI", out)

    def test_aws_secret_next_line(self):
        text = "AWS secret:\nwJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        out, findings = scrub(text)
        self.assertEqual(out, "AWS secret:\n[REDACTED:aws-secret-key]")
        self.assertEqual(findings[0].line, 2)

    def test_github_tokens(self):
        for tok in ("ghp_" + "a" * 36, "gho_" + "B" * 40, "github_pat_" + "x" * 22 + "_" + "y" * 10):
            with self.subTest(tok=tok):
                self.check(f"token {tok}", "github-token")

    def test_slack_token(self):
        self.check("xoxb-1234567890-abcdefghij", "slack-token")

    def test_openai_key(self):
        self.check("sk-" + "A" * 24, "openai-key")

    def test_anthropic_key_wins_over_openai(self):
        self.check("sk-ant-api03-" + "A" * 30, "anthropic-key")

    def test_stripe_key(self):
        self.check("sk_live_" + "Z" * 24, "stripe-key")
        self.check("rk_test_" + "Z" * 24, "stripe-key")

    def test_google_api_key(self):
        self.check("AIza" + "A" * 35, "google-api-key")

    def test_jwt(self):
        header = base64.urlsafe_b64encode(b'{"alg":"HS256","typ":"JWT"}').decode().rstrip("=")
        payload = base64.urlsafe_b64encode(b'{"sub":"1"}').decode().rstrip("=")
        self.check(f"Bearer {header}.{payload}.sigsigsigsig", "jwt")

    def test_password_assignment_redacts_value_only(self):
        out = self.check("password: hunter22", "password-assignment")
        self.assertEqual(out, "password: [REDACTED:password-assignment]")
        for form in ("passwd=abcdef1", "PWD = abcdef1", "api_key: abcdef1", "api-key=abcdef1",
                     "secret = abcdef1", "token: abcdef1", "pin = 1234", "PIN: 123456", "passcode = 9999"):
            with self.subTest(form=form):
                self.check(form, "password-assignment")


class PrivateKeyTests(unittest.TestCase):
    def test_multiline_openssh_block_collapses_to_one_line(self):
        body = "\n".join("b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW" for _ in range(20))
        text = f"my key\n-----BEGIN OPENSSH PRIVATE KEY-----\n{body}\n-----END OPENSSH PRIVATE KEY-----\nafter"
        out, findings = scrub(text)
        self.assertEqual(out, "my key\n[REDACTED:private-key]\nafter")
        self.assertEqual([(f.kind, f.line) for f in findings], [("private-key", 2)])

    def test_other_pem_kinds(self):
        for kind in ("RSA", "EC", "DSA", "PGP", ""):
            label = f"{kind} PRIVATE KEY".strip()
            text = f"-----BEGIN {label}-----\nabc\n-----END {label}-----"
            with self.subTest(kind=kind):
                out, _ = scrub(text)
                self.assertEqual(out, "[REDACTED:private-key]")

    def test_unterminated_block_redacts_to_eof(self):
        out, findings = scrub("x\n-----BEGIN RSA PRIVATE KEY-----\nline1\nline2")
        self.assertEqual(out, "x\n[REDACTED:private-key]")
        self.assertEqual(findings[0].line, 2)


class InlinePemTests(unittest.TestCase):
    """Seen in the wild: keys pasted as env values with literal \\n escapes."""

    def test_inline_pem_in_quoted_env_value(self):
        line = 'FIREBASE_KEY="-----BEGIN PRIVATE KEY-----\\nMIIEvQIBADANBg\\n-----END PRIVATE KEY-----\\n"\nNEXT=1'
        out, findings = scrub(line)
        self.assertEqual(out, 'FIREBASE_KEY="[REDACTED:private-key]"\nNEXT=1')
        self.assertEqual([(f.kind, f.line) for f in findings], [("private-key", 1)])

    def test_pem_starting_mid_line_and_ending_later(self):
        text = 'key: -----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY----- trailing\nafter'
        out, findings = scrub(text)
        self.assertEqual(out, "key: [REDACTED:private-key] trailing\nafter")
        self.assertEqual(findings[0].line, 1)


class CreditCardTests(unittest.TestCase):
    def test_luhn_valid_numbers_redacted_in_common_layouts(self):
        for form in ("4111111111111111", "4111 1111 1111 1111", "4111-1111-1111-1111", "5500 0000 0000 0004", "3400 000000 00009"):
            with self.subTest(form=form):
                out, findings = scrub(f"card {form} exp 12/29")
                self.assertEqual(out, "card [REDACTED:credit-card] exp 12/29")
                self.assertEqual(kinds(findings), ["credit-card"])

    def test_luhn_invalid_or_wrong_length_left_alone(self):
        for form in ("4111111111111112", "1234 5678 9012 3456", "123456789012", "12345678901234567890"):
            with self.subTest(form=form):
                out, findings = scrub(form)
                self.assertEqual(out, form)
                self.assertEqual(findings, [])

    def test_uuid_with_digit_only_segments_is_not_a_card(self):
        # Seen on real data: --check flagged `identifier:` frontmatter lines.
        for uuid in ("62055243-3164-4990-9107-064185390612", "A1B2C3D4-1234-5678-9012-345678901234",
                     "12345678-1234-5678-9012-34567890abcd"):
            with self.subTest(uuid=uuid):
                line = f'identifier: "{uuid}"'
                self.assertEqual(scrub(line), (line, []))

    def test_bare_luhn_valid_id_needs_a_card_keyword(self):
        snowflake = "1234567890123452"  # Luhn-valid by construction, no separators
        self.assertEqual(scrub(f"message id {snowflake}"), (f"message id {snowflake}", []))
        out, findings = scrub(f"visa {snowflake}")
        self.assertEqual(out, "visa [REDACTED:credit-card]")
        self.assertEqual(kinds(findings), ["credit-card"])

    def test_phone_and_timestamps_untouched(self):
        for form in ("+1 215 555 0100", "1782475200", "2026-09-16 18:22:12"):
            with self.subTest(form=form):
                self.assertEqual(scrub(form), (form, []))


class IdempotenceTests(unittest.TestCase):
    """A written bundle re-scanned by --check must be clean: markers are not secrets."""

    SAMPLES = [
        "password: hunter22",
        "api_key: sk-" + "A" * 24,
        "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
        "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "card 4111 1111 1111 1111",
        "pin = 123456",
    ]

    def test_scrubbing_twice_finds_nothing_new(self):
        for sample in self.SAMPLES:
            with self.subTest(sample=sample):
                once, findings = scrub(sample)
                self.assertTrue(findings)
                twice, again = scrub(once)
                self.assertEqual(twice, once)
                self.assertEqual(again, [])


class FalsePositiveTests(unittest.TestCase):
    CLEAN = [
        "7BED7E65-9973-4633-908C-CE62B5B03BF3",
        "https://example.com/a/very/long/path/segment/that/keeps/going/and/going/0123456789abcdef",
        "2026-09-16T18:22:12-04:00",
        "![alt text](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAUA)",
        "commit 3ecaaf9d2c6d03a2e52c8f375b8964fbb3860c1419c37a4a7fe00b0a824e61333",
        "The password reset flow works.",
        "token bucket algorithm",
        "sk-ipping this",
    ]

    def test_no_findings_on_clean_text(self):
        for line in self.CLEAN:
            with self.subTest(line=line):
                out, findings = scrub(line)
                self.assertEqual(out, line)
                self.assertEqual(findings, [])


class DocumentedLimitationTests(unittest.TestCase):
    """Honest misses: no keyword, no known prefix ⇒ not caught. Quarantine handles these."""

    def test_bare_high_entropy_string_is_missed(self):
        text = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        out, findings = scrub(text)
        self.assertEqual(out, text)
        self.assertEqual(findings, [])

    def test_prose_password_is_missed(self):
        text = "my wifi password is hunter2"
        out, findings = scrub(text)
        self.assertEqual(out, text)
        self.assertEqual(findings, [])


class MultiFindingTests(unittest.TestCase):
    def test_findings_are_per_line_and_ordered(self):
        text = "a\nAKIAIOSFODNN7EXAMPLE\nb\nxoxb-1234567890-abcdefghij\n"
        out, findings = scrub(text)
        self.assertEqual([(f.kind, f.line) for f in findings],
                         [("aws-access-key", 2), ("slack-token", 4)])
        self.assertEqual(out, "a\n[REDACTED:aws-access-key]\nb\n[REDACTED:slack-token]\n")

    def test_finding_is_a_named_tuple(self):
        f = Finding("jwt", 3)
        self.assertEqual((f.kind, f.line), ("jwt", 3))
        self.assertEqual(json.loads(json.dumps(f)), ["jwt", 3])


if __name__ == "__main__":
    unittest.main()


class CheckCliTests(unittest.TestCase):
    def setUp(self):
        import tempfile
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "notes" / "Notes").mkdir(parents=True)

    def run_check(self):
        import io, contextlib
        from scrub import main
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = main(["--check", str(self.root)])
        return code, buf.getvalue()

    def test_clean_bundle_exits_zero(self):
        (self.root / "notes" / "Notes" / "a.md").write_text("---\ntitle: a\n---\nhello\n")
        code, out = self.run_check()
        self.assertEqual((code, out), (0, ""))

    def test_planted_secret_exits_one_and_names_path_line_kind(self):
        (self.root / "notes" / "Notes" / "a.md").write_text("---\ntitle: a\n---\nhello\nAKIAIOSFODNN7EXAMPLE\n")
        (self.root / "notes" / "Notes" / "b.md").write_text("fine\n")
        code, out = self.run_check()
        self.assertEqual(code, 1)
        self.assertEqual(out, "notes/Notes/a.md:5 aws-access-key\n")

    def test_missing_notes_dir_is_clean(self):
        import shutil
        shutil.rmtree(self.root / "notes")
        self.assertEqual(self.run_check()[0], 0)
