"""The parity ledger must notice new capabilities and reject false verification."""

import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from macos_parity_inventory import MANIFEST, discover, validate  # noqa: E402


class ParityInventoryTests(unittest.TestCase):
    def test_every_discovered_capability_has_an_owned_row(self):
        self.assertEqual(validate(json.loads(MANIFEST.read_text()), discover()), [])

    def test_new_capability_without_row_fails(self):
        rows = json.loads(MANIFEST.read_text())
        found = {**discover(), "channel:synthetic-new-channel": "crates/new/Cargo.toml"}
        self.assertIn("untracked capability: channel:synthetic-new-channel", validate(rows, found))

    def test_sidecar_without_readme_is_discovered(self):
        self.assertIn("sidecar:discord-voice", discover())

    def test_nested_route_is_discovered(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src/api").mkdir(parents=True)
            (root / "src/api/routes.ts").write_text('router.get("/nested/parity", handler);\n')
            (root / "crates/augmentagent-cli/src").mkdir(parents=True)
            (root / "crates/augmentagent-cli/src/main.rs").write_text(
                "enum Cmd {\n    Doctor,\n}\n"
            )
            (root / "systemd").mkdir()
            (root / "scripts/systemd").mkdir(parents=True)
            (root / "sidecars").mkdir()
            self.assertIn("route:api/routes:GET:/nested/parity", discover(root))

    def test_deleted_guarded_tool_becomes_stale(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            (root / "crates/augmentagent-cli/src").mkdir(parents=True)
            (root / "crates/augmentagent-cli/src/main.rs").write_text("enum Cmd {\n}\n")
            (root / "systemd").mkdir()
            (root / "scripts/systemd").mkdir(parents=True)
            (root / "sidecars").mkdir()
            found = discover(root)
            self.assertNotIn("tool:codex-tool-bridge.py", found)

    def test_verified_requires_evidence(self):
        rows = json.loads(MANIFEST.read_text())
        rows[0] = {**rows[0], "status": "verified", "evidence": None}
        self.assertIn(f"verified without evidence: {rows[0]['id']}", validate(rows, discover()))

    def test_placeholder_evidence_cannot_verify_a_row(self):
        rows = json.loads(MANIFEST.read_text())
        rows[0] = {**rows[0], "status": "verified", "evidence": "TODO"}
        self.assertIn(f"verified without evidence: {rows[0]['id']}", validate(rows, discover()))

    def test_structured_passing_evidence_is_accepted(self):
        rows = json.loads(MANIFEST.read_text())
        rows[0] = {**rows[0], "status": "verified", "evidence": {
            "commit": "a" * 40,
            "macos_version": "macOS 15.7",
            "architecture": "arm64",
            "test_command": "cargo test -p augmentagent-channel-apple-notes",
            "result": "pass",
            "artifact": "https://github.com/nolanmak/Jarvis/actions/runs/123",
        }}
        self.assertEqual(validate(rows, discover()), [])


if __name__ == "__main__":
    unittest.main()
