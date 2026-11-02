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
            (root / "target/hidden.rs").unlink()
            output = root / "result.css"
            output.touch()
            cache.source_census(root, objects, head, generated_directories=["target"], generated_files=["result.css"])
            output.write_text("unexplained generated input")
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head, generated_directories=["target"], generated_files=["result.css"])
            with self.assertRaises(cache.FreshnessHold):
                cache.source_census(root, objects, head, generated_files=["source.rs"])

    def binding(self):
        return {"generation": str(uuid.uuid4()), "head": "a" * 40, "tree": "b" * 40,
                "contract_sha256": "c" * 64, "authority_sha256": "d" * 64,
                "cache_identity_sha256": "e" * 64, "source_inputs_sha256": "f" * 64}

    def test_declared_generated_outputs_have_separate_writable_roots_in_one_generation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binding = self.binding()
            target = self.prepare(root, binding, str(uuid.uuid4()))
            mounts = cache.prepare_outputs(target, {"node_modules": "directory", "public/tailwind.css": "file"},
                                          os.getuid(), os.getgid())
            self.assertEqual(set(mounts), {"node_modules", "public/tailwind.css"})
            (mounts["node_modules"] / "dependency").write_text("generated dependency")
            mounts["public/tailwind.css"].write_text("generated CSS")
            repeated = cache.prepare_outputs(target, {"node_modules": "directory", "public/tailwind.css": "file"},
                                            os.getuid(), os.getgid())
            self.assertEqual(repeated, mounts)
            self.assertEqual(repeated["public/tailwind.css"].read_text(), "generated CSS")
            other = self.prepare(root, binding | {"generation": str(uuid.uuid4())}, str(uuid.uuid4()))
            fresh = cache.prepare_outputs(other, {"public/tailwind.css": "file"}, os.getuid(), os.getgid())
            self.assertEqual(fresh["public/tailwind.css"].read_bytes(), b"")

    def test_generated_output_policy_cannot_escape_overlap_or_replace_control_records(self):
        for outputs in ({"../control.json": "file"}, {"/escape": "directory"},
                        {"node_modules": "directory", "node_modules/child": "directory"},
                        {"unknown": "symlink"}, {".git/config": "file"}):
            with self.subTest(outputs=outputs), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binding = self.binding()
                target = self.prepare(root, binding, str(uuid.uuid4()))
                before = self.control(root, binding).read_bytes()
                with self.assertRaises(cache.FreshnessHold):
                    cache.prepare_outputs(target, outputs, os.getuid(), os.getgid())
                self.assertEqual(self.control(root, binding).read_bytes(), before)

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

    def test_legacy_artifacts_are_retained_but_cannot_block_fresh_generation_recovery(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            legacy = root / "target"
            legacy.mkdir()
            (legacy / "keep").write_text("legacy")
            target = self.prepare(root, self.binding(), str(uuid.uuid4()))
            self.assertEqual(list(target.iterdir()), [])
            self.assertEqual((legacy / "keep").read_text(), "legacy")

    def test_new_authority_recovers_in_new_generation_without_certifying_prior_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binding = self.binding()
            old_target = self.prepare(root, binding, str(uuid.uuid4()))
            (old_target / "untrusted-artifact").write_text("prior authority")
            changed = binding | {"generation": str(uuid.uuid4()), "authority_sha256": "1" * 64}
            target = self.prepare(root, changed, str(uuid.uuid4()))
            self.assertEqual(list(target.iterdir()), [])
            self.assertNotEqual(target, old_target)
            self.assertEqual((old_target / "untrusted-artifact").read_text(), "prior authority")

    def test_reconstruction_recovers_after_torn_generation_without_promoting_old_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binding = self.binding()
            command = str(uuid.uuid4())
            target = self.prepare(root, binding, command)
            (target / "partial-output").write_text("incomplete")
            record = self.control(root, binding)
            record.write_bytes(b"{torn")
            with self.assertRaises(cache.FreshnessHold):
                self.prepare(root, binding, str(uuid.uuid4()), predecessor=command)
            next_binding = binding | {"generation": str(uuid.uuid4())}
            recovered = self.prepare(root, next_binding, str(uuid.uuid4()))
            self.assertEqual(list(recovered.iterdir()), [])
            self.assertEqual(record.read_bytes(), b"{torn")
            self.assertEqual((target / "partial-output").read_text(), "incomplete")

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

    def test_reclamation_is_bounded_preserves_active_generations_and_original_receipts(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);generations=[]
            for _ in range(4):
                binding=self.binding();target=self.prepare(root,binding,str(uuid.uuid4()))
                (target/'artifact').write_text('compiled source')
                generations.append((binding,target,self.control(root,binding).read_bytes()))
            protected=generations[0][0]['generation']
            retired=cache.reclaim(root,[protected],os.getuid(),os.getgid(),limit=2)
            self.assertEqual(len(retired),2)
            self.assertEqual((generations[0][1]/'artifact').read_text(),'compiled source')
            for binding,target,record in generations:
                if binding['generation'] in retired:
                    self.assertFalse(target.parent.exists())
                    receipt=cache._read_record(root/'ci-generation-retirements'/(binding['generation']+'.json'))
                    self.assertEqual(receipt['original'],json.loads(record))
                    self.assertEqual(receipt['state'],'reclaimed')
                    with self.assertRaises(cache.FreshnessHold):
                        self.prepare(root,binding,str(uuid.uuid4()))
            self.assertEqual(len(cache.reclaim(root,[protected],os.getuid(),os.getgid(),limit=2)),1)

    def test_reclamation_never_deletes_unknown_linked_or_foreign_allocations(self):
        for fault in ('torn','missing','extra-child','linked-target','foreign-target'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);binding=self.binding();target=self.prepare(root,binding,str(uuid.uuid4()))
                outside=root/'external-data';outside.mkdir();(outside/'keep').write_text('preserve')
                if fault=='torn':self.control(root,binding).write_text('{torn')
                elif fault=='missing':self.control(root,binding).unlink()
                elif fault=='extra-child':(target.parent/'unknown').write_text('preserve')
                elif fault=='linked-target':target.rmdir();target.symlink_to(outside)
                else:target.chmod(0o777)
                self.assertEqual(cache.reclaim(root,[],os.getuid(),os.getgid()),[])
                self.assertTrue(target.parent.exists())
                self.assertEqual((outside/'keep').read_text(),'preserve')

    def test_reclamation_crash_replays_the_same_original_receipt_without_reactivating(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding=self.binding();target=self.prepare(root,binding,str(uuid.uuid4()))
            (target/'artifact').write_text('compiled source');original=self.control(root,binding).read_bytes()
            with patch.object(cache.shutil,'rmtree',side_effect=OSError('simulated deletion failure')):
                with self.assertRaises(cache.FreshnessHold):cache.reclaim(root,[],os.getuid(),os.getgid())
            receipt=root/'ci-generation-retirements'/(binding['generation']+'.json')
            self.assertEqual(cache._read_record(receipt)['state'],'reclaiming')
            self.assertEqual(self.control(root,binding).read_bytes(),original)
            with self.assertRaises(cache.FreshnessHold):self.prepare(root,binding,str(uuid.uuid4()))
            self.assertEqual(cache.reclaim(root,[],os.getuid(),os.getgid()),[binding['generation']])
            self.assertEqual(cache._read_record(receipt)['original'],json.loads(original))
            self.assertFalse(target.parent.exists())

    def test_reclamation_recovers_partial_deletion_and_lost_completion_publication(self):
        for fault in ('control-deleted','completion-not-published'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);binding=self.binding();target=self.prepare(root,binding,str(uuid.uuid4()))
                original=self.control(root,binding).read_bytes()
                receipt=root/'ci-generation-retirements'/(binding['generation']+'.json')
                write=cache._write_record
                def fail_completion(path,value):
                    if value.get('state')=='reclaimed':raise OSError('publication interrupted')
                    return write(path,value)
                def partial_delete(path):
                    self.control(root,binding).unlink()
                    raise OSError('deletion interrupted')
                with patch.object(cache.shutil,'rmtree',side_effect=partial_delete) if fault=='control-deleted' else patch.object(cache,'_write_record',side_effect=fail_completion):
                    with self.assertRaises(cache.FreshnessHold):cache.reclaim(root,[],os.getuid(),os.getgid())
                self.assertEqual(cache._read_record(receipt)['state'],'reclaiming')
                self.assertEqual(cache.reclaim(root,[],os.getuid(),os.getgid()),[binding['generation']])
                self.assertFalse(target.parent.exists())
                final=cache._read_record(receipt)
                self.assertEqual(final['state'],'reclaimed')
                self.assertEqual(final['original'],json.loads(original))


if __name__ == "__main__":
    unittest.main()
