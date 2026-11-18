"""Gateway owner controls against real private temp state and disposable host fixtures."""
import importlib.util
import json
from pathlib import Path
import unittest
import os
import re
import subprocess
import sys
from scripts.tests import omniroute_install_test as installer_fixture


class OpsTests(unittest.TestCase):
    def setUp(self):
        self.host = installer_fixture.InstallTests(methodName='test_node_floor_versions')
        self.host.setUp()
        self.addCleanup(self.host.doCleanups)
        self.host.inst.install()
        spec = importlib.util.spec_from_file_location('omni_ops', Path(__file__).resolve().parents[1] / 'lib/omniroute_ops.py')
        self.mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.mod)
        self.ops = self.mod.Operations(self.host.inst)
        self.ops.breaker = lambda: {'state': 'closed'}
        self.probes = []
        self.ops.probe = lambda mode: self.probes.append(mode)
        self.alerts = []
        original = self.host.inst.request
        def request(method, path, *args, **kwargs):
            if path == '/api/monitoring/health':
                return {'circuitBreakers': {'open': 0}, 'quotaMonitor': {'statusCounts': {'healthy': 1, 'error': 0, 'exhausted': 0}}}, ''
            if path == '/api/providers/expiration':
                return {'summary': {'expired': 0, 'expiringSoon': 0}, 'list': []}, ''
            return original(method, path, *args, **kwargs)
        self.host.inst.request = request

    def config(self):
        return json.loads(self.host.inst.router.read_text())

    def route(self, mode):
        value = self.config();value['mode'] = mode
        self.host.mod.private_write(self.host.inst.router, json.dumps(value))

    def test_off_preserves_inflight_snapshot_and_next_call_is_direct(self):
        self.route('auto')
        inflight = self.config()
        before = len(self.host.calls)
        self.ops.off()
        self.assertEqual(inflight['mode'], 'auto')
        self.assertEqual(self.config()['mode'], 'direct')
        self.assertEqual(self.config()['api_key'], inflight['api_key'])
        self.assertEqual(self.host.calls[before:], [])
        backups = list(self.host.inst.router.parent.glob('model-router.json.bak-auto-*'))
        self.assertEqual(len(backups), 1)
        self.assertEqual(json.loads(backups[0].read_text()), inflight)
        self.assertEqual(backups[0].stat().st_mode & 0o777, 0o600)
        self.ops.off()
        self.assertEqual(len(list(self.host.inst.router.parent.glob('model-router.json.bak-*'))), 1)

    def test_status_summarizes_without_secrets_or_account_identity(self):
        report = self.ops.status()
        self.assertTrue(report['ok'])
        self.assertEqual(report['mode'], 'direct')
        self.assertEqual(report['gateway'], 'omniroute')
        self.assertEqual(report['daemon_breaker']['state'], 'closed')
        self.assertNotIn('sk-fixture', json.dumps(report))
        self.assertNotIn('oma_live', json.dumps(report))

    def test_status_never_calls_a_partially_red_pool_green(self):
        original = self.host.inst.request
        def request(method, path, *args, **kwargs):
            value, cookie = original(method, path, *args, **kwargs)
            if path == '/api/monitoring/health':
                value['quotaMonitor']['statusCounts'].update(healthy=2, error=1)
            return value, cookie
        self.host.inst.request = request
        self.assertFalse(self.ops.status()['ok'])

    def test_on_requires_green_status_and_real_routed_probe(self):
        self.ops.on('codex')
        self.assertEqual(self.config()['mode'], 'codex')
        self.assertEqual(self.probes, ['codex'])
        self.route('direct')
        self.ops.probe = lambda mode: (_ for _ in ()).throw(RuntimeError('probe failed'))
        with self.assertRaises(RuntimeError): self.ops.on('auto')
        self.assertEqual(self.config()['mode'], 'direct')
        self.ops.breaker = lambda: {'state': 'unavailable'}
        with self.assertRaises(RuntimeError): self.ops.on('auto')
        self.assertEqual(self.config()['mode'], 'direct')

    def test_broken_pin_leaves_old_version_direct_and_alerts_once(self):
        self.route('auto')
        def nonexistent(path): raise OSError('package not found')
        self.host.inst.download = nonexistent
        self.host.inst.pin = dict(self.host.pin, version='99.99.99')
        result = self.ops.execute('update', to='99.99.99', alert=self.alerts.append)
        self.assertFalse(result['ok'])
        self.assertEqual(self.config()['mode'], 'direct')
        self.assertEqual(self.host.version, '3.8.52')
        self.assertTrue(self.host.active)
        self.assertEqual(len(self.alerts), 1)

    def test_update_health_and_routed_probe_failure_roll_back_and_stay_direct(self):
        self.route('auto')
        self.host.inst.pin = dict(self.host.pin, version='3.8.53')
        self.host.desired = '3.8.53'
        self.ops.probe = lambda mode: (_ for _ in ()).throw(RuntimeError('routed failure'))
        result = self.ops.execute('update', alert=self.alerts.append)
        self.assertFalse(result['ok'])
        self.assertEqual(self.config()['mode'], 'direct')
        self.assertEqual(self.host.version, '3.8.52')
        self.assertTrue(self.host.active)
        self.assertTrue(any('restore' in call for call in self.host.calls))
        self.assertEqual(len(self.alerts), 1)

    def test_successful_update_retains_prior_pin_for_explicit_rollback(self):
        self.route('codex')
        self.host.inst.pin = dict(self.host.pin, version='3.8.53')
        self.host.desired = '3.8.53'
        result = self.ops.execute('update', alert=self.alerts.append)
        self.assertTrue(result['ok'], result)
        self.assertEqual(self.host.version, '3.8.53')
        self.assertEqual(self.config()['mode'], 'codex')
        self.assertEqual(self.alerts, [])
        self.ops.rollback()
        self.assertEqual(self.host.version, '3.8.52')
        self.assertEqual(self.config()['mode'], 'direct')
        self.assertTrue(self.host.active)

    def test_packaged_driver_off_and_broken_pin_use_private_state_without_global_enablement(self):
        self.route('auto')
        root = Path(__file__).resolve().parents[2]
        source = (root / 'crates/augmentagent-cli/src/gateway_ops.rs').read_text()
        driver = re.search(r'const DRIVER: &str = r#"(.*?)"#;', source, re.S).group(1)
        (self.host.inst.config / 'omniroute-install.lock').touch(mode=0o600)
        env = dict(os.environ, XDG_CONFIG_HOME=str(self.host.inst.config.parent),
                   XDG_DATA_HOME=str(self.host.inst.data.parent.parent))
        env.pop('AUGMENTAGENT_MODEL_ROUTER_CONFIG', None)
        def call(operation, **args):
            payload = {'repository': str(root), 'selftest_binary': sys.executable,
                       'installer': (root / 'scripts/lib/omniroute_install.py').read_text(),
                       'operations': (root / 'scripts/lib/omniroute_ops.py').read_text(),
                       'compiled_pin': (root / 'scripts/omniroute.pin').read_text(),
                       'request': {'operation': operation, **args}}
            return subprocess.run([sys.executable, '-I', '-B', '-c', driver], input=json.dumps(payload),
                                  env=env, capture_output=True, text=True, timeout=10)
        result = call('off')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.config()['mode'], 'direct')
        self.route('auto')
        result = call('update', to='999.0.0')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('alert', json.loads(result.stdout))
        self.assertEqual(self.config()['mode'], 'direct')
        self.assertNotIn('sk-fixture', result.stdout + result.stderr)

    def test_to_cannot_bypass_reviewed_integrity_pin(self):
        self.route('auto')
        result = self.ops.execute('update', to='999.0.0', alert=self.alerts.append)
        self.assertFalse(result['ok'])
        self.assertEqual(self.config()['mode'], 'direct')
        self.assertEqual(self.host.version, '3.8.52')
        self.assertEqual(len(self.alerts), 1)


if __name__ == '__main__': unittest.main()
