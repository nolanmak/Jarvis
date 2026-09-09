#!/usr/bin/env python3
"""Check outbound issue privacy without making GitHub requests."""
import json
import os
import pathlib
import subprocess
import tempfile
import unittest

SCRIPTS = pathlib.Path(__file__).resolve().parents[1]


class IssuePrivacyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.result = self.root / 'sent.json'
        fake = self.root / 'gh'
        fake.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
body = pathlib.Path(args[args.index('--body-file') + 1]).read_text() if '--body-file' in args else None
pathlib.Path(os.environ['TEST_SENT']).write_text(json.dumps({'args': args, 'body': body}))
''')
        fake.chmod(0o700)
        self.env = dict(os.environ, AA_GH_REAL=str(fake), TEST_SENT=str(self.result))

    def invoke(self, *args, stdin=None):
        self.result.unlink(missing_ok=True)
        return subprocess.run(['bash', str(SCRIPTS / 'aa-gh'), 'issue', *args],
                              cwd=self.root, env=self.env, input=stdin,
                              text=True, capture_output=True)

    def test_reads_pass_through(self):
        for verb in ['list', 'view']:
            self.assertEqual(self.invoke(verb, '--json', 'body').returncode, 0)
            self.assertEqual(json.loads(self.result.read_text())['args'], ['issue', verb, '--json', 'body'])

    def test_safe_inline_file_and_stdin_bodies_preserved(self):
        body = 'Synthetic person@example.com\nSecond line\n'
        file = self.root / 'body.txt'
        file.write_text(body)
        for args, stdin in [(['create', '--title', 'Example', '--body', body], None),
                            (['comment', '1', '--body-file', str(file)], None),
                            (['comment', '1', '--body-file=-'], body)]:
            with self.subTest(args=args):
                self.assertEqual(self.invoke(*args, stdin=stdin).returncode, 0)
                self.assertEqual(json.loads(self.result.read_text())['body'], body)

    def test_private_data_blocked_without_echoing_values(self):
        email = 'synthetic' + '@' + 'gmail.com'
        token = 'github_pat_' + 'X' * 40
        for args in [['create', '--title', email, '--body', 'safe'],
                     ['comment', '1', '--body', email],
                     ['comment', '1', '--body', token + ' pii-ok']]:
            result = self.invoke(*args)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(self.result.exists())
            self.assertNotIn(email, result.stderr)
            self.assertNotIn(token, result.stderr)

    def test_overrides_and_interactive_bodies_rejected(self):
        for extra in [[], ['--editor'], ['--body', 'safe', '-bhidden'],
                      ['--body', 'safe', '--body=other'],
                      ['--body', 'safe', '-tunchecked'],
                      ['--body', 'safe', '--title', 'a', '--title=b'],
                      ['--body', 'safe', '--template=test']]:
            with self.subTest(extra=extra):
                self.assertNotEqual(self.invoke('create', *extra).returncode, 0)
                self.assertFalse(self.result.exists())


if __name__ == '__main__':
    unittest.main()
