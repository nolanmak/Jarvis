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

    def test_allowed_command_executes_and_does_not_evaluate_shell_text(self):
        outcome=self.policy.run_command("printf 'hello; world'")
        self.assertEqual(outcome['exit_code'],0)
        self.assertEqual(outcome['stdout'],'hello; world')
        with self.assertRaises(bridge.Denied):
            self.policy.run_command('printf ok; id')

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
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Edit','Bash(cargo *)'],
            'environment':{'PATH':str(fakebin)+':/usr/bin','SYNTHETIC_TOKEN':'PRIVATE'}})
        result=policy.run_command('cargo test')
        self.assertEqual(result['exit_code'],0,result)
        self.assertIn('SYNTHETIC_BUILD_OK',result['stdout'])
        self.assertEqual((self.root/'source.rs').read_text(),'formatted source\n')
        self.assertEqual((self.root/'Cargo.lock').read_text(),'synthetic lock\n')
        self.assertFalse((self.root/'target').exists())
        self.assertEqual((self.root/'.env').read_text(),'SYNTHETIC_TOKEN=PRIVATE')

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
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Bash(cargo *)'],
            'environment':{'PATH':str(fakebin)+':/usr/bin','HOME':str(owner)}})
        result=policy.run_command('cargo test')
        self.assertEqual(result['exit_code'],0,result)
        self.assertIn('HOME_CONTENT_DENIED',result['stdout'])

    def test_real_cargo_can_build_and_test_a_dependency_free_fixture(self):
        import shutil
        if not shutil.which('cargo'):
            self.skipTest('Cargo is not installed')
        (self.root/'src').mkdir()
        (self.root/'Cargo.toml').write_text('[package]\nname="synthetic-build"\nversion="0.1.0"\nedition="2021"\n')
        (self.root/'src/lib.rs').write_text('#[test] fn synthetic_passes() { assert_eq!(2 + 2, 4); }\n')
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Edit','Bash(cargo *)']})
        outcome=policy.run_command('cargo test --offline',timeout=60)
        self.assertEqual(outcome['exit_code'],0,outcome)
        self.assertIn('1 passed',outcome['stdout'])
        self.assertFalse((self.root/'target').exists())

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
            'build_vm_config': os.environ['JARVIS_TEST_VM_CONFIG']}
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
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Bash(cargo *)'],
            'environment':{'PATH':str(fakebin)+':/usr/bin'}})
        outcome=policy.run_command('cargo test')
        self.assertEqual(outcome['exit_code'],0,outcome)
        pid=int(outcome['stdout'].strip())
        deadline=time.monotonic()+2
        while time.monotonic()<deadline:
            try:
                state=Path(f'/proc/{pid}/stat').read_text().split(') ',1)[1].split()[0]
            except FileNotFoundError:
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

    def test_real_npm_can_run_a_dependency_free_project_test(self):
        import shutil
        if not shutil.which('npm'):
            self.skipTest('npm is not installed')
        (self.root/'package.json').write_text(json.dumps({'name':'synthetic-build','version':'1.0.0',
            'scripts':{'test':"node -e \"require('node:assert').equal(2+2,4); console.log('NPM_FIXTURE_OK')\""}}))
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
            'write_roots':[str(self.root)],'allowed_tools':['Read','Write','Edit','Bash(npm *)']})
        outcome=policy.run_command('npm test --offline',timeout=60)
        self.assertEqual(outcome['exit_code'],0,outcome)
        self.assertIn('NPM_FIXTURE_OK',outcome['stdout'])
        self.assertFalse((self.root/'.npm-cache').exists())

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
        policy=bridge.Policy({'cwd':str(self.root),'read_roots':[str(self.root)],
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
                                 'Bash(aa-gh issue *)','mcp__socialapi__*'],
                'handoff_path':str(root/'handoff.json')})
            server=bridge.Server(policy)
            calls=[]
            def read_result(name, arguments):
                calls.append((name,arguments))
                return {'content':[{'type':'text','text':str(len(calls))}]}
            server.execute=read_result
            commands=['augmentagent gmail search --query Synthetic','aa-gh issue list --search Synthetic',
                      'augmentagent repo-docs list --source synthetic']
            for command in commands:
                first=server.call('Bash',{'command':command})
                second=server.call('Bash',{'command':command})
                self.assertNotEqual(first,second,'reconciliation reads must not return stale receipts')
            server.call('mcp__socialapi__get_post',{'id':'synthetic'})
            for command in ['aa-gh issue create --title Another',
                            'augmentagent gmail compose --body Synthetic',
                            'augmentagent gmail search --query Synthetic; aa-gh issue create --title Another']:
                with self.subTest(command=command), self.assertRaises(bridge.Denied):
                    server.call('Bash',{'command':command})
            self.assertEqual(len(calls),7)
            response=server.dispatch({'method':'tools/call','params':{
                'name':'Bash','arguments':{'command':'aa-gh issue create --title PRIVATE_SYNTHETIC_TITLE'}}})
            self.assertTrue(response['isError'])
            text=response['content'][0]['text']
            self.assertIn('read-only tools',text)
            self.assertIn('uncertain outcome',text)
            self.assertNotIn('PRIVATE_SYNTHETIC_TITLE',text)

    def test_primary_read_hooks_do_not_create_uncertain_mutation_receipts(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'handoff.json'
            journal=bridge.HandoffJournal(path)
            journal.observe_hook({'hook_event_name':'PreToolUse','tool_use_id':'synthetic-read',
                'tool_name':'Bash','tool_input':{'command':'augmentagent repo-docs sources'}})
            self.assertFalse(path.exists())

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

    def test_later_uncertain_attempt_cannot_be_hidden_by_an_older_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal=bridge.HandoffJournal(Path(tmp)/'handoff.json')
            event={'hook_event_name':'PreToolUse','tool_use_id':'synthetic-call-1',
                'tool_name':'mcp__fixture__update','tool_input':{'value':'Synthetic'}}
            journal.observe_hook(event)
            journal.observe_hook(dict(event,hook_event_name='PostToolUse',tool_response='first result'))
            journal.observe_hook(dict(event,tool_use_id='synthetic-call-2'))
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


if __name__ == '__main__':
    unittest.main()
