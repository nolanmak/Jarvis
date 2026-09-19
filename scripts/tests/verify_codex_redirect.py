#!/usr/bin/env python3
"""Prove the real Codex Responses client strips a router key on redirect.

Uses two local synthetic HTTP servers and a throwaway credential. No model,
Runpod job, owner config, or external network request is involved.
"""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading


SECRET = 'synthetic-redirect-credential'


def run_probe(client=subprocess.run):
    seen = {'origin': [], 'redirect': []}

    class Redirect(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            seen['origin'].append(self.headers.get('Authorization') == 'Bearer ' + SECRET
                                  and SECRET.encode() not in body)
            self.send_response(307)
            self.send_header('Location', f'http://localhost:{sink.server_port}/v1/responses')
            self.send_header('Content-Length', '0')
            self.end_headers()

    class Sink(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            headers = json.dumps(dict(self.headers.items())).encode()
            seen['redirect'].append(SECRET.encode() in body or SECRET.encode() in headers)
            response = json.dumps({
                'id': 'resp_synthetic', 'object': 'response', 'created_at': 1,
                'status': 'completed', 'model': 'synthetic-model',
                'output': [{'type': 'message', 'role': 'assistant',
                            'content': [{'type': 'output_text', 'text': 'READY'}]}],
            }).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(response)))
            self.end_headers()
            self.wfile.write(response)

    origin = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Redirect)
    sink = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Sink)
    threads = [threading.Thread(target=server.serve_forever, daemon=True)
               for server in (origin, sink)]
    for thread in threads:
        thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='jarvis-redirect-probe-') as workdir:
            instructions = Path(workdir) / 'instructions.md'
            instructions.write_text('Answer concisely. Use no tools.\n')
            base = f'http://127.0.0.1:{origin.server_port}/v1'
            overrides = [
                'model_provider="synthetic_router"',
                'model_providers.synthetic_router.name="Synthetic Router"',
                f'model_providers.synthetic_router.base_url={json.dumps(base)}',
                'model_providers.synthetic_router.wire_api="responses"',
                'model_providers.synthetic_router.env_key="SYNTHETIC_ROUTER_KEY"',
                'model_providers.synthetic_router.requires_openai_auth=false',
            ]
            command = [
                'codex', 'exec', '--json', '--skip-git-repo-check',
                '--ignore-user-config', '--ignore-rules', '--ephemeral',
                '--strict-config', '-c', 'approval_policy=never',
                '-c', 'project_doc_max_bytes=0',
                '-c', f'model_instructions_file={json.dumps(str(instructions))}',
                '-m', 'synthetic-model',
            ]
            for override in overrides:
                command.extend(['-c', override])
            command.extend(['-C', workdir, '-'])
            env = {name: value for name, value in os.environ.items()
                   if name in ('HOME', 'PATH', 'USER', 'LOGNAME', 'TERM', 'LANG', 'SHELL')}
            env['SYNTHETIC_ROUTER_KEY'] = SECRET
            client(command, input='Say READY only.\n', text=True,
                   capture_output=True, env=env, timeout=45, check=False)
        if not seen['origin'] or not all(seen['origin']):
            raise AssertionError('Codex did not send the test key only to the configured origin')
        if not seen['redirect']:
            raise AssertionError('Codex did not follow the cross-origin redirect')
        if any(seen['redirect']):
            raise AssertionError('Codex forwarded the router key across an origin change')
        return {'origin_requests': len(seen['origin']),
                'redirect_requests': len(seen['redirect']),
                'credential_forwarded': False}
    finally:
        for server in (origin, sink):
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(timeout=2)


if __name__ == '__main__':
    print(json.dumps(run_probe()))
