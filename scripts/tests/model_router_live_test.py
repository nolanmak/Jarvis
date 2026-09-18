"""Opt-in installed 9Router QA. Only synthetic accounts and local model replies.
JARVIS_TEST_MODEL_ROUTER_CONFIG=/path/model-router.json python3 -m unittest discover -s scripts/tests -p model_router_live_test.py -v
"""
import http.server
import json
import os
import threading
import unittest
import urllib.error
import urllib.request
import uuid
from pathlib import Path


@unittest.skipUnless(os.environ.get('JARVIS_TEST_MODEL_ROUTER_CONFIG'), 'requires explicitly selected local 9Router')
class RouterFailover(unittest.TestCase):
    def test_account_quota_failover_and_disable(self):
        config = json.loads(Path(os.environ['JARVIS_TEST_MODEL_ROUTER_CONFIG']).read_text())
        base = config['base_url'].removesuffix('/v1')
        self.assertTrue(base.startswith('http://127.0.0.1:'))
        seen = []
        class Upstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length',0)))
                key = self.headers.get('Authorization')
                seen.append(key)
                if key == 'Bearer exhausted-synthetic-account':
                    status,body=429,{'error':{'message':'synthetic quota exhausted','type':'rate_limit_error'}}
                else:
                    status,body=200,{'id':'qa','object':'chat.completion','created':1,'model':'qa-model','choices':[{'index':0,'message':{'role':'assistant','content':'ACCOUNT_TWO_OK'},'finish_reason':'stop'}],'usage':{'prompt_tokens':1,'completion_tokens':1,'total_tokens':2}}
                data=json.dumps(body).encode()
                self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Upstream)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        cookie=None
        def api(endpoint, body=None, method=None, inference=False):
            headers={'Content-Type':'application/json'}
            if cookie: headers['Cookie']=cookie
            if inference: headers['Authorization']='Bearer '+config['api_key']
            req=urllib.request.Request(base+endpoint,data=None if body is None else json.dumps(body).encode(),headers=headers,method=method)
            with urllib.request.urlopen(req,timeout=30) as response:
                return json.load(response),response.headers.get('Set-Cookie','').split(';')[0]
        node=None
        try:
            _,cookie=api('/api/auth/login',{'password':config['admin_password']})
            prefix='qa-'+uuid.uuid4().hex[:10]
            node,_=api('/api/provider-nodes',{'name':'Synthetic account failover QA','prefix':prefix,'apiType':'chat','baseUrl':f'http://127.0.0.1:{server.server_port}/v1'})
            provider=node['node']['id']
            api('/api/providers',{'provider':provider,'name':'Synthetic exhausted account','apiKey':'exhausted-synthetic-account','priority':1})
            second,_=api('/api/providers',{'provider':provider,'name':'Synthetic healthy account','apiKey':'healthy-synthetic-account','priority':2})
            request={'model':prefix+'/qa-model','messages':[{'role':'user','content':'Synthetic test only'}],'stream':False}
            response,_=api('/v1/chat/completions',request,inference=True)
            self.assertEqual(response['choices'][0]['message']['content'],'ACCOUNT_TWO_OK')
            self.assertEqual(seen,['Bearer exhausted-synthetic-account','Bearer healthy-synthetic-account'])
            api('/api/providers/'+second['connection']['id'],{'isActive':False},method='PUT')
            with self.assertRaises(urllib.error.HTTPError) as error:
                api('/v1/chat/completions',request,inference=True)
            self.assertIn(error.exception.code,[429,503])
            api('/api/providers/'+second['connection']['id'],{'isActive':True},method='PUT')
            response,_=api('/v1/chat/completions',request,inference=True)
            self.assertEqual(response['choices'][0]['message']['content'],'ACCOUNT_TWO_OK')
        finally:
            if node: api('/api/provider-nodes/'+node['node']['id'],method='DELETE')
            server.shutdown();server.server_close();thread.join()

    def test_chat_completion_forwards_idempotency_key(self):
        config = json.loads(Path(os.environ['JARVIS_TEST_MODEL_ROUTER_CONFIG']).read_text())
        base = config['base_url'].removesuffix('/v1')
        self.assertTrue(base.startswith('http://127.0.0.1:'))
        upstream_host = config.get('upstream_host', '127.0.0.1')
        seen = []

        class Upstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', 0)))
                seen.append((self.headers.get('Idempotency-Key'), sorted(self.headers.keys())))
                body = ({'id': 'qa', 'object': 'chat.completion', 'created': 1,
                         'model': 'qa-model', 'choices': [{'index': 0,
                         'message': {'role': 'assistant', 'content': 'SYNTHETIC_OK'},
                         'finish_reason': 'stop'}]} if len(seen) == 1 else
                        {'error': {'message': 'synthetic reconciliation required',
                                   'type': 'reconciliation_required'}})
                data = json.dumps(body).encode()
                self.send_response(200 if len(seen) == 1 else 409)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        bind_host = '0.0.0.0' if upstream_host == 'host.docker.internal' else '127.0.0.1'
        server = http.server.ThreadingHTTPServer((bind_host, 0), Upstream)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        cookie = None

        def api(endpoint, body=None, method=None, inference=False, idempotency_key=None):
            headers = {'Content-Type': 'application/json'}
            if cookie:
                headers['Cookie'] = cookie
            if inference:
                headers['Authorization'] = 'Bearer ' + config['api_key']
            if idempotency_key:
                headers['Idempotency-Key'] = idempotency_key
            request = urllib.request.Request(base + endpoint,
                    data=None if body is None else json.dumps(body).encode(),
                    headers=headers, method=method)
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response), response.headers.get('Set-Cookie', '').split(';')[0]

        node = None
        try:
            _, cookie = api('/api/auth/login', {'password': config['admin_password']})
            prefix = 'qa-' + uuid.uuid4().hex[:10]
            node, _ = api('/api/provider-nodes', {'name': 'Synthetic idempotency QA',
                            'prefix': prefix, 'apiType': 'chat',
                            'baseUrl': f'http://{upstream_host}:{server.server_port}/v1'})
            provider = node['node']['id']
            api('/api/providers', {'provider': provider, 'name': 'Synthetic idempotency account',
                                   'apiKey': 'synthetic-idempotency-account'})
            body = {'model': prefix + '/qa-model', 'messages': [{'role': 'user',
                    'content': 'Synthetic idempotency test only'}], 'stream': False}
            result, _ = api('/v1/chat/completions', body, inference=True,
                            idempotency_key='synthetic-logical-turn-1')
            self.assertEqual(result['choices'][0]['message']['content'], 'SYNTHETIC_OK')
            body['messages'][0]['content'] = 'Synthetic 409 retry test only'
            with self.assertRaises(urllib.error.HTTPError) as response:
                api('/v1/chat/completions', body, inference=True)
            try:
                self.assertEqual(len(seen), 2, '9Router retried a reconciliation-required response')
                self.assertEqual((seen[0][0], response.exception.code),
                                 ('synthetic-logical-turn-1', 409),
                                 '9Router must preserve the request key and reconciliation status')
            finally:
                response.exception.close()
        finally:
            if node:
                api('/api/provider-nodes/' + node['node']['id'], method='DELETE')
            server.shutdown()
            server.server_close()
            thread.join()


