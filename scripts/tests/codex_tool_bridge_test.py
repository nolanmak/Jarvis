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

# Tests that run commands or render PDFs go through the kernel sandbox. A host
# that cannot enforce it (kernels older than Linux 6.12, no libseccomp, no
# Poppler) skips them with the reason, or fails them in CI, where
# REQUIRE_ENFORCEABLE_SANDBOX=1.
CAPABILITIES_SPEC = importlib.util.spec_from_file_location(
    'host_capabilities', Path(__file__).with_name('host_capabilities.py'))
capabilities = importlib.util.module_from_spec(CAPABILITIES_SPEC)
CAPABILITIES_SPEC.loader.exec_module(capabilities)
SANDBOX_UNAVAILABLE = capabilities.sandbox_unavailable_reason()
POPPLER_UNAVAILABLE = capabilities.poppler_unavailable_reason()
requires_sandbox = capabilities.requirement(SANDBOX_UNAVAILABLE)
requires_poppler = capabilities.requirement(POPPLER_UNAVAILABLE)


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

    def test_repeated_protocol_call_id_cannot_repeat_or_change_a_write(self):
        server = bridge.Server(self.policy)
        original_write = self.policy.write
        writes = []

        def counted_write(path, content):
            writes.append((path, content))
            return original_write(path, content)

        self.policy.write = counted_write
        request = {'jsonrpc': '2.0', 'id': 41, 'method': 'tools/call',
                   'params': {'name': 'Write', 'arguments': {
                       'file_path': 'note.md', 'content': 'first'}}}

        def send(value):
            return json.loads(bridge.safe_dispatch(server, json.dumps(value).encode() + b'\n'))

        first = send(request)
        replay = send(request)
        changed = send({**request, 'params': {'name': 'Write', 'arguments': {
            'file_path': 'note.md', 'content': 'changed'}}})
        self.assertEqual(replay, first)
        self.assertEqual(changed['error']['code'], -32600)
        self.assertEqual(writes, [('note.md', 'first')])
        self.assertEqual((self.root / 'note.md').read_text(), 'first')

    def test_lost_tool_reply_requires_inspection_without_replaying_the_write(self):
        server = bridge.Server(self.policy)
        original_dispatch = server.dispatch
        calls = []

        def lost_reply(request):
            calls.append(request['id'])
            original_dispatch(request)
            raise LookupError('synthetic reply lost after write')

        server.dispatch = lost_reply
        request = {'jsonrpc': '2.0', 'id': 42, 'method': 'tools/call',
                   'params': {'name': 'Write', 'arguments': {
                       'file_path': 'note.md', 'content': 'written once'}}}
        line = json.dumps(request).encode() + b'\n'
        first = json.loads(bridge.safe_dispatch(server, line))
        replay = json.loads(bridge.safe_dispatch(server, line))
        self.assertEqual(first['error']['code'], -32603)
        self.assertTrue(replay['result']['isError'])
        self.assertIn('uncertain', replay['result']['content'][0]['text'])
        self.assertEqual(calls, [42])
        self.assertEqual((self.root / 'note.md').read_text(), 'written once')

    def test_receipt_limit_still_allows_fresh_reconciliation_reads(self):
        (self.root / 'note.md').write_text('known state')
        server = bridge.Server(self.policy)
        server.call_receipts = {(int, index): (b'synthetic', '{}') for index in range(1024)}

        def send(identifier, name, arguments):
            request = {'jsonrpc': '2.0', 'id': identifier, 'method': 'tools/call',
                       'params': {'name': name, 'arguments': arguments}}
            return json.loads(bridge.safe_dispatch(server, json.dumps(request).encode() + b'\n'))

        read = send(1025, 'Read', {'file_path': 'note.md'})
        self.assertIn('known state', read['result']['content'][0]['text'])
        self.assertEqual(send(1026, 'Write', {'file_path': 'note.md', 'content': 'changed'})['error']['code'], -32000)
        self.assertEqual((self.root / 'note.md').read_text(), 'known state')
        self.assertEqual(len(server.call_receipts), 1024)

    def test_gmail_attachment_download_cannot_bypass_mutation_receipt_limit(self):
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)],
            'allowed_tools': ['Bash(augmentagent gmail get-attachment *)']})
        server = bridge.Server(policy)
        server.call_receipts = {(int, index): (b'synthetic', '{}') for index in range(1024)}
        executed = []
        server.dispatch = lambda request: executed.append(request) or {'content': []}
        for index, suffix in enumerate(('', ' --out '+str(self.root / 'attachment.pdf'))):
            request = {'jsonrpc': '2.0', 'id': 2000 + index, 'method': 'tools/call',
                'params': {'name': 'Bash', 'arguments': {'command':
                    'augmentagent gmail get-attachment --message-id synthetic'+suffix}}}
            response = json.loads(bridge.safe_dispatch(server, json.dumps(request).encode()+b'\n'))
            self.assertEqual(response['error']['code'], -32000)
        self.assertEqual(executed, [])

    def test_scoped_image_read_returns_original_bytes_as_mcp_image(self):
        import base64
        # Synthetic eight-pixel-square PNG; no private fixture assets.
        original = base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg==')
        (self.root / 'pixel.png').write_bytes(original)
        server = bridge.Server(self.policy)
        response = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Read', 'arguments': {'file_path': 'pixel.png'}}})
        self.assertFalse(response.get('isError', False))
        self.assertEqual(response['content'][0]['type'], 'image')
        self.assertEqual(response['content'][0]['mimeType'], 'image/png')
        self.assertEqual(base64.b64decode(response['content'][0]['data']), original)
        self.assertEqual((self.root / 'pixel.png').read_bytes(), original)

    @requires_sandbox
    @requires_poppler
    def test_pdf_read_renders_selected_pages_and_preserves_original(self):
        import base64
        original = (Path(__file__).parent / 'fixtures/scoped-document.pdf').read_bytes()
        (self.root / 'document.pdf').write_bytes(original)
        server = bridge.Server(self.policy)
        result = server.call('Read', {'file_path': 'document.pdf', 'pages': '2'})
        self.assertIn('Page 2 of 2', result['content'][0]['text'])
        self.assertEqual(len(result['content']), 2)
        self.assertEqual(result['content'][1]['type'], 'image')
        self.assertTrue(base64.b64decode(result['content'][1]['data']).startswith(b'\x89PNG\r\n\x1a\n'))
        self.assertEqual((self.root / 'document.pdf').read_bytes(), original)
        result = server.call('Read', {'file_path': 'document.pdf'})
        self.assertEqual(len(result['content']), 4)

    def test_pdf_page_ranges_reject_invalid_or_excessive_requests(self):
        original = (Path(__file__).parent / 'fixtures/scoped-document.pdf').read_bytes()
        (self.root / 'document.pdf').write_bytes(original)
        server = bridge.Server(self.policy)
        for pages in ('0', '2-1', '1-21', '1,2', '1-999999999', '3'):
            with self.subTest(pages=pages), self.assertRaises(bridge.Denied):
                server.call('Read', {'file_path': 'document.pdf', 'pages': pages})
        with self.assertRaises(bridge.Denied):
            server.call('Read', {'file_path': 'document.pdf', 'offset': 1})
        self.policy.write('text.txt', 'synthetic')
        with self.assertRaises(bridge.Denied):
            server.call('Read', {'file_path': 'text.txt', 'pages': '1'})

    def test_image_read_keeps_file_scope_and_rejects_text_line_arguments(self):
        (self.root / 'image.png').write_bytes(b'\x89PNG\r\n\x1a\nSYNTHETIC')
        server = bridge.Server(self.policy)
        with self.assertRaises(bridge.Denied):
            server.call('Read', {'file_path': 'image.png', 'offset': 1})
        (self.root / 'image-escape.png').symlink_to(self.outside)
        with self.assertRaises(bridge.Denied):
            server.call('Read', {'file_path': 'image-escape.png'})

    def test_file_tool_schema_supports_narrow_reads_searches_and_replace_all(self):
        server=bridge.Server(self.policy)
        self.policy.write('notes/source.txt','first\nSYNTHETIC needle\nlast\n')
        self.policy.write('unrelated.txt','needle elsewhere\n')
        self.assertEqual(server.call('Read',{'file_path':'notes/source.txt','offset':2,'limit':1}),
                         'SYNTHETIC needle\n')
        hits=json.loads(server.call('Grep',{'pattern':'synthetic','path':'notes/source.txt','ignore_case':True}))
        self.assertEqual(len(hits),1)
        self.assertEqual(hits[0]['line'],2)
        self.assertEqual(json.loads(server.call('Glob',{'pattern':'*.txt','path':'notes'})),['source.txt'])
        self.policy.write('repeat.txt','old old')
        server.call('Edit',{'file_path':'repeat.txt','old_string':'old','new_string':'new','replace_all':True})
        self.assertEqual(self.policy.read('repeat.txt'),'new new')
        schemas={tool['name']:tool['inputSchema'] for tool in server.tools()}
        self.assertEqual(schemas['Read']['required'],['file_path'])
        self.assertIn('timeout',schemas['Bash']['properties'])

    def test_file_tool_optional_arguments_are_validated_before_execution(self):
        server=bridge.Server(self.policy)
        self.policy.write('note.txt','before')
        for arguments in [
            {'file_path':'note.txt','offset':0},
            {'file_path':'note.txt','limit':True},
            {'file_path':'note.txt','unknown':'ignored input'},
        ]:
            with self.subTest(arguments=arguments), self.assertRaises(bridge.Denied):
                server.call('Read',arguments)
        with self.assertRaises(bridge.Denied):
            server.call('Grep',{'pattern':'x','path':str(self.outside)})

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

    def test_unimplemented_local_tools_fail_readiness_instead_of_disappearing(self):
        for tool in ('LS', 'NotebookEdit'):
            with self.subTest(tool=tool), self.assertRaisesRegex(bridge.Denied, 'unsupported local tool'):
                policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
                                        'allowed_tools': [tool]})
                bridge.Server(policy).tools()

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

    def test_hard_link_escape_is_denied_for_read_grep_edit_write_and_skipped_by_glob(self):
        import os
        # A pre-existing hard link: a name inside the scope, an inode outside it.
        os.link(self.outside, self.root / 'linked.txt')
        (self.root / 'plain.txt').write_text('SYNTHETIC_PUBLIC\n')
        server = bridge.Server(self.policy)
        with self.assertRaises(bridge.Denied):
            self.policy.read('linked.txt')
        response = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Read', 'arguments': {'file_path': 'linked.txt'}}})
        self.assertTrue(response['isError'])
        self.assertNotIn('SYNTHETIC_PRIVATE', json.dumps(response))
        self.assertEqual(self.policy.grep('SYNTHETIC'), [
            {'path': 'plain.txt', 'line': 1, 'text': 'SYNTHETIC_PUBLIC'}])
        self.assertEqual(self.policy.grep('SYNTHETIC', 'linked.txt'), [])
        self.assertEqual(self.policy.glob('*.txt'), ['plain.txt'])
        for action in (lambda: self.policy.edit('linked.txt', 'PRIVATE', 'CHANGED'),
                       lambda: self.policy.write('linked.txt', 'REPLACED')):
            with self.assertRaises(bridge.Denied):
                action()
        self.assertEqual(self.outside.read_text(), 'SYNTHETIC_PRIVATE')
        self.assertEqual(os.stat(self.root / 'linked.txt').st_nlink, 2)
        # The rule is about the inode, not where the other name lives.
        os.link(self.root / 'plain.txt', self.root / 'alias.txt')
        with self.assertRaises(bridge.Denied):
            self.policy.read('plain.txt')

    def test_bridge_and_command_sandbox_share_one_file_verification_helper(self):
        import os
        verifier = bridge.file_verification()
        self.assertEqual(Path(verifier.__file__).resolve(),
                         (Path(bridge.__file__).parent / 'codex-command-sandbox.py').resolve())
        (self.root / 'plain.txt').write_text('SYNTHETIC_PUBLIC\n')
        self.assertEqual(self.policy.read('plain.txt'), 'SYNTHETIC_PUBLIC\n')
        granted = [access for descriptor, access in verifier.source_read_entries([str(self.root)])
                   if access != 1 << 3]
        self.assertEqual(len(granted), 1)
        original = verifier.regular_private_file
        verifier.regular_private_file = lambda info: False
        self.addCleanup(setattr, verifier, 'regular_private_file', original)
        with self.assertRaises(bridge.Denied):
            self.policy.read('plain.txt')
        self.assertEqual(self.policy.glob('*.txt'), [])
        granted = [access for descriptor, access in verifier.source_read_entries([str(self.root)])
                   if access != 1 << 3]
        self.assertEqual(granted, [])

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

    @requires_sandbox
    def test_allowed_command_executes_and_does_not_evaluate_shell_text(self):
        outcome=self.policy.run_command("printf 'hello; world'")
        self.assertEqual(outcome['exit_code'],0)
        self.assertEqual(outcome['stdout'],'hello; world')
        with self.assertRaises(bridge.Denied):
            self.policy.run_command('printf ok; id')

    @requires_sandbox
    def test_bash_result_survives_mcp_dispatch(self):
        response=bridge.Server(self.policy).dispatch({'method':'tools/call','params':{
            'name':'Bash','arguments':{'command':'printf DISPATCH_OK'}}})
        self.assertFalse(response.get('isError',False))
        result=json.loads(response['content'][0]['text'])
        self.assertEqual(result['exit_code'],0)
        self.assertEqual(result['stdout'],'DISPATCH_OK')

    def test_command_cannot_read_outside_scope(self):
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[],'allowed_tools':['Bash(cat *)']})
        outcome=policy.run_command(f'cat {self.outside}')
        self.assertNotEqual(outcome['exit_code'],0)
        self.assertNotIn('SYNTHETIC_PRIVATE',outcome['stdout'])

    def test_command_cannot_read_credential_files_inside_read_scope(self):
        (self.root/'.env').write_text('SYNTHETIC_SECRET')
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[],'allowed_tools':['Bash(cat *)']})
        result=policy.run_command('cat .env')
        self.assertNotEqual(result['exit_code'],0)
        self.assertNotIn('SYNTHETIC_SECRET',result['stdout'])

    def test_service_commands_keep_approval_and_file_scope_boundaries(self):
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Bash(augmentagent gmail *)','Bash(aa-gh issue create *)']})
        policy.check_service_argv(['augmentagent','gmail','search','--query','synthetic'])
        policy.check_service_argv(['augmentagent','gmail','compose','--body','synthetic','--post'])
        for argv in [
            ['augmentagent','gmail','send-now','--body','bad'],
            ['augmentagent','gmail','send','synthetic-draft'],
            ['augmentagent','gmail','get-attachment','--out',str(self.outside)],
            ['augmentagent','gmail','get-attachment','--out='+str(self.outside)],
            ['aa-gh','issue','create','--body-file',str(self.outside)],
            ['aa-gh','issue','create','-F'+str(self.outside)],
            ['augmentagent','gmail','compose','--attachment','.env'],
            ['augmentagent','gmail','search','--db',str(self.outside)],
        ]:
            with self.subTest(argv=argv),self.assertRaises(bridge.Denied):
                policy.check_service_argv(argv)

    @requires_sandbox
    def test_build_command_uses_disposable_workspace_and_syncs_safe_source_changes(self):
        fakebin=Path(self.temp.name)/'bin';fakebin.mkdir()
        cargo=fakebin/'cargo'
        cargo.write_text("""#!/usr/bin/python3
from pathlib import Path
import os
assert 'SYNTHETIC_TOKEN' not in os.environ
assert not Path('.env').exists()
Path('source.rs').write_text('formatted source\\n')
Path('Cargo.lock').write_text('synthetic lock\\n')
Path('target').mkdir(exist_ok=True)
Path('target/artifact').write_text('generated build output')
print('SYNTHETIC_BUILD_OK')
""")
        cargo.chmod(0o700)
        (self.root/'source.rs').write_text('source')
        (self.root/'.env').write_text('SYNTHETIC_TOKEN=PRIVATE')
        policy=bridge.Policy({'build_runner':'host','cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Edit','Bash(cargo *)'],
            'environment':{'PATH':str(fakebin)+':/usr/bin','SYNTHETIC_TOKEN':'PRIVATE'}})
        result=policy.run_command('cargo test')
        self.assertEqual(result['exit_code'],0,result)
        self.assertIn('SYNTHETIC_BUILD_OK',result['stdout'])
        self.assertEqual((self.root/'source.rs').read_text(),'formatted source\n')
        self.assertEqual((self.root/'Cargo.lock').read_text(),'synthetic lock\n')
        self.assertFalse((self.root/'target').exists())
        self.assertEqual((self.root/'.env').read_text(),'SYNTHETIC_TOKEN=PRIVATE')

    @requires_sandbox
    def test_build_preserves_home_identity_without_granting_home_file_access(self):
        fakebin=Path(self.temp.name)/'bin';fakebin.mkdir()
        owner=Path(self.temp.name)/'owner';owner.mkdir()
        (owner/'private.txt').write_text('SYNTHETIC_PRIVATE')
        cargo=fakebin/'cargo'
        cargo.write_text('''#!/usr/bin/python3
import os
from pathlib import Path
assert os.environ.get('HOME'), 'OS home identity was dropped'
try:
    (Path(os.environ['HOME'])/'private.txt').read_text()
except PermissionError:
    print('HOME_CONTENT_DENIED')
else:
    raise AssertionError('home file escaped confinement')
''')
        cargo.chmod(0o700)
        policy=bridge.Policy({'build_runner':'host','cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Bash(cargo *)'],
            'environment':{'PATH':str(fakebin)+':/usr/bin','HOME':str(owner)}})
        result=policy.run_command('cargo test')
        self.assertEqual(result['exit_code'],0,result)
        self.assertIn('HOME_CONTENT_DENIED',result['stdout'])

    def test_dependency_copy_preserves_links_as_data_and_rejects_nonregular_roots(self):
        import os
        source = Path(self.temp.name) / 'dependencies'; source.mkdir()
        private = Path(self.temp.name) / 'private.txt'; private.write_text('SYNTHETIC_PRIVATE')
        (source / 'link').symlink_to(private)
        (source / 'index.js').write_text('SYNTHETIC_PACKAGE')
        target = Path(self.temp.name) / 'copy'
        bridge.copy_dependency_tree(source, target)
        self.assertTrue((target / 'link').is_symlink())
        self.assertEqual(os.readlink(target / 'link'), str(private))
        self.assertEqual((target / 'index.js').read_text(), 'SYNTHETIC_PACKAGE')
        self.assertEqual(private.read_text(), 'SYNTHETIC_PRIVATE')
        root_link = Path(self.temp.name) / 'root-link'; root_link.symlink_to(source)
        with self.assertRaises(bridge.Denied):
            bridge.copy_dependency_tree(root_link, Path(self.temp.name) / 'invalid')
        os.mkfifo(source / 'fifo')
        with self.assertRaises((bridge.Denied, OSError)):
            bridge.copy_dependency_tree(source, Path(self.temp.name) / 'fifo-copy')

    @requires_sandbox
    def test_real_cargo_can_build_and_test_a_dependency_free_fixture(self):
        import shutil
        if not shutil.which('cargo'):
            self.skipTest('Cargo is not installed')
        (self.root/'src').mkdir()
        (self.root/'Cargo.toml').write_text('[package]\nname="synthetic-build"\nversion="0.1.0"\nedition="2021"\n')
        (self.root/'src/lib.rs').write_text('#[test] fn synthetic_passes() { assert_eq!(2 + 2, 4); }\n')
        policy=bridge.Policy({'build_runner':'host','cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Edit','Bash(cargo *)']})
        outcome=policy.run_command('cargo test --offline',timeout=60)
        self.assertEqual(outcome['exit_code'],0,outcome)
        self.assertIn('1 passed',outcome['stdout'])
        self.assertFalse((self.root/'target').exists())

    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG') and
                         __import__('os').environ.get('JARVIS_TEST_PACKAGE_NETWORK'),
                         'requires private KVM runtime and explicit public crate GET probe')
    def test_vm_builds_uncached_public_crate_and_reuses_it_offline(self):
        import os
        private = Path(self.temp.name)
        registry = private / 'empty-registry'; registry.mkdir()
        config = json.loads(Path(os.environ['JARVIS_TEST_VM_CONFIG']).read_text())
        config['registry'] = str(registry)
        config_file = private / 'runtime.json'; config_file.write_text(json.dumps(config)); config_file.chmod(0o600)
        (self.root / 'src').mkdir()
        (self.root / 'Cargo.toml').write_text('[package]\nname="synthetic-registry-probe"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nitoa="=1.0.15"\n')
        (self.root / 'src/lib.rs').write_text('#[test] fn formats_fixture() { assert_eq!(itoa::Buffer::new().format(42), "42"); }\n')
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)], 'allowed_tools': ['Read', 'Write', 'Bash(cargo *)'],
            'build_vm_config': str(config_file), 'build_scratch_dir': os.environ.get('JARVIS_TEST_BUILD_SCRATCH')})
        self.addCleanup(policy.close)
        first = policy.run_command('cargo test', timeout=90)
        self.assertEqual(first['exit_code'], 0, first)
        self.assertIn('1 passed', first['stdout'])
        second = policy.run_command('cargo test --offline', timeout=90)
        self.assertEqual(second['exit_code'], 0, second)
        self.assertEqual(list(registry.iterdir()), [], 'operator cache must stay read-only')
        self.assertIn('registry+https://github.com/rust-lang/crates.io-index', (self.root / 'Cargo.lock').read_text())
        self.assertNotIn('127.0.0.1', (self.root / 'Cargo.lock').read_text())
        self.assertFalse((self.root / 'target').exists())
        self.assertFalse((self.root / '.cargo-home').exists(), 'private caches must not be reconciled into source')

    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG') and
                         __import__('os').environ.get('JARVIS_TEST_PACKAGE_NETWORK'),
                         'requires private KVM runtime and explicit public package GET probe')
    def test_vm_installs_uncached_public_npm_package_without_general_network(self):
        import os
        (self.root / 'package.json').write_text(json.dumps({'name': 'synthetic-registry-probe',
            'version': '1.0.0', 'scripts': {'test': 'node probe.js'}}))
        (self.root / 'probe.js').write_text("""const assert=require('assert'),fs=require('fs'),net=require('net'),https=require('https');
assert.equal(require('is-number')(42),true); assert.equal(require('is-number')('abc'),false);
assert.deepEqual(fs.readdirSync('/sys/class/net'),['lo']);
assert.throws(()=>fs.readFileSync('/root/registry-key.pem'),error=>error.code==='EACCES');
const direct=new Promise((resolve,reject)=>{
 const socket=net.connect({host:'1.1.1.1',port:80}); socket.once('connect',()=>reject(new Error('general network escaped')));
 socket.once('error',()=>resolve());
});
function denied(options,expected) { return new Promise((resolve,reject)=>{
 const request=https.request('https://registry.npmjs.org/is-number',options,response=>{
  try { assert.equal(response.statusCode,expected); response.resume(); response.once('end',resolve); }
  catch(error) { reject(error); }
 }); request.once('error',reject); request.end();
}); }
Promise.all([direct,denied({method:'POST'},405),denied({servername:'registry.npmjs.org',headers:{Host:'example.invalid'}},403)])
 .then(()=>console.log('SYNTHETIC_PUBLIC_PACKAGE_OK_NETWORK_BLOCKED')).catch(error=>{console.error(error);process.exitCode=1;});""")
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)], 'allowed_tools': ['Read', 'Write', 'Bash(npm *)'],
            'build_vm_config': os.environ['JARVIS_TEST_VM_CONFIG'], 'build_scratch_dir': os.environ.get('JARVIS_TEST_BUILD_SCRATCH')})
        self.addCleanup(policy.close)
        installed = policy.run_command('npm install is-number@7.0.0 --no-audit --no-fund --fetch-retries=0 --fetch-timeout=10000', timeout=60)
        self.assertEqual(installed['exit_code'], 0, installed)
        self.assertEqual(json.loads((self.root / 'package.json').read_text())['dependencies']['is-number'], '^7.0.0')
        self.assertIn('https://registry.npmjs.org/is-number/', (self.root / 'package-lock.json').read_text())
        self.assertNotIn('127.0.0.1', (self.root / 'package-lock.json').read_text())
        built = policy.run_command('npm test --offline', timeout=60)
        self.assertEqual(built['exit_code'], 0, built)
        self.assertIn('SYNTHETIC_PUBLIC_PACKAGE_OK_NETWORK_BLOCKED', built['stdout'])
        self.assertFalse((self.root / 'node_modules').exists())
        fresh = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)], 'allowed_tools': ['Read', 'Write', 'Bash(npm *)'],
            'build_vm_config': os.environ['JARVIS_TEST_VM_CONFIG'], 'build_scratch_dir': os.environ.get('JARVIS_TEST_BUILD_SCRATCH')})
        self.addCleanup(fresh.close)
        restored = fresh.run_command('npm ci --no-audit --no-fund --fetch-retries=0 --fetch-timeout=10000', timeout=60)
        self.assertEqual(restored['exit_code'], 0, restored)


    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG'), 'requires private KVM runtime')
    def test_vm_npm_ci_installs_local_locked_dependency_and_next_build_uses_it(self):
        import io
        import os
        import tarfile
        with tarfile.open(self.root / 'fixture.tgz', 'w:gz') as archive:
            for name, data in {
                'package/package.json': json.dumps({'name': 'fixture', 'version': '1.0.0',
                    'scripts': {'install': 'node-gyp rebuild && node install.js'}}),
                'package/index.js': "if(require('./build/Release/fixture.node').answer!==42) throw new Error('native fixture failed'); module.exports='SYNTHETIC_INSTALLED';",
                'package/install.js': "require('fs').writeFileSync('installed.txt','SYNTHETIC_GUEST_INSTALL');",
                'package/binding.gyp': json.dumps({'targets': [{'target_name': 'fixture', 'sources': ['fixture.cc']}]}),
                'package/fixture.cc': '#include <node_api.h>\nnapi_value Init(napi_env env, napi_value exports) { napi_value answer; napi_create_int32(env,42,&answer); napi_set_named_property(env,exports,"answer",answer); return exports; }\nNAPI_MODULE(NODE_GYP_MODULE_NAME, Init)\n',
            }.items():
                raw = data.encode(); info = tarfile.TarInfo(name); info.size = len(raw)
                archive.addfile(info, io.BytesIO(raw))
        (self.root / 'package.json').write_text(json.dumps({'name': 'synthetic-install', 'version': '1.0.0',
            'dependencies': {'fixture': 'file:fixture.tgz'}, 'scripts': {'test': 'node test.js'}}))
        (self.root / 'test.js').write_text("""const fs=require('fs');
if(require('fixture')!=='SYNTHETIC_INSTALLED') throw new Error('old dependency');
if(fs.readFileSync('node_modules/fixture/installed.txt','utf8')!=='SYNTHETIC_GUEST_INSTALL') throw new Error('install script missing');
try { fs.writeFileSync('node_modules/fixture/installed.txt','MUST_NOT_WRITE'); throw new Error('writable cache'); }
catch(error) { if(error.code!=='EROFS') throw error; }
console.log('SYNTHETIC_INSTALL_BUILD_OK');""")
        # Generate the local-file lock without lifecycle scripts or network.
        locked = subprocess.run(['npm', 'install', '--package-lock-only', '--ignore-scripts', '--offline',
            '--no-audit', '--no-fund'], cwd=self.root, capture_output=True, text=True,
            env={'PATH': os.defpath, 'HOME': self.temp.name, 'NPM_CONFIG_USERCONFIG': '/dev/null',
                 'NPM_CONFIG_GLOBALCONFIG': str(Path(self.temp.name) / 'empty-global-config')}, timeout=30)
        self.assertEqual(locked.returncode, 0, locked.stderr)
        old = self.root / 'node_modules/fixture'; old.mkdir(parents=True)
        (old / 'index.js').write_text("module.exports='SYNTHETIC_OLD';")
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)], 'allowed_tools': ['Read', 'Write', 'Edit', 'Bash(npm *)'],
            'build_vm_config': os.environ['JARVIS_TEST_VM_CONFIG'], 'build_scratch_dir': os.environ.get('JARVIS_TEST_BUILD_SCRATCH')})
        installed = policy.run_command('npm ci --offline --no-audit --no-fund', timeout=60)
        self.assertEqual(installed['exit_code'], 0, installed)
        built = policy.run_command('npm test --offline', timeout=60)
        self.assertEqual(built['exit_code'], 0, built)
        self.assertIn('SYNTHETIC_INSTALL_BUILD_OK', built['stdout'])
        self.assertEqual((old / 'index.js').read_text(), "module.exports='SYNTHETIC_OLD';")
        self.assertFalse((old / 'installed.txt').exists(), 'guest install must not mutate host dependencies')
        owned_cache = Path(policy._node_install_cache[2].name)
        self.assertTrue(owned_cache.exists())
        changed = json.loads((self.root / 'package.json').read_text())
        changed['dependencies']['fixture'] = 'file:different-fixture.tgz'
        (self.root / 'package.json').write_text(json.dumps(changed))
        stale = policy.run_command('npm test --offline', timeout=60)
        self.assertNotEqual(stale['exit_code'], 0, 'changed resolution must not reuse this bridge install')
        bridge.Server(policy).close()
        self.assertFalse(owned_cache.exists(), 'closing the bridge must remove its private dependency copy')


    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG'), 'requires private KVM runtime')
    def test_vm_install_rejects_concurrent_manifest_edit_before_sync(self):
        import os
        from unittest.mock import patch
        (self.root / 'package.json').write_text(json.dumps({'name': 'synthetic-race', 'version': '1.0.0'}))
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)], 'allowed_tools': ['Read', 'Write', 'Bash(npm *)'],
            'build_vm_config': os.environ['JARVIS_TEST_VM_CONFIG'], 'build_scratch_dir': os.environ.get('JARVIS_TEST_BUILD_SCRATCH')})
        original_copy = bridge.BuildSnapshot.__init__
        def concurrent_owner_edit(snapshot, *args, **kwargs):
            original_copy(snapshot, *args, **kwargs)
            (self.root / 'package.json').write_text(json.dumps({'name': 'synthetic-owner-edit', 'version': '2.0.0'}))
        with patch.object(bridge.BuildSnapshot, '__init__', concurrent_owner_edit):
            with self.assertRaisesRegex(bridge.Denied, 'manifests changed'):
                policy.run_command('npm install --offline --no-audit --no-fund', timeout=60)
        self.assertEqual(json.loads((self.root / 'package.json').read_text())['name'], 'synthetic-owner-edit')
        self.assertFalse((self.root / 'package-lock.json').exists(), 'stale lock must not reach the checkout before rejection')
        self.assertIsNone(policy._node_install_cache)

    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG'), 'requires a private VM runtime')
    def test_vm_build_dispatch_supports_loopback_dependencies_and_guarded_reconciliation(self):
        import os
        (self.root / 'node_modules/fixture').mkdir(parents=True)
        (self.root / 'node_modules/fixture/index.js').write_text("module.exports='SYNTHETIC_DEPENDENCY';")
        self.policy.write('source.txt', 'before')
        self.policy.write('obsolete.txt', 'remove this source')
        self.policy.write('package.json', json.dumps({'scripts': {'test': 'node job.js'}}))
        self.policy.write('job.js', """const net=require('node:net'),fs=require('node:fs');
const child=require('node:child_process').spawnSync('setsid',['true']);
if(child.status!==0) throw new Error('session failed');
const server=net.createServer(); server.listen(0,'127.0.0.1',()=>{
 console.log(require('fixture')); fs.writeFileSync('source.txt','after');
 fs.writeFileSync('asset.bin',Buffer.from([0,255,7]));
 if(fs.existsSync('obsolete.txt')) fs.unlinkSync('obsolete.txt');server.close();
});""")
        config = {'cwd': str(self.root), 'read_roots': [str(self.root)], 'write_roots': [str(self.root)],
            'allowed_tools': ['Read', 'Write', 'Bash(npm *)'],
            'environment': {'PATH': '/nonexistent-synthetic-host-bin'},
            'build_vm_config': os.environ['JARVIS_TEST_VM_CONFIG'], 'build_scratch_dir': os.environ.get('JARVIS_TEST_BUILD_SCRATCH')}
        outcome = bridge.Policy(config).run_command(f'npm --prefix={self.root} test --offline', timeout=30)
        self.assertEqual(outcome['exit_code'], 0, outcome)
        self.assertIn('SYNTHETIC_DEPENDENCY', outcome['stdout'])
        self.assertEqual(self.policy.read('source.txt'), 'after')
        self.assertEqual((self.root/'asset.bin').read_bytes(),b'\x00\xff\x07')
        self.assertFalse((self.root/'obsolete.txt').exists())
        self.policy.write('source.txt', 'before')
        hook = Path(self.temp.name) / 'deny-write.py'
        hook.write_text("print('{\"decision\":\"block\"}')")
        config['settings'] = {'hooks': {'PreToolUse': [{'matcher': 'Write',
            'hooks': [{'type': 'command', 'command': f'{sys.executable} {hook}'}]}]}}
        with self.assertRaises(bridge.Denied):
            bridge.Policy(config).run_command('npm test --offline', timeout=30)
        self.assertEqual(self.policy.read('source.txt'), 'before')

    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG'), 'requires private KVM runtime')
    def test_vm_npm_workspace_dependencies_are_available_and_readonly(self):
        import os
        package=self.root/'packages/worker space'
        (package/'node_modules/fixture').mkdir(parents=True)
        dependency=package/'node_modules/fixture/index.js'
        dependency.write_text("module.exports='SYNTHETIC_NESTED_DEPENDENCY';")
        (package/'package.json').write_text(json.dumps({'scripts':{'test':'node job.js'}}))
        (package/'job.js').write_text("""const fs=require('node:fs');
console.log(require('fixture'));
let denied=false;
try { fs.writeFileSync('node_modules/fixture/index.js','UNAUTHORIZED'); }
catch(error) { denied=true; }
if(!denied) throw new Error('dependency mount was writable');
fs.writeFileSync('result.txt','SYNTHETIC_WORKSPACE_OK');
""")
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Bash(npm *)'],
            'build_vm_config':os.environ['JARVIS_TEST_VM_CONFIG'],'build_scratch_dir':os.environ.get('JARVIS_TEST_BUILD_SCRATCH')})
        outcome=policy.run_command("npm --prefix 'packages/worker space' test --offline",timeout=30)
        self.assertEqual(outcome['exit_code'],0,outcome)
        self.assertIn('SYNTHETIC_NESTED_DEPENDENCY',outcome['stdout'])
        self.assertEqual((package/'result.txt').read_text(),'SYNTHETIC_WORKSPACE_OK')
        self.assertEqual(dependency.read_text(),"module.exports='SYNTHETIC_NESTED_DEPENDENCY';")

    def linked_dependency_fixture(self):
        main=Path(self.temp.name)/'main'
        main.mkdir()
        def git(*args):
            subprocess.run(['git',*args],cwd=main,check=True,capture_output=True)
        git('init','--initial-branch=main')
        package={'name':'synthetic','version':'1.0.0','dependencies':{'fixture':'1.0.0'},
            'scripts':{'test':'node job.js'}}
        (main/'package.json').write_text(json.dumps(package))
        (main/'package-lock.json').write_text(json.dumps({'lockfileVersion':3,'packages':{'':package}}))
        (main/'job.js').write_text("console.log(require('fixture'));")
        git('add','.')
        git('-c','user.name=Synthetic','-c','user.email=fixture@example.com','commit','-qm','fixture')
        linked=Path(self.temp.name)/'linked'
        git('worktree','add','-b','synthetic-build',str(linked))
        (main/'node_modules/fixture').mkdir(parents=True)
        (main/'node_modules/fixture/index.js').write_text("module.exports='SYNTHETIC_DEPENDENCY';")
        policy=bridge.Policy({'cwd':str(linked),'read_roots':[str(linked)],
            'write_roots':[str(linked)],'allowed_tools':['Read','Write','Bash(npm *)']})
        return main,linked,policy

    def test_linked_worktree_reuses_dependencies_only_with_matching_resolution(self):
        main,linked,policy=self.linked_dependency_fixture()
        self.assertEqual(policy.node_dependency_roots(), [('node_modules',main/'node_modules')])
        # Changing project code or test scripts does not change dependency resolution.
        package=json.loads((linked/'package.json').read_text())
        package['scripts']['test']='node changed-test.js'
        (linked/'package.json').write_text(json.dumps(package))
        self.assertEqual(policy.node_dependency_roots(), [('node_modules',main/'node_modules')])
        package['dependencies']['fixture']='2.0.0'
        (linked/'package.json').write_text(json.dumps(package))
        with self.assertRaisesRegex(bridge.Denied,'dependency'):
            policy.node_dependency_roots()

    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG'), 'requires private KVM runtime')
    def test_vm_build_in_fresh_worktree_uses_readonly_checkout_dependencies(self):
        import os
        main,linked,policy=self.linked_dependency_fixture()
        policy.build_vm_config=os.environ['JARVIS_TEST_VM_CONFIG'];policy.build_runner='vm'
        policy._scratch=bridge.BuildScratch(os.environ.get('JARVIS_TEST_BUILD_SCRATCH'))
        (linked/'job.js').write_text("""const fs=require('node:fs');
if(require('fixture')!=='SYNTHETIC_DEPENDENCY') throw new Error('missing dependency');
let denied=false;
try {fs.writeFileSync('node_modules/fixture/index.js','UNAUTHORIZED');} catch(error) {denied=true;}
if(!denied) throw new Error('writable dependency');
fs.writeFileSync('result.txt','SYNTHETIC_LINKED_BUILD_OK');
""")
        result=policy.run_command('npm test --offline',timeout=30)
        self.assertEqual(result['exit_code'],0,result)
        self.assertEqual((linked/'result.txt').read_text(),'SYNTHETIC_LINKED_BUILD_OK')
        self.assertFalse((linked/'node_modules').exists())
        self.assertEqual((main/'node_modules/fixture/index.js').read_text(),"module.exports='SYNTHETIC_DEPENDENCY';")
        with self.assertRaises(bridge.Denied): policy.read(str(main/'package.json'))

    def test_linked_dependencies_ignore_unrelated_or_unlocked_installs(self):
        main,linked,policy=self.linked_dependency_fixture()
        (main/'unrelated/node_modules').mkdir(parents=True)
        (main/'unrelated/package.json').write_text('{}')
        (main/'unrelated/package-lock.json').write_text('{}')
        for root in (main,linked):
            (root/'sidecar').mkdir()
            (root/'sidecar/package.json').write_text('{"dependencies":{"fixture":"1.0.0"}}')
        (main/'sidecar/node_modules').mkdir()
        (main/'sidecar/package-lock.json').write_text('{}')
        self.assertEqual(policy.node_dependency_roots(), [('node_modules',main/'node_modules')])

    def test_linked_worktree_rejects_changed_lock_and_symlinked_manifest(self):
        main,linked,policy=self.linked_dependency_fixture()
        lock=linked/'package-lock.json'
        original=lock.read_bytes()
        lock.write_text('{}')
        with self.assertRaises(bridge.Denied): policy.node_dependency_roots()
        lock.write_bytes(original)
        manifest=linked/'package.json'
        manifest.unlink();manifest.symlink_to(main/'package.json')
        with self.assertRaises(bridge.Denied): policy.node_dependency_roots()

    def test_dependency_discovery_rejects_links_and_excludes_control_and_build_paths(self):
        for relative in ['node_modules','packages/widget/node_modules','.git/node_modules','target/node_modules']:
            (self.root/relative).mkdir(parents=True)
        found=self.policy.node_dependency_roots()
        self.assertEqual({relative for relative,_ in found},{'node_modules','packages/widget/node_modules'})
        nested=self.root/'packages/widget/node_modules'
        nested.rmdir();nested.symlink_to(self.root/'node_modules')
        with self.assertRaises(bridge.Denied): self.policy.node_dependency_roots()

    def test_build_reconciles_binary_sources_and_deletions(self):
        (self.root/'asset.bin').write_bytes(b'\x00\xffold')
        (self.root/'obsolete.rs').write_text('obsolete')
        snapshot=bridge.BuildSnapshot(self.policy,Path(self.temp.name)/'snapshot')
        (snapshot.root/'asset.bin').write_bytes(b'\x00\xffnew')
        (snapshot.root/'new.bin').write_bytes(b'\x00\xffcreated')
        (snapshot.root/'obsolete.rs').unlink()
        (snapshot.root/'target').mkdir()
        (snapshot.root/'target/output.bin').write_bytes(b'\xffexcluded')
        snapshot.sync()
        self.assertEqual((self.root/'asset.bin').read_bytes(),b'\x00\xffnew')
        self.assertEqual((self.root/'new.bin').read_bytes(),b'\x00\xffcreated')
        self.assertFalse((self.root/'obsolete.rs').exists())
        self.assertFalse((self.root/'target').exists())

    def test_build_deletion_refuses_concurrent_edits_and_symlink_substitution(self):
        (self.root/'source.rs').write_text('original')
        snapshot=bridge.BuildSnapshot(self.policy,Path(self.temp.name)/'snapshot')
        (snapshot.root/'source.rs').unlink()
        (self.root/'source.rs').write_text('concurrent')
        with self.assertRaises(bridge.Denied): snapshot.sync()
        self.assertEqual((self.root/'source.rs').read_text(),'concurrent')
        (self.root/'source.rs').write_text('original')
        (snapshot.root/'source.rs').symlink_to(self.outside)
        with self.assertRaises(bridge.Denied): snapshot.sync()
        self.assertEqual((self.root/'source.rs').read_text(),'original')

    def test_binary_reconciliation_does_not_skip_text_hook_contracts(self):
        import re
        snapshot=bridge.BuildSnapshot(self.policy,Path(self.temp.name)/'snapshot')
        (snapshot.root/'asset.bin').write_bytes(b'\xffnew')
        self.policy.hooks=[(re.compile('Write'),['true'])]
        with self.assertRaisesRegex(bridge.Denied,'hook'):
            snapshot.sync()
        self.assertFalse((self.root/'asset.bin').exists())

    def test_build_reconciliation_preserves_concurrent_source_edits(self):
        (self.root/'source.rs').write_text('original')
        snapshot=bridge.BuildSnapshot(self.policy,Path(self.temp.name)/'snapshot')
        (snapshot.root/'source.rs').write_text('formatter output')
        (self.root/'source.rs').write_text('concurrent editor output')
        with self.assertRaisesRegex(bridge.Denied,'source changed during build'):
            snapshot.sync()
        self.assertEqual((self.root/'source.rs').read_text(),'concurrent editor output')

    @requires_sandbox
    def test_build_background_child_is_stopped_when_parent_exits(self):
        import os
        import time
        fakebin=Path(self.temp.name)/'bin';fakebin.mkdir()
        cargo=fakebin/'cargo'
        cargo.write_text('''#!/usr/bin/python3
import subprocess
child=subprocess.Popen(['/usr/bin/python3','-c','import time; time.sleep(30)'])
print(child.pid, flush=True)
''')
        cargo.chmod(0o700)
        policy=bridge.Policy({'build_runner':'host','cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Bash(cargo *)'],
            'environment':{'PATH':str(fakebin)+':/usr/bin'}})
        outcome=policy.run_command('cargo test')
        self.assertEqual(outcome['exit_code'],0,outcome)
        pid=int(outcome['stdout'].strip())
        deadline=time.monotonic()+2
        while time.monotonic()<deadline:
            try:
                state=Path(f'/proc/{pid}/stat').read_text().split(') ',1)[1].split()[0]
            except (FileNotFoundError, ProcessLookupError):  # exited before or during the read
                return
            if state=='Z':
                return
            time.sleep(0.01)
        self.fail('command descendant survived process-group cleanup')

    def test_build_reconciliation_does_not_follow_created_symlinks(self):
        snapshot=bridge.BuildSnapshot(self.policy,Path(self.temp.name)/'snapshot')
        (snapshot.root/'escape.txt').symlink_to(self.outside)
        snapshot.sync()
        self.assertFalse((self.root/'escape.txt').exists())

    @requires_sandbox
    def test_git_diff_reads_worktree_metadata_without_writing_it(self):
        subprocess.run(['git','init','-q',str(self.root)],check=True)
        (self.root/'tracked.txt').write_text('before\n')
        subprocess.run(['git','-C',str(self.root),'add','tracked.txt'],check=True)
        subprocess.run(['git','-C',str(self.root),'-c','user.name=Synthetic Tester',
                        '-c','user.email=tester@example.com','commit','-qm','fixture'],check=True)
        (self.root/'tracked.txt').write_text('after\n')
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[],'allowed_tools':['Bash(git diff*)','Bash(git status*)']})
        result=policy.run_command('git diff -- tracked.txt')
        self.assertEqual(result['exit_code'],0,result)
        self.assertIn('+after',result['stdout'])
        with self.assertRaises(bridge.Denied):
            policy.run_command('git diff --ext-diff')

    @requires_sandbox
    def test_real_npm_can_run_a_dependency_free_project_test(self):
        import shutil
        if not shutil.which('npm'):
            self.skipTest('npm is not installed')
        (self.root/'package.json').write_text(json.dumps({'name':'synthetic-build','version':'1.0.0',
            'scripts':{'test':"node -e \"require('node:assert').equal(2+2,4); console.log('NPM_FIXTURE_OK')\""}}))
        policy=bridge.Policy({'build_runner':'host','cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Edit','Bash(npm *)']})
        outcome=policy.run_command('npm test --offline',timeout=60)
        self.assertEqual(outcome['exit_code'],0,outcome)
        self.assertIn('NPM_FIXTURE_OK',outcome['stdout'])
        self.assertFalse((self.root/'.npm-cache').exists())

    @requires_sandbox
    def test_npm_uses_existing_dependencies_without_allowing_changes(self):
        import shutil
        if not shutil.which('npm'):
            self.skipTest('npm is not installed')
        module=self.root/'node_modules/synthetic-dependency'
        module.mkdir(parents=True)
        (module/'index.js').write_text('module.exports = 42;\n')
        (self.root/'test.js').write_text('''const assert = require('node:assert');
const fs = require('node:fs');
assert.equal(require('synthetic-dependency'), 42);
assert.throws(() => fs.writeFileSync(require.resolve('synthetic-dependency'), 'bad'),
              error => error.code === 'EACCES' || error.code === 'EPERM');
console.log('DEPENDENCY_FIXTURE_OK');
''')
        (self.root/'package.json').write_text(json.dumps({'name':'synthetic-build','version':'1.0.0',
            'scripts':{'test':'node test.js'}}))
        policy=bridge.Policy({'build_runner':'host','cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Edit','Bash(npm *)']})
        outcome=policy.run_command('npm test --offline',timeout=60)
        self.assertEqual(outcome['exit_code'],0,outcome)
        self.assertIn('DEPENDENCY_FIXTURE_OK',outcome['stdout'])
        self.assertEqual((module/'index.js').read_text(),'module.exports = 42;\n')

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


