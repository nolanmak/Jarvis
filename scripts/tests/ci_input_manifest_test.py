import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import struct


spec=importlib.util.spec_from_file_location('ci_input_manifest',Path(__file__).resolve().parents[1]/'ci-input-manifest.py')
manifest=importlib.util.module_from_spec(spec);spec.loader.exec_module(manifest)


class InputManifestTests(unittest.TestCase):
    def test_restored_generated_input_is_still_observed_as_a_write(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);generated=root/'generated.rs';generated.write_text('original input')
            os.utime(generated,ns=(946684800000000000,946684800000000000))
            before=manifest.snapshot({'generated':root})
            journal=manifest.ChangeJournal({'generated':root})
            generated.write_text('different input compiled in the middle')
            generated.write_text('original input')
            os.utime(generated,ns=(946684800000000000,946684800000000000))
            self.assertEqual(before['sha256'],manifest.snapshot({'generated':root})['sha256'])
            observation=journal.finish()
            self.assertTrue(observation['changed'],'before/after snapshots hid a changed and restored compiler input')

    def test_equal_old_timestamp_never_hides_different_content(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'source.rs';source.write_text('old')
            os.utime(source,ns=(1,1));before=manifest.snapshot({'source':root})
            source.write_text('new');os.utime(source,ns=(1,1))
            self.assertNotEqual(before['sha256'],manifest.snapshot({'source':root})['sha256'])

    def test_unchanged_input_identity_survives_an_equivalent_reconstruction(self):
        with tempfile.TemporaryDirectory() as first,tempfile.TemporaryDirectory() as second:
            for root in [Path(first),Path(second)]:(root/'input.rs').write_text('same input')
            self.assertEqual(manifest.snapshot({'source':Path(first)})['sha256'],
                             manifest.snapshot({'source':Path(second)})['sha256'])

    def test_repeated_read_only_steps_do_not_report_a_write(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'input.rs';source.write_text('same input')
            journal=manifest.ChangeJournal({'generated':root})
            self.assertEqual(source.read_text(),'same input')
            observed=journal.finish()
            self.assertFalse(observed['changed'])
            self.assertEqual(observed['observed_events'],0)
            with self.assertRaises(manifest.FreshnessHold):journal.finish()

    def test_nested_and_new_subtree_changes_are_observed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);nested=root/'existing';nested.mkdir();source=nested/'input.rs';source.write_text('original')
            journal=manifest.ChangeJournal({'generated':root})
            source.write_text('changed');source.write_text('original')
            self.assertTrue(journal.finish()['changed'])
            journal=manifest.ChangeJournal({'generated':root})
            new=root/'new';new.mkdir();(new/'input.rs').write_text('transient');(new/'input.rs').unlink();new.rmdir()
            observed=journal.finish();self.assertTrue(observed['changed']);self.assertEqual(observed['changed_roots'],['generated'])

    def test_kernel_overflow_or_lost_watch_is_unknown_and_releases_the_journal(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            for mask in [0x4000,0x2000,0x8000]:
                journal=manifest.ChangeJournal({'generated':root})
                with mock.patch.object(manifest.os,'read',return_value=struct.pack('iIII',-1,mask,0,0)):
                    with self.assertRaises(manifest.FreshnessHold):journal.finish()
                self.assertEqual(journal.fd,-1)

    def test_unknown_links_special_files_and_budget_excess_refuse_a_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'source';source.write_text('original source')
            with self.assertRaises(manifest.FreshnessHold):manifest.snapshot({'source':root},max_bytes=1)
            os.symlink('/etc/passwd',root/'unknown')
            with self.assertRaises(manifest.FreshnessHold):manifest.snapshot({'source':root})
            (root/'unknown').unlink();os.mkfifo(root/'pipe')
            with self.assertRaises(manifest.FreshnessHold):manifest.snapshot({'source':root})


if __name__=='__main__':unittest.main()
