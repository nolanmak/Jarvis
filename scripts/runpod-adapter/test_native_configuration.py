"""Native Linux deployment uses private paths and loopback instead of Docker."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

class NativeConfiguration(unittest.TestCase):
    def test_native_service_uses_configured_private_paths_and_loopback(self):
        with tempfile.TemporaryDirectory() as tmp:
            code = '''import runpy, http.server, json
class Server:
 def __init__(self, address, handler): self.address=address
 def serve_forever(self): print(json.dumps(self.address))
http.server.ThreadingHTTPServer=Server
state=runpy.run_path('server.py', run_name='__main__')
print(state['ROUTES'])
'''
            env = {**os.environ, 'RUNPOD_API_KEY':'synthetic-runpod', 'ADAPTER_API_KEY':'synthetic-client',
                   'RUNPOD_ADAPTER_ROUTES':tmp+'/routes.json', 'RUNPOD_ADAPTER_JOURNAL':tmp+'/journal.sqlite',
                   'RUNPOD_ADAPTER_HOST':'127.0.0.1','RUNPOD_ADAPTER_PORT':'20129'}
            result=subprocess.run([sys.executable,'-c',code],cwd=Path(__file__).parent,env=env,capture_output=True,text=True,check=True)
            lines=result.stdout.splitlines()
            self.assertEqual(json.loads(lines[0]), ['127.0.0.1',20129])
            self.assertEqual(lines[1],tmp+'/routes.json')
            self.assertTrue(Path(tmp+'/journal.sqlite').exists())
