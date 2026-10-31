#!/usr/bin/env python3
"""Explicit 512 MiB synthetic VM proof; no models, accounts, or real credentials."""
import argparse
import hashlib
import http.server
import importlib.util
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

BYTES = 512 * 1024


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--execute', action='store_true', required=True)
    parser.add_argument('--runtime-config', type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='native-vm-io-', dir='/tmp') as name:
        root = Path(name)
        spec = importlib.util.spec_from_file_location('probe_provision', Path(__file__).with_name('provision-native-vm-probe.py'))
        provision = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(provision)
        provision.stage_runtime(root / 'runtime', Path('/usr/bin/python3.12'))
        marker = root / 'gateway-progress'
        class Provider(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                if self.path != '/v1/probe' or self.rfile.read(int(self.headers.get('Content-Length', '0'))) != b'synthetic':
                    self.send_error(400)
                    return
                # The synthetic MCP bridge deliberately does not drain its
                # input until this request crosses both control dispatchers.
                marker.write_text('gateway completed while native pipes were full')
                self.send_response(200)
                self.send_header('Content-Length', '4')
                self.end_headers()
                self.wfile.write(b'live')
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Provider)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        port = server.server_address[1]
        bridge = r'''import sys,time
from pathlib import Path
size=int(sys.argv[2])
sys.stdout.buffer.write(b'O'*size);sys.stdout.buffer.flush()
marker=Path(sys.argv[1]);deadline=time.monotonic()+20
while not marker.exists():
    if time.monotonic()>deadline:raise SystemExit(4)
    time.sleep(.01)
data=sys.stdin.buffer.read(size)
if data!=b'I'*size:raise SystemExit(5)
sys.stdout.buffer.write(b'ACK');sys.stdout.buffer.flush()
'''
        worker = r'''import ctypes,hashlib,http.client,json,os,socket,ssl,sys,threading,time,urllib.parse
from pathlib import Path
size=int(sys.argv[1]);port=int(sys.argv[2])
assert os.getuid()!=0
print(json.dumps({'stage':'worker_started'}),flush=True)
relay=socket.socket(socket.AF_UNIX);relay.connect('/tmp/native-mcp.sock')
print(json.dumps({'stage':'relay_connected'}),flush=True)
written=threading.Event()
def send_input():
    relay.sendall(b'I'*size);written.set()
threading.Thread(target=send_input,daemon=True).start()
# Do not drain stdin or MCP output until after provider network progress.
time.sleep(.5)
proxy=urllib.parse.urlsplit(os.environ['HTTPS_PROXY'])
client=http.client.HTTPSConnection(proxy.hostname,proxy.port,context=ssl.create_default_context(),timeout=15)
client.set_tunnel('127.0.0.1',port)
client.request('POST','/v1/probe',body=b'synthetic')
response=client.getresponse();assert response.status==200 and response.read()==b'live';client.close()
print(json.dumps({'stage':'gateway_completed'}),flush=True)
received=b''
while len(received)<size+3:
    data=relay.recv(min(65536,size+3-len(received)))
    assert data;received+=data
assert received==b'O'*size+b'ACK' and written.wait(5)
print(json.dumps({'stage':'mcp_completed'}),flush=True)
relay.close()
stdin=sys.stdin.buffer.read();assert stdin==b'P'*size
print(json.dumps({'stage':'stdin_completed'}),flush=True)
home=Path.home();(home/'stdin-sha256').write_text(hashlib.sha256(stdin).hexdigest())
# Leave nondumpable descendants that continue making late forks until the
# root supervisor quiesces the entire UID. None may survive into export.
pid=os.fork()
if pid==0:
    ctypes.CDLL(None).prctl(4,0,0,0,0)
    (home/'nondumpable-parent').write_text(str(os.getpid()))
    for index in range(24):
        child=os.fork()
        if child==0:
            ctypes.CDLL(None).prctl(4,0,0,0,0)
            (home/('late-child-'+str(index))).write_text(str(os.getpid()))
            while True:time.sleep(.1)
        time.sleep(.01)
    while True:time.sleep(.1)
deadline=time.monotonic()+5
while not (home/'late-child-2').exists():
    assert time.monotonic()<deadline;time.sleep(.01)
print(json.dumps({'gateway_live':True,'stdin_bytes':len(stdin),'mcp_input_bytes':size,'mcp_output_bytes':len(received),'descendants_started':True}),flush=True)
'''
        worker = 'try:\n' + ''.join('    ' + line + '\n' for line in worker.splitlines()) + r'''
except BaseException as error:
    import json
    frames=[];trace=error.__traceback__
    while trace is not None:
        frames.append(trace.tb_frame.f_code.co_name);trace=trace.tb_next
    print(json.dumps({'synthetic_failure':True,'exception':type(error).__name__,'errno':getattr(error,'errno',None),'functions':frames}),flush=True)
    raise
'''
        session = root / 'session'
        session.mkdir(mode=0o700)
        config = {'version': 1, 'runtime_config': str(args.runtime_config), 'guest_runtime': str(root / 'runtime'),
            'memory_mb': 512, 'session_directory': str(session), 'resume': False,
            'executable': '/usr/bin/python3.12', 'argv': ['-I', '-S', '-c', worker, str(BYTES), str(port)],
            'environment': {'LANG': 'C.UTF-8'}, 'auth_file': None, 'auth_relative': '.codex/auth.json', 'assets': [],
            'mcp_argv': ['/usr/bin/python3', '-I', '-S', '-c', bridge, str(marker), str(BYTES)],
            'gateway': {'version': 1, 'authorities': [{'authority': f'127.0.0.1:{port}', 'tls': False,
                'allow_loopback': True, 'routes': [{'path_prefix': '/v1/', 'methods': ['POST'], 'websocket': False}]}],
                'limits': {'max_connections': 2, 'max_request_bytes': 1024, 'max_response_bytes': 4096,
                           'max_frame_bytes': 65536, 'idle_timeout_seconds': 20}}}
        path = root / 'spec.json'
        path.write_text(json.dumps(config));path.chmod(0o600)
        environment = {'PATH': '/usr/bin:/bin', 'JARVIS_NATIVE_VM_READY': str(root / 'ready'),
                       'JARVIS_NATIVE_VM_CONTINUE': str(root / 'continue')}
        process = subprocess.Popen([sys.executable, '-I', str(Path(__file__).with_name('native-provider-vm.py')), str(path)],
            env=environment, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 45
            while not (root / 'ready').exists():
                if process.poll() is not None:
                    raise RuntimeError('synthetic VM failed before admission')
                if time.monotonic() > deadline:
                    raise TimeoutError('synthetic VM readiness deadline')
                time.sleep(.02)
            (root / 'continue').write_text('admitted\n');(root / 'continue').chmod(0o600)
            stdout, stderr = process.communicate(b'P' * BYTES, timeout=45)
            observed = []
            for line in stdout.splitlines():
                try:
                    value = json.loads(line)
                    if isinstance(value, dict):
                        observed.append(value)
                except (ValueError, UnicodeError):
                    pass
            if process.returncode:
                allowed = {'worker_started', 'relay_connected', 'gateway_completed', 'mcp_completed', 'stdin_completed'}
                stages = [v['stage'] for v in observed if v.get('stage') in allowed]
                exceptions = re.findall(rb'^([A-Za-z]+(?:Error|Exception))(?::|$)', stderr, re.M)
                gateway_failures = []
                host_failures = []
                for line in stderr.splitlines():
                    try:
                        diagnostic = json.loads(line)
                        if isinstance(diagnostic, dict):
                            gateway_failures.extend(diagnostic.get('native_gateway_failures', []))
                            if isinstance(diagnostic.get('native_vm_failure'), str):
                                host_failures.append(diagnostic['native_vm_failure'])
                    except (ValueError, UnicodeError):
                        pass
                print(json.dumps({'passed':False,'exit':process.returncode,'stages':stages,'gateway_failures':gateway_failures,'host_failures':host_failures,'synthetic_failures':[v for v in observed if v.get('synthetic_failure') is True],
                    'exception_classes':[v.decode('ascii') for v in exceptions],
                    'stderr_sha256':hashlib.sha256(stderr).hexdigest()}),flush=True)
                raise RuntimeError('synthetic VM acceptance failed; sanitized evidence reported')
            result = observed[-1]
            assert result == {'gateway_live': True, 'stdin_bytes': BYTES, 'mcp_input_bytes': BYTES,
                              'mcp_output_bytes': BYTES + 3, 'descendants_started': True}, result
            completed = json.loads((session / 'complete.json').read_text())
            quiet = completed['worker_quiescence']
            assert quiet['quiet_scans'] >= 2 and quiet['remaining_workers'] == 0
            assert quiet['signalled'] >= 2 and quiet['reaped'] >= 2, quiet
            home = session / completed['generation']
            assert (home / 'stdin-sha256').read_text() == hashlib.sha256(b'P' * BYTES).hexdigest()
            assert (home / 'nondumpable-parent').exists() and len(list(home.glob('late-child-*'))) >= 3
            assert marker.exists()
            print(json.dumps({'passed': True, 'memory_mb': 512, 'gateway_while_stdin_and_mcp_backpressured': True,
                'exact_stream_bytes_preserved': True, 'nondumpable_descendants_quiesced': True,
                'export_requires_quiescence': True, 'quiescence': quiet}, sort_keys=True))
        finally:
            if process.poll() is None:
                process.kill();process.wait()
            server.shutdown();server.server_close()


if __name__ == '__main__':
    main()
