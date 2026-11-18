"""Owner gateway operations. Loaded with the packaged installer by the CLI.

The missing #1655 breaker adapter is explicitly unavailable: enabling must stay
closed until a real process-shared breaker observation can be wired.
"""
if 'Installer' not in globals():
    from scripts.lib.omniroute_install import Installer, private_read, private_write, UNIT
import datetime
import json
import os
from pathlib import Path
import shutil
import tempfile
import uuid


class Operations:
    def __init__(self, installer, selftest_binary='augmentagent'):
        self.inst = installer
        self.selftest_binary = selftest_binary

    def breaker(self):
        return {'state': 'unavailable', 'reason': 'daemon gateway breaker observation not integrated (#1655)'}

    def config(self):
        value = json.loads(private_read(self.inst.router))
        if not isinstance(value, dict) or value.get('mode', 'direct') not in ('direct', 'auto', 'claude', 'codex', 'qwen', 'glm'):
            raise RuntimeError('invalid gateway routing configuration')
        base = value.get('base_url')
        if not isinstance(base, str) or not base.endswith('/v1'):
            raise RuntimeError('invalid configured gateway base URL')
        self.inst.base = base[:-3]
        return value

    def set_mode(self, mode):
        value = self.config()
        old = value.get('mode', 'direct')
        if old == mode:
            return
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
        backup = self.inst.router.with_name(self.inst.router.name + '.bak-' + old + '-' + stamp + '-' + uuid.uuid4().hex[:8])
        private_write(backup, json.dumps(value, indent=2) + '\n')
        value['mode'] = mode
        private_write(self.inst.router, json.dumps(value, indent=2) + '\n')

    def off(self):
        self.set_mode('direct')
        return {'ok': True, 'mode': 'direct'}

    def status(self):
        config = self.config()
        report = {'ok': False, 'mode': config.get('mode', 'direct'),
                  'gateway': config.get('gateway', 'omniroute'), 'daemon_breaker': self.breaker()}
        if report['gateway'] != 'omniroute':
            report['reason'] = 'gateway kind is not managed OmniRoute'
            return report
        report['managed'] = config['base_url'] == 'http://127.0.0.1:20128/v1'
        report['unit_active'] = self.inst.command(['systemctl', '--user', 'is-active', '--quiet', UNIT]).returncode == 0
        token = self.inst.secret('read')
        if not token:
            report['reason'] = 'gateway read token unavailable'
            return report
        try:
            self.inst.request('GET', '/api/health')
            report['liveness'] = True
            health, _ = self.inst.request('GET', '/api/monitoring/health', key=token)
            expiration, _ = self.inst.request('GET', '/api/providers/expiration', key=token)
            # Exact v3.8.52 shapes: aggregated breakers; nested quota counts.
            counts = health['quotaMonitor']['statusCounts']
            opened = health['circuitBreakers']['open']
            expired = expiration['summary']['expired']
            expiring = expiration['summary']['expiringSoon']
            numbers = [counts[k] for k in ('healthy', 'error', 'exhausted')] + [opened, expired, expiring]
            if not all(type(v) is int and v >= 0 for v in numbers):
                raise ValueError('invalid health counters')
            report.update(status_counts={k: counts[k] for k in ('healthy', 'error', 'exhausted')},
                          gateway_breakers_open=opened, expiration_counts={'expired': expired, 'expiring_soon': expiring})
            report['ok'] = ((report['unit_active'] or not report['managed']) and report['daemon_breaker'].get('state') == 'closed'
                            and not opened and not expired and counts['healthy'] > 0
                            and counts['error'] == 0 and counts['exhausted'] == 0)
        except (OSError, ValueError, KeyError, TypeError, RuntimeError):
            report['reason'] = 'gateway health observation unavailable'
        return report

    def probe(self, mode):
        if mode not in ('auto', 'claude', 'codex'):
            raise RuntimeError('routed probe requires a supported gateway mode')
        config = self.config()
        config['mode'] = mode
        # Probe through a private one-call config; never temporarily enable the
        # persisted config for other in-flight/next owner conversations.
        with tempfile.TemporaryDirectory(prefix='.gateway-probe-', dir=self.inst.config) as directory:
            path = Path(directory) / 'model-router.json'
            private_write(path, json.dumps(config) + '\n')
            profile = 'codex' if mode == 'codex' else 'claude'
            self.inst.checked([self.selftest_binary, 'reasoner-selftest', '--routed', '--profile', profile],
                              extra_env={'AUGMENTAGENT_MODEL_ROUTER_CONFIG': str(path)})

    def on(self, mode='auto'):
        if mode not in ('auto', 'claude', 'codex'):
            raise RuntimeError('unsupported gateway mode')
        if not self.status()['ok']:
            raise RuntimeError('gateway health or daemon breaker is not green')
        self.probe(mode)
        if not self.status()['ok']:
            raise RuntimeError('gateway health changed during routed probe')
        self.set_mode(mode)
        return {'ok': True, 'mode': mode}

    def update(self, to=None):
        previous_mode = self.config().get('mode', 'direct')
        try:
            if not getattr(self, 'pin_available', True):
                raise RuntimeError('reviewed repository pin file unavailable')
            if self.config()['base_url'] != 'http://127.0.0.1:20128/v1':
                raise RuntimeError('remote gateway updates are not supported')
            if to is not None and to != self.inst.pin['version']:
                raise RuntimeError('--to must match the reviewed version and sha512 in scripts/omniroute.pin')
            def verify():
                self.probe(previous_mode if previous_mode != 'direct' else 'auto')
                if previous_mode != 'direct':
                    if not self.status()['ok']:
                        raise RuntimeError('gateway health or breaker refuses restored routing')
                    self.set_mode(previous_mode)
            previous = json.loads(private_read(self.inst.receipt))
            if previous == self.inst.pin:
                self.off()
                self.inst.install(upgrade=True)
                verify()
            else:
                self.inst.install(upgrade=True, before_update=self.off, after_health=verify)
            return {'ok': True, 'mode': self.config().get('mode', 'direct')}
        except Exception:
            self.off()
            raise

    def rollback(self):
        self.off()
        if self.config()['base_url'] != 'http://127.0.0.1:20128/v1':
            raise RuntimeError('remote gateway rollback is not supported')
        receipt = json.loads(private_read(self.inst.config / 'omniroute-rollback.json'))
        artifact = self.inst.archive.parent / 'previous.tgz'
        import hashlib, base64
        digest = 'sha512-' + base64.b64encode(hashlib.sha512(artifact.read_bytes()).digest()).decode()
        if digest != receipt['pin']['integrity']:
            raise RuntimeError('rollback artifact integrity mismatch')
        backup_id = receipt['backup_id']
        import re
        if not isinstance(backup_id, str) or not re.fullmatch(r'[A-Za-z0-9_-]+', backup_id):
            raise RuntimeError('invalid backup ID')
        self.inst.checked(['systemctl', '--user', 'stop', UNIT])
        self.inst.checked(['npm', 'install', '-g', str(artifact), '--ignore-scripts=false', '--no-audit', '--no-fund'])
        self.inst.checked([self.inst.binary, 'restore', backup_id, '--yes'])
        self.inst.checked(['systemctl', '--user', 'restart', UNIT])
        self.inst.wait_health()
        self.inst.health(self.inst.secret('inference'))
        shutil.copyfile(artifact, self.inst.archive)
        self.inst.archive.chmod(0o600)
        private_write(self.inst.receipt, json.dumps(receipt['pin']) + '\n')
        return {'ok': True, 'mode': 'direct'}

    def execute(self, operation, mode='auto', to=None, alert=None):
        try:
            if operation == 'status': return self.status()
            if operation == 'off': return self.off()
            if operation == 'on': return self.on(mode)
            if operation == 'update': return self.update(to)
            if operation == 'rollback': return self.rollback()
            raise RuntimeError('unsupported gateway operation')
        except Exception:
            # No response body, path, key, token, email or subprocess output.
            report = {'ok': False, 'error': 'gateway ' + operation + ' failed; inspect local configuration and health'}
            if operation in ('update', 'rollback'):
                report['alert'] = 'Gateway ' + operation + ' failed. Verify direct routing and inspect the local gateway before enabling it.'
                if alert:
                    alert(report['alert'])
            return report
