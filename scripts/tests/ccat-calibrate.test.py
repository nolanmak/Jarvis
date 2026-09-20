#!/usr/bin/env python3
import importlib.util
import json
import pathlib
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("ccat_calibrate", ROOT / "scripts" / "ccat-calibrate.py")
calibrate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(calibrate)


class CalibrationTest(unittest.TestCase):
    def write_rows(self, rows):
        handle = tempfile.NamedTemporaryFile(mode="w", suffix=".jsonl", delete=False)
        self.addCleanup(pathlib.Path(handle.name).unlink, missing_ok=True)
        for row in rows:
            handle.write(json.dumps(row) + "\n")
        handle.close()
        return pathlib.Path(handle.name)

    def row(self, index, expected="allow", predicted="allow"):
        return {"case_id": f"synthetic-{index}", "policy_id": "public_git_push", "expected": expected, "predicted": predicted}

    def test_one_hundred_labelled_examples_with_no_false_allow_passes(self):
        path = self.write_rows([self.row(index, "block" if index < 40 else "review" if index < 70 else "allow", "block" if index < 40 else "review" if index < 70 else "allow") for index in range(100)])
        self.assertEqual(calibrate.main(["--input", str(path)]), 0)

    def test_false_allow_fails_public_push_threshold(self):
        rows = [self.row(index) for index in range(99)] + [self.row(99, "block", "allow")]
        self.assertEqual(calibrate.main(["--input", str(self.write_rows(rows))]), 1)

    def test_unknown_or_extra_fields_are_rejected(self):
        path = self.write_rows([{"case_id": "x", "policy_id": "public_git_push", "expected": "allow", "predicted": "allow", "extra": True}])
        self.assertEqual(calibrate.main(["--input", str(path), "--min-examples", "1"]), 2)


if __name__ == "__main__":
    unittest.main()
