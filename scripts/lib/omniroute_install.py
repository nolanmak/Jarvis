#!/usr/bin/env python3
"""Explicit Linux user-service installation of optional, pinned OmniRoute.

Secrets are records in Secret Service (secret-tool), mirrored only where the
service/client needs them in private files. No production install is implicit.
"""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
UNIT = 'augmentagent-omniroute.service'
BASE = 'http://127.0.0.1:20128'
COMPRESSION = {'enabled': False, 'defaultMode': 'off', 'autoTriggerMode': 'off',
               'cavemanConfig': {'enabled': False}, 'cavemanOutputMode': {'enabled': False},
               'rtkConfig': {'enabled': False}, 'stackedPipeline': [], 'comboOverrides': {}}


def supported_node(value):
    match = re.fullmatch(r'v?(\d+)\.(\d+)\.(\d+)', value.strip())
    if not match:
        return False
    version = tuple(map(int, match.groups()))
    return (22, 22, 2) <= version < (23, 0, 0) or (24, 0, 0) <= version < (27, 0, 0)


def private_read(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077):
            raise RuntimeError('installer state must be owner-private regular files')
        return stream.read()


def private_write(path, value):
    path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    if path.is_symlink():
        raise RuntimeError('refusing symbolic link installer state')
    descriptor, name = tempfile.mkstemp(prefix='.' + path.name, dir=path.parent)
    try:
        with os.fdopen(descriptor, 'w') as stream:
            stream.write(value)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def systemd_string(value, executable=False):
    # systemd specifiers and ExecStart environment expansion must remain literal.
    value = str(value).replace('%', '%%')
    return json.dumps(value.replace('$', '$$') if executable else value)