class HandoffTests(unittest.TestCase):
    def test_operator_reconciliation_records_observed_completion_without_replay(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.execute('mcp__fixture__create', {}, lambda: {'isError': True})
            fingerprint = journal.inspect()[0]['fingerprint']
            result = {'content': [{'type': 'text', 'text': 'synthetic-created'}]}
            journal.reconcile({'index': 0, 'fingerprint': fingerprint, 'outcome': 'completed',
                'evidence': 'Synthetic service lookup confirms the created object.', 'result': result})
            def forbidden():
                self.fail('reconciled effect must not execute again')
            restarted = bridge.HandoffJournal(journal.path)
            self.assertEqual(restarted.execute('mcp__fixture__create', {}, forbidden), result)
            self.assertEqual(restarted.load()['operations'][0]['reconciliation']['outcome'], 'completed')
            with self.assertRaises(bridge.Denied):
                restarted.reconcile({'index': 0, 'fingerprint': fingerprint, 'outcome': 'not_applied',
                    'evidence': 'Stale conflicting decision.'})

    def test_operator_verified_absence_allows_one_new_attempt_and_retains_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.execute('mcp__fixture__create', {}, lambda: {'isError': True})
            journal.reconcile({'index': 0, 'fingerprint': journal.inspect()[0]['fingerprint'],
                'outcome': 'not_applied', 'evidence': 'Synthetic service confirms request was rejected before commit.'})
            result = {'content': [{'type': 'text', 'text': 'synthetic-retry-created'}]}
            count = []
            def apply():
                count.append(1)
                return result
            self.assertEqual(journal.execute('mcp__fixture__create', {}, apply), result)
            self.assertEqual(journal.execute('mcp__fixture__create', {}, apply), result)
            self.assertEqual(count, [1])
            self.assertEqual([row['status'] for row in journal.load()['operations']], ['not_applied', 'completed'])

    def test_reconciliation_rejects_active_request_stale_state_and_missing_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.execute('mcp__fixture__create', {}, lambda: {'isError': True})
            decision = {'index': 0, 'fingerprint': journal.inspect()[0]['fingerprint'],
                'outcome': 'not_applied', 'evidence': 'Synthetic authoritative lookup.'}
            before = journal.path.read_bytes()
            marker = journal.path.with_suffix('.active')
            marker.write_text('synthetic-running')
            with self.assertRaises(bridge.Denied):
                journal.reconcile(decision)
            marker.unlink()
            for invalid in [dict(decision, fingerprint='stale'), dict(decision, evidence=''),
                    dict(decision, index=True), dict(decision, outcome='completed'),
                    dict(decision, outcome='completed', result={'isError': True}),
                    dict(decision, outcome='unknown')]:
                with self.assertRaises(bridge.Denied):
                    journal.reconcile(invalid)
                self.assertEqual(journal.path.read_bytes(), before)

    def test_reconciliation_serializes_with_launcher_and_rejects_untrusted_locks(self):
        import fcntl
        import os
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.execute('mcp__fixture__create', {}, lambda: {'isError': True})
            decision = {'index': 0, 'fingerprint': journal.inspect()[0]['fingerprint'],
                'outcome': 'not_applied', 'evidence': 'Synthetic authoritative lookup.'}
            before = journal.path.read_bytes()
            lock = journal.path.with_suffix('.lifecycle-lock')
            descriptor = os.open(lock, os.O_CREAT | os.O_RDWR, 0o600)
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                with self.assertRaises(bridge.Denied):
                    journal.reconcile(decision)
            finally:
                os.close(descriptor)
            lock.unlink()
            outside = Path(tmp) / 'outside'
            outside.write_text('SYNTHETIC_UNCHANGED')
            lock.symlink_to(outside)
            with self.assertRaises((bridge.Denied, OSError)):
                journal.reconcile(decision)
            self.assertEqual(outside.read_text(), 'SYNTHETIC_UNCHANGED')
            self.assertEqual(journal.path.read_bytes(), before)

    def test_permission_refused_primary_call_does_not_wedge_later_calls(self):
        # Claude Code fires PreToolUse, then PermissionRequest (which carries no
        # tool_use_id) for a call outside --allowedTools, and in -p mode the
        # request is refused with no Post hook. The refused call never ran, so
        # it must not leave a "started" row that blocks every later call.
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            refused = {'tool_name': 'Bash', 'tool_input': {'command': 'synthetic-cli status 1'},
                'tool_use_id': 'synthetic-refused', 'hook_event_name': 'PreToolUse'}
            journal.observe_hook(refused)
            permission = {k: v for k, v in refused.items() if k != 'tool_use_id'}
            journal.observe_hook(dict(permission, hook_event_name='PermissionRequest'))
            later = {'tool_name': 'Bash', 'tool_input': {'command': 'synthetic-cli create'},
                'tool_use_id': 'synthetic-later', 'hook_event_name': 'PreToolUse'}
            journal.observe_hook(later)
            journal.observe_hook(dict(later, hook_event_name='PostToolUse', tool_response='synthetic-created'))
            self.assertEqual([row['status'] for row in journal.load()['operations']], ['refused', 'completed'])
            # A late result for the refused call conflicts with the refusal.
            with self.assertRaises(bridge.Denied):
                journal.observe_hook(dict(refused, hook_event_name='PostToolUse', tool_response='late'))
            # The refused call never ran, so a handoff may run it afresh.
            self.assertEqual(journal.execute('Bash', {'command': 'synthetic-cli status 1'},
                lambda: 'synthetic-ran'), 'synthetic-ran')

    def test_failed_primary_call_unblocks_other_calls_but_not_a_blind_handoff_retry(self):
        # A primary call that exits non-zero (e.g. an HTTP 422 from a service)
        # is seen by the primary, so it is no longer in flight and must not
        # block every later call in the turn. Its effect is still uncertain,
        # so a handoff may not silently re-run that same operation.
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            failed = {'tool_name': 'Bash', 'tool_input': {'command': 'synthetic-cli submit'},
                'tool_use_id': 'synthetic-failed', 'hook_event_name': 'PreToolUse'}
            journal.observe_hook(failed)
            journal.observe_hook(dict(failed, hook_event_name='PostToolUseFailure', error='exit 1'))
            later = {'tool_name': 'Bash', 'tool_input': {'command': 'synthetic-cli other'},
                'tool_use_id': 'synthetic-later', 'hook_event_name': 'PreToolUse'}
            journal.observe_hook(later)
            journal.observe_hook(dict(later, hook_event_name='PostToolUse', tool_response='synthetic-ok'))
            self.assertEqual([row['status'] for row in journal.load()['operations']], ['failed', 'completed'])
            # The identical operation is still never replayed without reconciliation.
            with self.assertRaises(bridge.ReconciliationRequired):
                journal.observe_hook(dict(failed, tool_use_id='synthetic-retry'))
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.observe_hook(dict(failed))
            journal.observe_hook(dict(failed, hook_event_name='PostToolUseFailure', error='exit 1'))
            with self.assertRaises(bridge.ReconciliationRequired):
                journal.execute('Bash', {'command': 'synthetic-cli submit'}, lambda: 'blind-retry')

    def test_permission_request_only_clears_a_started_call_with_identical_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.observe_hook({'tool_name': 'Bash', 'tool_input': {'command': 'synthetic-cli create'},
                'tool_use_id': 'synthetic-running', 'hook_event_name': 'PreToolUse'})
            journal.observe_hook({'tool_name': 'Bash', 'tool_input': {'command': 'synthetic-cli other'},
                'hook_event_name': 'PermissionRequest'})
            self.assertEqual([row['status'] for row in journal.load()['operations']], ['started'])
            with self.assertRaises(bridge.ReconciliationRequired):
                journal.observe_hook({'tool_name': 'Bash', 'tool_input': {'command': 'synthetic-cli next'},
                    'tool_use_id': 'synthetic-next', 'hook_event_name': 'PreToolUse'})

    def test_reconciled_absence_allows_primary_retry_but_blocks_late_old_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            event = {'tool_name': 'mcp__fixture__create', 'tool_input': {},
                'tool_use_id': 'synthetic-old', 'hook_event_name': 'PreToolUse'}
            journal.observe_hook(event)
            journal.reconcile({'index': 0, 'fingerprint': journal.inspect()[0]['fingerprint'],
                'outcome': 'not_applied', 'evidence': 'Synthetic service confirms absence.'})
            before = journal.path.read_bytes()
            with self.assertRaises(bridge.Denied):
                journal.observe_hook(dict(event, hook_event_name='PostToolUse', tool_response='stale'))
            self.assertEqual(journal.path.read_bytes(), before)
            journal.observe_hook(dict(event, tool_use_id='synthetic-new'))
            journal.observe_hook(dict(event, tool_use_id='synthetic-new', hook_event_name='PostToolUse',
                tool_response='synthetic-created'))
            self.assertEqual([row['status'] for row in journal.load()['operations']], ['not_applied', 'completed'])

    def test_reconciliation_cli_keeps_private_payloads_out_of_errors_and_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.execute('mcp__fixture__create', {'body': 'SYNTHETIC_PRIVATE_BODY'}, lambda: {'isError': True})
            command = [sys.executable, str(SPEC.origin)]
            status = subprocess.run(command + ['--handoff-status', str(journal.path)], capture_output=True, text=True)
            self.assertEqual(status.returncode, 0, status.stderr)
            self.assertNotIn('SYNTHETIC_PRIVATE_BODY', status.stdout + status.stderr)
            decision = {'index': 0, 'fingerprint': json.loads(status.stdout)[0]['fingerprint'],
                'outcome': 'not_applied', 'evidence': 'Synthetic authoritative lookup.'}
            outcome = subprocess.run(command + ['--handoff-reconcile', str(journal.path)],
                input=json.dumps(decision), capture_output=True, text=True)
            self.assertEqual(outcome.returncode, 0, outcome.stderr)
            invalid = subprocess.run(command + ['--handoff-reconcile', str(journal.path)],
                input='SYNTHETIC_PRIVATE_BODY', capture_output=True, text=True)
            self.assertNotEqual(invalid.returncode, 0)
            self.assertNotIn('SYNTHETIC_PRIVATE_BODY', invalid.stdout + invalid.stderr)

    def test_memory_reconciliation_reads_stay_fresh_while_writes_are_uncertain(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);workspace=root/'workspace';workspace.mkdir()
            path=root/'handoff.json'
            journal=bridge.HandoffJournal(path)
            with self.assertRaises(ConnectionError):
                journal.execute('mcp__memory__memory_write',{'body':'Synthetic'},
                    lambda: (_ for _ in ()).throw(ConnectionError('uncertain')))
            policy=bridge.Policy({'cwd':str(workspace),'read_roots':[str(workspace)],
                'write_roots':[],'allowed_tools':['mcp__memory__*'],'handoff_path':str(path)})
            server=bridge.Server(policy)
            calls=[]
            def read(name, arguments):
                calls.append(name)
                return {'content':[{'type':'text','text':str(len(calls))}]}
            server.execute=read
            for leaf in ['memory_search','memory_recent','search_conversation_history',
                         'read_conversation_thread','search_messages','conversation_stats']:
                name='mcp__memory__'+leaf
                self.assertNotEqual(server.call(name,{}),server.call(name,{}))
                journal.observe_hook({'hook_event_name':'PreToolUse','tool_use_id':'read-'+leaf,
                    'tool_name':name,'tool_input':{}})
            for leaf in ['memory_write','memory_delete','memory_unknown']:
                with self.assertRaises(bridge.ReconciliationRequired):
                    server.call('mcp__memory__'+leaf,{})
            self.assertEqual(len(calls),12)
            self.assertEqual(len(journal.load()['operations']),1)

    def test_discovery_and_memory_reads_work_through_hook_with_uncertain_write(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = bridge.HandoffJournal(Path(tmp) / 'operations.json')
            journal.save({'version': 1, 'operations': [{'tool': 'mcp__fixture__write',
                'arguments': {}, 'status': 'started'}]})
            original = journal.path.read_bytes()
            for name, arguments in [('ToolSearch', {'query': 'select:mcp__memory__search_conversation_history'}),
                                    ('mcp__memory__search_conversation_history', {'query': 'synthetic'}),
                                    ('mcp__memory__read_conversation_thread', {'thread_id': 'synthetic'})]:
                for phase in ('PreToolUse', 'PostToolUse', 'PostToolUseFailure'):
                    event = {'tool_name': name, 'tool_input': arguments, 'tool_use_id': 'synthetic-read',
                        'hook_event_name': phase, 'tool_response': {'content': []}}
                    result = subprocess.run([sys.executable, '-I', str(SPEC.origin), '--handoff-hook',
                        str(journal.path)], input=json.dumps(event), capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(journal.path.read_bytes(), original)
            for name in ('mcp__fixture__ToolSearch', 'ToolSearchAndWrite', 'Write'):
                with self.assertRaises(bridge.ReconciliationRequired):
                    journal.observe_hook({'tool_name': name, 'tool_input': {},
                        'tool_use_id': 'synthetic-write', 'hook_event_name': 'PreToolUse'})

    def test_uncertain_write_allows_fresh_reconciliation_reads_but_no_more_writes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);workspace=root/'workspace';workspace.mkdir()
            journal=bridge.HandoffJournal(root/'handoff.json')
            def uncertain():
                raise ConnectionError('synthetic ambiguous outcome')
            with self.assertRaises(ConnectionError):
                journal.execute('Bash',{'command':'aa-gh issue create --title Synthetic'},uncertain)
            policy=bridge.Policy({'cwd':str(workspace),'read_roots':[str(workspace)],'write_roots':[],
                'allowed_tools':['Bash(augmentagent gmail *)','Bash(augmentagent repo-docs *)',
                                 'Bash(augmentagent finance *)','Bash(augmentagent calendar *)',
                                 'Bash(augmentagent meetup *)','Bash(augmentagent linkedin *)',
                                 'Bash(aa-gh issue *)','mcp__socialapi__*'],
                'handoff_path':str(root/'handoff.json')})
            server=bridge.Server(policy)
            calls=[]
            def read_result(name, arguments):
                calls.append((name,arguments))
                return {'content':[{'type':'text','text':str(len(calls))}]}
            server.execute=read_result
            commands=['augmentagent gmail search --query Synthetic','aa-gh issue list --search Synthetic',
                      'augmentagent repo-docs list --source synthetic',
                      'augmentagent finance status',
                      'augmentagent finance transactions --start 2026-01-01',
                      'augmentagent finance summary',
                      'augmentagent calendar list-events --days 1',
                      'augmentagent meetup events code-coffee-philly',
                      'augmentagent linkedin recent-dms --limit 3']
            for command in commands:
                first=server.call('Bash',{'command':command})
                second=server.call('Bash',{'command':command})
                self.assertNotEqual(first,second,'reconciliation reads must not return stale receipts')
            server.call('mcp__socialapi__get_post',{'id':'synthetic'})
            for command in ['aa-gh issue create --title Another',
                            'augmentagent gmail compose --body Synthetic',
                            'augmentagent finance connect --alias Synthetic',
                            'augmentagent calendar create-event --summary Synthetic',
                            'augmentagent linkedin dm --with Synthetic',
                            'augmentagent gmail search --query Synthetic; aa-gh issue create --title Another']:
                with self.subTest(command=command), self.assertRaises(bridge.Denied):
                    server.call('Bash',{'command':command})
            self.assertEqual(len(calls),19)
            response=server.dispatch({'method':'tools/call','params':{
                'name':'Bash','arguments':{'command':'aa-gh issue create --title PRIVATE_SYNTHETIC_TITLE'}}})
            self.assertTrue(response['isError'])
            text=response['content'][0]['text']
            self.assertIn('read-only tools',text)
            self.assertIn('uncertain outcome',text)
            self.assertNotIn('PRIVATE_SYNTHETIC_TITLE',text)

    def test_gmail_attachment_download_waits_for_uncertain_mutation_reconciliation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); workspace = root / 'workspace'; workspace.mkdir()
            journal = bridge.HandoffJournal(root / 'handoff.json')
            with self.assertRaises(ConnectionError):
                journal.execute('mcp__fixture__create', {},
                    lambda: (_ for _ in ()).throw(ConnectionError('synthetic uncertain effect')))
            policy = bridge.Policy({'cwd': str(workspace), 'read_roots': [str(workspace)],
                'write_roots': [str(workspace)],
                'allowed_tools': ['Bash(augmentagent gmail get-attachment *)'],
                'handoff_path': str(journal.path)})
            server = bridge.Server(policy)
            executed = []
            server.execute = lambda name, arguments: executed.append(arguments) or {'content': []}
            for suffix in ('', ' --out '+str(workspace / 'attachment.pdf')):
                with self.subTest(suffix=suffix), self.assertRaises(bridge.ReconciliationRequired):
                    server.call('Bash', {'command':
                        'augmentagent gmail get-attachment --message-id synthetic'+suffix})
            self.assertEqual(executed, [])
            self.assertEqual([row['status'] for row in journal.load()['operations']], ['started'])

    def test_primary_read_hooks_do_not_create_uncertain_mutation_receipts(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'handoff.json'
            journal=bridge.HandoffJournal(path)
            for index, command in enumerate([
                    'augmentagent repo-docs sources', 'augmentagent finance status',
                    'augmentagent finance transactions --start 2026-01-01',
                    'augmentagent finance summary', 'augmentagent calendar list-events --days 1',
                    'augmentagent meetup events code-coffee-philly',
                    'augmentagent linkedin recent-dms --limit 3']):
                journal.observe_hook({'hook_event_name':'PreToolUse',
                    'tool_use_id':f'synthetic-read-{index}', 'tool_name':'Bash',
                    'tool_input':{'command':command}})
                self.assertFalse(path.exists(), command)

    def test_claude_hook_records_before_execution_and_codex_reuses_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'handoff.json'
            event={'hook_event_name':'PreToolUse','tool_use_id':'synthetic-call-1',
                'tool_name':'mcp__fixture__create_issue','tool_input':{'title':'Synthetic'}}
            journal=bridge.HandoffJournal(path)
            journal.observe_hook(event)
            state=json.loads(path.read_text())
            self.assertEqual(state['operations'][0]['status'],'started')
            result={'content':[{'type':'text','text':'synthetic-issue-42'}]}
            journal.observe_hook(dict(event,hook_event_name='PostToolUse',tool_response=result))
            def forbidden():
                self.fail('Codex must not repeat completed Claude operation')
            self.assertEqual(bridge.HandoffJournal(path).execute(event['tool_name'],event['tool_input'],forbidden),result)

    def test_claude_failed_tool_remains_uncertain_and_unmatched_result_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal=bridge.HandoffJournal(Path(tmp)/'handoff.json')
            event={'hook_event_name':'PreToolUse','tool_use_id':'synthetic-call-1',
                'tool_name':'mcp__fixture__create_issue','tool_input':{'title':'Synthetic'}}
            with self.assertRaises(bridge.Denied):
                journal.observe_hook(dict(event,hook_event_name='PostToolUse',tool_response='unknown'))
            journal.observe_hook(event)
            journal.observe_hook(dict(event,hook_event_name='PostToolUseFailure',error='synthetic error'))
            with self.assertRaisesRegex(bridge.Denied,'reconciliation'):
                journal.execute(event['tool_name'],event['tool_input'],lambda:None)

    def test_primary_new_tool_id_cannot_repeat_a_completed_external_action(self):
        operations=[('mcp__fixture__create_issue',{'title':'Synthetic'}),
                    ('Bash',{'command':'aa-gh issue create --title Synthetic'}),
                    ('Bash',{'command':'augmentagent gmail compose --body Synthetic'})]
        for name, arguments in operations:
            with self.subTest(tool=name), tempfile.TemporaryDirectory() as tmp:
                journal=bridge.HandoffJournal(Path(tmp)/'handoff.json')
                event={'hook_event_name':'PreToolUse','tool_use_id':'synthetic-first',
                    'tool_name':name,'tool_input':arguments}
                journal.observe_hook(event)
                journal.observe_hook(dict(event,hook_event_name='PostToolUse',tool_response='synthetic-created'))
                before=journal.path.read_bytes()
                retry=dict(event,tool_use_id='synthetic-retry')
                with self.assertRaisesRegex(bridge.Denied,'completed'):
                    journal.observe_hook(retry)
                self.assertEqual(journal.path.read_bytes(),before)
                hook=subprocess.run([sys.executable,'-I',str(Path(bridge.__file__)),
                    '--handoff-hook',str(journal.path)],input=json.dumps(retry),text=True,capture_output=True)
                self.assertEqual(hook.returncode,2)
                self.assertIn('already completed',hook.stderr)
                self.assertNotIn('Synthetic',hook.stderr)
                self.assertEqual(hook.stdout,'')
                # A separately identified user request is not deduplicated with
                # this one merely because the requested action is identical.
                fresh=bridge.HandoffJournal(Path(tmp)/'another-request.json')
                fresh.observe_hook(retry)
                self.assertEqual(len(fresh.load()['operations']),1)

    def test_failed_post_tool_checkpoint_stays_uncertain_and_is_never_replayed(self):
        # #1039 C3, through the shipped --handoff-hook entry point.
        import fcntl
        import os
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'operations.json'
            def hook(event):
                return subprocess.run([sys.executable,'-I',str(Path(bridge.__file__)),'--handoff-hook',str(path)],
                    input=json.dumps(event),text=True,capture_output=True)
            def status():
                return [row['status'] for row in json.loads(path.read_text())['operations']]
            event={'hook_event_name':'PreToolUse','tool_use_id':'synthetic-call-1',
                'tool_name':'mcp__fixture__send','tool_input':{'to':'synthetic'}}
            self.assertEqual(hook(event).returncode,0)
            finished=dict(event,hook_event_name='PostToolUse',
                tool_response={'content':[{'type':'text','text':'synthetic-receipt'}]})
            # The journal lock is held elsewhere.
            with open(str(path)+'.lock','a') as lock:
                fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
                busy=hook(finished)
            self.assertEqual(busy.returncode,2)
            self.assertIn('reconciliation required',busy.stderr)
            self.assertNotIn('synthetic',busy.stderr)
            self.assertEqual(status(),['started'])
            # The completion receipt cannot be written (root ignores directory modes).
            if os.geteuid()!=0:
                os.chmod(tmp,0o500)
                try:
                    unwritable=hook(finished)
                finally:
                    os.chmod(tmp,0o700)
                self.assertEqual(unwritable.returncode,2)
                self.assertEqual(status(),['started'])
            # A failed tool is not evidence that its effect is absent: it is
            # recorded as failed (no longer in flight), never as absent.
            self.assertEqual(hook(dict(event,hook_event_name='PostToolUseFailure',error='synthetic')).returncode,0)
            self.assertEqual(status(),['failed'])
            before=path.read_bytes()
            # Neither provider replays it: Claude's retry is blocked...
            self.assertEqual(hook(dict(event,tool_use_id='synthetic-retry')).returncode,2)
            # ...and Codex's broker refuses before running the effect.
            def forbidden():
                self.fail('an uncertain operation must not be replayed')
            with self.assertRaises(bridge.ReconciliationRequired):
                bridge.HandoffJournal(path).execute(event['tool_name'],event['tool_input'],forbidden)
            self.assertEqual(path.read_bytes(),before)
            self.assertEqual([row['status'] for row in bridge.HandoffJournal(path).inspect()],['failed'])

    def test_primary_can_rerun_a_local_build_after_editing_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal=bridge.HandoffJournal(Path(tmp)/'handoff.json')
            event={'hook_event_name':'PreToolUse','tool_use_id':'synthetic-first',
                'tool_name':'Bash','tool_input':{'command':'cargo test --offline'}}
            journal.observe_hook(event)
            journal.observe_hook(dict(event,hook_event_name='PostToolUse',tool_response='passed'))
            journal.observe_hook(dict(event,tool_use_id='synthetic-after-edit'))
            self.assertEqual(len(journal.load()['operations']),2)

    def test_command_receipt_survives_quoting_spacing_and_timeout_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal=bridge.HandoffJournal(Path(tmp)/'handoff.json')
            original={'command':'aa-gh issue create --title Synthetic','timeout':30}
            equivalent={'command':"aa-gh  issue create --title 'Synthetic'",'timeout':90}
            result={'content':[{'type':'text','text':'synthetic-created'}]}
            journal.execute('Bash',original,lambda:result)
            def forbidden():
                self.fail('equivalent command repeated external action')
            self.assertEqual(journal.execute('Bash',equivalent,forbidden),result)
            with self.assertRaises(bridge.CompletedOperation):
                journal.observe_hook({'hook_event_name':'PreToolUse','tool_use_id':'synthetic-retry',
                    'tool_name':'Bash','tool_input':equivalent})
            changed={'command':'aa-gh issue create --title Different'}
            different={'content':[{'type':'text','text':'synthetic-second'}]}
            self.assertEqual(journal.execute('Bash',changed,lambda:different),different)
            self.assertEqual(len(journal.load()['operations']),2)

    def test_later_uncertain_attempt_cannot_be_hidden_by_an_older_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal=bridge.HandoffJournal(Path(tmp)/'handoff.json')
            event={'hook_event_name':'PreToolUse','tool_use_id':'synthetic-call-1',
                'tool_name':'mcp__fixture__update','tool_input':{'value':'Synthetic'}}
            journal.observe_hook(event)
            journal.observe_hook(dict(event,hook_event_name='PostToolUse',tool_response='first result'))
            # Historical journals may already contain a repeated attempt from
            # before primary hooks blocked identical completed actions.
            state=journal.load()
            state['operations'].append({'tool':event['tool_name'],'arguments':event['tool_input'],
                'primary_id':'synthetic-call-2','status':'started'})
            journal.save(state)
            with self.assertRaisesRegex(bridge.Denied,'reconciliation'):
                journal.execute(event['tool_name'],event['tool_input'],lambda:None)

    def test_server_reuses_external_receipt_but_refreshes_local_reads(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);workspace=root/'workspace';workspace.mkdir()
            config={'cwd':str(workspace),'read_roots':[str(workspace)],'write_roots':[],
                'allowed_tools':['Read','mcp__fixture__create_issue'],
                'handoff_path':str(root/'handoff.json')}
            calls=[]
            class Remote:
                def request(self, method, arguments):
                    calls.append(arguments)
                    return {'content':[{'type':'text','text':'synthetic-issue-42'}]}
            def server():
                instance=bridge.Server(bridge.Policy(config))
                instance.discovered=True
                instance.remotes={'fixture':Remote()}
                instance.remote_tools={'mcp__fixture__create_issue':('fixture','create_issue',{})}
                return instance
            first=server().call('mcp__fixture__create_issue',{'title':'Synthetic'})
            self.assertEqual(server().call('mcp__fixture__create_issue',{'title':'Synthetic'}),first)
            self.assertEqual(len(calls),1)
            (workspace/'note.txt').write_text('before')
            instance=server()
            self.assertEqual(instance.call('Read',{'file_path':'note.txt'}),'before')
            (workspace/'note.txt').write_text('after')
            self.assertEqual(instance.call('Read',{'file_path':'note.txt'}),'after')

    def test_handoff_state_cannot_be_in_model_read_scope(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(bridge.Denied,'outside model tool scopes'):
                bridge.Policy({'cwd':tmp,'read_roots':[tmp],'allowed_tools':['Read'],
                    'handoff_path':str(Path(tmp)/'handoff.json')})

    def test_reported_tool_error_leaves_uncertain_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal=bridge.HandoffJournal(Path(tmp)/'handoff.json')
            failure={'isError':True,'content':[{'type':'text','text':'synthetic error'}]}
            self.assertEqual(journal.execute('mcp__fixture__create',{},lambda:failure),failure)
            with self.assertRaisesRegex(bridge.Denied,'reconciliation'):
                journal.execute('mcp__fixture__create',{},lambda:None)

    def test_completed_external_operation_is_replayed_after_restart(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'handoff.json'
            journal=bridge.HandoffJournal(path)
            calls=[]
            arguments={'title':'Synthetic issue','body':'Synthetic scope'}
            def create():
                calls.append('create')
                return {'content':[{'type':'text','text':'synthetic-issue-42'}]}
            expected=journal.execute('mcp__fixture__create_issue',arguments,create)
            restarted=bridge.HandoffJournal(path)
            actual=restarted.execute('mcp__fixture__create_issue',dict(reversed(list(arguments.items()))),create)
            self.assertEqual(actual,expected)
            self.assertEqual(calls,['create'])
            self.assertEqual(path.stat().st_mode & 0o777,0o600)

    def test_uncertain_external_result_blocks_replay_and_new_mutations(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'handoff.json'
            journal=bridge.HandoffJournal(path)
            calls=[]
            def disconnected():
                calls.append('external effect')
                raise ConnectionError('synthetic connection dropped after accepting operation')
            with self.assertRaises(ConnectionError):
                journal.execute('mcp__fixture__create_issue',{'title':'Synthetic'},disconnected)
            restarted=bridge.HandoffJournal(path)
            for args in ({'title':'Synthetic'},{'title':'Another synthetic issue'}):
                with self.assertRaisesRegex(bridge.Denied,'reconciliation'):
                    restarted.execute('mcp__fixture__create_issue',args,disconnected)
            self.assertEqual(calls,['external effect'])

    def test_corrupt_or_symlink_journal_never_executes_operation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);path=root/'handoff.json'
            path.write_text('not valid json');path.chmod(0o600)
            def forbidden():
                self.fail('operation must not run with untrusted handoff state')
            with self.assertRaises(bridge.Denied):
                bridge.HandoffJournal(path).execute('Write',{'file_path':'note'},forbidden)
            path.unlink();path.symlink_to(root/'missing')
            with self.assertRaises(bridge.Denied):
                bridge.HandoffJournal(path).execute('Write',{'file_path':'note'},forbidden)


class RemoteToolsTests(unittest.TestCase):
    def test_stdio_proxy_preserves_results_and_does_not_expose_other_tools(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake = root / 'fake.py'
            fake.write_text("""import json,sys
for line in sys.stdin:
 v=json.loads(line)
 if 'id' not in v: continue
 m=v['method']
 if m=='initialize': r={'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
 elif m=='tools/list': r={'tools':[{'name':name,'description':'synthetic tool','inputSchema':{'type':'object','properties':{}}} for name in ['search','delete']]}
 elif m=='tools/call': r={'content':[{'type':'text','text':'REMOTE_FIXTURE_OK'}]}
 else: r={}
 print(json.dumps({'jsonrpc':'2.0','id':v['id'],'result':r}),flush=True)
""")
            policy = bridge.Policy({'cwd': tmp, 'allowed_tools': ['mcp__fixture__search'],
                'settings': {'mcpServers': {'fixture': {'command': sys.executable,
                                                   'args': ['-I', str(fake)]}}}})
            server = bridge.Server(policy)
            self.addCleanup(server.close)
            self.assertEqual([t['name'] for t in server.tools()], ['mcp__fixture__search'])
            result = server.dispatch({'method':'tools/call', 'params':{
                'name':'mcp__fixture__search','arguments':{}}})
            self.assertEqual(result['content'][0]['text'], 'REMOTE_FIXTURE_OK')
            denied = server.dispatch({'method':'tools/call', 'params':{
                'name':'mcp__fixture__delete','arguments':{}}})
            self.assertTrue(denied['isError'])
            server.close()
            self.assertTrue(all(remote.process.poll() is not None for remote in server.remotes.values()))

    def test_hung_stdio_initialization_times_out_and_reaps_child(self):
        import time
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            fake=root/'hang.py'
            fake.write_text("import time\ntime.sleep(60)\n")
            policy=bridge.Policy({'cwd':tmp,'allowed_tools':['mcp__fixture__search'],
                'settings':{'mcpServers':{'fixture':{'command':sys.executable,
                    'args':['-I',str(fake)],'timeout':0.1}}}})
            server=bridge.Server(policy);self.addCleanup(server.close)
            start=time.monotonic()
            with self.assertRaisesRegex(bridge.Readiness, 'JARVIS_READINESS:mcp_timeout'): server.tools()
            self.assertLess(time.monotonic()-start,3)

    def test_missing_declared_remote_tool_fails_readiness(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            fake=root/'empty.py'
            fake.write_text("""import json,sys
for line in sys.stdin:
 v=json.loads(line)
 if 'id' not in v: continue
 r={'protocolVersion':'2024-11-05'} if v['method']=='initialize' else {'tools':[]}
 print(json.dumps({'jsonrpc':'2.0','id':v['id'],'result':r}),flush=True)
""")
            policy=bridge.Policy({'cwd':tmp,'allowed_tools':['mcp__fixture__missing'],
                'settings':{'mcpServers':{'fixture':{'command':sys.executable,'args':['-I',str(fake)]}}}})
            server=bridge.Server(policy);self.addCleanup(server.close)
            with self.assertRaisesRegex(bridge.Readiness, 'JARVIS_READINESS:mcp_tools'):
                server.dispatch({'method':'initialize','params':{}})

    def test_missing_mcp_binary_reports_sanitized_readiness_not_unsupported_method(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);config=root/'policy.json'
            config.write_text(json.dumps({'cwd':str(root),'allowed_tools':['mcp__fixture__search'],
                'settings':{'mcpServers':{'fixture':{'command':str(root/'PRIVATE_SYNTHETIC_BINARY'),
                    'env':{'SYNTHETIC_SECRET':'PRIVATE_SYNTHETIC_TOKEN'}}}}}));config.chmod(0o600)
            request=json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize'})+'\n'
            result=subprocess.run([sys.executable,str(SPEC.origin),str(config)],input=request,
                text=True,capture_output=True,timeout=5)
            self.assertEqual(result.returncode,0,result.stderr)
            response=json.loads(result.stdout)
            self.assertEqual(response['error']['code'],-32001)
            self.assertIn('JARVIS_READINESS:mcp_start',response['error']['message'])
            self.assertNotIn('PRIVATE_SYNTHETIC',result.stdout+result.stderr)

    def test_http_proxy_carries_session_and_auth_without_replaying_requests(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import threading
        received = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_POST(self):
                msg = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                received.append((msg['method'], self.headers.get('Authorization'),
                                 self.headers.get('Mcp-Session-Id')))
                if 'id' not in msg:
                    self.send_response(202); self.end_headers(); return
                result = {'content':[{'type':'text','text':'HTTP_FIXTURE_OK'}]}
                if msg['method']=='initialize':
                    result = {'protocolVersion':'2025-03-26','capabilities':{'tools':{}},
                              'serverInfo':{'name':'fixture','version':'1'}}
                elif msg['method']=='tools/list':
                    result = {'tools':[{'name':'lookup','inputSchema':{'type':'object','properties':{}}}]}
                data=json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}).encode()
                self.send_response(200)
                self.send_header('Content-Type','application/json')
                self.send_header('Mcp-Session-Id','synthetic-session')
                self.send_header('Content-Length',str(len(data))); self.end_headers()
                self.wfile.write(data)
        httpd=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=httpd.serve_forever,daemon=True);thread.start()
        self.addCleanup(httpd.server_close);self.addCleanup(httpd.shutdown)
        with tempfile.TemporaryDirectory() as tmp:
            policy=bridge.Policy({'cwd':tmp,'allowed_tools':['mcp__fixture__lookup'],
                'environment':{'SYNTHETIC_API_KEY':'synthetic-token'},
                'settings':{'mcpServers':{'fixture':{
                    'type':'http','url':f'http://127.0.0.1:{httpd.server_port}/mcp',
                    'headers':{'Authorization':'Bearer ${SYNTHETIC_API_KEY}'}}}}})
            server=bridge.Server(policy);self.addCleanup(server.close)
            server.tools()
            result=server.dispatch({'method':'tools/call','params':{
                'name':'mcp__fixture__lookup','arguments':{}}})
            self.assertEqual(result['content'][0]['text'],'HTTP_FIXTURE_OK')
        self.assertEqual(sum(method=='tools/call' for method,_,_ in received),1)
        self.assertTrue(all(auth=='Bearer synthetic-token' for _,auth,_ in received))
        self.assertTrue(all(session=='synthetic-session' for method,_,session in received if method!='initialize'))

    def test_http_timeout_is_diagnosed_without_retry_or_clearing_uncertain_effect(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import threading
        for phase in ['initialize','tools/call']:
            received=[]
            release=threading.Event()
            class Handler(BaseHTTPRequestHandler):
                def log_message(self,*args): pass
                def do_POST(self):
                    request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                    method=request['method'];received.append(method)
                    if method==phase:
                        release.wait(2);return
                    if 'id' not in request:
                        self.send_response(202);self.end_headers();return
                    result=({'protocolVersion':'2025-03-26','capabilities':{'tools':{}},
                             'serverInfo':{'name':'fixture','version':'1'}} if method=='initialize'
                            else {'tools':[{'name':'create','inputSchema':{'type':'object','properties':{}}}]})
                    data=json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}).encode()
                    self.send_response(200);self.send_header('Content-Type','application/json')
                    self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
            httpd=ThreadingHTTPServer(('127.0.0.1',0),Handler)
            threading.Thread(target=httpd.serve_forever,daemon=True).start()
            try:
                with self.subTest(phase=phase), tempfile.TemporaryDirectory() as tmp:
                    root=Path(tmp);workspace=root/'workspace';workspace.mkdir()
                    journal=root/'handoff.json'
                    policy=bridge.Policy({'cwd':str(workspace),'allowed_tools':['mcp__fixture__create'],
                        'handoff_path':str(journal),
                        'settings':{'mcpServers':{'fixture':{'type':'http','timeout':0.05,
                            'url':f'http://127.0.0.1:{httpd.server_port}/mcp'}}}})
                    server=bridge.Server(policy)
                    try:
                        with self.assertRaisesRegex(bridge.Readiness,'JARVIS_READINESS:mcp_timeout'):
                            server.tools()
                            if phase=='tools/call':
                                server.call('mcp__fixture__create',{})
                    finally:
                        server.close()
                    self.assertEqual(received.count(phase),1)
                    if phase=='tools/call':
                        state=bridge.HandoffJournal(journal).load()
                        self.assertEqual(state['operations'][0]['status'],'started')
                    else:
                        self.assertFalse(journal.exists())
            finally:
                release.set();httpd.shutdown();httpd.server_close()


def make_deep_tree(parent, depth, leaf='deep.txt'):
    """Build a directory chain deeper than PATH_MAX-relative helpers allow."""
    import os
    descriptor = os.open(parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for _ in range(depth):
            os.mkdir('d', dir_fd=descriptor)
            child = os.open('d', os.O_RDONLY | os.O_DIRECTORY, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        handle = os.open(leaf, os.O_WRONLY | os.O_CREAT, 0o600, dir_fd=descriptor)
        os.write(handle, b'SYNTHETIC_DEEP needle\n')
        os.close(handle)
    finally:
        os.close(descriptor)


def remove_tree_iteratively(path):
    # shutil.rmtree recurses per directory level on Python 3.12.
    import os
    if not os.path.lexists(path):
        return
    for base, directories, files in os.walk(path, topdown=False):
        for name in files:
            os.unlink(os.path.join(base, name))
        for name in directories:
            target = os.path.join(base, name)
            if os.path.islink(target):
                os.unlink(target)
            else:
                os.rmdir(target)
    os.rmdir(path)


class BridgeResilienceTests(unittest.TestCase):
    """#1042: no model or client input may terminate the bridge process."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'workspace'
        self.root.mkdir()
        # Runs before the temporary directory cleanup (cleanups are LIFO).
        self.addCleanup(remove_tree_iteratively, str(self.root / 'd'))
        self.config = {'cwd': str(self.root), 'read_roots': [str(self.root)],
                       'write_roots': [str(self.root)],
                       'allowed_tools': ['Read', 'Write', 'Edit', 'Glob', 'Grep']}
        self.policy = bridge.Policy(self.config)

    def run_bridge(self, lines):
        config = Path(self.temp.name) / 'policy.json'
        config.write_text(json.dumps(self.config))
        config.chmod(0o600)
        run = subprocess.run([sys.executable, '-I', str(SPEC.origin), str(config)],
                             input=b''.join(lines), capture_output=True, timeout=60)
        return run, [json.loads(line) for line in run.stdout.splitlines()]

    @staticmethod
    def line(message):
        return json.dumps(message).encode() + b'\n'

    def test_deep_path_write_is_denied_without_creating_a_junk_tree(self):
        with self.assertRaisesRegex(bridge.Denied, 'depth'):
            self.policy.write('d/' * 1500 + 'f', 'SYNTHETIC')
        self.assertFalse((self.root / 'd').exists())

    def test_path_depth_and_length_caps_apply_to_reads_and_writes(self):
        limit = bridge.MAX_PATH_DEPTH
        self.policy.write('w/' * (limit - 1) + 'f', 'SYNTHETIC_AT_LIMIT')
        self.assertEqual(self.policy.read('w/' * (limit - 1) + 'f'), 'SYNTHETIC_AT_LIMIT')
        with self.assertRaisesRegex(bridge.Denied, 'depth'):
            self.policy.write('w/' * limit + 'f', 'SYNTHETIC_TOO_DEEP')
        self.assertFalse((self.root.joinpath(*['w'] * limit)).exists())
        make_deep_tree(self.root, limit + 4)
        with self.assertRaisesRegex(bridge.Denied, 'depth'):
            self.policy.read('d/' * (limit + 4) + 'deep.txt')
        long_name = '/'.join(['n' * 250] * 20) + '/f'
        for action in (lambda: self.policy.write(long_name, 'SYNTHETIC'),
                       lambda: self.policy.read(long_name)):
            with self.assertRaisesRegex(bridge.Denied, 'length'):
                action()
        self.assertFalse((self.root / ('n' * 250)).exists())

    def test_glob_and_grep_over_an_existing_deep_tree_do_not_crash_dispatch(self):
        (self.root / 'shallow.txt').write_text('SYNTHETIC_SHALLOW needle\n')
        make_deep_tree(self.root, 1500)
        server = bridge.Server(self.policy)
        glob = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Glob', 'arguments': {'pattern': '**/*.txt'}}})
        self.assertFalse(glob.get('isError', False), glob)
        self.assertEqual(json.loads(glob['content'][0]['text']), ['shallow.txt'])
        grep = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Grep', 'arguments': {'pattern': 'needle'}}})
        self.assertFalse(grep.get('isError', False), grep)
        self.assertEqual([hit['path'] for hit in json.loads(grep['content'][0]['text'])], ['shallow.txt'])

    def test_recursion_and_memory_errors_in_a_tool_become_tool_errors(self):
        server = bridge.Server(self.policy)
        for error in (RecursionError, MemoryError):
            def explode(*args, **kwargs):
                raise error('synthetic')
            self.policy.glob = explode
            with self.subTest(error=error.__name__):
                response = server.dispatch({'method': 'tools/call', 'params': {
                    'name': 'Glob', 'arguments': {'pattern': '*'}}})
                self.assertTrue(response['isError'])

    def test_unexpected_failure_is_an_internal_error_not_a_crash(self):
        server = bridge.Server(self.policy)
        def explode(request):
            raise LookupError('SYNTHETIC_PRIVATE_DETAIL')
        server.dispatch = explode
        response = json.loads(bridge.safe_dispatch(server, self.line(
            {'jsonrpc': '2.0', 'id': 3, 'method': 'ping'})))
        self.assertEqual(response['id'], 3)
        self.assertEqual(response['error']['code'], -32603)
        self.assertNotIn('SYNTHETIC_PRIVATE_DETAIL', json.dumps(response))

    def test_stdio_bridge_survives_deep_tree_and_deep_write(self):
        make_deep_tree(self.root, 1500)
        run, replies = self.run_bridge([
            self.line({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
                       'params': {'name': 'Glob', 'arguments': {'pattern': '*'}}}),
            self.line({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call',
                       'params': {'name': 'Write', 'arguments': {'file_path': 'e/' * 1500 + 'f', 'content': 'x'}}}),
            self.line({'jsonrpc': '2.0', 'id': 3, 'method': 'ping'}),
        ])
        self.assertEqual(run.returncode, 0, run.stderr[-2000:])
        self.assertEqual([reply['id'] for reply in replies], [1, 2, 3])
        self.assertIn('result', replies[0])
        self.assertTrue(replies[1]['result']['isError'])
        self.assertEqual(replies[2]['result'], {})
        self.assertFalse((self.root / 'e').exists())

    def test_malformed_line_is_answered_and_next_request_is_served(self):
        run, replies = self.run_bridge([
            b'{"jsonrpc": "2.0", "id": 1, "method": \n',
            b'\xff\xfe not utf-8\n',
            b'[[[[[[[[[[' * 20000 + b'\n',
            self.line({'jsonrpc': '2.0', 'id': 2, 'method': 'ping'}),
        ])
        self.assertEqual(run.returncode, 0, run.stderr[-2000:])
        self.assertEqual(len(replies), 4, replies)
        for reply in replies[:3]:
            self.assertEqual(reply, {'jsonrpc': '2.0', 'id': None,
                                     'error': {'code': -32700, 'message': 'Parse error'}})
        self.assertEqual(replies[3], {'jsonrpc': '2.0', 'id': 2, 'result': {}})

    def test_non_object_and_malformed_requests_get_invalid_request(self):
        run, replies = self.run_bridge([
            b'[1, 2]\n', b'5\n', b'"ping"\n', b'null\n',
            self.line({'jsonrpc': '2.0', 'id': 4, 'method': 5}),
            self.line({'jsonrpc': '1.0', 'id': 5, 'method': 'ping'}),
            self.line({'jsonrpc': '2.0', 'id': {'nested': 1}, 'method': 'ping'}),
            self.line({'jsonrpc': '2.0', 'id': True, 'method': 'ping'}),
            # Notifications and client responses are never answered.
            self.line({'jsonrpc': '2.0', 'method': 'tools/call', 'params': {'name': 5}}),
            self.line({'jsonrpc': '2.0', 'id': 6, 'result': {}}),
            self.line({'jsonrpc': '2.0', 'id': 7, 'method': 'ping'}),
        ])
        self.assertEqual(run.returncode, 0, run.stderr[-2000:])
        invalid = {'code': -32600, 'message': 'Invalid Request'}
        self.assertEqual(replies, [
            {'jsonrpc': '2.0', 'id': None, 'error': invalid},
            {'jsonrpc': '2.0', 'id': None, 'error': invalid},
            {'jsonrpc': '2.0', 'id': None, 'error': invalid},
            {'jsonrpc': '2.0', 'id': None, 'error': invalid},
            {'jsonrpc': '2.0', 'id': 4, 'error': invalid},
            {'jsonrpc': '2.0', 'id': 5, 'error': invalid},
            {'jsonrpc': '2.0', 'id': None, 'error': invalid},
            {'jsonrpc': '2.0', 'id': None, 'error': invalid},
            {'jsonrpc': '2.0', 'id': 7, 'result': {}},
        ])

    def test_invalid_tool_call_params_get_invalid_params(self):
        calls = [{'name': 5}, 'Read', {'arguments': {}}, {'name': 'Read', 'arguments': 'note.txt'},
                 {'name': 'Read', 'arguments': None}]
        messages = [self.line({'jsonrpc': '2.0', 'id': index, 'method': 'tools/call', 'params': params})
                    for index, params in enumerate(calls)]
        messages.append(self.line({'jsonrpc': '2.0', 'id': 'after', 'method': 'tools/call'}))
        messages.append(self.line({'jsonrpc': '2.0', 'id': 'served', 'method': 'ping'}))
        run, replies = self.run_bridge(messages)
        self.assertEqual(run.returncode, 0, run.stderr[-2000:])
        self.assertEqual([reply['id'] for reply in replies], [0, 1, 2, 3, 4, 'after', 'served'])
        for reply in replies[:6]:
            self.assertEqual(reply['error'], {'code': -32602, 'message': 'Invalid params'}, reply)
        self.assertEqual(replies[6]['result'], {})

    def test_oversized_request_lines_are_discarded_and_reading_continues(self):
        import io
        request = b'{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"arguments":{"id":7,"content":"'
        stream = io.BytesIO(request + b'x' * 50 + b'"}}}\n' + b'{"ok": 1}\n' + b'{"id":9,' + b'y' * 60)
        lines = list(bridge.read_request_lines(stream, 64))
        self.assertEqual(len(lines), 3)
        self.assertIsInstance(lines[0], bridge.OversizedRequest)
        self.assertEqual(lines[1], b'{"ok": 1}\n')
        self.assertIsInstance(lines[2], bridge.OversizedRequest)
        # Only an id at the head of a compact request is trusted; never a nested one.
        self.assertEqual(json.loads(bridge.oversized_response(lines[0])),
                         {'jsonrpc': '2.0', 'id': 42, 'error': {'code': -32600, 'message': 'Request exceeds size limit'}})
        self.assertIsNone(json.loads(bridge.oversized_response(lines[2]))['id'])

    def test_oversized_line_with_a_non_json_leading_id_does_not_kill_the_bridge(self):
        # Review repro: leading-zero ids matched the recovery regex, then
        # json.loads(b'01') raised outside the protective try.
        filler = b'x' * (bridge.MAX_REQUEST_BYTES + 1024)
        for head in (b'{"jsonrpc":"2.0","id":01,', b'{"jsonrpc":"2.0","id":-0123,'):
            with self.subTest(head=head):
                run, replies = self.run_bridge([
                    head + b'"method":"ping","params":{"pad":"' + filler + b'"}}\n',
                    self.line({'jsonrpc': '2.0', 'id': 2, 'method': 'ping'}),
                ])
                self.assertEqual(run.returncode, 0, run.stderr[-2000:])
                self.assertEqual(replies, [
                    {'jsonrpc': '2.0', 'id': None, 'error': {'code': -32600, 'message': 'Request exceeds size limit'}},
                    {'jsonrpc': '2.0', 'id': 2, 'result': {}},
                ])

    def test_request_line_cap_fits_a_maximal_write_but_nothing_much_larger(self):
        # The admitted bound: a Write of MAX_FILE_BYTES of text needing two bytes
        # of escaping per byte ('"' and '\\' each become two bytes).
        self.assertLessEqual(bridge.MAX_REQUEST_BYTES, 24 * 1024 * 1024)
        content = '"\\' * (bridge.MAX_FILE_BYTES // 2)
        request = self.line({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {
            'name': 'Write', 'arguments': {'file_path': 'maximal.txt', 'content': content}}})
        self.assertGreater(len(request), 2 * bridge.MAX_FILE_BYTES)
        self.assertLess(len(request), bridge.MAX_REQUEST_BYTES)
        run, replies = self.run_bridge([request, self.line({'jsonrpc': '2.0', 'id': 2, 'method': 'ping'})])
        self.assertEqual(run.returncode, 0, run.stderr[-2000:])
        self.assertEqual(replies[0]['id'], 1)
        self.assertFalse(replies[0]['result'].get('isError', False), replies[0])
        self.assertEqual((self.root / 'maximal.txt').read_bytes(), content.encode())
        self.assertEqual(replies[1], {'jsonrpc': '2.0', 'id': 2, 'result': {}})

    def test_write_of_dense_control_characters_can_exceed_the_cap_and_is_refused_cleanly(self):
        # JSON escapes each such character as six bytes (), so 4 MiB of
        # them already exceed the 24 MiB line cap: refused before parsing.
        content = '\x01' * (4 * 1024 * 1024)
        request = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {
            'name': 'Write', 'arguments': {'file_path': 'control.txt', 'content': content}}},
            separators=(',', ':')).encode() + b'\n'
        self.assertGreater(len(request), bridge.MAX_REQUEST_BYTES)
        run, replies = self.run_bridge([request, self.line({'jsonrpc': '2.0', 'id': 2, 'method': 'ping'})])
        self.assertEqual(run.returncode, 0, run.stderr[-2000:])
        self.assertEqual(replies, [
            {'jsonrpc': '2.0', 'id': 1, 'error': {'code': -32600, 'message': 'Request exceeds size limit'}},
            {'jsonrpc': '2.0', 'id': 2, 'result': {}},
        ])
        self.assertFalse((self.root / 'control.txt').exists())
        self.assertEqual([path.name for path in self.root.iterdir()], [])

    def test_oversized_id_recovery_accepts_only_json_integers_and_cannot_raise_out(self):
        for head, expected in [(b'{"jsonrpc":"2.0","id":0,', 0), (b'{"jsonrpc":"2.0","id":-7,', -7),
                               (b'{"jsonrpc":"2.0","id":120,', 120), (b'{"jsonrpc":"2.0","id":"a-1",', 'a-1'),
                               (b'{"jsonrpc":"2.0","id":01,', None), (b'{"jsonrpc":"2.0","id":-0123,', None),
                               (b'{"jsonrpc":"2.0","id":00}', None)]:
            with self.subTest(head=head):
                response = json.loads(bridge.oversized_response(bridge.OversizedRequest(head)))
                self.assertEqual(response['id'], expected)
        from unittest.mock import patch
        def broken(request):
            raise ValueError('SYNTHETIC_RECOVERY_BUG')
        with patch.object(bridge, 'oversized_response', broken):
            response = json.loads(bridge.safe_dispatch(bridge.Server(self.policy),
                                                       bridge.OversizedRequest(b'{"jsonrpc":"2.0","id":5,')))
        self.assertEqual(response['error']['code'], -32603)
        self.assertNotIn('SYNTHETIC_RECOVERY_BUG', json.dumps(response))


GREP_PARITY_TREE = {
    'data/numbers.csv': 'id,value\n1,100\n22,2000\n333,30000\n',
    'notes/alpha.md': 'Synthetic Alpha heading\nalpha beta gamma\nTODO: write synthetic tests\n',
    'notes/beta.txt': 'beta\nBETA upper\n  indented beta\n',
    'src/lib.rs': 'pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn sub(a: i32, b: i32) -> i32 { a - b }\n',
    'src/main.rs': 'fn main() {\n    println!("synthetic");\n}\n// TODO(owner): synthetic\n',
}
# Fixed expectations in the regex subset shared by Python re and ripgrep.
GREP_PARITY_TABLE = [
    ('beta', False, [('notes/alpha.md', 2), ('notes/beta.txt', 1), ('notes/beta.txt', 3)]),
    ('beta', True, [('notes/alpha.md', 2), ('notes/beta.txt', 1), ('notes/beta.txt', 2), ('notes/beta.txt', 3)]),
    ('^TODO', False, [('notes/alpha.md', 3)]),
    ('TODO', False, [('notes/alpha.md', 3), ('src/main.rs', 4)]),
    (r'\bfn\s+\w+\(', False, [('src/lib.rs', 1), ('src/lib.rs', 2), ('src/main.rs', 1)]),
    (r'[0-9]{3,}$', False, [('data/numbers.csv', 2), ('data/numbers.csv', 3), ('data/numbers.csv', 4)]),
    ('alpha|gamma', False, [('notes/alpha.md', 2)]),
    ('synthetic', True, [('notes/alpha.md', 1), ('notes/alpha.md', 3), ('src/main.rs', 2), ('src/main.rs', 4)]),
    (r'a \+ b|a - b', False, [('src/lib.rs', 1), ('src/lib.rs', 2)]),
    (r'^\s+\S', False, [('notes/beta.txt', 3), ('src/main.rs', 2)]),
    ('^beta$', False, [('notes/beta.txt', 1)]),
    (r'\d+,\d{4}', False, [('data/numbers.csv', 3), ('data/numbers.csv', 4)]),
]


class BoundedGrepTests(unittest.TestCase):
    """#1038: a model-supplied pattern cannot stall the bridge or its CLI slot."""
    PATHOLOGICAL = '(a+)+$'

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'workspace'
        self.root.mkdir()
        # 40 characters of backtracking bait: exponential for Python's re.
        (self.root / 'bait.txt').write_text('a' * 40 + 'b\n')
        self.config = Path(self.temp.name) / 'policy.json'
        self.config.write_text(json.dumps({'cwd': str(self.root), 'read_roots': [str(self.root)],
                                           'write_roots': [], 'allowed_tools': ['Read', 'Grep']}))
        self.config.chmod(0o600)

    def write_tree(self):
        for relative, content in GREP_PARITY_TREE.items():
            (self.root / relative).parent.mkdir(parents=True, exist_ok=True)
            (self.root / relative).write_text(content)
        (self.root / 'bait.txt').unlink()

    def start_bridge(self):
        import os
        import signal
        process = subprocess.Popen([sys.executable, '-I', str(SPEC.origin), str(self.config)],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        def stop():
            try:
                os.kill(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            process.stdin.close()
            process.stdout.close()
        self.addCleanup(stop)
        self.pending = b''
        return process

    def send(self, process, message):
        process.stdin.write(json.dumps(message).encode() + b'\n')
        process.stdin.flush()

    def receive(self, process, seconds):
        import os
        import select
        import time
        deadline = time.monotonic() + seconds
        while b'\n' not in self.pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                return None
            chunk = os.read(process.stdout.fileno(), 65536)
            if not chunk:
                return None
            self.pending += chunk
        line, self.pending = self.pending.split(b'\n', 1)
        return json.loads(line)

    def test_pathological_pattern_returns_within_two_seconds_and_bridge_keeps_serving(self):
        import time
        process = self.start_bridge()
        self.send(process, {'jsonrpc': '2.0', 'id': 1, 'method': 'ping'})
        self.assertEqual(self.receive(process, 10)['id'], 1)
        started = time.monotonic()
        self.send(process, {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {
            'name': 'Grep', 'arguments': {'pattern': self.PATHOLOGICAL}}})
        self.send(process, {'jsonrpc': '2.0', 'id': 3, 'method': 'ping'})
        reply = self.receive(process, 5)
        elapsed = time.monotonic() - started
        self.assertIsNotNone(reply, 'bridge stalled on a pathological Grep pattern')
        self.assertLess(elapsed, 2.0)
        self.assertEqual(reply['id'], 2)
        self.assertTrue(reply['result']['isError'])
        text = reply['result']['content'][0]['text']
        # Matching ran out, so the advice is about the pattern, not the path.
        self.assertIn('time limit', text)
        self.assertIn('simplify the pattern', text)
        self.assertNotIn('narrow the path', text)
        self.assertEqual(self.receive(process, 2), {'jsonrpc': '2.0', 'id': 3, 'result': {}})

    def test_parent_death_during_a_long_match_ends_the_bridge_and_its_matcher(self):
        import os
        import select
        import time
        program = """import json,subprocess,sys
child=subprocess.Popen([sys.executable,'-I',sys.argv[1],sys.argv[2]],stdin=subprocess.PIPE,stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL)
def send(message):
    child.stdin.write((json.dumps(message)+'\\n').encode()); child.stdin.flush()
send({'jsonrpc':'2.0','id':1,'method':'ping'})
assert json.loads(child.stdout.readline())['id']==1
send({'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'Grep','arguments':{'pattern':sys.argv[3]}}})
print(child.pid,flush=True)
sys.stdin.readline()
"""
        parent = subprocess.Popen([sys.executable, '-c', program, str(SPEC.origin), str(self.config), self.PATHOLOGICAL],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        pidfd = None
        pid = None
        try:
            pid = int(parent.stdout.readline())
            pidfd = os.pidfd_open(pid)
            time.sleep(0.2)
            children = None
            try:
                children = [int(value) for value in
                            Path(f'/proc/{pid}/task/{pid}/children').read_text().split()]
            except FileNotFoundError:
                pass  # kernel without CONFIG_PROC_CHILDREN
            parent.communicate('exit\n', timeout=5)
            watcher = select.poll()
            watcher.register(pidfd, select.POLLIN)
            self.assertTrue(watcher.poll(1000), 'bridge kept running a match after its parent died')
            if children is not None:
                self.assertTrue(children, 'no matcher process was observed during the search')
            deadline = time.monotonic() + 2
            for child in children or []:
                while time.monotonic() < deadline:
                    try:
                        state = Path(f'/proc/{child}/stat').read_text().rsplit(') ', 1)[1].split()[0]
                    except (FileNotFoundError, ProcessLookupError):
                        break
                    if state == 'Z':
                        break
                    time.sleep(0.02)
                else:
                    self.fail('matcher process outlived the bridge')
        finally:
            if pidfd is not None:
                import signal
                try:
                    signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.close(pidfd)
            if parent.poll() is None:
                parent.kill()
                parent.communicate(timeout=5)

    def test_legitimate_patterns_match_the_fixed_expected_table(self):
        self.write_tree()
        policy = bridge.Policy(json.loads(self.config.read_text()))
        server = bridge.Server(policy)
        for pattern, ignore_case, expected in GREP_PARITY_TABLE:
            with self.subTest(pattern=pattern, ignore_case=ignore_case):
                hits = json.loads(server.call('Grep', {'pattern': pattern, 'ignore_case': ignore_case}))
                self.assertEqual([(hit['path'], hit['line']) for hit in hits], expected)
                for hit in hits:
                    self.assertEqual(hit['text'], GREP_PARITY_TREE[hit['path']].splitlines()[hit['line'] - 1])
        single = json.loads(server.call('Grep', {'pattern': 'beta', 'path': 'notes/beta.txt'}))
        self.assertEqual([(hit['path'], hit['line']) for hit in single], [('beta.txt', 1), ('beta.txt', 3)])
        invalid = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Grep', 'arguments': {'pattern': '(unclosed'}}})
        self.assertTrue(invalid['isError'])

    @unittest.skipUnless(__import__('shutil').which('rg'), 'ripgrep (the Claude Grep engine) is not installed')
    def test_ripgrep_matches_the_same_fixed_table(self):
        self.write_tree()
        for pattern, ignore_case, expected in GREP_PARITY_TABLE:
            with self.subTest(pattern=pattern, ignore_case=ignore_case):
                run = subprocess.run(['rg', '--line-number', '--no-heading', '--with-filename', '--color', 'never',
                                      *(['-i'] if ignore_case else []), '-e', pattern, '.'],
                                     cwd=self.root, capture_output=True, text=True, timeout=30)
                self.assertIn(run.returncode, (0, 1), run.stderr)
                hits = []
                for line in run.stdout.splitlines():
                    path, number, _ = line.split(':', 2)
                    hits.append((path.removeprefix('./'), int(number)))
                self.assertEqual(sorted(hits), expected)

    def grep_with_file_reads_taking(self, seconds_per_file, pattern, **options):
        """Run Grep while every file read advances an injected clock."""
        from unittest.mock import patch
        self.write_tree()
        policy = bridge.Policy(json.loads(self.config.read_text()))
        now = [1000.0]
        read_at = bridge.Policy._read_at
        def slow_read(instance, directory, leaf):
            now[0] += seconds_per_file
            return read_at(instance, directory, leaf)
        with patch.object(bridge, 'SEARCH_CLOCK', lambda: now[0]), \
                patch.object(bridge, 'GREP_READ_SECONDS', 10), \
                patch.object(bridge.Policy, '_read_at', slow_read):
            return bridge.Server(policy).call('Grep', dict(pattern=pattern, **options))

    def test_slow_file_reading_does_not_count_against_the_matching_bound(self):
        # Five files at 1.5 s each: far past the 1.5 s matching bound, inside the read bound.
        result = self.grep_with_file_reads_taking(1.5, 'synthetic', ignore_case=True)
        self.assertIsInstance(result, str, 'a complete search has no truncation note')
        self.assertEqual([(hit['path'], hit['line']) for hit in json.loads(result)],
                         [('notes/alpha.md', 1), ('notes/alpha.md', 3), ('src/main.rs', 2), ('src/main.rs', 4)])

    def test_exhausted_read_budget_returns_partial_results_with_a_path_note(self):
        # 4 s per file against the 10 s read bound: three files are read, then it stops.
        result = self.grep_with_file_reads_taking(4.0, 'synthetic', ignore_case=True)
        self.assertFalse(result.get('isError', False), result)
        hits, note = json.loads(result['content'][0]['text']), result['content'][1]['text']
        self.assertEqual([(hit['path'], hit['line']) for hit in hits],
                         [('notes/alpha.md', 1), ('notes/alpha.md', 3)])
        self.assertIn('partial', note)
        self.assertIn('10 s', note)
        self.assertIn('3 files', note)
        self.assertIn('narrow the path', note)
        self.assertNotIn('pattern', note)

    def test_scan_byte_cap_returns_partial_results_with_a_path_note(self):
        from unittest.mock import patch
        self.write_tree()
        policy = bridge.Policy(json.loads(self.config.read_text()))
        server = bridge.Server(policy)
        total = sum(len(content.encode()) for content in GREP_PARITY_TREE.values())
        with patch.object(bridge, 'MAX_SEARCH_BYTES', total):
            self.assertEqual(len(json.loads(server.call('Grep', {'pattern': 'beta'}))), 3)
        first_two = sum(len(GREP_PARITY_TREE[name].encode()) for name in ('data/numbers.csv', 'notes/alpha.md'))
        with patch.object(bridge, 'MAX_SEARCH_BYTES', first_two):
            result = server.dispatch({'method': 'tools/call', 'params': {
                'name': 'Grep', 'arguments': {'pattern': 'beta'}}})
        self.assertFalse(result.get('isError', False), result)
        self.assertEqual([(hit['path'], hit['line']) for hit in json.loads(result['content'][0]['text'])],
                         [('notes/alpha.md', 2)])
        note = result['content'][1]['text']
        self.assertIn('partial', note)
        self.assertIn('scan limit', note)
        self.assertIn('2 files', note)
        self.assertIn('narrow the path', note)

    def matching_timeout_text(self, child_cpu_seconds):
        """Force a matching timeout; report `child_cpu_seconds` over 1.5 s of wall clock."""
        from types import SimpleNamespace
        from unittest.mock import patch
        self.write_tree()
        policy = bridge.Policy(json.loads(self.config.read_text()))
        clock = iter([100.0, 101.5])
        usage = iter([SimpleNamespace(ru_utime=4.0, ru_stime=1.0),
                      SimpleNamespace(ru_utime=4.0 + child_cpu_seconds * 0.9, ru_stime=1.0 + child_cpu_seconds * 0.1)])
        # A zero wall bound times out before the (plain literal) matcher can
        # finish, without generating any real CPU load.
        with patch.object(bridge, 'GREP_MATCH_SECONDS', 0), \
                patch.object(bridge, 'MATCH_CLOCK', lambda: next(clock)), \
                patch.object(bridge, 'CHILD_RUSAGE', lambda: next(usage)):
            response = bridge.Server(policy).dispatch({'method': 'tools/call', 'params': {
                'name': 'Grep', 'arguments': {'pattern': 'beta', 'ignore_case': True}}})
        self.assertTrue(response['isError'], response)
        return response['content'][0]['text']

    def test_cpu_bound_matching_timeout_advises_simplifying_the_pattern(self):
        text = self.matching_timeout_text(1.4)
        self.assertIn('time limit', text)
        self.assertIn('simplify the pattern', text)
        self.assertNotIn('busy', text)

    def test_matching_timeout_while_waiting_for_cpu_reports_a_busy_host(self):
        text = self.matching_timeout_text(0.2)
        self.assertIn('time limit', text)
        self.assertIn('busy', text)
        self.assertIn('retry', text)
        self.assertIn('narrow the path', text)
        self.assertNotIn('simplify the pattern', text)

    def test_walk_entry_limit_returns_partial_grep_results_but_still_denies_glob(self):
        from unittest.mock import patch
        self.write_tree()
        policy = bridge.Policy(dict(json.loads(self.config.read_text()), allowed_tools=['Grep', 'Glob']))
        server = bridge.Server(policy)
        # Entries in walk order: data, data/numbers.csv, notes, notes/alpha.md, ...
        with patch.object(bridge, 'MAX_WALK_ENTRIES', 3):
            result = server.call('Grep', {'pattern': '0'})
            with self.assertRaises(bridge.Denied):
                server.call('Glob', {'pattern': '**/*'})
        self.assertEqual([(hit['path'], hit['line']) for hit in json.loads(result['content'][0]['text'])],
                         [('data/numbers.csv', 2), ('data/numbers.csv', 3), ('data/numbers.csv', 4)])
        self.assertIn('entry limit', result['content'][1]['text'])
        self.assertIn('narrow the path', result['content'][1]['text'])


class HelperReadinessTests(unittest.TestCase):
    """#1043 review: the shared verification helper is checked at bridge startup."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'workspace'
        self.root.mkdir()
        (self.root / 'note.txt').write_text('SYNTHETIC_NOTE')

    def serve_from(self, launch, write_roots, with_helper):
        import shutil
        launch.mkdir(parents=True, exist_ok=True)
        shutil.copy(SPEC.origin, launch / 'tool-bridge.py')
        if with_helper:
            shutil.copy(Path(SPEC.origin).with_name('codex-command-sandbox.py'), launch / 'codex-command-sandbox.py')
        config = Path(self.temp.name) / 'policy.json'
        config.write_text(json.dumps({'cwd': str(self.root), 'read_roots': [str(self.root)],
                                      'write_roots': [str(root) for root in write_roots],
                                      'allowed_tools': ['Read', 'Write']}))
        config.chmod(0o600)
        messages = [
            {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {}},
            {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'},
            {'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call',
             'params': {'name': 'Read', 'arguments': {'file_path': 'note.txt'}}},
            {'jsonrpc': '2.0', 'id': 4, 'method': 'ping'},
        ]
        run = subprocess.run([sys.executable, '-I', str(launch / 'tool-bridge.py'), str(config)],
                             input=''.join(json.dumps(m) + '\n' for m in messages),
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        return run, [json.loads(line) for line in run.stdout.splitlines()]

    def assert_not_ready(self, run, replies):
        self.assertEqual([reply['id'] for reply in replies], [1, 2, 3, 4])
        for reply in replies[:3]:
            self.assertEqual(reply['error']['code'], -32001, reply)
            self.assertIn('JARVIS_READINESS:mcp_start ', reply['error']['message'])
        self.assertEqual(replies[3]['result'], {})
        self.assertIn('verification helper', run.stderr)
        self.assertNotIn('SYNTHETIC_NOTE', run.stdout)

    def test_missing_verification_helper_fails_readiness_instead_of_denying_tools(self):
        run, replies = self.serve_from(Path(self.temp.name) / 'launch', [], with_helper=False)
        self.assert_not_ready(run, replies)

    def test_model_writable_verification_helper_fails_readiness_at_startup(self):
        run, replies = self.serve_from(self.root / 'launch', [self.root], with_helper=True)
        self.assert_not_ready(run, replies)

    def test_packaged_helper_outside_write_roots_is_ready(self):
        run, replies = self.serve_from(Path(self.temp.name) / 'launch', [self.root], with_helper=True)
        self.assertEqual(replies[0]['result']['serverInfo']['name'], 'jarvis-tools')
        self.assertEqual(replies[2]['result']['content'][0]['text'], 'SYNTHETIC_NOTE')

    def test_cached_helper_is_still_refused_for_a_policy_that_can_write_it(self):
        helper_directory = Path(bridge.__file__).resolve().parent
        self.assertIsNotNone(bridge.file_verification())
        with self.assertRaisesRegex(bridge.Denied, 'model-writable'):
            bridge.file_verification([helper_directory])
        policy = bridge.Policy({'cwd': str(helper_directory), 'read_roots': [str(helper_directory)],
                                'write_roots': [str(helper_directory)], 'allowed_tools': ['Read']})
        with self.assertRaises(bridge.Denied):
            policy.read('codex-command-sandbox.py')


class BuildRunnerReadinessTests(unittest.TestCase):
    """#1041: builds never silently fall back to the host runner."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'workspace'
        self.root.mkdir()
        self.fakebin = Path(self.temp.name) / 'bin'
        self.fakebin.mkdir()
        for name in ('cargo', 'npm', 'npx', 'printf'):
            tool = self.fakebin / name
            tool.write_text('#!/bin/sh\necho SYNTHETIC_HOST_RUN\n')
            tool.chmod(0o700)

    def policy(self, **extra):
        config = {'cwd': str(self.root), 'read_roots': [str(self.root)],
                  'write_roots': [str(self.root)],
                  'allowed_tools': ['Bash(cargo *)', 'Bash(npm *)', 'Bash(npx *)', 'Bash(printf *)'],
                  'environment': {'PATH': str(self.fakebin) + ':/usr/bin'}}
        config.update(extra)
        return bridge.Policy(config)

    def test_missing_vm_config_fails_closed_for_every_build_command(self):
        for runner in ({}, {'build_runner': 'unavailable'}, {'build_runner': 'vm'}):
            policy = self.policy(**runner)
            self.assertIsNone(policy.build_runner)
            for command in ('cargo test', 'npm test', 'npx tsc'):
                with self.subTest(runner=runner, command=command):
                    with self.assertRaises(bridge.Readiness) as raised:
                        policy.run_command(command)
                    self.assertIn('JARVIS_READINESS:build_vm_unavailable', str(raised.exception))
                    self.assertIn('AUGMENTAGENT_BUILD_VM=host', str(raised.exception))

    def test_readiness_reaches_the_model_as_a_named_tool_error(self):
        server = bridge.Server(self.policy())
        response = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Bash', 'arguments': {'command': 'cargo build'}}})
        self.assertTrue(response['isError'])
        self.assertIn('JARVIS_READINESS:build_vm_unavailable', response['content'][0]['text'])

    def test_runner_selection_is_explicit(self):
        self.assertEqual(self.policy(build_runner='host').build_runner, 'host')
        config = Path(self.temp.name) / 'runtime.json'
        self.assertEqual(self.policy(build_vm_config=str(config)).build_runner, 'vm')
        with self.assertRaises(bridge.Denied):
            self.policy(build_runner='container')

    @requires_sandbox
    def test_non_build_commands_are_unaffected_by_a_missing_vm(self):
        outcome = self.policy().run_command('printf synthetic')
        self.assertEqual(outcome['exit_code'], 0, outcome)
        # Non-build commands always run in the host command sandbox.
        self.assertEqual(list(outcome)[0], 'runner')
        self.assertEqual(outcome['runner'], 'host')

    @requires_sandbox
    def test_host_opt_out_runs_builds_on_the_host_and_names_the_runner(self):
        outcome = self.policy(build_runner='host').run_command('cargo test')
        self.assertEqual(outcome['exit_code'], 0, outcome)
        self.assertIn('SYNTHETIC_HOST_RUN', outcome['stdout'])
        self.assertEqual(outcome['runner'], 'host')
        # First key, so a truncated serialisation still names the runner.
        self.assertTrue(json.dumps(outcome).startswith('{"runner": "host"'))

    def test_vm_runner_outcome_names_the_runner(self):
        policy = self.policy(build_vm_config=str(Path(self.temp.name) / 'runtime.json'))
        policy.run_vm_build = lambda argv, timeout: {'exit_code': 0, 'stdout': '', 'stderr': ''}
        outcome = policy.run_command('cargo test')
        self.assertEqual(outcome['runner'], 'vm')
        self.assertEqual(list(outcome)[0], 'runner')

    def call_bash(self, policy, command, timeout=120):
        return bridge.Server(policy).dispatch({'method': 'tools/call', 'params': {
            'name': 'Bash', 'arguments': {'command': command, 'timeout': timeout}}})

    def test_quoted_build_command_is_gated_and_names_the_runner(self):
        policy = self.policy(build_vm_config=str(Path(self.temp.name) / 'runtime.json'))
        policy.run_vm_build = lambda argv, timeout: {'exit_code': 0, 'stdout': '', 'stderr': ''}
        response = self.call_bash(policy, '"cargo" test')
        self.assertTrue(response['content'][0]['text'].startswith('{"runner": "vm"'), response)

    def test_vm_denial_after_the_build_started_keeps_the_runner(self):
        policy = self.policy(build_vm_config=str(Path(self.temp.name) / 'runtime.json'))
        def broker_fails_mid_build(argv, timeout):
            policy.mark_command_started()
            raise bridge.Denied('VM execution or cleanup failed')
        policy.run_vm_build = broker_fails_mid_build
        response = self.call_bash(policy, 'cargo test')
        self.assertTrue(response['isError'])
        self.assertTrue(response['content'][0]['text'].startswith('[runner=vm] '), response)

    def test_refusal_before_any_process_starts_carries_no_runner(self):
        policy = self.policy(build_runner='host', environment={'PATH': str(Path(self.temp.name) / 'empty')})
        response = self.call_bash(policy, 'cargo test')
        self.assertTrue(response['isError'])
        self.assertNotIn('[runner=', response['content'][0]['text'])
        policy = self.policy(build_vm_config=str(Path(self.temp.name) / 'runtime.json'))
        policy.run_vm_build = lambda argv, timeout: (_ for _ in ()).throw(bridge.Denied('VM runtime is unavailable'))
        self.assertNotIn('[runner=', self.call_bash(policy, 'cargo test')['content'][0]['text'])

    @requires_sandbox
    def test_host_build_timeout_after_start_keeps_the_runner(self):
        slow = self.fakebin / 'cargo'
        slow.write_text('#!/bin/sh\nexec sleep 30\n')
        response = self.call_bash(self.policy(build_runner='host'), 'cargo test', timeout=1)
        self.assertTrue(response['isError'])
        self.assertTrue(response['content'][0]['text'].startswith('[runner=host] '), response)


class BuildScratchTests(unittest.TestCase):
    """#1036: VM build scratch lives under the configured root, never /tmp."""

    def setUp(self):
        import os
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        base = Path(self.temp.name)
        self.root = base / 'workspace'; self.root.mkdir()
        (self.root / 'Cargo.toml').write_text('[package]\nname="synthetic"\n')
        self.scratch = base / 'scratch'; self.scratch.mkdir(mode=0o700)
        # Anything that falls back to the default temp dir lands here.
        self.default_tmp = base / 'default-tmp'; self.default_tmp.mkdir()
        original = tempfile.tempdir
        tempfile.tempdir = str(self.default_tmp)
        self.addCleanup(setattr, tempfile, 'tempdir', original)
        self.registry = base / 'operator-registry'
        (self.registry / 'cache/index.crates.io-synthetic').mkdir(parents=True)
        (self.registry / 'index/index.crates.io-synthetic').mkdir(parents=True)
        (self.registry / 'cache/index.crates.io-synthetic/synthetic-1.0.0.crate').write_bytes(b'x' * 64)
        (self.registry / 'src').mkdir()
        (self.registry / 'src/extracted.rs').write_text('not copied')
        artifact = base / 'runtime-artifact'; artifact.write_text('synthetic')
        self.runtime_config = {key: str(artifact) for key in ('qemu', 'kernel', 'busybox', 'firmware',
                               'data_dir', 'library_dir', 'module_dir')}
        self.runtime_config.update(modules=[str(artifact)], memory_mb=512, registry=str(self.registry))
        self.calls = []
        test = self

        class FakeVm:
            class Unavailable(ValueError):
                pass

            class Runtime:
                @staticmethod
                def load(path):
                    runtime = FakeVm.Runtime(); runtime.config = test.runtime_config
                    return runtime

            @staticmethod
            def run(runtime, workspace, argv, environment, timeout=120, node_workspaces=(),
                    download_info=None, scratch_dir=None, build_cache=None):
                test.calls.append({'workspace': Path(workspace), 'argv': argv, 'environment': environment,
                                   'timeout': timeout, 'scratch_dir': scratch_dir, 'build_cache': build_cache})
                return {'exit_code': 0, 'stdout': '', 'stderr': ''}
        self.fake_vm = FakeVm

    def policy(self, **extra):
        config = {'cwd': str(self.root), 'read_roots': [str(self.root)], 'write_roots': [str(self.root)],
                  'allowed_tools': ['Read', 'Write', 'Bash(cargo *)', 'Bash(printf *)'],
                  'build_vm_config': str(Path(self.temp.name) / 'runtime.json'),
                  'build_scratch_dir': str(self.scratch), 'build_timeout_secs': 600}
        config.update(extra)
        policy = bridge.Policy(config)
        policy._vm_helper = lambda: self.fake_vm
        policy._scratch.cache_bytes = 64 * 1024**2  # keep the synthetic image small
        policy._scratch.statvfs = self.fake_statvfs(free_bytes=500 * 1024**3)
        self.addCleanup(policy.close)
        return policy

    @staticmethod
    def fake_statvfs(free_bytes):
        import os
        def statvfs(target):
            return os.statvfs_result((4096, 4096, 0, 0, free_bytes // 4096, 0, 0, 0, 0, 255))
        return statvfs

    def under(self, path, root):
        return Path(path).resolve().is_relative_to(Path(root).resolve())

    def test_snapshot_control_and_cache_paths_resolve_under_the_scratch_root(self):
        policy = self.policy()
        policy.run_command('cargo build')
        call = self.calls[0]
        self.assertTrue(self.under(call['workspace'], self.scratch), call)
        self.assertTrue(self.under(call['scratch_dir'], self.scratch), call)
        self.assertTrue(self.under(call['build_cache'], self.scratch), call)
        self.assertNotIn('CARGO_HOME', call['environment'], 'the guest points Cargo at the build cache')
        self.assertEqual(list(self.default_tmp.iterdir()), [], 'nothing may use the default temp dir')

    def test_missing_or_unsafe_scratch_root_fails_closed_naming_the_path(self):
        import os
        missing = Path(self.temp.name) / 'absent-scratch'
        policy = self.policy(build_scratch_dir=str(missing))
        with self.assertRaises(bridge.Readiness) as raised:
            policy.run_command('cargo build')
        self.assertIn('JARVIS_READINESS:build_scratch_unavailable', str(raised.exception))
        self.assertIn(str(missing), str(raised.exception))
        self.assertEqual(self.calls, [], 'no build may run without scratch')
        response = bridge.Server(policy).dispatch({'method': 'tools/call', 'params': {
            'name': 'Bash', 'arguments': {'command': 'cargo build'}}})
        self.assertTrue(response['content'][0]['text'].startswith('JARVIS_READINESS:build_scratch_unavailable'))
        for mode in (0o777, 0o755, 0o750, 0o500):
            self.scratch.chmod(mode)
            with self.subTest(mode=oct(mode)), self.assertRaises(bridge.Readiness):
                self.policy().run_command('cargo build')
        self.scratch.chmod(0o700)
        link = Path(self.temp.name) / 'scratch-link'; link.symlink_to(self.scratch)
        with self.assertRaises(bridge.Readiness):
            self.policy(build_scratch_dir=str(link)).run_command('cargo build')
        with self.assertRaises(bridge.Readiness):
            self.policy(build_scratch_dir=None).run_command('cargo build')
        self.assertEqual(self.calls, [])

    def test_scratch_root_inside_a_write_root_makes_only_builds_unavailable(self):
        inside = self.root / 'scratch'; inside.mkdir(mode=0o700)
        (self.root / 'note.md').write_text('synthetic')
        for scratch in (str(inside), 'relative/scratch'):
            policy = self.policy(build_scratch_dir=scratch)  # the bridge still starts
            self.assertEqual(policy.read('note.md'), 'synthetic')
            with self.subTest(scratch=scratch), self.assertRaises(bridge.Readiness) as raised:
                policy.run_command('cargo build')
            self.assertIn('JARVIS_READINESS:build_scratch_unavailable', str(raised.exception))
        self.assertEqual(list(inside.iterdir()), [])
        self.assertEqual(self.calls, [])

    def test_low_free_space_is_refused_before_any_file_is_created(self):
        policy = self.policy()
        policy._scratch.cache_bytes = 12 * 1024**3
        policy._scratch.headroom_bytes = 20 * 1024**3
        policy._scratch.statvfs = self.fake_statvfs(free_bytes=31 * 1024**3)
        with self.assertRaises(bridge.Readiness) as raised:
            policy.run_command('cargo build')
        self.assertIn('JARVIS_READINESS:build_scratch_space', str(raised.exception))
        self.assertIn(str(self.scratch), str(raised.exception))
        self.assertEqual(list(self.scratch.iterdir()), [], 'nothing is created when space is short')
        self.assertEqual(self.calls, [])
        policy._scratch.statvfs = self.fake_statvfs(free_bytes=32 * 1024**3)
        policy._scratch.cache_bytes = 64 * 1024**2
        policy.run_command('cargo build')
        self.assertEqual(len(self.calls), 1)

    def test_admission_holds_an_exclusive_lock_on_the_scratch_root(self):
        import fcntl, os
        policy = self.policy()
        observed = []
        original = policy._scratch._require_space
        def probe(root_fd):
            other = os.open(self.scratch, os.O_RDONLY | os.O_DIRECTORY)
            try:
                fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
                observed.append('unlocked')
            except BlockingIOError:
                observed.append('locked')
            finally:
                os.close(other)
            return original(root_fd)
        policy._scratch._require_space = probe
        policy.run_command('cargo build')
        self.assertEqual(observed, ['locked'], 'space check and session creation must be one critical section')
        other = os.open(self.scratch, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)  # released afterwards
        finally:
            os.close(other)

    def test_concurrent_sessions_share_one_budget_and_reserve_unallocated_growth(self):
        import os
        cap = 64 * 1024**2
        first = self.policy()
        first.run_command('cargo build')
        image = first._scratch.cache
        with open(image, 'r+b') as handle:  # the first session's build wrote 8 MiB
            handle.seek(1024**2); handle.write(os.urandom(8 * 1024**2))
        allocated = os.stat(image).st_blocks * 512
        second = self.policy()
        second._scratch.headroom_bytes = 0
        # Budget: allocated blocks of existing images plus the new cap.
        second._scratch.budget_bytes = allocated + cap - 1
        with self.assertRaises(bridge.Readiness) as raised:
            second.run_command('cargo build')
        self.assertIn('build_scratch_space', str(raised.exception))
        second._scratch.budget_bytes = allocated + cap
        # Free space must also cover the first image's unallocated growth.
        second._scratch.statvfs = self.fake_statvfs(free_bytes=(cap - allocated) + cap - 4096)
        with self.assertRaises(bridge.Readiness):
            second.run_command('cargo build')
        second._scratch.statvfs = self.fake_statvfs(free_bytes=(cap - allocated) + cap + 4096)
        second.run_command('cargo build')
        self.assertEqual(len([p for p in self.scratch.iterdir()]), 2)

    def test_default_space_limits_are_documented_values(self):
        self.assertEqual(bridge.BuildScratch.CACHE_BYTES, 12 * 1024**3)
        self.assertEqual(bridge.BuildScratch.HEADROOM_BYTES, 20 * 1024**3)
        self.assertEqual(bridge.BuildScratch.BUDGET_BYTES, 24 * 1024**3)

    def test_host_volume_full_after_a_build_is_a_named_readiness_error(self):
        policy = self.policy()
        def fills_the_volume(*args, **kwargs):
            policy._scratch.statvfs = self.fake_statvfs(free_bytes=0)
            raise self.fake_vm.Unavailable('VM did not produce a trusted command result')
        self.fake_vm.run = staticmethod(fills_the_volume)
        response = bridge.Server(policy).dispatch({'method': 'tools/call', 'params': {
            'name': 'Bash', 'arguments': {'command': 'cargo build'}}})
        text = response['content'][0]['text']
        self.assertTrue(text.startswith('[runner=vm] JARVIS_READINESS:build_cache_full'), text)
        self.assertIn(str(self.scratch), text)

    def test_guest_reporting_a_full_cache_image_is_a_named_readiness_error(self):
        policy = self.policy()
        def image_full(*args, **kwargs):
            raise self.fake_vm.Unavailable('VM build cache is full')
        self.fake_vm.run = staticmethod(image_full)
        with self.assertRaises(bridge.Readiness) as raised:
            policy.run_command('cargo build')
        self.assertIn('JARVIS_READINESS:build_cache_full', str(raised.exception))
        self.assertIn('build-cache.img', str(raised.exception))

    def test_two_cargo_calls_in_a_session_reuse_the_target_directory(self):
        import os
        policy = self.policy()
        policy.run_command('cargo build')
        policy.run_command('cargo test')
        first, second = self.calls
        # The target directory and Cargo home live in this one image.
        self.assertEqual(first['build_cache'], second['build_cache'])
        self.assertNotEqual(first['workspace'], second['workspace'], 'sources are still a fresh snapshot')
        image = Path(first['build_cache'])
        self.assertEqual(image.stat().st_mode & 0o777, 0o600)
        self.assertEqual(image.stat().st_size, 64 * 1024**2)
        self.assertLess(image.stat().st_blocks * 512, image.stat().st_size, 'the image is sparse')
        with open(image, 'rb') as handle:
            handle.seek(1024 + 56)
            self.assertEqual(handle.read(2), b'\x53\xef', 'an ext filesystem the guest can mount')

    def test_session_close_removes_its_scratch_and_records_its_owner(self):
        import os
        policy = self.policy()
        policy.run_command('cargo build')
        sessions = [path for path in self.scratch.iterdir() if path.name.startswith('jarvis-vm-session-')]
        self.assertEqual(len(sessions), 1)
        owner = json.loads((sessions[0] / 'owner.json').read_text())
        self.assertEqual(owner['pid'], os.getpid())
        self.assertEqual(owner['start_time'], bridge.process_start_time(os.getpid()))
        policy.close()
        self.assertEqual(list(self.scratch.iterdir()), [])

    def test_build_commands_default_to_the_configured_timeout(self):
        server = bridge.Server(self.policy(build_timeout_secs=600))
        server.execute('Bash', {'command': 'cargo build'})
        self.assertEqual(self.calls[-1]['timeout'], 600)
        server.execute('Bash', {'command': 'cargo build', 'timeout': 30})
        self.assertEqual(self.calls[-1]['timeout'], 30, 'an explicit timeout is honoured')
        policy = self.policy(build_timeout_secs=5000)
        policy.run_command('cargo build')
        self.assertEqual(self.calls[-1]['timeout'], 900, 'capped at the tool maximum')
        del_config = self.policy(build_timeout_secs=None)
        del_config.run_command('cargo build')
        self.assertGreaterEqual(self.calls[-1]['timeout'], bridge.BUILD_TIMEOUT_DEFAULT)
        self.assertGreaterEqual(bridge.BUILD_TIMEOUT_DEFAULT, 600)

    def test_registry_is_never_copied_on_the_host(self):
        policy = self.policy()
        copies = []
        original = bridge.copy_dependency_tree
        bridge.copy_dependency_tree = lambda *args, **kwargs: copies.append(args)
        self.addCleanup(setattr, bridge, 'copy_dependency_tree', original)
        policy.run_command('cargo build')
        policy.run_command('cargo test')
        self.assertEqual(copies, [], 'the guest seeds its Cargo home from the read-only registry mount')
        self.assertEqual(sorted(p.name for p in policy._scratch.session.iterdir()),
                         ['build-cache.img', 'owner.json', 'tmp'])

    def test_snapshot_preserves_source_mtimes_so_cargo_fingerprints_stay_fresh(self):
        import os
        source = self.root / 'lib.rs'; source.write_text('pub fn synthetic() {}\n')
        os.utime(source, ns=(1_000_000_000_123, 1_000_000_000_456))
        snapshot = bridge.BuildSnapshot(self.policy(), self.scratch / 'snapshot')
        self.assertEqual((snapshot.root / 'lib.rs').stat().st_mtime_ns, 1_000_000_000_456)

    def test_scratch_readiness_is_raised_before_the_command_starts(self):
        policy = self.policy(build_scratch_dir=str(Path(self.temp.name) / 'absent'))
        response = bridge.Server(policy).dispatch({'method': 'tools/call', 'params': {
            'name': 'Bash', 'arguments': {'command': 'cargo build'}}})
        self.assertNotIn('[runner=', response['content'][0]['text'])

    @unittest.skipUnless(__import__('os').environ.get('JARVIS_TEST_VM_CONFIG')
                         and __import__('os').environ.get('JARVIS_TEST_BUILD_SCRATCH'),
                         'requires a provisioned private KVM runtime and a scratch root')
    def test_real_vm_second_cargo_build_in_a_session_is_incremental(self):
        import os, time
        # A cached registry dependency makes a cold build visibly longer.
        (self.root / 'Cargo.toml').write_text('[package]\nname="synthetic-incremental"\nversion="0.1.0"\nedition="2021"\n'
                                              '[dependencies]\nserde={version="=1.0.228",features=["derive"]}\n')
        (self.root / 'src').mkdir()
        (self.root / 'src/lib.rs').write_text('#[derive(serde::Serialize)] pub struct Pair { pub a: u64, pub b: u64 }\n'
                                              'pub fn add(pair: &Pair) -> u64 { pair.a + pair.b }\n'
                                              '#[test] fn adds() { assert_eq!(add(&Pair { a: 2, b: 2 }), 4); }\n')
        policy = bridge.Policy({'cwd': str(self.root), 'read_roots': [str(self.root)],
            'write_roots': [str(self.root)], 'allowed_tools': ['Read', 'Write', 'Bash(cargo *)'],
            'build_vm_config': os.environ['JARVIS_TEST_VM_CONFIG'],
            'build_scratch_dir': os.environ['JARVIS_TEST_BUILD_SCRATCH'], 'build_timeout_secs': 600})
        self.addCleanup(policy.close)
        timings, outcomes = [], []
        for command in ('cargo test --offline', 'cargo test --offline'):
            started = time.monotonic()
            outcome = policy.run_command(command)
            timings.append(time.monotonic() - started)
            outcomes.append(outcome)
            self.assertEqual(outcome['exit_code'], 0, outcome)
            self.assertIn('1 passed', outcome['stdout'])
        print(f'\nSYNTHETIC_VM_TIMINGS first={timings[0]:.1f}s second={timings[1]:.1f}s')
        self.assertIn('Compiling serde', outcomes[0]['stderr'])
        self.assertNotIn('Compiling', outcomes[1]['stderr'], 'second build must reuse the session target')
        self.assertLess(timings[1], timings[0])
        self.assertEqual(list(self.default_tmp.iterdir()), [])


class TransportTests(unittest.TestCase):
    def test_bridge_survives_launcher_thread_exit_while_parent_process_is_alive(self):
        import queue
        import threading
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            config=root/'policy.json'
            config.write_text(json.dumps({'cwd':str(root),'read_roots':[str(root)],
                'write_roots':[],'allowed_tools':['Read']}))
            config.chmod(0o600)
            launched=queue.Queue()
            def launch():
                child=subprocess.Popen([sys.executable,str(SPEC.origin),str(config)],
                    stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
                child.stdin.write(json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize'})+'\n');child.stdin.flush()
                initialized=child.stdout.readline()
                launched.put((child,initialized))
            thread=threading.Thread(target=launch);thread.start();thread.join(timeout=10)
            self.assertFalse(thread.is_alive())
            child,initialized=launched.get(timeout=1)
            try:
                self.assertIn('result',json.loads(initialized))
                child.stdin.write(json.dumps({'jsonrpc':'2.0','id':2,'method':'ping'})+'\n');child.stdin.flush()
                response=child.stdout.readline()
                self.assertTrue(response,'bridge died when only its launcher thread exited')
                self.assertEqual(json.loads(response)['id'],2)
            finally:
                child.terminate();child.communicate(timeout=5)

    def test_bridge_stops_when_parent_process_exits_even_if_stdin_stays_open(self):
        import os
        import select
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);config=root/'policy.json'
            config.write_text(json.dumps({'cwd':str(root),'read_roots':[str(root)],
                'write_roots':[],'allowed_tools':['Read']}));config.chmod(0o600)
            # A separate inherited pipe remains open in this test process, so
            # EOF alone cannot provide the lifecycle guarantee being tested.
            read_fd,write_fd=os.pipe()
            program="""import json,os,subprocess,sys
child=subprocess.Popen([sys.executable,sys.argv[1],sys.argv[2]],stdin=int(sys.argv[3]),stdout=subprocess.PIPE)
ready=child.stdout.readline()
assert 'result' in json.loads(ready)
print(child.pid,flush=True)
sys.stdin.readline()
"""
            parent=subprocess.Popen([sys.executable,'-c',program,str(SPEC.origin),str(config),str(read_fd)],
                stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,pass_fds=(read_fd,),text=True)
            pidfd=None
            try:
                os.write(write_fd,(json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize'})+'\n').encode())
                pid=int(parent.stdout.readline());pidfd=os.pidfd_open(pid)
                parent.communicate('exit\n',timeout=5)
                watcher=select.poll();watcher.register(pidfd,select.POLLIN)
                self.assertTrue(watcher.poll(5000),'bridge outlived its parent process')
            finally:
                os.close(read_fd);os.close(write_fd)
                if pidfd is not None:
                    import signal
                    try: signal.pidfd_send_signal(pidfd,signal.SIGKILL)
                    except ProcessLookupError: pass
                    os.close(pidfd)
                if parent.poll() is None:
                    parent.kill();parent.communicate(timeout=5)

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



class ReadAllowanceTests(unittest.TestCase):
    """#1045: single-file Read exceptions outside the read roots.

    The query preset's scope guard lets Claude Read inbound attachment temp
    files (`/tmp/aa-{txt,img,doc}-<id>-<idx>.<ext>`, `$AUGMENTAGENT_IMESSAGE_TMP_DIR/<name>`).
    The bridge admits the same reads as policy allowances: Read only, one
    named directory, one pattern-matched leaf. The directory is world-writable
    in production, so the leaf must be a private, unshared regular file owned by
    this process's user. A synthetic directory stands in for /tmp here.
    The daemon writes attachments under umask 0002 (0664, its own group).
    """
    PATTERN = r'aa-(txt|img|doc)-[0-9]+-[0-9]+\.[a-zA-Z0-9]+'

    def setUp(self):
        import os
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / 'wiki'
        self.root.mkdir()
        self.inbox = self.base / 'inbox'
        self.inbox.mkdir(mode=0o700)
        self.outside = self.base / 'outside.txt'
        self.outside.write_text('SYNTHETIC_PRIVATE')
        self.attachment = self.private(self.inbox / 'aa-txt-7-0.md', 'SYNTHETIC_ATTACHMENT')
        self.policy = self.make_policy()
        self.addCleanup(os.chmod, self.inbox, 0o700)

    @staticmethod
    def private(path, text):
        path.write_text(text)
        path.chmod(0o600)
        return path

    def allowance(self, directory=None, pattern=None, tools=('Read',)):
        return {'tools': list(tools), 'directory': str(directory or self.inbox),
                'name_pattern': pattern or self.PATTERN}

    def make_policy(self, allowances=None, tools=('Read', 'Write', 'Edit', 'Glob', 'Grep'), **extra):
        config = {'cwd': str(self.root), 'read_roots': [str(self.root)], 'write_roots': [str(self.root)],
                  'allowed_tools': list(tools),
                  'read_allowances': [self.allowance()] if allowances is None else allowances}
        config.update(extra)
        return bridge.Policy(config)

    def assertDeniedRead(self, path, policy=None):
        server = bridge.Server(policy or self.policy)
        with self.assertRaises(bridge.Denied, msg=str(path)):
            server.call('Read', {'file_path': str(path)})
        response = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Read', 'arguments': {'file_path': str(path)}}})
        self.assertTrue(response.get('isError'), str(path))
        self.assertNotIn('SYNTHETIC', json.dumps(response))

    def test_allowed_attachment_reads_as_original_text_and_image(self):
        import base64
        server = bridge.Server(self.policy)
        response = server.dispatch({'method': 'tools/call', 'params': {
            'name': 'Read', 'arguments': {'file_path': str(self.attachment)}}})
        self.assertFalse(response.get('isError', False), response)
        self.assertEqual(response['content'][0]['text'], 'SYNTHETIC_ATTACHMENT')
        self.assertEqual(server.call('Read', {'file_path': str(self.attachment), 'offset': 1, 'limit': 1}),
                         'SYNTHETIC_ATTACHMENT')
        pixel = base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg==')
        image = self.inbox / 'aa-img-7-1.png'
        image.write_bytes(pixel)
        image.chmod(0o600)
        result = server.call('Read', {'file_path': str(image)})
        self.assertEqual(result['content'][0]['mimeType'], 'image/png')
        self.assertEqual(base64.b64decode(result['content'][0]['data']), pixel)

    def test_allowance_grants_read_only_never_search_write_or_edit(self):
        server = bridge.Server(self.policy)
        for tool, arguments in [
            ('Glob', {'pattern': '*', 'path': str(self.inbox)}),
            ('Glob', {'pattern': 'aa-txt-*', 'path': str(self.base)}),
            ('Grep', {'pattern': 'SYNTHETIC', 'path': str(self.inbox)}),
            ('Grep', {'pattern': 'SYNTHETIC', 'path': str(self.attachment)}),
            ('Write', {'file_path': str(self.attachment), 'content': 'UNAUTHORIZED'}),
            ('Write', {'file_path': str(self.inbox / 'aa-txt-8-0.md'), 'content': 'UNAUTHORIZED'}),
            ('Edit', {'file_path': str(self.attachment), 'old_string': 'SYNTHETIC', 'new_string': 'UNAUTHORIZED'}),
        ]:
            with self.subTest(tool=tool, arguments=arguments):
                response = server.dispatch({'method': 'tools/call', 'params': {'name': tool, 'arguments': arguments}})
                self.assertTrue(response.get('isError'), response)
                self.assertNotIn('SYNTHETIC', json.dumps(response))
        self.assertEqual(self.attachment.read_text(), 'SYNTHETIC_ATTACHMENT')
        self.assertFalse((self.inbox / 'aa-txt-8-0.md').exists())
        # The allowance never implies the Read tool itself.
        self.assertDeniedRead(self.attachment, self.make_policy(tools=('Glob',)))

    def test_allowance_rejects_lookalike_names_and_path_tricks(self):
        for name in ('aa-txt-..', 'aa-txt-a-0.md', 'aa-pdf-7-0.md', 'aa-txt-7-0.md.bak', 'aa-txt-7-0.',
                     'lookalike.txt', 'aa-txt-7-0'):
            self.private(self.inbox / name, 'SYNTHETIC_OUTSIDE_SCOPE')
        (self.inbox / 'nested').mkdir()
        self.private(self.inbox / 'nested' / 'aa-txt-7-0.md', 'SYNTHETIC_OUTSIDE_SCOPE')
        (self.base / 'inbox-evil').mkdir()
        self.private(self.base / 'inbox-evil' / 'aa-txt-7-0.md', 'SYNTHETIC_OUTSIDE_SCOPE')
        inbox = str(self.inbox)
        for path in [
            inbox + '/aa-txt-..', inbox + '/aa-txt-a-0.md', inbox + '/aa-pdf-7-0.md',
            inbox + '/aa-txt-7-0.md.bak', inbox + '/aa-txt-7-0.', inbox + '/aa-txt-7-0',
            inbox + '/aa-txt-7-0.md/../lookalike.txt', inbox + '/aa-txt-7-0.md/../../outside.txt',
            inbox + '/aa-txt-7-0.md/../aa-txt-7-0.md', inbox + '//aa-txt-7-0.md', inbox + '/./aa-txt-7-0.md',
            inbox + '/nested/aa-txt-7-0.md', inbox + '/nested/../aa-txt-7-0.md', inbox + '-evil/aa-txt-7-0.md',
            '../inbox/aa-txt-7-0.md', 'aa-txt-7-0.md', inbox + '/aa-txt-7-0.md\x00', inbox + '/aa-txt-7-0.md/',
        ]:
            with self.subTest(path=path):
                self.assertDeniedRead(path)
        # A one-segment name pattern (the iMessage session dir's) also fits `.` and `..`.
        session = self.make_policy([self.allowance(pattern=r'[A-Za-z0-9._-]+')])
        self.assertEqual(session.read(str(self.attachment)), 'SYNTHETIC_ATTACHMENT')
        for path in (inbox + '/..', inbox + '/.', inbox + '/../outside.txt', inbox + '/nested/aa-txt-7-0.md'):
            with self.subTest(path=path):
                self.assertDeniedRead(path, session)

    def test_allowance_refuses_planted_links_and_shared_or_special_files(self):
        import os
        os.symlink(self.outside, self.inbox / 'aa-txt-8-0.md')
        os.symlink(self.attachment, self.inbox / 'aa-txt-8-1.md')
        os.link(self.outside, self.inbox / 'aa-txt-9-0.md')
        self.private(self.inbox / 'aa-txt-10-1.md', 'SYNTHETIC_SHARED').chmod(0o666)
        self.private(self.inbox / 'aa-txt-10-2.md', 'SYNTHETIC_SHARED').chmod(0o602)
        os.mkfifo(self.inbox / 'aa-txt-11-0.md', 0o600)
        (self.inbox / 'aa-txt-12-0.md').mkdir()
        names = ['aa-txt-8-0.md', 'aa-txt-8-1.md', 'aa-txt-9-0.md', 'aa-txt-10-1.md', 'aa-txt-10-2.md',
                 'aa-txt-11-0.md', 'aa-txt-12-0.md', 'aa-txt-13-0.md']
        # Group write through a group other than the daemon's own.
        other_groups = [group for group in os.getgroups() if group != os.getgid()]
        if other_groups:
            shared = self.private(self.inbox / 'aa-txt-10-0.md', 'SYNTHETIC_SHARED')
            os.chown(shared, -1, other_groups[0])
            shared.chmod(0o660)
            names.append(shared.name)
        for name in names:
            with self.subTest(name=name):
                self.assertDeniedRead(self.inbox / name)
        # The hard-linked inode is refused even though the model reached it by an allowed name.
        self.assertEqual(os.stat(self.outside).st_nlink, 2)

    def test_daemon_umask_attachment_is_readable(self):
        # umask 0002: the daemon's own attachments are 0664 in its primary group.
        self.attachment.chmod(0o664)
        self.assertEqual(bridge.Server(self.policy).call('Read', {'file_path': str(self.attachment)}),
                         'SYNTHETIC_ATTACHMENT')

    def test_allowance_refuses_files_owned_by_another_user(self):
        import os
        import stat
        uid, gid = os.getuid(), os.getgid()
        def info(mode=stat.S_IFREG | 0o600, links=1, owner=uid, group=gid):
            return os.stat_result((mode, 1, 1, links, owner, group, 5, 0, 0, 0))
        def permitted(metadata):
            return bridge.allowance_file_permitted(metadata, uid, gid)
        self.assertTrue(permitted(info()))
        self.assertTrue(permitted(info(mode=stat.S_IFREG | 0o664)), 'daemon umask 0002, own group')
        self.assertFalse(permitted(info(owner=uid + 1)), 'foreign owner')
        self.assertFalse(permitted(info(owner=uid + 1, mode=stat.S_IFREG | 0o644)), 'foreign owner, not writable')
        self.assertFalse(permitted(info(owner=0)), 'root-owned')
        self.assertFalse(permitted(info(mode=stat.S_IFREG | 0o660, group=gid + 1)), 'writable by another group')
        self.assertFalse(permitted(info(mode=stat.S_IFREG | 0o602)), 'world-writable')
        self.assertFalse(permitted(info(mode=stat.S_IFREG | 0o666)), 'world-writable, own group')
        self.assertFalse(permitted(info(links=2)), 'hard link')
        self.assertFalse(permitted(info(mode=stat.S_IFLNK | 0o777)), 'symlink')
        self.assertFalse(permitted(info(mode=stat.S_IFDIR | 0o700)), 'directory')
        # End to end with a real file another user owns, when the host has one.
        system = Path('/etc/hostname')
        try:
            real = os.lstat(system)
        except OSError:
            self.skipTest('no foreign-owned regular file fixture on this host')
        if not stat.S_ISREG(real.st_mode) or real.st_nlink != 1 or real.st_uid == uid or uid == 0:
            self.skipTest('no foreign-owned regular file fixture on this host')
        policy = self.make_policy([self.allowance(directory=system.parent, pattern='hostname')])
        self.assertDeniedRead(system, policy)

    def test_allowance_directory_opens_without_symlinks_and_must_not_be_shared(self):
        import os
        os.symlink(self.inbox, self.base / 'inbox-link')
        os.symlink(self.base, self.base / 'alias')
        for directory in (self.base / 'inbox-link', self.base / 'alias' / 'inbox'):
            with self.subTest(directory=directory):
                policy = self.make_policy([self.allowance(directory=directory)])
                self.assertDeniedRead(directory / 'aa-txt-7-0.md', policy)
        self.inbox.chmod(0o777)
        self.assertDeniedRead(self.attachment)
        self.inbox.chmod(0o770)
        self.assertDeniedRead(self.attachment)
        # A sticky shared directory, like /tmp, protects entries of other owners.
        self.inbox.chmod(0o1777)
        self.assertEqual(bridge.Server(self.policy).call('Read', {'file_path': str(self.attachment)}),
                         'SYNTHETIC_ATTACHMENT')

    def test_malformed_allowances_fail_closed(self):
        valid = self.allowance()
        for entries in [
            {}, 'aa-txt', [str(self.inbox)],
            [dict(valid, tools=['Read', 'Glob'])], [dict(valid, tools=['Grep'])], [dict(valid, tools='Read')],
            [dict(valid, tools=[])], [dict(valid, directory='/')], [dict(valid, directory='relative/inbox')],
            [dict(valid, directory=str(self.inbox) + '/')], [dict(valid, directory=str(self.inbox) + '/../inbox')],
            [dict(valid, directory='/' + str(self.inbox))], [dict(valid, directory=str(self.inbox) + '/.')],
            [dict(valid, directory=str(self.inbox) + '\x00')], [dict(valid, directory=None)],
            [dict(valid, name_pattern='(')], [dict(valid, name_pattern=7)], [dict(valid, name_pattern='')],
            [dict(valid, recursive=True)], [{'tools': ['Read'], 'directory': str(self.inbox)}],
        ]:
            with self.subTest(entries=entries), self.assertRaises(bridge.Denied):
                self.make_policy(entries)

    def test_handoff_state_cannot_match_a_read_allowance(self):
        with self.assertRaisesRegex(bridge.Denied, 'outside model tool scopes'):
            self.make_policy(handoff_path=str(self.inbox / 'aa-txt-99-0.md'))


if __name__ == '__main__':
    unittest.main()
