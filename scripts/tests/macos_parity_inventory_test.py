"""The parity ledger must notice new capabilities and reject false verification."""

import json
from pathlib import Path
import sys
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

    def test_verified_requires_evidence(self):
        rows = json.loads(MANIFEST.read_text())
        rows[0] = {**rows[0], "status": "verified", "evidence": None}
        self.assertIn(f"verified without evidence: {rows[0]['id']}", validate(rows, discover()))


if __name__ == "__main__":
    unittest.main()