class Installer:
    def __init__(self, config, data, pin):
        self.config, self.data, self.pin = Path(config), Path(data), pin
        self.env_file = self.config / 'omniroute.env'
        self.receipt = self.config / 'omniroute-install.json'
        self.pending = self.config / 'omniroute-pending.json'
        self.router = Path(os.environ.get('AUGMENTAGENT_MODEL_ROUTER_CONFIG', self.config / 'model-router.json'))
        self.unit = self.config.parent / 'systemd/user' / UNIT
        self.archive = self.data / 'installer/installed.tgz'
        self.binary = shutil.which('omniroute')
        self.cookie = None

    def command(self, argv, extra_env=None, **kwargs):
        env = dict(os.environ)
        env.pop('OMNIROUTE_SKIP_POSTINSTALL', None)
        env.update(DATA_DIR=str(self.data), OMNIROUTE_NO_UPDATE_NOTIFIER='1')
        env.update(extra_env or {})
        return subprocess.run(argv, text=True, capture_output=True, env=env, **kwargs)

    def checked(self, argv, **kwargs):
        result = self.command(argv, **kwargs)
        if result.returncode:
            # Never echo external output: secret-tool, login and tokens carry secrets.
            raise RuntimeError(f'{Path(argv[0]).name} {argv[1]} failed; inspect locally')
        return result.stdout.strip()

    def secret(self, name, value=None):
        account = 'omniroute/' + name
        if value is None:
            result = self.command(['secret-tool', 'lookup', 'service', 'augmentagent', 'account', account])
            if result.returncode == 0 and result.stdout.strip():
                return result.stdout.strip()
            if result.returncode not in (0, 1):
                raise RuntimeError('Secret Service lookup failed')
            return None
        self.checked(['secret-tool', 'store', '--label=AugmentAgent OmniRoute ' + name,
                      'service', 'augmentagent', 'account', account], input=value + '\n')
        if self.secret(name) != value:
            raise RuntimeError('Secret Service did not retain credential')
        return value

    def request(self, method, path, body=None, cookie=None, key=None):
        headers = {'Content-Type': 'application/json'}
        if cookie:
            headers['Cookie'] = cookie
        if key:
            headers['Authorization'] = 'Bearer ' + key
        request = urllib.request.Request(getattr(self, 'base', BASE) + path, method=method, headers=headers,
                                         data=None if body is None else json.dumps(body).encode())
        # No ambient proxy and no redirects with operator credentials.
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, *args, **kwargs):
                return None
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        with opener.open(request, timeout=10) as response:
            if response.status != 200 and response.status != 201:
                raise RuntimeError('unexpected OmniRoute response status')
            return json.load(response), response.headers.get('Set-Cookie', '').split(';')[0]

    def download(self, path):
        version = self.pin['version']
        if not re.fullmatch(r'\d+\.\d+\.\d+', version):
            raise RuntimeError('invalid OmniRoute version pin')
        url = f'https://registry.npmjs.org/omniroute/-/omniroute-{version}.tgz'
        digest = hashlib.sha512()
        with urllib.request.urlopen(url, timeout=60) as response, path.open('wb') as out:
            while chunk := response.read(1024 * 1024):
                digest.update(chunk)
                out.write(chunk)
        actual = 'sha512-' + base64.b64encode(digest.digest()).decode()
        if not secrets.compare_digest(actual, self.pin['integrity']):
            path.unlink(missing_ok=True)
            raise RuntimeError('OmniRoute package integrity mismatch; nothing installed')

    def write_unit(self):
        if not self.binary or not Path(self.binary).is_absolute():
            raise RuntimeError('could not locate installed omniroute executable')
        path = ':'.join(dict.fromkeys([str(Path(shutil.which('node') or '/usr/bin/node').parent),
                                       str(Path(self.binary).parent), '/usr/local/bin', '/usr/bin', '/bin']))
        private_write(self.unit, f'''[Unit]
Description=AugmentAgent optional OmniRoute gateway
After=network-online.target

[Service]
Type=simple
EnvironmentFile={systemd_string(self.env_file)}
Environment=OMNIROUTE_HEADLESS=1
Environment=OMNIROUTE_SERVER_HOST=127.0.0.1
Environment=REQUIRE_API_KEY=true
Environment=API_PORT=20128
Environment=DASHBOARD_PORT=20128
Environment=OMNIROUTE_NO_UPDATE_NOTIFIER=1
Environment={systemd_string('DATA_DIR=' + str(self.data))}
Environment={systemd_string('PATH=' + path)}
ExecStart={systemd_string(self.binary, executable=True)} serve --headless --port 20128
Restart=on-failure
RestartSec=5
UMask=0077
NoNewPrivileges=true

[Install]
WantedBy=default.target
''')
        self.checked(['systemd-analyze', '--user', 'verify', str(self.unit)])
        self.checked(['systemctl', '--user', 'daemon-reload'])

    def listener(self):
        output = self.checked(['ss', '-H', '-ltn'])
        addresses = []
        for line in output.splitlines():
            fields = line.split()
            if len(fields) >= 5 and fields[3].rsplit(':', 1)[-1] == '20128':
                addresses.append(fields[3])
        if not addresses or set(addresses) != {'127.0.0.1:20128'}:
            raise RuntimeError('OmniRoute listener is not exclusively IPv4 loopback on port 20128')

    def health(self, key=None):
        self.checked(['systemctl', '--user', 'is-active', '--quiet', UNIT])
        self.listener()
        for endpoint in ['/api/health', '/api/health/ping', '/api/monitoring/health']:
            self.request('GET', endpoint, cookie=self.cookie)
        if key:
            self.request('GET', '/v1/models', key=key)

    def wait_health(self):
        for attempt in range(45):
            try:
                self.request('GET', '/api/health')
                return
            except (OSError, ValueError, RuntimeError):
                if attempt == 44:
                    raise RuntimeError('OmniRoute health failed') from None
                time.sleep(1)

    def login(self):
        password = self.secret('INITIAL_PASSWORD')
        if not password:
            raise RuntimeError('missing OmniRoute management password in keyring')
        _, cookie = self.request('POST', '/api/auth/login', {'password': password})
        if not cookie.startswith('auth_token='):
            raise RuntimeError('OmniRoute login did not return an authenticated session')
        self.cookie = cookie
        return password

    def bootstrap(self):
        password = self.login()
        self.request('PATCH', '/api/settings', {'requireLogin': True, 'currentPassword': password}, self.cookie)
        settings, _ = self.request('GET', '/api/settings', cookie=self.cookie)
        if settings.get('cloudEnabled') is not False or settings.get('requireLogin') is not True:
            raise RuntimeError('existing cloud sync/login settings are unsafe; disable cloud sync before installing')
        # Fresh installs default to no tunnels. Never mutate the host Tailscale
        # setup or silently adopt an existing exposed instance.
        for provider in ['cloudflared', 'ngrok', 'tailscale']:
            status, _ = self.request('GET', '/api/tunnels/' + provider, cookie=self.cookie)
            if status.get('running') or status.get('enabled') or status.get('tunnelUrl') or status.get('publicUrl'):
                raise RuntimeError('existing tunnel must be disabled by its owner')
        self.request('PUT', '/api/settings/compression', COMPRESSION, self.cookie)
        compression, _ = self.request('GET', '/api/settings/compression', cookie=self.cookie)
        for key in ['enabled', 'defaultMode', 'autoTriggerMode', 'cavemanConfig', 'cavemanOutputMode', 'rtkConfig']:
            value = compression.get(key)
            expected = COMPRESSION[key]
            if isinstance(expected, dict):
                valid = isinstance(value, dict) and value.get('enabled') is False
            else:
                valid = value == expected
            if not valid:
                raise RuntimeError('could not verify compression disabled')
        # /api/monitoring/compression reports memo statistics, not the global mode.
        self.request('GET', '/api/monitoring/compression', cookie=self.cookie)
        key = self.secret('inference')
        if not key:
            result, _ = self.request('POST', '/api/keys', {'name': 'AugmentAgent inference', 'scopes': []}, self.cookie)
            key = result['key']
            self.secret('inference', key)
        for scope in ['read', 'write']:
            if not self.secret(scope):
                result, _ = self.request('POST', '/api/cli/tokens',
                                         {'name': 'AugmentAgent ' + scope, 'scope': scope}, self.cookie)
                self.secret(scope, result['token'])
        self.health(key)
        if not self.router.exists():
            private_write(self.router, json.dumps({
                'version': 1, 'gateway': 'omniroute', 'mode': 'direct', 'base_url': BASE + '/v1', 'api_key': key,
                'models': {'claude': {'quality': 'cc/claude-opus-4-8', 'fast': 'cc/claude-haiku-4-5-20251001'},
                           'codex': {'quality': 'cx/gpt-5.6-terra', 'fast': 'cx/gpt-5.6-luna'}},
            }, indent=2) + '\n')

    def validate_existing(self, allow_routed=False):
        for path in [self.env_file, self.receipt, self.pending, self.router]:
            if path.exists() or path.is_symlink():
                content = private_read(path)
                if path == self.router:
                    value = json.loads(content)
                    if (value.get('gateway') != 'omniroute' or (value.get('mode', 'direct') != 'direct' and not allow_routed)
                            or value.get('base_url') != BASE + '/v1'):
                        raise RuntimeError('existing router config must be migrated explicitly; installer keeps mode direct')

    def install(self, upgrade=False, before_update=None, after_health=None):
        if not supported_node(self.checked(['node', '--version'])):
            raise RuntimeError('Node >=22.22.2 <23 or >=24 <27 required; upgrade Node separately')
        if (set(self.pin) != {'version', 'integrity'}
                or not re.fullmatch(r'\d+\.\d+\.\d+', self.pin['version'])
                or not re.fullmatch(r'sha512-[A-Za-z0-9+/]{86}==', self.pin['integrity'])):
            raise RuntimeError('invalid OmniRoute pin')
        self.validate_existing(allow_routed=before_update is not None)
        previous = json.loads(private_read(self.receipt)) if self.receipt.exists() else None
        if previous and previous != self.pin and not upgrade:
            raise RuntimeError('pin changed; use --upgrade to take a backup before updating')
        if previous == self.pin:
            if not self.binary or self.checked([self.binary, '--version']) != self.pin['version']:
                raise RuntimeError('installed OmniRoute drift; restore the pinned installation explicitly')
            self.login()
            key = self.secret('inference')
            if not key or not self.secret('read') or not self.secret('write'):
                raise RuntimeError('missing installed OmniRoute keyring credentials')
            self.health(key)
            return
        pending = json.loads(private_read(self.pending)) if self.pending.exists() else None
        if pending and pending != self.pin:
            raise RuntimeError('unfinished installation has a different pin; recover it before changing versions')
        if not previous and not pending and self.binary and self.command([self.binary, '--version']).returncode == 0:
            raise RuntimeError('existing unmanaged OmniRoute installation; refusing to overwrite it')
        self.config.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.data.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(self.data, 0o700)
        # Persist secrets before starting the first service. Bootstrap password
        # stays in the keyring for future operator-authenticated installer runs.
        credentials = {}
        for name in ['JWT_SECRET', 'API_KEY_SECRET', 'INITIAL_PASSWORD']:
            credentials[name] = self.secret(name) or self.secret(name, secrets.token_hex(32))
            if not re.fullmatch(r'[A-Za-z0-9_-]{32,}', credentials[name]):
                raise RuntimeError('invalid keyring secret format')
        private_write(self.env_file, ''.join(f'{k}={v}\n' for k, v in credentials.items()
                                            if not previous or k != 'INITIAL_PASSWORD'))
        self.archive.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        with tempfile.TemporaryDirectory(prefix='stage-', dir=self.archive.parent) as staging:
            candidate = Path(staging) / 'candidate.tgz'
            self.download(candidate)
            backup_id = None
            rollback_archive = Path(staging) / 'installed.tgz'
            if previous:
                if not self.archive.is_file() or self.archive.is_symlink():
                    raise RuntimeError('previous verified package is unavailable for rollback')
                actual = 'sha512-' + base64.b64encode(hashlib.sha512(self.archive.read_bytes()).digest()).decode()
                if actual != previous['integrity']:
                    raise RuntimeError('rollback package integrity mismatch')
                os.link(self.archive, rollback_archive)
                before = set((self.data / 'backups').glob('omniroute-backup-*'))
                try:
                    self.checked([self.binary, 'backup'])
                except Exception:
                    raise
                after = set((self.data / 'backups').glob('omniroute-backup-*'))
                created = [p for p in after - before if (p / 'backup-info.json').is_file()]
                if len(created) != 1:
                    raise RuntimeError('backup did not produce one verified backup ID; update refused')
                backup_id = created[0].name.removeprefix('omniroute-backup-')
            if not previous:
                private_write(self.pending, json.dumps(self.pin) + '\n')
            try:
                if previous:
                    if before_update:
                        before_update()
                    self.checked(['systemctl', '--user', 'stop', UNIT])
                self.checked(['npm', 'install', '-g', str(candidate), '--ignore-scripts=false', '--no-audit', '--no-fund'])
                self.binary = self.binary or shutil.which('omniroute')
                if not self.binary or self.checked([self.binary, '--version']) != self.pin['version']:
                    raise RuntimeError('installed version differs from pin')
                self.write_unit()
                self.checked(['systemctl', '--user', 'restart', UNIT])
                self.wait_health()
                self.listener()
                self.bootstrap()
                private_write(self.env_file, ''.join(f'{k}={v}\n' for k, v in credentials.items() if k != 'INITIAL_PASSWORD'))
                # Reload without INITIAL_PASSWORD after its hash is persisted.
                self.checked(['systemctl', '--user', 'restart', UNIT])
                self.wait_health()
                self.health(self.secret('inference'))
                if after_health:
                    after_health()
                self.checked(['systemctl', '--user', 'enable', UNIT])
                if previous:
                    import shutil
                    saved = self.archive.parent / 'previous.tgz'
                    shutil.copyfile(rollback_archive, saved)
                    saved.chmod(0o600)
                    private_write(self.config / 'omniroute-rollback.json', json.dumps({'pin': previous, 'backup_id': backup_id}) + '\n')
                candidate.chmod(0o600)
                os.replace(candidate, self.archive)
                private_write(self.receipt, json.dumps(self.pin) + '\n')
                self.pending.unlink(missing_ok=True)
            except Exception as error:
                self.checked(['systemctl', '--user', 'stop', UNIT])
                if previous:
                    self.checked(['npm', 'install', '-g', str(rollback_archive), '--ignore-scripts=false', '--no-audit', '--no-fund'])
                    self.checked([self.binary, 'restore', backup_id, '--yes'])
                    os.replace(rollback_archive, self.archive)
                    private_write(self.receipt, json.dumps(previous) + '\n')
                    self.checked(['systemctl', '--user', 'restart', UNIT])
                    self.wait_health()
                    self.health(self.secret('inference'))
                    raise RuntimeError('OmniRoute update failed; rollback restored prior package and backup') from error
                raise

    def uninstall(self, purge=False):
        if not self.receipt.exists() and not self.pending.exists():
            raise RuntimeError('no owned OmniRoute installation receipt; refusing uninstall')
        self.validate_existing()
        self.checked(['systemctl', '--user', 'disable', '--now', UNIT])
        self.checked(['npm', 'rm', '-g', 'omniroute'])
        self.unit.unlink(missing_ok=True)
        self.checked(['systemctl', '--user', 'daemon-reload'])
        if purge:
            shutil.rmtree(self.data)
            self.env_file.unlink(missing_ok=True)
            (self.config / 'omniroute-rollback.json').unlink(missing_ok=True)
            self.router.unlink(missing_ok=True)
            for name in ['JWT_SECRET', 'API_KEY_SECRET', 'INITIAL_PASSWORD', 'inference', 'read', 'write']:
                result = self.command(['secret-tool', 'clear', 'service', 'augmentagent', 'account', 'omniroute/' + name])
                if result.returncode not in (0, 1):
                    raise RuntimeError('could not clear OmniRoute keyring record')
        self.receipt.unlink(missing_ok=True)
        self.pending.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--upgrade', action='store_true', help='apply the repository pin with backup/health rollback')
    parser.add_argument('--uninstall', action='store_true', help='stop/remove the owned service and npm package; keep data')
    parser.add_argument('--purge', action='store_true', help='with --uninstall, also remove owned data/config/credentials')
    args = parser.parse_args()
    if args.purge and not args.uninstall or args.upgrade and args.uninstall:
        parser.error('--purge requires --uninstall; --upgrade cannot uninstall')
    if sys.platform != 'linux':
        parser.error('requires a Linux systemd user session; macOS service installation is deferred')
    for program in ['node', 'npm', 'secret-tool', 'systemctl', 'systemd-analyze', 'ss']:
        if not shutil.which(program):
            parser.error(program + ' is required')
    config = Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config')) / 'augmentagent'
    data = Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')) / 'augmentagent/omniroute'
    if not config.is_absolute() or not data.is_absolute() or data.resolve().is_relative_to(ROOT.resolve()):
        parser.error('XDG roots must be absolute; OmniRoute data must stay outside the repository')
    config.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock = os.open(config / 'omniroute-install.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        installer = Installer(config, data, json.loads((ROOT / 'scripts/omniroute.pin').read_text()))
        if args.uninstall:
            installer.uninstall(args.purge)
        else:
            installer.install(args.upgrade)
    finally:
        os.close(lock)
    if args.uninstall:
        print('OmniRoute uninstalled; ' + ('owned data purged.' if args.purge else 'DATA_DIR and credentials retained.'))
        return
    print('OmniRoute is healthy on 127.0.0.1:20128; routing remains direct.')
    print('Inspect: augmentagent gateway status. Enable only by owner choice: augmentagent gateway on')


if __name__ == '__main__':
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        # Exception causes/HTTP bodies may contain credentials; do not print them.
        print('OmniRoute installation failed: ' + (str(error) if isinstance(error, RuntimeError) else type(error).__name__), file=sys.stderr)
        sys.exit(1)