@unittest.skipUnless(os.environ.get('JARVIS_TEST_ROUTER_AGENT_BIN'), 'requires built agent and installed Claude/Codex CLIs')
class RealCliTransport(unittest.TestCase):
    def test_both_real_clis_use_gateway_auth_and_streams(self):
        import subprocess
        import tempfile
        seen=[]
        class Gateway(http.server.BaseHTTPRequestHandler):
            def log_message(self,*args): pass
            def do_POST(self):
                body=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or b'{}')
                seen.append((self.path,dict(self.headers),body))
                if 'count_tokens' in self.path:
                    data=b'{"input_tokens":10}';self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data);return
                self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Connection','close');self.end_headers()
                def event(kind,data): self.wfile.write(('event: '+kind+'\ndata: '+json.dumps(data)+'\n\n').encode());self.wfile.flush()
                if 'messages' in self.path:
                    event('message_start',{'type':'message_start','message':{'id':'msg_qa','type':'message','role':'assistant','model':body.get('model'),'content':[],'stop_reason':None,'usage':{'input_tokens':10,'output_tokens':0}}})
                    event('content_block_start',{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}})
                    event('content_block_delta',{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'ROUTER_SMOKE_OK'}})
                    event('content_block_stop',{'type':'content_block_stop','index':0})
                    event('message_delta',{'type':'message_delta','delta':{'stop_reason':'end_turn','stop_sequence':None},'usage':{'output_tokens':5}})
                    event('message_stop',{'type':'message_stop'})
                else:
                    item={'id':'msg_qa','type':'message','role':'assistant','status':'completed','content':[{'type':'output_text','text':'ROUTER_SMOKE_OK','annotations':[]}]}
                    response={'id':'resp_qa','object':'response','created_at':1,'status':'completed','model':body.get('model'),'output':[item],'usage':{'input_tokens':10,'output_tokens':5,'total_tokens':15}}
                    event('response.created',{'type':'response.created','response':{**response,'status':'in_progress','output':[]}})
                    event('response.output_item.added',{'type':'response.output_item.added','output_index':0,'item':{**item,'status':'in_progress','content':[]}})
                    event('response.content_part.added',{'type':'response.content_part.added','item_id':'msg_qa','output_index':0,'content_index':0,'part':{'type':'output_text','text':'','annotations':[]}})
                    event('response.output_text.delta',{'type':'response.output_text.delta','item_id':'msg_qa','output_index':0,'content_index':0,'delta':'ROUTER_SMOKE_OK'})
                    event('response.output_item.done',{'type':'response.output_item.done','output_index':0,'item':item})
                    event('response.completed',{'type':'response.completed','response':response})
                self.close_connection=True
        server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Gateway)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                cfg=Path(tmp)/'router.json'
                value={'version':1,'mode':'codex','base_url':f'http://127.0.0.1:{server.server_port}/v1','api_key':'synthetic-router-key','models':{'claude':{'quality':'cc/claude-opus-4-8','fast':'cc/claude-haiku-4-5'},'codex':{'quality':'cx/gpt-5.6-terra','fast':'cx/gpt-5.6-luna'}}}
                for provider,endpoint in [('codex','responses'),('claude','messages')]:
                    with self.subTest(provider=provider):
                        value['mode']=provider;cfg.write_text(json.dumps(value));cfg.chmod(0o600)
                        env={**os.environ,'AUGMENTAGENT_MODEL_ROUTER_CONFIG':str(cfg),'AUGMENTAGENT_REASONER_CHAIN':provider,'XDG_STATE_HOME':tmp,'AUGMENTAGENT_REASONER_TIMEOUT_SECS':'45','CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1'}
                        result=subprocess.run([os.environ['JARVIS_TEST_ROUTER_AGENT_BIN'],'reasoner-selftest','--prompt','Reply ROUTER_SMOKE_OK'],env=env,cwd=tmp,text=True,capture_output=True,timeout=60)
                        self.assertEqual(result.returncode,0,(result.stdout+result.stderr)[-5000:])
                        self.assertIn('response: ROUTER_SMOKE_OK',result.stdout)
                        calls=[(h,b) for p,h,b in seen if p.split('?')[0].endswith('/'+endpoint)]
                        self.assertTrue(calls,'CLI never reached the gateway')
                        headers,body=calls[-1]
                        self.assertEqual(body['model'],value['models'][provider]['quality'])
                        headers={k.lower():v for k,v in headers.items()}
                        self.assertIn('synthetic-router-key',[headers.get('x-api-key'),headers.get('authorization','').removeprefix('Bearer ')])
        finally:
            server.shutdown();server.server_close();thread.join()

if __name__ == '__main__': unittest.main()
