"""Opt-in real provider discovery contract; synthetic MCP data only."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest


@unittest.skipUnless(os.environ.get('JARVIS_TEST_LIVE_DISCOVERY'), 'requires explicit live Claude QA')
class LiveDiscoveryTests(unittest.TestCase):
    def test_uncertain_effect_does_not_block_discovery_or_fresh_history_reads(self):
        helper = Path(__file__).resolve().parents[1] / 'codex-tool-bridge.py'
        with tempfile.TemporaryDirectory(prefix='jarvis-discovery-fixture-') as directory:
            root = Path(directory)
            journal = root / 'operations.json'
            journal.write_text(json.dumps({'version': 1, 'operations': [
                {'tool': 'mcp__fixture__write', 'arguments': {}, 'status': 'started'}]}))
            journal.chmod(0o600)
            before = journal.read_bytes()
            server = root / 'memory.py'
            server.write_text('''import json,sys,pathlib
root=pathlib.Path(__file__).parent
count=0
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r: continue
 m=r.get('method')
 if m=='initialize': v={'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'memory','version':'1'}}
 elif m=='tools/list':
  v={'tools':[{'name':n,'description':'Read synthetic conversation history.','inputSchema':{'type':'object','properties':{'query':{'type':'string'}},'required':['query']}} for n in ['search_conversation_history','read_conversation_thread']]}
 elif m=='tools/call':
  count+=1
  with (root/'calls.jsonl').open('a') as f: f.write(json.dumps(r['params'])+'\\n')
  v={'content':[{'type':'text','text':'SYNTHETIC_FRESH_RECEIPT_'+str(count)}]}
 else: v={}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)
''')
            observer = root / 'observe.py'
            observer.write_text("import json,sys,pathlib\ne=json.load(sys.stdin)\nwith (pathlib.Path(__file__).parent/'observed.jsonl').open('a') as f: f.write(json.dumps({'tool':e.get('tool_name'),'phase':e.get('hook_event_name')})+'\\n')\n")
            mcp = root / 'mcp.json'
            mcp.write_text(json.dumps({'mcpServers': {'memory': {'command': 'python3', 'args': ['-I', str(server)]}}}))
            command = 'python3 -I ' + shlex.quote(str(helper)) + ' --handoff-hook ' + shlex.quote(str(journal)) + ' || exit 2'
            settings = root / 'settings.json'
            settings.write_text(json.dumps({'hooks': {event: [{'matcher': '.*', 'hooks': [
                {'type': 'command', 'command': command},
                {'type': 'command', 'command': 'python3 -I ' + shlex.quote(str(observer))}]}]
                for event in ['PreToolUse', 'PostToolUse', 'PostToolUseFailure']}}))
            environment = {key: value for key, value in os.environ.items()
                           if key in ('HOME', 'PATH', 'USER', 'LANG', 'XDG_RUNTIME_DIR')}
            environment['ENABLE_TOOL_SEARCH'] = 'true'
            result = subprocess.run(['claude', '-p', '--tools', 'ToolSearch', '--allowedTools',
                'ToolSearch,mcp__memory__search_conversation_history,mcp__memory__read_conversation_thread',
                '--strict-mcp-config', '--mcp-config', str(mcp), '--settings', str(settings),
                '--setting-sources', '', '--no-session-persistence', '--model', 'haiku', '--output-format', 'json',
                'Run this synthetic read-only tool check. First use ToolSearch to discover both memory history tools. '
                'Call search_conversation_history twice sequentially with query=synthetic to verify fresh results, '
                'then call read_conversation_thread with query=synthetic. Return the three actual receipts.'],
                cwd=root, env=environment, capture_output=True, text=True, timeout=180)
            self.assertEqual(result.returncode, 0, result.stderr[-1000:])
            answer = json.loads(result.stdout)
            self.assertFalse(answer.get('is_error'), answer.get('result'))
            calls = [json.loads(line) for line in (root / 'calls.jsonl').read_text().splitlines()]
            self.assertEqual([call['name'] for call in calls],
                ['search_conversation_history', 'search_conversation_history', 'read_conversation_thread'])
            for n in range(1, 4):
                self.assertIn('SYNTHETIC_FRESH_RECEIPT_' + str(n), answer['result'])
            events = [json.loads(line) for line in (root / 'observed.jsonl').read_text().splitlines()]
            self.assertTrue(any(e['tool'] == 'ToolSearch' and e['phase'] == 'PreToolUse' for e in events))
            self.assertEqual(journal.read_bytes(), before)


if __name__ == '__main__':
    unittest.main()
