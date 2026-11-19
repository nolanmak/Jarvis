#!/usr/bin/env python3
"""Workspace shell mode of the tool bridge (#2217).

A yolo turn may run a short list of programs in its scratch workspace. They
go through the same kernel sandbox as every other host command; the policy
flag `workspace_shell` only lets them write inside the policy's write roots
and replaces their environment with a fixed minimal one. These tests run
real commands in the sandbox, so they need a host that enforces it.
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('codex_tool_bridge_ws', SCRIPTS / 'codex-tool-bridge.py')
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)

CAPABILITIES_SPEC = importlib.util.spec_from_file_location(
    'host_capabilities', Path(__file__).with_name('host_capabilities.py'))
capabilities = importlib.util.module_from_spec(CAPABILITIES_SPEC)
CAPABILITIES_SPEC.loader.exec_module(capabilities)
requires_sandbox = capabilities.requirement(capabilities.sandbox_unavailable_reason())

TOOLS = ['Read', 'Write', 'Edit', 'Bash(python3 *)', 'Bash(ls)', 'Bash(ls *)', 'Bash(cat *)', 'Bash(mkdir *)']


class WorkspaceShellTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        base = Path(self.temp.name).resolve()
        self.workspace = base / 'workspace'
        self.workspace.mkdir()
        self.wiki = base / 'wiki'
        self.wiki.mkdir()
        (self.wiki / 'page.md').write_text('WIKI_PAGE')
        self.outside = base / 'outside'
        self.outside.mkdir()
        (self.outside / 'private.txt').write_text('SYNTHETIC_PRIVATE')

    def policy(self, workspace_shell=True, environment=None):
        config = {
            'cwd': str(self.workspace),
            'read_roots': [str(self.workspace), str(self.wiki)],
            'write_roots': [str(self.workspace)],
            'allowed_tools': TOOLS,
            'environment': environment or {},
        }
        if workspace_shell is not None:
            config['workspace_shell'] = workspace_shell
        policy = bridge.Policy(config)
        self.addCleanup(policy.close)
        return policy

    def python(self, policy, code):
        return policy.run_command('python3 -c ' + bridge.shlex.quote(code))

    def test_the_flag_is_off_unless_it_is_exactly_true(self):
        self.assertTrue(self.policy(True).workspace_shell)
        for value in (None, False, 1, 'true', 'yes'):
            self.assertFalse(self.policy(value).workspace_shell, value)

    def test_a_program_that_is_not_listed_is_refused_before_anything_runs(self):
        with self.assertRaises(bridge.Denied):
            self.policy().run_command('curl https://example.com')
        with self.assertRaises(bridge.Denied):
            self.policy().run_command('bash -c true')

    @requires_sandbox
    def test_a_listed_program_writes_in_the_workspace_and_reads_the_wiki(self):
        policy = self.policy()
        result = self.python(policy, "open('note.txt','w').write('made here'); print(open('note.txt').read())")
        self.assertEqual(result['exit_code'], 0, result)
        self.assertEqual((self.workspace / 'note.txt').read_text(), 'made here')
        result = policy.run_command('cat ' + str(self.wiki / 'page.md'))
        self.assertEqual((result['exit_code'], result['stdout']), (0, 'WIKI_PAGE'), result)
        result = policy.run_command('mkdir nested')
        self.assertEqual(result['exit_code'], 0, result)
        self.assertTrue((self.workspace / 'nested').is_dir())

    @requires_sandbox
    def test_writes_and_reads_outside_the_roots_fail_also_through_a_child_process(self):
        policy = self.policy()
        target = self.outside / 'escaped.txt'
        wiki_target = self.wiki / 'planted.md'
        for path in (target, wiki_target):
            direct = self.python(policy, f"open({str(path)!r},'w').write('x')")
            self.assertNotEqual(direct['exit_code'], 0, direct)
            child = self.python(policy, "import subprocess,sys; "
                f"sys.exit(subprocess.run(['python3','-c','open({str(path)!r},\"w\").write(\"x\")']).returncode)")
            self.assertNotEqual(child['exit_code'], 0, child)
            self.assertFalse(path.exists(), path)
        read = self.python(policy, f"print(open({str(self.outside / 'private.txt')!r}).read())")
        self.assertNotEqual(read['exit_code'], 0, read)
        self.assertNotIn('SYNTHETIC_PRIVATE', read['stdout'])

    @requires_sandbox
    def test_no_network_also_through_a_child_process(self):
        policy = self.policy()
        connect = "import socket; s=socket.socket(); s.settimeout(2); s.connect(('1.1.1.1', 53))"
        self.assertNotEqual(self.python(policy, connect)['exit_code'], 0)
        child = self.python(policy, "import subprocess,sys; "
            f"sys.exit(subprocess.run(['python3','-c',{connect!r}]).returncode)")
        self.assertNotEqual(child['exit_code'], 0, child)

    @requires_sandbox
    def test_the_command_environment_is_exactly_three_variables(self):
        policy = self.policy(environment={
            'AUGMENTAGENT_DB': '/var/lib/jarvis/data.db',
            'COMPOSIO_API_KEY': 'integration-credential',
            'PATH': '/opt/custom/bin:/usr/bin:/bin',
        })
        result = self.python(policy, "import os, json; print(json.dumps(dict(os.environ), sort_keys=True))")
        self.assertEqual(result['exit_code'], 0, result)
        import json
        seen = json.loads(result['stdout'])
        # The interpreter may add its own bookkeeping variables; none of the
        # profile's integration environment may be present.
        self.assertEqual(seen.get('PATH'), '/usr/local/bin:/usr/bin:/bin')
        self.assertEqual(seen.get('HOME'), str(self.workspace))
        self.assertEqual(seen.get('LANG'), 'C.UTF-8')
        self.assertNotIn('AUGMENTAGENT_DB', seen)
        self.assertNotIn('COMPOSIO_API_KEY', seen)
        self.assertNotIn('integration-credential', result['stdout'])
        self.assertLessEqual(set(seen) - {'PATH', 'HOME', 'LANG'}, {'LC_CTYPE', 'PWD', 'OLDPWD', 'SHLVL', '_'})

    @requires_sandbox
    def test_without_the_flag_host_commands_stay_read_only_and_keep_the_profile_environment(self):
        policy = self.policy(None, environment={'JARVIS_FIXTURE_MARKER': 'kept'})
        result = self.python(policy, "open('note.txt','w').write('x')")
        self.assertNotEqual(result['exit_code'], 0, result)
        self.assertFalse((self.workspace / 'note.txt').exists())
        result = self.python(policy, "import os; print(os.environ.get('JARVIS_FIXTURE_MARKER'))")
        self.assertEqual((result['exit_code'], result['stdout'].strip()), (0, 'kept'), result)


class WebToolsAreReadOnly(unittest.TestCase):
    def test_search_and_fetch_are_read_only_and_the_pin_change_is_not(self):
        self.assertTrue(bridge.read_only_operation('mcp__web__search', {'query': 'x'}))
        self.assertTrue(bridge.read_only_operation('mcp__web__fetch', {'url': 'https://example.com'}))
        self.assertFalse(bridge.read_only_operation('mcp__web__set_provider', {'kind': 'search', 'provider': 'exa'}))
        self.assertFalse(bridge.read_only_operation('mcp__web__anything_else', {}))


if __name__ == '__main__':
    unittest.main()
