"""Exercise the built CLI MCP over stdio with an isolated DB and HTTP fixture.
Run after cargo build -p augmentagent-cli --bin augmentagent.
"""
import http.server,json,os,sqlite3,subprocess,tempfile,threading
from pathlib import Path
binary = os.environ.get('TEST_AUGMENTAGENT_BIN', str(Path(__file__).resolve().parents[2] / 'target/debug/augmentagent'))
attempt='11111111-1111-4111-8111-111111111111'
calls=[]
class Handler(http.server.BaseHTTPRequestHandler):
 protocol_version="HTTP/1.1"
 def log_message(self,*args): pass
 def do_POST(self):
  assert self.headers['Authorization']=='Bearer synthetic-db-key'
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  calls.append((self.path,body))
  if self.path.endswith('/start'):
   result={'attempt_id':attempt,'auth_url':'https://connect.composio.dev/link/synthetic','connection_id':'synthetic','entity_id':'owner'}
  else:
   assert body=={'attempt_id':attempt}
   result={'status':'ACTIVE','connected':self.path.endswith('/complete')}
  data=json.dumps(result).encode()
  self.send_response(201 if self.path.endswith('/start') else 200)
  self.send_header('Transfer-Encoding','chunked');self.end_headers()
  self.wfile.write(f'{len(data):x}\r\n'.encode()+data+b'\r\n0\r\n\r\n')
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
threading.Thread(target=server.serve_forever,daemon=True).start()
with tempfile.TemporaryDirectory() as tmp:
 db=os.path.join(tmp,'fixture.db')
 with sqlite3.connect(db) as c:
  c.execute('CREATE TABLE config (key TEXT, value TEXT)')
  c.execute('INSERT INTO config VALUES (?,?)',('dashboard_api_key','synthetic-db-key'))
 requests=[{'jsonrpc':'2.0','id':1,'method':'initialize'}, {'jsonrpc':'2.0','method':'notifications/initialized'}, {'jsonrpc':'2.0','id':2,'method':'tools/list'}]
 for i,name in enumerate(['start_auth','auth_status','complete_auth'],3):
  requests.append({'jsonrpc':'2.0','id':i,'method':'tools/call','params':{'name':name,'arguments':{'provider':'gmail'} if name=='start_auth' else {'attempt_id':attempt}}})
 result=subprocess.run([binary,'composio-auth-tool'],cwd=tmp,
  env={'PATH':os.environ['PATH'],'AUGMENTAGENT_DB':db,'DASHBOARD_PORT':str(server.server_port)},
  input=''.join(json.dumps(r)+'\n' for r in requests),text=True,capture_output=True,timeout=20,check=True)
 replies=[json.loads(line) for line in result.stdout.splitlines()]
 assert len(replies)==5
 assert {t['name'] for t in replies[1]['result']['tools']}=={'start_auth','auth_status','complete_auth'}
 for reply in replies[2:]: assert not reply['result'].get('isError'),reply
 assert json.loads(replies[-1]['result']['content'][0]['text'])['connected'] is True
 assert len(calls)==3
 print('PASS: real CLI MCP initialize/list/start/status/complete, DB credential fallback, nondefault port, and chunked responses (synthetic HTTP provider).')
server.shutdown()
