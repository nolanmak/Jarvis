#!/usr/bin/env python3
"""Exercise the packaged adapter with paused synthetic routes and private state."""
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid


COMPOSE = Path(__file__).with_name('compose.yaml')
CLIENT_KEY = 'synthetic-compose-client-key'


def docker_compose(project, env, *args):
    return subprocess.run(['docker', 'compose', '-f', str(COMPOSE), '-p', project, *args],
                          env=env, capture_output=True, text=True, timeout=120)


def read_status(url, key=None, body=None):
    headers = {'Content-Type': 'application/json'}
    if key:
        headers['Authorization'] = 'Bearer ' + key
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                     headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=2) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        with error:
            return error.code, json.load(error)


def main():
    bare_env = {name: value for name, value in os.environ.items()
                if not name.startswith('RUNPOD_ADAPTER_')}
    missing = docker_compose('jarvis-adapter-config-check', bare_env, 'config', '--quiet')
    if missing.returncode == 0:
        raise AssertionError('missing private deployment inputs were accepted')
    with tempfile.TemporaryDirectory(prefix='jarvis-adapter-compose-') as scratch:
        root = Path(scratch)
        state = root / 'state'
        state.mkdir(mode=0o700)
        routes = root / 'routes.json'
        routes.write_text(json.dumps({
            'qwen38-27b': {'type': 'ollama-queue',
                           'base_url': 'https://api.runpod.ai/v2/synthetic',
                           'enabled': False, 'disabled_reason': 'Synthetic route is paused'},
            'glm-5.3-flash': {'type': 'openai',
                              'base_url': 'https://synthetic.api.runpod.ai/openai/v1',
                              'enabled': False, 'disabled_reason': 'Synthetic route is paused'},
        }))
        credentials = root / 'adapter.env'
        credentials.write_text('RUNPOD_API_KEY=synthetic-compose-runpod-key\n'
                               f'ADAPTER_API_KEY={CLIENT_KEY}\n')
        credentials.chmod(0o600)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        project = 'jarvis-adapter-qa-' + uuid.uuid4().hex[:8]
        image = 'runpod-openai-adapter:compose-qa-' + uuid.uuid4().hex[:8]
        env = {**bare_env,
               'RUNPOD_ADAPTER_UID': str(int(os.environ.get('JARVIS_TEST_CONTAINER_UID', os.getuid()))),
               'RUNPOD_ADAPTER_ENV_FILE': str(credentials),
               'RUNPOD_ADAPTER_ROUTES_FILE': str(routes),
               'RUNPOD_ADAPTER_STATE_DIR': str(state),
               'RUNPOD_ADAPTER_PORT': str(port),
               'RUNPOD_ADAPTER_IMAGE': image}
        try:
            started = docker_compose(project, env, 'up', '-d', '--build')
            if started.returncode:
                raise AssertionError('packaged adapter did not start: ' + started.stderr[-1000:])
            published = docker_compose(project, env, 'port', 'adapter', '8000')
            if published.returncode or not published.stdout.strip().startswith('127.0.0.1:'):
                raise AssertionError('adapter was not bound to loopback')
            base = f'http://127.0.0.1:{port}'
            for _ in range(40):
                try:
                    status, health = read_status(base + '/health')
                    if status == 200 and health.get('status') == 'ok':
                        break
                except (OSError, ValueError):
                    pass
                time.sleep(0.25)
            else:
                raise AssertionError('packaged adapter did not become healthy')
            status, _ = read_status(base + '/v1/models')
            if status != 401:
                raise AssertionError('model catalog accepted an unauthenticated client')
            status, catalog = read_status(base + '/v1/models', CLIENT_KEY)
            if status != 200 or {item['id'] for item in catalog['data']} != {'qwen38-27b', 'glm-5.3-flash'}:
                raise AssertionError('packaged adapter lost its configured model routes')
            for model in ('qwen38-27b', 'glm-5.3-flash'):
                status, _ = read_status(base + '/v1/chat/completions', CLIENT_KEY,
                                        {'model': model, 'messages': [{'role': 'user', 'content': 'Synthetic'}]})
                if status != 503:
                    raise AssertionError(f'{model} was not paused before inference')
            journal = state / 'jobs.sqlite3'
            if (not journal.is_file() or stat.S_IMODE(state.stat().st_mode) != 0o700
                    or stat.S_IMODE(journal.stat().st_mode) != 0o600):
                raise AssertionError('private persistent job journal was not created')
            print(json.dumps({'catalog': ['glm-5.3-flash', 'qwen38-27b'],
                              'paused_routes': 2, 'journal_private': True,
                              'loopback_only': True}))
        finally:
            docker_compose(project, env, 'down', '--remove-orphans')
            subprocess.run(['docker', 'image', 'rm', image], capture_output=True, timeout=30)


if __name__ == '__main__':
    main()
