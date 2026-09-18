#!/usr/bin/env python3
"""Read-only preflight for the Runpod adapter and 9Router client path."""
import argparse
import json
import os
from pathlib import Path
import stat
import urllib.error
import urllib.parse
import urllib.request


class DeploymentError(Exception):
    pass


class RefuseRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def private_env(path: Path) -> dict:
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        with os.fdopen(fd, 'rb') as source:
            info = os.fstat(source.fileno())
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                    or info.st_mode & 0o077 or info.st_size > 65536):
                raise DeploymentError(f'{path.name} must be an owner-private regular file')
            raw = source.read(65537)
    except OSError as error:
        raise DeploymentError(f'{path.name} cannot be read privately') from error
    if len(raw) > 65536:
        raise DeploymentError(f'{path.name} is too large')
    settings = {}
    try:
        for line in raw.decode().splitlines():
            if not line or line.lstrip().startswith('#'):
                continue
            key, value = line.split('=', 1)
            if not key.isidentifier() or key in settings:
                raise ValueError('invalid env line')
            settings[key] = value.strip('"\'')
    except (UnicodeError, ValueError) as error:
        raise DeploymentError(f'{path.name} has invalid env syntax') from error
    return settings


def require(settings: dict, key: str) -> str:
    value = settings.get(key)
    if not value:
        raise DeploymentError(f'{key} is missing')
    return value


def approved_base(url: str, *, adapter: bool, allowed_hosts=()) -> str:
    parsed = urllib.parse.urlsplit(url)
    try:
        port = parsed.port
    except ValueError as error:
        raise DeploymentError('gateway URL is invalid') from error
    host = parsed.hostname or ''
    authority = f'{host}:{port}' if port else host
    local = host in ('127.0.0.1', 'localhost', '::1')
    remote_allowed = authority in allowed_hosts
    if (url != url.strip() or parsed.username is not None or parsed.password is not None
            or parsed.query or parsed.fragment or parsed.scheme not in ('http', 'https')
            or (adapter and (not local or parsed.path not in ('', '/')))
            or (not adapter and parsed.path.rstrip('/') != '/v1')
            or (not local and not remote_allowed)
            or (parsed.scheme == 'http' and not local and not host.endswith('.ts.net'))):
        raise DeploymentError('gateway URL is not approved')
    return url.rstrip('/')


def read_json(url: str, key: str, service: str) -> dict:
    request = urllib.request.Request(url, headers={'Authorization': f'Bearer {key}'})
    try:
        with urllib.request.build_opener(RefuseRedirect).open(request, timeout=5) as response:
            raw = response.read(65537)
    except (OSError, urllib.error.HTTPError, urllib.error.URLError) as error:
        if isinstance(error, urllib.error.HTTPError):
            error.close()
        raise DeploymentError(f'{service} is unreachable or rejected the client credential') from error
    if len(raw) > 65536:
        raise DeploymentError(f'{service} returned an oversized response')
    try:
        value = json.loads(raw)
    except (ValueError, UnicodeError) as error:
        raise DeploymentError(f'{service} returned invalid JSON') from error
    if not isinstance(value, dict):
        raise DeploymentError(f'{service} returned invalid JSON')
    return value


def model_ids(response: dict, service: str, expected: set[str]) -> list[str]:
    data = response.get('data')
    if not isinstance(data, list):
        raise DeploymentError(f'{service} returned no model catalog')
    ids = {item.get('id') for item in data if isinstance(item, dict) and isinstance(item.get('id'), str)}
    if not expected.issubset(ids):
        raise DeploymentError(f'{service} is missing a configured Runpod model')
    return sorted(ids)


def verify(adapter_env: Path, router_env: Path, adapter_url: str,
           allowed_router_hosts=()) -> dict:
    adapter_settings = private_env(adapter_env)
    router_settings = private_env(router_env)
    require(adapter_settings, 'RUNPOD_API_KEY')
    adapter_key = require(adapter_settings, 'ADAPTER_API_KEY')
    router_key = require(router_settings, 'OPENAI_API_KEY')
    router_url = require(router_settings, 'OPENAI_BASE_URL')
    adapter_base = approved_base(adapter_url, adapter=True)
    router_base = approved_base(router_url, adapter=False,
                                allowed_hosts=allowed_router_hosts)
    health = read_json(adapter_base + '/health', adapter_key, 'adapter')
    if health.get('status') != 'ok':
        raise DeploymentError('adapter health is not ready')
    adapter_models = model_ids(read_json(adapter_base + '/v1/models', adapter_key, 'adapter'),
                               'adapter', {'qwen38-27b', 'glm-5.3-flash'})
    router_models = model_ids(read_json(router_base + '/models', router_key, '9Router'),
                              '9Router', {'runpod/qwen38-27b', 'runpod/glm-5.3-flash'})
    return {'adapter_models': adapter_models, 'router_models': router_models}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--adapter-env', type=Path, required=True)
    parser.add_argument('--router-env', type=Path, required=True)
    parser.add_argument('--adapter-url', default='http://127.0.0.1:20129')
    parser.add_argument('--allow-router-host', action='append', default=[])
    args = parser.parse_args()
    try:
        print(json.dumps(verify(args.adapter_env, args.router_env, args.adapter_url,
                                args.allow_router_host)))
    except DeploymentError as error:
        parser.exit(1, f'Preflight failed: {error}\n')
