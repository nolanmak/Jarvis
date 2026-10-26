import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'autoship-scope-guard.py'
spec = importlib.util.spec_from_file_location('autoship_scope_guard', SCRIPT)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class ScopeGuardTests(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.TemporaryDirectory()
        self.addCleanup(self.root.cleanup)
        self.base = Path(self.root.name).resolve()
        self.work = self.base / 'work'
        self.control = self.base / 'control'
        self.config = self.base / 'provider-config'
        for path in (self.work, self.control, self.config):
            path.mkdir()
        self.policy = dict(version=1, write_roots=[str(self.work)],
                           protected_roots=[str(self.control), str(self.config)],
                           network='provider_transport')

    def run_guard(self, code, policy=None):
        path = self.base / 'policy.json'
        path.write_text(json.dumps(policy or self.policy))
        path.chmod(0o600)
        return subprocess.run([sys.executable, '-I', str(SCRIPT), str(path), '--',
                               sys.executable, '-B', '-c', code],
                              capture_output=True, text=True, timeout=10)

    def require_kernel(self):
        result = self.run_guard('pass')
        if result.returncode == 126:
            self.skipTest('Landlock ABI 6/libseccomp unavailable; production launch fails closed')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_same_uid_child_cannot_rewrite_audit_or_original_provider_settings(self):
        self.require_kernel()
        audit = self.control / 'tool-audit.log'
        settings = self.config / 'settings.json'
        audit.write_text('original-audit')
        settings.write_text('original-settings')
        child = ('import pathlib; '
                 f'pathlib.Path({str(audit)!r}).write_text("tampered")')
        code = f'''import pathlib, subprocess, sys
pathlib.Path({str(self.work / 'allowed')!r}).write_text('ok')
for raw in {list(map(str, [audit, settings]))!r}:
 try:
  pathlib.Path(raw).write_text('tampered')
  raise SystemExit('outside write succeeded')
 except PermissionError:
  pass
p=subprocess.run([sys.executable, '-c', {child!r}], capture_output=True)
assert p.returncode != 0
'''
        result = self.run_guard(code)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(audit.read_text(), 'original-audit')
        self.assertEqual(settings.read_text(), 'original-settings')
        self.assertEqual((self.work / 'allowed').read_text(), 'ok')

    def test_audit_anchor_cannot_be_truncated_deleted_renamed_or_replaced(self):
        self.require_kernel()
        anchor_dir = self.base / 'audit-anchors'
        anchor_dir.mkdir()
        anchor = anchor_dir / 'run.json'
        anchor.write_text('trusted-anchor')
        audit = self.control / 'run.jsonl'
        audit.write_text('trusted-record')
        replacement = self.work / 'replacement'
        replacement.write_text('forged')
        code = f'''import os
for path in {list(map(str, [anchor, audit]))!r}:
 for operation in [lambda: open(path, 'w'), lambda: os.unlink(path),
                   lambda: os.rename(path, {str(self.work / 'stolen')!r}),
                   lambda: os.replace({str(replacement)!r}, path)]:
  try:
   operation()
   raise SystemExit('audit mutation succeeded')
  except PermissionError:
   pass
'''
        result = self.run_guard(code)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(anchor.read_text(), 'trusted-anchor')
        self.assertEqual(audit.read_text(), 'trusted-record')

    def test_symlink_in_workspace_does_not_grant_target_write(self):
        self.require_kernel()
        target = self.control / 'record'
        target.write_text('original')
        link = self.work / 'escape'
        link.symlink_to(target)
        result = self.run_guard(f'import pathlib; pathlib.Path({str(link)!r}).write_text("tampered")')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.read_text(), 'original')

    def test_network_denial_is_enforced_when_requested(self):
        self.require_kernel()
        policy = dict(self.policy, network='deny')
        result = self.run_guard('import socket; socket.socket()', policy)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Operation not permitted', result.stderr)

    def test_unimplemented_hostname_policy_cannot_claim_enforcement(self):
        policy = dict(self.policy, network=['api.example.com'])
        result = self.run_guard(f'open({str(self.work / "executed")!r}, "w")', policy)
        self.assertEqual(result.returncode, 126)
        self.assertFalse((self.work / 'executed').exists())

    def test_writable_ancestor_or_descendant_of_control_state_is_rejected(self):
        nested = self.control / 'nested'
        nested.mkdir()
        for root in [self.base, self.control, nested]:
            policy = dict(self.policy, write_roots=[str(root)])
            with self.assertRaises(RuntimeError):
                guard.validated_policy(policy)

    def test_policy_symlink_and_nonprivate_file_are_refused(self):
        path = self.base / 'actual'
        path.write_text(json.dumps(self.policy))
        path.chmod(0o644)
        with self.assertRaises(RuntimeError):
            guard.private_json(path)
        path.chmod(0o600)
        link = self.base / 'linked'
        link.symlink_to(path)
        with self.assertRaises(OSError):
            guard.private_json(link)


if __name__ == '__main__':
    unittest.main()
