#!/usr/bin/python3
"""Scripted GitHub boundary; the real hosted reader still validates policy/jobs."""
import base64, hashlib, json, os, pathlib, runpy, sys
root = pathlib.Path(os.environ['CI_REPLAY_POLICY_ROOT'])
fixture = pathlib.Path(os.environ['CI_REPLAY_ROOT'])
state = json.loads((fixture / 'scenario.json').read_text())
head = state['head']
contract = json.loads((root / 'scripts/ci-contract.json').read_text())
hosted = runpy.run_path(str(root / 'scripts/autoship-hosted-ci.py'))
expected = hosted['expected_jobs'](contract, ['crates/augmentagent-cli/src/fixture.rs'], 'pull_request')
paths = sorted(expected)
repository = 'fixture/project'
args = sys.argv[1:]
with (fixture / 'calls.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\n')
if args[:2] == ['repo', 'view']:
    print(repository); sys.exit(0)
if args[:2] == ['pr', 'checks']:
    print(json.dumps([{'name': 'privacy', 'state': 'SUCCESS'}, {'name': 'node-release', 'state': state.get('required_state', 'FAILURE')}])); sys.exit(0)
if args and args[0] == 'run':
    if state.get('missing_logs'): sys.exit(1)
    print(state['log']); sys.exit(0)
if not args or args[0] != 'api': sys.exit(1)
endpoint = args[1]
if '/actions/jobs/' in endpoint and endpoint.endswith('/logs'):
    if state.get('missing_logs'): sys.exit(1)
    print(state['log']); sys.exit(0)
if '/actions/jobs/' in endpoint:
    job = int(endpoint.rsplit('/', 1)[1]); run = job // 100; path = paths[run - 100]
    name = sorted(expected[path])[job % 100]
    value = {'id': job, 'run_id': run, 'run_attempt': 1, 'head_sha': head, 'name': name, 'status': 'completed', 'conclusion': state.get('job_conclusion', 'failure')}
elif endpoint.endswith('/pulls/1669'):
    value = {'head': {'sha': head}, 'base': {'sha': state['base'], 'ref': 'main', 'repo': {'full_name': repository}}, 'state': 'open', 'changed_files': 1}
elif '/pulls/1669/files' in endpoint:
    value = [{'filename': 'crates/augmentagent-cli/src/fixture.rs'}]
elif '/contents/' in endpoint:
    path = endpoint.split('/contents/', 1)[1].split('?', 1)[0]
    content = (root / path).read_bytes()
    # A head cut before main's CI policy changed carries the old policy bytes.
    if state.get('stale_policy') and endpoint.endswith('?ref=' + head):
        content += b'\n# stale policy\n'
    value = {'type': 'file', 'encoding': 'base64', 'content': base64.b64encode(content).decode()}
elif '/actions/runs?' in endpoint:
    runs = [{'id': 100 + i, 'run_attempt': 1, 'head_sha': head, 'event': 'pull_request', 'path': path, 'repository': {'full_name': repository}, 'pull_requests': [{'number': 1669}]} for i, path in enumerate(paths)]
    if state.get('pending'): runs = []
    value = {'total_count': len(runs), 'workflow_runs': runs}
elif '/actions/runs/' in endpoint and '/jobs?' in endpoint:
    run = int(endpoint.split('/actions/runs/')[1].split('/')[0]); path = paths[run - 100]
    names = sorted(expected[path])
    value = {'jobs': [{'id': run * 100 + i, 'run_id': run, 'run_attempt': 1, 'head_sha': head, 'name': name, 'status': 'completed', 'conclusion': 'failure' if name == state['job_name'] else 'success', 'html_url': f'https://github.com/{repository}/actions/runs/{run}/job/{run * 100 + i}'} for i, name in enumerate(names)]}
else:
    sys.exit(1)
print(json.dumps(value))
