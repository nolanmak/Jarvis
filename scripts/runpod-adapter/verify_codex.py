"""Opt-in live probe of a Runpod model through 9Router and Codex Responses.

Requires OPENAI_BASE_URL and OPENAI_API_KEY in the environment. This launches
Codex with a clean environment and never puts the key in argv or test output.
"""
import json
import os
import pathlib
import subprocess
import sys
import tempfile

MODELS = {'qwen': 'runpod/qwen38-27b', 'glm': 'runpod/glm-5.3-flash'}


def run(profile):
    model = MODELS[profile]
    base = os.environ['OPENAI_BASE_URL'].rstrip('/')
    key = os.environ['OPENAI_API_KEY']
    with tempfile.TemporaryDirectory(prefix='jarvis-codex-model-probe-') as workdir:
        instructions = pathlib.Path(workdir) / 'instructions.md'
        instructions.write_text('Answer the user concisely. Do not use tools for this probe.\n')
        overrides = [
            'model_provider="augmentagent_router"',
            'model_providers.augmentagent_router.name="9Router"',
            f'model_providers.augmentagent_router.base_url={json.dumps(base)}',
            'model_providers.augmentagent_router.wire_api="responses"',
            'model_providers.augmentagent_router.env_key="AUGMENTAGENT_ROUTER_API_KEY"',
            'model_providers.augmentagent_router.requires_openai_auth=false',
            'model_providers.augmentagent_router.http_headers={ "X-9Router-Token-Saver" = "off" }',
        ]
        args = ['codex', 'exec', '--json', '--skip-git-repo-check',
                '--ignore-user-config', '--ignore-rules', '--ephemeral', '--strict-config',
                '-c', 'approval_policy=never', '-c', 'project_doc_max_bytes=0',
                '-c', f'model_instructions_file={json.dumps(str(instructions))}', '-m', model]
        for override in overrides:
            args.extend(['-c', override])
        args.extend(['-C', workdir, '-'])
        env = {name: value for name, value in os.environ.items()
               if name in ('HOME', 'PATH', 'USER', 'LOGNAME', 'TERM', 'LANG', 'SHELL')}
        env['AUGMENTAGENT_ROUTER_API_KEY'] = key
        completed = subprocess.run(args, input='Say READY only.\n', text=True,
                                   capture_output=True, env=env, timeout=360)
        answers = []
        for line in completed.stdout.splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            item = event.get('item', {})
            if event.get('type') == 'item.completed' and item.get('type') == 'agent_message':
                answers.append(item.get('text', ''))
        if completed.returncode != 0 or not any(text.strip() for text in answers):
            print(f'{model}: failed: exit={completed.returncode}, assistant_messages={len(answers)}', file=sys.stderr)
            raise SystemExit(1)
        print(f'{model}: {answers[-1].strip()}')


if __name__ == '__main__':
    if len(sys.argv) != 2 or sys.argv[1] not in MODELS:
        raise SystemExit('usage: verify_codex.py qwen|glm')
    run(sys.argv[1])
