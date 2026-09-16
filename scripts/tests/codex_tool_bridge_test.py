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
            with self.assertRaises(bridge.Denied): server.tools()
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
            with self.assertRaises(bridge.Denied):
                server.dispatch({'method':'initialize','params':{}})

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
