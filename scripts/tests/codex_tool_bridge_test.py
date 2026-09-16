"""Provider-independent tool policy contracts; synthetic data only."""
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    'bridge', Path(__file__).parents[1] / 'codex-tool-bridge.py')
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)


class ToolPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'workspace'
        self.root.mkdir()
        self.outside = Path(self.temp.name) / 'outside.txt'
        self.outside.write_text('SYNTHETIC_PRIVATE')
        self.policy = bridge.Policy({
            'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)],
            'allowed_tools': ['Read', 'Write', 'Edit', 'Glob', 'Grep',
                              'Bash(printf *)', 'Bash(git status*)'],
        })

    def test_read_write_edit_roundtrip(self):
        self.policy.write('note.md', 'alpha\nbeta\n')
        self.assertEqual(self.policy.read('note.md'), 'alpha\nbeta\n')
        self.policy.edit('note.md', 'beta', 'gamma')
        self.assertEqual(self.policy.read('note.md'), 'alpha\ngamma\n')

    def test_nested_write_search_and_glob_exclude_secret_paths(self):
        self.policy.write('notes/project.md', 'synthetic project needle')
        (self.root / '.env').write_text('needle SECRET')
        (self.root / 'escape.md').symlink_to(self.outside)
        self.assertEqual(self.policy.glob('**/*.md'), ['notes/project.md'])
        hits = self.policy.grep('needle')
        self.assertEqual(len(hits), 1)
        self.assertEqual(hits[0]['path'], 'notes/project.md')
        self.assertEqual(hits[0]['line'], 1)

    def test_original_guard_denial_and_crash_both_block_tool_execution(self):
        hook = Path(self.temp.name) / 'guard.py'
        for program in ["raise RuntimeError('synthetic crash')",
                        "print('{\"decision\":\"block\"}')",
                        "print('malformed JSON')"]:
            hook.write_text(program)
            policy = bridge.Policy({
                'cwd': str(self.root), 'read_roots': [str(self.root)],
                'write_roots': [str(self.root)], 'allowed_tools': ['Write'],
                'settings': {'hooks': {'PreToolUse': [{'matcher': 'Write', 'hooks': [
                    {'type': 'command', 'command': f'{sys.executable} {hook}'}]}]}}})
            with self.subTest(program=program), self.assertRaises(bridge.Denied):
                bridge.Server(policy).call('Write', {'file_path': 'blocked.txt', 'content': 'bad'})
            self.assertFalse((self.root / 'blocked.txt').exists())

    def test_unknown_hook_event_cannot_be_silently_ignored(self):
        with self.assertRaises(bridge.Denied):
            bridge.Policy({'cwd': str(self.root), 'allowed_tools': [],
                           'settings': {'hooks': {'FutureEvent': []}}})

    def test_readonly_profile_cannot_write_or_edit(self):
        p = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
                           'write_roots': [], 'allowed_tools': ['Read']})
        with self.assertRaises(bridge.Denied):
            p.write('bad.txt', 'bad')
        self.assertFalse((self.root / 'bad.txt').exists())

    def test_traversal_absolute_and_symlink_escape_are_denied(self):
        (self.root / 'link').symlink_to(self.outside)
        for name in ['../outside.txt', str(self.outside), 'link']:
            with self.subTest(name=name), self.assertRaises(bridge.Denied):
                self.policy.read(name)
        (self.root / 'dirlink').symlink_to(self.outside.parent, target_is_directory=True)
        with self.assertRaises(bridge.Denied):
            self.policy.write('dirlink/created.txt', 'bad')
        self.assertFalse((self.outside.parent / 'created.txt').exists())

    def test_search_rejects_intermediate_directory_symlinks(self):
        outside_dir = Path(self.temp.name) / 'external'
        (outside_dir / 'nested').mkdir(parents=True)
        (outside_dir / 'nested' / 'private-name.txt').write_text('secret')
        (self.root / 'escape').symlink_to(outside_dir, target_is_directory=True)
        with self.assertRaises(bridge.Denied):
            self.policy.glob('*', 'escape/nested')

    def test_secret_and_control_paths_are_not_tool_readable_or_writable(self):
        for name in ['.env', '.env.local', '.git/config', '.codex/auth.json',
                     '.ssh/id_ed25519', '.claude/settings.json']:
            with self.subTest(name=name), self.assertRaises(bridge.Denied):
                self.policy.write(name, 'SYNTHETIC_SECRET')

    def test_command_matches_tokens_and_never_shell_syntax(self):
        self.assertEqual(self.policy.command_argv('printf "hello world"'),
                         ['printf', 'hello world'])
        self.assertEqual(self.policy.command_argv('git status --short'),
                         ['git', 'status', '--short'])
        for command in ['printf ok; id', 'printf $(id)', 'printf `id`',
                        'printf ok | cat', 'printf ok > output',
                        'printf ok\nid', 'git status-evil', 'env printf ok',
                        'bash -c id', 'printf <(id)']:
            with self.subTest(command=command), self.assertRaises(bridge.Denied):
                self.policy.command_argv(command)

    def test_literal_shell_characters_in_argument_are_data(self):
        self.assertEqual(self.policy.command_argv("printf 'hello; world'"),
                         ['printf', 'hello; world'])

    def test_unknown_tool_or_malformed_policy_fails_closed(self):
        with self.assertRaises(bridge.Denied):
            bridge.Policy({'cwd': str(self.root), 'allowed_tools': ['FutureTool']})
        with self.assertRaises(bridge.Denied):
            bridge.Policy({'cwd': '/', 'read_roots': ['/'], 'allowed_tools': ['Read']})

    def test_edit_requires_unique_match(self):
        self.policy.write('note.md', 'same same')
        with self.assertRaises(bridge.Denied):
            self.policy.edit('note.md', 'same', 'changed')
        self.assertEqual(self.policy.read('note.md'), 'same same')


class TransportTests(unittest.TestCase):
    def test_stdio_exposes_only_declared_tools_and_denies_unknown_calls(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'workspace'
            root.mkdir()
            (root / 'note.txt').write_text('synthetic note')
            config = Path(tmp) / 'policy.json'
            config.write_text(json.dumps({'cwd': str(root), 'read_roots': [str(root)],
                                         'write_roots': [], 'allowed_tools': ['Read']}))
            config.chmod(0o600)
            messages = [
                {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {}},
                {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'},
                {'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call',
                 'params': {'name': 'Read', 'arguments': {'file_path': 'note.txt'}}},
                {'jsonrpc': '2.0', 'id': 4, 'method': 'tools/call',
                 'params': {'name': 'Write', 'arguments': {'file_path': 'bad.txt', 'content': 'bad'}}},
            ]
            run = subprocess.run([sys.executable, '-I', str(SPEC.origin), str(config)],
                                 input=''.join(json.dumps(m)+'\n' for m in messages),
                                 capture_output=True, text=True, timeout=10)
            self.assertEqual(run.returncode, 0, run.stderr)
            replies = [json.loads(line) for line in run.stdout.splitlines()]
            self.assertEqual(len(replies), 4)
            self.assertEqual([t['name'] for t in replies[1]['result']['tools']], ['Read'])
            self.assertEqual(replies[2]['result']['content'][0]['text'], 'synthetic note')
            self.assertTrue(replies[3]['result']['isError'])
            self.assertFalse((root / 'bad.txt').exists())


if __name__ == '__main__':
    unittest.main()
