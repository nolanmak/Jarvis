"""Filesystem/protocol tests only; root/VM/artifact acceptance is separate."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import subprocess
import unittest
import uuid
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("ci_cache_generation", ROOT / "scripts/ci-cache-generation.py")
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class GenerationTests(unittest.TestCase):
    def test_actual_source_census_rejects_changed_untracked_and_aliased_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null", "-c", "user.name=Fixture",
                    "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "-C", str(root), *args], text=True).strip()
            git("init", "-q")
            source = root / "source.rs"
            source.write_text("original")
            git("add", ".")
            git("commit", "-qm", "fixture")
            head = git("rev-parse", "HEAD")
            objects = root / ".git/objects"
            original = cache.source_census(root, objects, head)
            source.write_text("changed")
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head)
            source.write_text("original")
            self.assertEqual(cache.source_census(root, objects, head), original)
            rogue = root / "untracked.rs"
            rogue.write_text("unbound")
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head)
            rogue.unlink()
            source.rename(rogue)
            source.symlink_to(rogue)
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head)

    def test_generated_mount_points_are_empty_and_cannot_hide_git_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null", "-c", "user.name=Fixture",
                    "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "-C", str(root), *args], text=True).strip()
            git("init", "-q")
            (root / "source.rs").write_text("source")
            git("add", ".")
            git("commit", "-qm", "fixture")
            head, objects = git("rev-parse", "HEAD"), root / ".git/objects"
            (root / "target").mkdir()
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head)
            cache.source_census(root, objects, head, generated_directories=["target"])
            (root / "target/hidden.rs").write_text("hidden")
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head, generated_directories=["target"])
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head, generated_directories=["source.rs"])

    def binding(self):
        return {"generation": str(uuid.uuid4()), "head": "a" * 40, "tree": "b" * 40,
                "contract_sha256": "c" * 64, "authority_sha256": "d" * 64,
                "cache_identity_sha256": "e" * 64, "source_inputs_sha256": "f" * 64}

    def prepare(self, root, binding, command, **kwargs):
        return cache.prepare(root, binding, command, os.getuid(), os.getgid(), **kwargs)

    def control(self, root, binding):
        return root / "ci-source-generations" / binding["generation"] / "control.json"

    def test_reconstruction_cannot_observe_the_old_target_but_acknowledged_steps_share_one(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binding = self.binding()
            first = str(uuid.uuid4())
            target = self.prepare(root, binding, first)
            (target / "old-artifact").write_text("source A")
            second = str(uuid.uuid4())
            same = self.prepare(root, binding, second, predecessor=first)
            self.assertEqual(same, target)
            self.assertEqual((same / "old-artifact").read_text(), "source A")
            new_binding = binding | {"generation": str(uuid.uuid4())}
            reconstructed = self.prepare(root, new_binding, str(uuid.uuid4()))
            self.assertNotEqual(reconstructed, target)
            self.assertEqual(list(reconstructed.iterdir()), [])
            self.assertEqual((target / "old-artifact").read_text(), "source A")

    def test_missing_or_wrong_completion_cannot_reuse_or_rewrite_a_generation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binding = self.binding()
            first = str(uuid.uuid4())
            target = self.prepare(root, binding, first)
            marker = target / "keep"
            marker.write_text("original")
            before = self.control(root, binding).read_bytes()
            for predecessor, command in [(None, str(uuid.uuid4())), (str(uuid.uuid4()), str(uuid.uuid4())), (first, first)]:
                with self.assertRaises(cache.FreshnessHold) as held:
                    self.prepare(root, binding, command, predecessor=predecessor)
                self.assertEqual(held.exception.reason, "cleanup_uncertain")
                self.assertEqual(self.control(root, binding).read_bytes(), before)
                self.assertEqual(marker.read_text(), "original")

    def test_source_or_authority_changes_cannot_inherit_an_acknowledged_target(self):
        for field in self.binding():
            if field == "generation":
                continue
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binding = self.binding()
                first = str(uuid.uuid4())
                target = self.prepare(root, binding, first)
                marker = target / "keep"
                marker.write_text("original")
                before = self.control(root, binding).read_bytes()
                changed = binding | {field: "1" * len(binding[field])}
                with self.assertRaises(cache.FreshnessHold):
                    self.prepare(root, changed, str(uuid.uuid4()), predecessor=first)
                self.assertEqual(self.control(root, binding).read_bytes(), before)
                self.assertEqual(marker.read_text(), "original")

    def test_legacy_artifacts_never_become_a_generation_or_get_deleted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            legacy = root / "target"
            legacy.mkdir()
            (legacy / "keep").write_text("legacy")
            with self.assertRaises(cache.FreshnessHold) as held:
                self.prepare(root, self.binding(), str(uuid.uuid4()))
            self.assertEqual(held.exception.reason, "legacy_format")
            self.assertEqual((legacy / "keep").read_text(), "legacy")
            self.assertFalse((root / "ci-source-generations").exists())

    def test_torn_missing_unknown_and_linked_records_cannot_activate(self):
        for fault in ("missing", "torn", "duplicate", "unknown-version", "unknown-field", "hardlink", "symlink", "oversize"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binding = self.binding()
                first = str(uuid.uuid4())
                target = self.prepare(root, binding, first)
                record = self.control(root, binding)
                original = record.read_bytes()
                if fault == "missing":
                    record.unlink()
                elif fault == "torn":
                    record.write_bytes(b"{not-json")
                elif fault == "duplicate":
                    record.write_bytes(b'{"version":999,' + original[1:])
                elif fault in ("unknown-version", "unknown-field"):
                    value = json.loads(original)
                    value["version" if fault == "unknown-version" else "unknown"] = 999
                    record.write_text(json.dumps(value))
                elif fault == "hardlink":
                    os.link(record, root / "other-link")
                elif fault == "symlink":
                    other = root / "other-record"
                    record.rename(other)
                    record.symlink_to(other)
                else:
                    record.write_bytes(b"x" * 8193)
                with self.assertRaises(cache.FreshnessHold):
                    self.prepare(root, binding, str(uuid.uuid4()), predecessor=first)
                self.assertTrue(target.is_dir())

    def test_invalid_generation_cannot_escape_or_allocate(self):
        for generation in ("../escape", str(uuid.uuid1()), "A" * 36, None):
            with self.subTest(generation=generation), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                with self.assertRaises(cache.FreshnessHold):
                    self.prepare(root, self.binding() | {"generation": generation}, str(uuid.uuid4()))
                self.assertEqual(list(root.iterdir()), [])

    def test_crash_before_record_publication_preserves_old_acknowledgement(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binding = self.binding()
            first = str(uuid.uuid4())
            target = self.prepare(root, binding, first)
            before = self.control(root, binding).read_bytes()
            with patch.object(cache.os, "replace", side_effect=OSError("simulated publication crash")):
                with self.assertRaises(OSError):
                    self.prepare(root, binding, str(uuid.uuid4()), predecessor=first)
            self.assertEqual(self.control(root, binding).read_bytes(), before)
            self.assertTrue(target.is_dir())
            self.assertEqual(list(target.parent.glob(".preparing-*")), [])

    def test_hold_projection_contains_only_bounded_codes_and_evidence_digest(self):
        for reason in (*cache.RECOVERY, "untrusted/private/path"):
            hold = cache.FreshnessHold(reason, b"private raw metadata")
            value = hold.diagnostic()
            self.assertEqual(set(value), {"version", "reason", "recovery", "evidence_sha256"})
            self.assertIn(value["reason"], cache.RECOVERY)
            self.assertEqual(value["recovery"], cache.RECOVERY[value["reason"]])
            self.assertRegex(value["evidence_sha256"], "^[0-9a-f]{64}$")
            self.assertNotIn("private", json.dumps(value))
            self.assertEqual(cache.validate_hold(value), value)
        valid = cache.FreshnessHold("metadata_missing").diagnostic()
        for changed in [{"version": True}, {"version": 999}, {"reason": []}, {"reason": "not-a-code"},
                        {"recovery": "not-a-code"}, {"evidence_sha256": "not-a-digest"}, {"extra": "private"}]:
            self.assertEqual(cache.validate_hold(valid | changed)["reason"], "freshness_unknown")


if __name__ == "__main__":
    unittest.main()
