#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
fake_count codex
PROBE_INPUT=$(cat)
export PROBE_INPUT

exec python3 -I - "$@" <<'PY'
import json
import os
import re
import select
import subprocess
import sys

prompt = os.environ['PROBE_INPUT']
path = next(line.removeprefix('TOOL_PROBE_FILE: ') for line in prompt.splitlines()
            if line.startswith('TOOL_PROBE_FILE: '))
spec = next(arg for arg in sys.argv[1:] if arg.startswith('mcp_servers.jarvis='))
args = json.loads('[' + spec.split('args=[', 1)[1].split(']', 1)[0] + ']')
bridge = subprocess.Popen(['python3', *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE)

def call(identifier, method, params):
    bridge.stdin.write((json.dumps({'jsonrpc': '2.0', 'id': identifier,
                                    'method': method, 'params': params}) + '\n').encode())
    bridge.stdin.flush()
    if not select.select([bridge.stdout], [], [], 30)[0]:
        raise TimeoutError(method)
    reply = json.loads(bridge.stdout.readline())
    return reply['result']

call(1, 'initialize', {})
arguments = {'file_path': path}
result = call(2, 'tools/call', {'name': 'Read', 'arguments': arguments})
print(json.dumps({'type': 'item.completed', 'item': {
    'type': 'mcp_tool_call', 'server': 'jarvis', 'tool': 'Read',
    'arguments': arguments, 'result': result,
}}), flush=True)
body = '\n'.join(part.get('text', '') for part in result.get('content', []))
match = re.search(r'TOOL_PROBE_[0-9a-f-]+', body)
print(json.dumps({'type': 'item.completed', 'item': {
    'type': 'agent_message', 'text': match.group(0) if match else 'READ_FAILED',
}}), flush=True)
bridge.stdin.close()
bridge.wait(timeout=10)
PY
