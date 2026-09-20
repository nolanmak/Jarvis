#!/usr/bin/env python3
"""Black-box tests for the public-push hook using local bare remotes only."""
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
GATE = ROOT / "scripts" / "ccat-public-push-gate.sh"


class PublicPushGateTest(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp(prefix="ccat-push-test-"))
        self.remote = self.tmp / "remote.git"
        self.repo = self.tmp / "repo"
        self.state = self.tmp / "state"
        self.calls = self.tmp / "calls"
        self.fake = self.tmp / "fake-ccat"
        self.receipt_consumer = self.tmp / "fake-receipt-consumer"
        self.calibration = self.tmp / "calibration.jsonl"
        self.command(["git", "init", "--bare", str(self.remote)])
        self.command(["git", "init", str(self.repo)])
        self.command(["git", "-C", str(self.repo), "config", "user.email", "fixture@example.com"])
        self.command(["git", "-C", str(self.repo), "config", "user.name", "Fixture"])
        self.command(["git", "-C", str(self.repo), "remote", "add", "origin", str(self.remote)])
        hook = self.repo / ".git" / "hooks" / "pre-push"
        hook.write_text(f"#!/usr/bin/env bash\nexec {GATE} \"$@\"\n")
        hook.chmod(0o755)
        self.fake.write_text("""#!/usr/bin/env bash
set -euo pipefail
touch \"$FAKE_CALLS\"
hash=$(sha256sum \"$2\" | awk '{print $1}')
case \"${FAKE_OUTCOME:-allow}\" in
  allow) echo \"policy=public_git_push version=1 outcome=Allow payload_sha256=$hash provider=fake model=fake\"; exit 0 ;;
  review) echo \"policy=public_git_push version=1 outcome=Review payload_sha256=$hash provider=fake model=fake\"; exit 10 ;;
  block) echo \"policy=public_git_push version=1 outcome=Block payload_sha256=$hash provider=fake model=fake\"; exit 11 ;;
  *) exit 12 ;;
esac
""")
        self.fake.chmod(0o755)
        self.receipt_consumer.write_text("""#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == "receipt-verify" && -f "$2" ]]
mkdir -p "$6/augmentagent/ccat-receipts"
mv "$2" "$6/augmentagent/ccat-receipts/synthetic.used"
""")
        self.receipt_consumer.chmod(0o755)
        self.calibration.write_text("".join(json.dumps({"case_id": f"synthetic-{index}", "policy_id": "public_git_push", "expected": "allow", "predicted": "allow"}) + "\n" for index in range(100)))
        (self.repo / "safe.txt").write_text("safe initial content\n")
        self.command(["git", "-C", str(self.repo), "add", "."])
        self.command(["git", "-C", str(self.repo), "commit", "-m", "initial"])

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def command(self, command, check=True, env=None):
        merged = os.environ | (env or {})
        return subprocess.run(command, text=True, capture_output=True, check=check, env=merged)

    def gate_env(self, **extra):
        return {
            "AUGMENTAGENT_CCAT_PUBLIC_REMOTES": "origin",
            "AUGMENTAGENT_CCAT_ENABLED": "true",
            "CCAT_BIN": str(self.fake),
            "CCAT_RECEIPT_BIN": str(self.receipt_consumer),
            "FAKE_CALLS": str(self.calls),
            "XDG_STATE_HOME": str(self.state),
            "AUGMENTAGENT_CCAT_CALIBRATION_REPORT": str(self.calibration),
            **extra,
        }

    def test_allow_creates_remote_ref(self):
        result = self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/main"], check=False, env=self.gate_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.calls.exists())
        self.command(["git", "--git-dir", str(self.remote), "show-ref", "--verify", "refs/heads/main"])

    def test_deterministic_secret_scan_blocks_before_ccat(self):
        (self.repo / "leak.txt").write_text("token=ghp_abcdefghijklmnopqrstuvwxyz1234567890\n")  # pii-ok: synthetic leak fixture
        self.command(["git", "-C", str(self.repo), "add", "."])
        self.command(["git", "-C", str(self.repo), "commit", "-m", "leak"])
        result = self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/main"], check=False, env=self.gate_env())
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertNotIn("refs/heads/main", self.command(["git", "--git-dir", str(self.remote), "show-ref"], check=False).stdout)

    def test_block_never_creates_a_remote_ref(self):
        result = self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/main"], check=False, env=self.gate_env(FAKE_OUTCOME="block"))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.calls.exists())
        self.assertNotIn("refs/heads/main", self.command(["git", "--git-dir", str(self.remote), "show-ref"], check=False).stdout)

    def test_unconfigured_remote_is_not_gated(self):
        env = self.gate_env(AUGMENTAGENT_CCAT_PUBLIC_REMOTES="other")
        self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/main"], env=env)
        self.assertFalse(self.calls.exists())

    def test_missing_calibration_blocks_before_ccat(self):
        result = self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/main"], check=False, env=self.gate_env(AUGMENTAGENT_CCAT_CALIBRATION_REPORT=str(self.tmp / "missing.jsonl")))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())

    def test_review_receipt_is_bound_and_single_use(self):
        first = self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/main"], check=False, env=self.gate_env(FAKE_OUTCOME="review"))
        self.assertNotEqual(first.returncode, 0)
        self.assertIn("payload_sha256=", first.stderr, first.stderr)
        payload_hash = next(part.split("=", 1)[1] for part in first.stderr.split() if part.startswith("payload_sha256="))
        local_sha = self.command(["git", "-C", str(self.repo), "rev-parse", "HEAD"]).stdout.strip()
        receipt = self.tmp / "receipt.json"
        receipt.write_text(json.dumps({"version": 1, "receipt_id": "receipt_123", "payload_sha256": payload_hash, "local_sha": local_sha, "remote_ref": "refs/heads/main", "expires_at_epoch": int(time.time()) + 60}))
        receipt.chmod(0o600)
        env = self.gate_env(FAKE_OUTCOME="review", CCAT_APPROVAL_RECEIPT=str(receipt))
        self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/main"], env=env)
        self.assertFalse(receipt.exists())
        second = self.command(["git", "-C", str(self.repo), "push", "origin", "HEAD:refs/heads/again"], check=False, env=env)
        self.assertNotEqual(second.returncode, 0)


if __name__ == "__main__":
    unittest.main()
