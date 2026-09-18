#!/usr/bin/env python3
"""Install the pinned local 9Router service and provision private dashboard access.

Run as the same user as the agent. Existing accounts and routing are preserved.
The daemon's provider chain must contain claude,codex (see docs/model-router.md).
"""
import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import time
import urllib.request

REVISION = '17c4cc76877bd1755030a8414f8d0083f48dcccf'
REPO = 'https://github.com/decolua/9router.git'
ROOT = Path(__file__).resolve().parent.parent


def private_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    tmp = path.with_name(path.name + '.' + secrets.token_hex(8))
    try:
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as out:
            out.write(content)
            out.flush()
            os.fsync(out.fileno())
        os.replace(tmp, path)
    finally:
        tmp.unlink(missing_ok=True)


def api(endpoint, body=None, cookie=None):
    headers = {'Content-Type': 'application/json'}
    if cookie:
        headers['Cookie'] = cookie
    request = urllib.request.Request('http://127.0.0.1:20128' + endpoint,
                                     data=None if body is None else json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(request, timeout=15) as response:
        return json.load(response), response.headers.get('Set-Cookie', '').split(';')[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--built-source', type=Path, help='Reuse a checkout built at the pinned revision (for local development)')
    args = parser.parse_args()
    if sys.platform != 'linux':
        raise SystemExit('This installer uses systemd user services and requires Linux.')
    config_root = Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config')) / 'augmentagent'
    data_root = Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')) / 'augmentagent/9router'
    runtime = data_root / (REVISION + '-runpod-reconciliation-1')
    runtime.mkdir(parents=True, exist_ok=True)
    if not (runtime / 'custom-server.js').exists():
        source = args.built_source or data_root / ('source-' + REVISION)
        if not source.exists():
            subprocess.run(['git','clone',REPO,str(source)],check=True)
            subprocess.run(['git','checkout','--detach',REVISION],cwd=source,check=True)
        actual = subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip()
        if actual != REVISION:
            raise SystemExit('Source checkout does not match the pinned revision')
        router_patch = ROOT / 'sidecars/9router/runpod-reconciliation.patch'
        already_applied = subprocess.run(['git','apply','--reverse','--check',str(router_patch)],
                                         cwd=source,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode == 0
        if args.built_source:
            if not already_applied:
                raise SystemExit('Built 9Router source is missing the Runpod reconciliation patch')
        elif not already_applied:
            subprocess.run(['git','apply','--check',str(router_patch)],cwd=source,check=True)
            subprocess.run(['git','apply',str(router_patch)],cwd=source,check=True)
        if not args.built_source:
            shutil.copyfile(ROOT / 'sidecars/9router/package-lock.json',source / 'package-lock.json')
            subprocess.run(['npm','ci','--no-audit','--no-fund'],cwd=source,check=True)
            subprocess.run(['npm','run','build'],cwd=source,check=True)
        built = source / '.next/standalone'
        if not (built / 'custom-server.js').is_file():
            raise SystemExit('Missing standalone build')
        shutil.copytree(built,runtime,dirs_exist_ok=True)
    service_env = config_root / '9router.env'
    if not service_env.exists():
        private_write(service_env, 'JWT_SECRET=' + secrets.token_hex(32) + '\nINITIAL_PASSWORD=' + secrets.token_urlsafe(32) + '\n')
    secrets_map = dict(line.split('=',1) for line in service_env.read_text().splitlines() if '=' in line)
    unit_dir = Path(os.environ.get('XDG_CONFIG_HOME',Path.home()/'.config')) / 'systemd/user'
    unit_dir.mkdir(parents=True,exist_ok=True)
    node = shutil.which('node')
    if not node:
        raise SystemExit('Node.js 22+ is required')
    # Quoted systemd strings use JSON-compatible escaping for these paths.
    unit = f'''[Unit]
Description=AugmentAgent local model account router (9Router)
After=network-online.target

[Service]
Type=simple
WorkingDirectory={str(runtime).replace("%", "%%")}
EnvironmentFile={str(service_env).replace("%", "%%")}
Environment=NODE_ENV=production
Environment=HOSTNAME=127.0.0.1
Environment=PORT=20128
Environment=NEXT_TELEMETRY_DISABLED=1
Environment=ENABLE_REQUEST_LOGS=false
Environment="DATA_DIR={data_root / 'data'}"
ExecStart={json.dumps(node)} {json.dumps(str(runtime / 'custom-server.js'))}
Restart=on-failure
RestartSec=5
UMask=0077
NoNewPrivileges=true

[Install]
WantedBy=default.target
'''
    private_write(unit_dir/'augmentagent-model-router.service',unit)
    subprocess.run(['systemd-analyze','--user','verify',str(unit_dir/'augmentagent-model-router.service')],check=True)
    subprocess.run(['systemctl','--user','daemon-reload'],check=True)
    subprocess.run(['systemctl','--user','enable','augmentagent-model-router.service'],check=True)
    subprocess.run(['systemctl','--user','restart','augmentagent-model-router.service'],check=True)
    for _ in range(45):
        try:
            api('/api/health')
            break
        except (OSError,ValueError):
            time.sleep(1)
    else:
        raise SystemExit('9Router did not become healthy; inspect its systemd journal')
    _, cookie = api('/api/auth/login',{'password':secrets_map['INITIAL_PASSWORD']})
    if not cookie.startswith('auth_token='):
        raise SystemExit('Could not authenticate to the local router')
    # Disable all prompt rewriting and automatic cross-provider capacity adapters.
    request = urllib.request.Request('http://127.0.0.1:20128/api/settings', method='PATCH',
        headers={'Cookie':cookie,'Content-Type':'application/json'},data=json.dumps({
            'requireLogin':True,'requireApiKey':True,'cloudEnabled':False,'tunnelEnabled':False,
            'rtkEnabled':False,'headroomEnabled':False,'cavemanEnabled':False,'ponytailEnabled':False,'pxpipeEnabled':False,
            'fallbackStrategy':'fill-first','providerStrategies':{'claude':{'fallbackStrategy':'fill-first'},'codex':{'fallbackStrategy':'fill-first'}},
            'capacityAdapter':{k:{'enabled':False,'models':[]} for k in ['vision','pdf','audioInput','videoInput']},
        }).encode())
    with urllib.request.urlopen(request,timeout=15) as response:
        response.read()
    config_path = Path(os.environ.get('AUGMENTAGENT_MODEL_ROUTER_CONFIG',config_root/'model-router.json'))
    if not config_path.exists():
        key,_ = api('/api/keys',{'name':'AugmentAgent'},cookie)
        private_write(config_path,json.dumps({
            'version':1,'mode':'direct','base_url':'http://127.0.0.1:20128/v1',
            'api_key':key['key'],'admin_password':secrets_map['INITIAL_PASSWORD'],
            'models':{'claude':{'quality':'cc/claude-opus-4-8','fast':'cc/claude-haiku-4-5-20251001'},
                      'codex':{'quality':'cx/gpt-5.6-terra','fast':'cx/gpt-5.6-luna'}},
        },indent=2)+'\n')
    print('9Router is running on 127.0.0.1:20128. Credentials were stored privately.')
    print('Open Settings → Models & accounts in the agent dashboard to connect each account.')
    print('Routing stays on existing CLI accounts until you choose a route. See docs/model-router.md.')


if __name__ == '__main__':
    main()
