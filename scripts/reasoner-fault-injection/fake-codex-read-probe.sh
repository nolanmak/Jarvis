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
read_id = 2
if 'PROBE_CREATE_CONTENT' in os.environ:
    write_arguments = {'file_path': path, 'content': os.environ['PROBE_CREATE_CONTENT']}
    write_result = call(2, 'tools/call', {'name': 'Write', 'arguments': write_arguments})
    print(json.dumps({'type': 'item.completed', 'item': {
        'type': 'mcp_tool_call', 'server': 'jarvis', 'tool': 'Write',
        'arguments': write_arguments, 'result': write_result,
    }}), flush=True)
    if write_result.get('isError'):
        raise RuntimeError('synthetic artifact Write was denied')
    read_id = 3
arguments = {'file_path': path}
result = call(read_id, 'tools/call', {'name': 'Read', 'arguments': arguments})
print(json.dumps({'type': 'item.completed', 'item': {
    'type': 'mcp_tool_call', 'server': 'jarvis', 'tool': 'Read',
    'arguments': arguments, 'result': result,
}}), flush=True)
if os.environ.get('PROBE_MEMORY') == '1':
    memory_arguments = {}
    memory = call(read_id + 1, 'tools/call', {
        'name': 'mcp__memory__memory_recent', 'arguments': memory_arguments,
    })
    print(json.dumps({'type': 'item.completed', 'item': {
        'type': 'mcp_tool_call', 'server': 'jarvis',
        'tool': 'mcp__memory__memory_recent',
        'arguments': memory_arguments, 'result': memory,
    }}), flush=True)
    if memory.get('isError') or not any(
        'MEMORY_FIXTURE_OK' in part.get('text', '') for part in memory.get('content', [])
    ):
        raise RuntimeError('synthetic memory tool did not return its result')
body = '\n'.join(part.get('text', '') for part in result.get('content', []))
match = re.search(r'TOOL_PROBE_[0-9a-f-]+', body)
print(json.dumps({'type': 'item.completed', 'item': {
    'type': 'agent_message', 'text': match.group(0) if match else 'READ_FAILED',
}}), flush=True)
bridge.stdin.close()
bridge.wait(timeout=10)
PY
