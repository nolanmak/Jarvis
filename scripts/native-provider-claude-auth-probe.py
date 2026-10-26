#!/usr/bin/env python3
"""Explicit Claude native transport/resume proof using declared exact policy.
No policy grants or credentials are inferred, copied or refreshed. Run in a
bounded transient unit only when model calls are explicitly authorized.
"""
import importlib.util
import json
from pathlib import Path
import re

spec = importlib.util.spec_from_file_location('native_transport_probe', Path(__file__).with_name('native-provider-codex-auth-probe.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
probe.PROVIDER = 'claude'
probe.INFERENCE_KIND = 'messages'
probe.ALLOW_WEBSOCKET = False
probe.MODEL = 'claude-opus-4-8'


def native_argv(session=None):
    # Verified against installed Claude2.1.310 --help. --bare is intentionally
    # absent because it disables OAuth, unlike --safe-mode.
    args = ['--print', '--verbose', '--output-format', 'stream-json', '--model', probe.MODEL,
        '--effort', 'high', '--safe-mode', '--tools', '', '--setting-sources', '',
        '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}', '--disable-slash-commands',
        '--system-prompt', 'Return only the requested fixed text. Never call tools or read files.']
    if session:
        args += ['--resume', session]
    return args


def validate_profile(profile):
    if profile.get('auth_relative') != '.claude/.credentials.json':
        raise ValueError('Claude read-only OAuth mount must be explicitly declared')
    authorities = profile.get('gateway', {}).get('authorities', [])
    if not authorities or any(not entry.get('routes') or any(route.get('exact') is not True
            for route in entry['routes']) for entry in authorities):
        raise ValueError('Claude proof requires explicit exact routes, never inferred prefixes')


def observations(path, expected_session=None):
    if path.stat().st_size > probe.MAX_LOG:
        raise ValueError('provider output exceeds inspection bound')
    session, models, completed, assistant, result = None, set(), 0, False, None
    with path.open('rb') as stream:
        for line in stream:
            if len(line) > 2 * 1024**2:
                raise ValueError('provider event exceeds inspection bound')
            if not line.strip():
                continue
            event = json.loads(line)
            current = event.get('session_id')
            if current is not None:
                if not isinstance(current, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,256}', current):
                    raise ValueError('invalid actual native session')
                if session is not None and session != current:
                    raise ValueError('native session changed during turn')
                session = current
            message = event.get('message', {})
            if not isinstance(message, dict):
                raise ValueError('invalid native message object')
            # UI notifications use an event string; only stream_event carries
            # the structured Anthropic stream payload. Never mistake UI data
            # for content or let malformed stream events bypass tool checks.
            nested = event.get('event', {}) if event.get('type') == 'stream_event' else {}
            if not isinstance(nested, dict) or not isinstance(nested.get('message', {}), dict) or not isinstance(nested.get('content_block', {}), dict):
                raise ValueError('invalid native stream event object')
            for observed in (event.get('model'), message.get('model'), nested.get('message', {}).get('model')):
                if observed is not None:
                    if not isinstance(observed, str):
                        raise ValueError('invalid observed model')
                    models.add(observed)
            blocks = message.get('content', [])
            if not isinstance(blocks, list):
                raise ValueError('invalid native content')
            if any(block.get('type') in ('tool_use', 'server_tool_use', 'tool_result') for block in blocks):
                raise ValueError('tool use invalidates transport-only proof')
            if nested.get('content_block', {}).get('type') in ('tool_use', 'server_tool_use', 'tool_result'):
                raise ValueError('streamed tool start invalidates transport-only proof')
            if event.get('type') == 'assistant':
                assistant = True
            if event.get('type') == 'result':
                if event.get('subtype') != 'success' or event.get('is_error') is not False:
                    raise ValueError('native result failed')
                result = event.get('result')
                completed += 1
            if event.get('type') == 'error':
                raise ValueError('native error event')
    if not session or models != {probe.MODEL} or not assistant or completed != 1 or result != probe.TOKEN:
        raise ValueError('native quality/session/completion observation incomplete')
    if expected_session is not None and expected_session != session:
        raise ValueError('resume changed actual native session')
    return {'native_session': session, 'completed_turns': completed, 'tool_calls': 0,
        'exact_reply': True, 'observed_model': probe.MODEL, 'stdout_sha256': probe.digest_file(path)}


probe.native_argv = native_argv
probe.observations = observations
probe.validate_profile = validate_profile

if __name__ == '__main__':
    try:
        raise SystemExit(probe.main())
    except Exception:
        print(json.dumps({'status': 'preflight_failed', 'detail': 'private Claude policy validation failed; no credential values emitted'}))
        raise SystemExit(1)
