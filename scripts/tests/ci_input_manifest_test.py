import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import struct
import mmap


spec=importlib.util.spec_from_file_location('ci_input_manifest',Path(__file__).resolve().parents[1]/'ci-input-manifest.py')
manifest=importlib.util.module_from_spec(spec);spec.loader.exec_module(manifest)


class InputManifestTests(unittest.TestCase):
    def test_writable_memory_mapping_cannot_hide_a_restored_input(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'input.bin';source.write_bytes(b'original')
            journal=manifest.ChangeJournal({'generated':root})
            with source.open('r+b') as stream:
                with mmap.mmap(stream.fileno(),0) as mapping:
                    mapping[:]=b'modified';mapping.flush();mapping[:]=b'original';mapping.flush()
            self.assertTrue(journal.finish()['changed'])
    def test_npm_executable_and_directory_aliases_stay_inside_declared_namespaces(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);modules=root/'node_modules';modules.mkdir();binaries=modules/'.bin';binaries.mkdir()
            package=modules/'tool';package.mkdir();script=package/'cli.js';script.write_text('original')
            (binaries/'tool').symlink_to('../tool/cli.js');(modules/'alias').symlink_to('tool',target_is_directory=True)
            before=manifest.snapshot({'modules':modules});journal=manifest.ChangeJournal({'modules':modules})
            script.read_text();self.assertFalse(journal.finish()['changed'])
            journal=manifest.ChangeJournal({'modules':modules});script.write_text('changed');script.write_text('original')
            observed=journal.finish();self.assertTrue(observed['changed'])
            self.assertEqual(before['sha256'],manifest.snapshot({'modules':modules})['sha256'])

    def test_alias_chains_may_not_leave_a_declared_root_and_return_through_an_external_link(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);declared=root/'declared';declared.mkdir();(declared/'input').write_text('original')
            (root/'external').symlink_to(declared,target_is_directory=True)
            (declared/'alias').symlink_to('../external/input')
            with self.assertRaises(manifest.FreshnessHold):manifest.snapshot({'inputs':declared})

    def test_cross_namespace_alias_and_cycles_have_explicit_bounded_behavior(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);modules=root/'modules';modules.mkdir();generated=root/'generated';generated.mkdir()
            (generated/'input.js').write_text('generated input');(modules/'input.js').symlink_to('../generated/input.js')
            before=manifest.snapshot({'modules':modules,'generated':generated})
            self.assertTrue(any(entry[2]=='symlink' and entry[-2:]==['generated','input.js'] for entry in before['entries']))
            (modules/'a').symlink_to('b');(modules/'b').symlink_to('a')
            with self.assertRaises(manifest.FreshnessHold):manifest.snapshot({'modules':modules,'generated':generated})
    def test_declared_single_file_output_is_watched_without_watching_its_neighbors(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);output=root/'generated.css';output.write_text('original');other=root/'tracked.css';other.write_text('source')
            journal=manifest.ChangeJournal({'css':output});other.read_text()
            self.assertFalse(journal.finish()['changed'])
            journal=manifest.ChangeJournal({'css':output});output.write_text('changed');output.write_text('original')
            self.assertTrue(journal.finish()['changed'])
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
