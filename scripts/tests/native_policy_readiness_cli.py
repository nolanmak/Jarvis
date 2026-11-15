"""Exercise the packaged read-only readiness entry point in isolated repositories.

AUGMENTAGENT_TEST_BINARY must name the current packaged executable explicitly.
No real provider, production policy or workflow store is used.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class NativePolicyReadiness(unittest.TestCase):
    def test_unlaunchable_declarations_never_pass_credentials_or_runtime(self):
        binary = Path(os.environ['AUGMENTAGENT_TEST_BINARY']).resolve(strict=True)
        for fault in ('refresh-file', 'refresh-environment', 'refresh-fileless',
                      'memory-low', 'memory-high', 'sessions-zero', 'bytes-low'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                subprocess.run(['git', 'init', '-q', str(root)], check=True)
                marker = root / 'provider-called'
                provider = root / 'fake-provider'
                provider.write_text('#!/bin/sh\ntouch "' + str(marker) + '"\nexit 99\n')
                provider.chmod(0o700)
                auth = root / 'owner-auth.json'
                auth.write_text(json.dumps({'OPENAI_API_KEY': 'private-fixture-key'}))
                auth.chmod(0o600)
                policy = {'version': 1, 'runtime_config': str(root / 'runtime.json'),
                          'guest_runtime': str(root / 'guest'), 'state_root': str(root),
                          'memory_mb': 1024, 'max_state_bytes': 8589934592, 'max_sessions': 16,
                          'providers': {'codex': {'gateway': {}, 'auth_file': str(auth),
                                                 'auth_relative': '.codex/auth.json'}}}
                profile = policy['providers']['codex']
                if fault.startswith('refresh-'):
                    profile['refresh_auth'] = True
                    if fault != 'refresh-file':
                        profile['environment'] = ['CODEX_API_KEY']
                    if fault == 'refresh-fileless':
                        del profile['auth_file']
                    expected = 'credential-invalid'
                else:
                    key, value = {'memory-low': ('memory_mb', 511), 'memory-high': ('memory_mb', 4097),
                                  'sessions-zero': ('max_sessions', 0), 'bytes-low': ('max_state_bytes', 1024)}[fault]
                    policy[key] = value
                    expected = 'policy-invalid'
                path = root / 'policy.json'
                path.write_text(json.dumps(policy)); path.chmod(0o600)
                before = {str(p.relative_to(root)): p.read_bytes() for p in root.rglob('*') if p.is_file()}
                env = {'PATH': '/usr/bin:/bin', 'HOME': str(root), 'XDG_STATE_HOME': str(root / 'state'),
                       'AUGMENTAGENT_NATIVE_VM_POLICY': str(path), 'CODEX_CLI': str(provider),
                       'CLAUDE_CLI': str(provider), 'CODEX_API_KEY': 'private-environment-fixture',
                       'AUGMENTAGENT_REASONER_CHAIN': 'codex'}
                result = subprocess.run([str(binary), 'autopr', 'readiness', '--json'], cwd=root,
                                        env=env, capture_output=True, timeout=30)
                self.assertEqual(result.returncode, 3, result.stderr.decode())
                body = json.loads(result.stdout)
                self.assertFalse(body['ready'])
                self.assertFalse(marker.exists(), 'readiness spawned a provider')
                self.assertNotIn(b'private-fixture-key', result.stdout + result.stderr)
                self.assertNotIn(b'private-environment-fixture', result.stdout + result.stderr)
                self.assertEqual(before, {str(p.relative_to(root)): p.read_bytes()
                                          for p in root.rglob('*') if p.is_file()})
                self.assertEqual(body['credentials']['codex']['status'], expected)
                self.assertEqual(body['roles']['builder']['status'], 'no-eligible-provider')
                self.assertEqual(body['roles']['adjudicator']['status'], 'provider-unavailable')
                if expected == 'policy-invalid':
                    self.assertEqual(body['runtime']['status'], expected)


if __name__ == '__main__':
    unittest.main()
