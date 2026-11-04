"""Real filesystem capture mechanics; no test result is CI authority."""
import importlib.util
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('capture_views',ROOT/'ci-cargo-views.py')
views=importlib.util.module_from_spec(spec);spec.loader.exec_module(views)


class CaptureRootsTests(unittest.TestCase):
    def capture(self,source,destination,roots=('out',),*,check=lambda:None,owners=None):
        descriptor=os.open(source,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:return views.capture_roots(descriptor,destination,list(roots),
            owners=tuple(sorted({os.getuid(),os.getgid()})) if owners is None else owners,check=check)
        finally:os.close(descriptor)

    def tree(self,root):
        source=root/'source';(source/'out/empty').mkdir(parents=True)
        data=source/'out'/os.fsdecode(b'raw-\xff');data.write_bytes(b'binary\x00\xffpayload');data.chmod(0o750)
        os.utime(data,ns=(946684800000000000,946684800000000001))
        return source,data

    def test_exact_bytes_empty_directories_and_detached_sealed_inodes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data=self.tree(root);before=views._key(data.stat())
            destination=root/'capture';record=self.capture(source,destination)
            copied=destination/data.relative_to(source)
            self.assertEqual(copied.read_bytes(),b'binary\x00\xffpayload')
            self.assertNotEqual(copied.stat().st_ino,data.stat().st_ino)
            self.assertTrue((destination/'out/empty').is_dir())
            self.assertEqual(views._key(data.stat()),before)
            self.assertEqual(stat.S_IMODE(copied.stat().st_mode),0o555)
            for path in [destination,*destination.rglob('*')]:
                self.assertEqual(path.stat().st_uid,os.getuid())
                self.assertFalse(path.stat().st_mode&0o222)
            self.assertEqual(record['copied_bytes'],len(b'binary\x00\xffpayload'))
            self.assertFalse(record['input_closure_complete']);self.assertFalse(record['shipping_authority'])

    def test_unsupported_entries_and_external_hardlinks_are_refused_without_following(self):
        for kind in ('symlink','hardlink','fifo'):
            with self.subTest(kind=kind),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);source,data=self.tree(root);outside=root/'outside';outside.write_bytes(b'private')
                extra=source/'out/unsupported'
                if kind=='symlink':extra.symlink_to(outside)
                elif kind=='hardlink':os.link(outside,extra)
                else:os.mkfifo(extra)
                with self.assertRaises(views.FreshnessHold):self.capture(source,root/'capture')
                self.assertEqual(outside.read_bytes(),b'private')

    def test_symlink_in_a_selected_root_component_cannot_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,_=self.tree(root);(source/'alias').symlink_to(source/'out',target_is_directory=True)
            with self.assertRaises(views.FreshnessHold):self.capture(source,root/'capture',('alias/empty',))

    def test_changed_bytes_during_copy_cannot_publish_a_capture(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data=self.tree(root);identity=(data.stat().st_dev,data.stat().st_ino)
            original=os.read;changed=[]
            def racing_read(descriptor,size):
                block=original(descriptor,size);info=os.fstat(descriptor)
                if block and not changed and (info.st_dev,info.st_ino)==identity:
                    changed.append(True);data.write_bytes(b'changed during read')
                return block
            with patch.object(views.os,'read',side_effect=racing_read),self.assertRaises(views.FreshnessHold):
                self.capture(source,root/'capture')
            self.assertEqual(changed,[True])

    def test_new_directory_member_during_copy_cannot_publish_a_capture(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data=self.tree(root);destination=root/'capture';changed=[]
            def check():
                if not changed and (destination/data.relative_to(source)).exists():
                    changed.append(True);(source/'out/late').write_bytes(b'new input')
            with self.assertRaises(views.FreshnessHold):self.capture(source,destination,check=check)
            self.assertEqual(changed,[True])

    def test_changed_staged_bytes_cannot_claim_the_original_source_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data=self.tree(root);destination=root/'capture';changed=[]
            def check():
                copied=destination/data.relative_to(source)
                if not changed and copied.exists() and copied.stat().st_size==len(b'binary\x00\xffpayload'):
                    changed.append(True);copied.chmod(0o600);copied.write_bytes(b'corrupt staged payload');copied.chmod(0o555)
            with self.assertRaises(views.FreshnessHold):self.capture(source,destination,check=check)
            self.assertEqual(changed,[True])

    def test_owner_bounds_and_cancellation_are_enforced(self):
        for fault in ('owner','bytes','entries','cancel'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);source,_=self.tree(root)
                def check():
                    if fault=='cancel':raise views.FreshnessHold('cleanup_uncertain')
                with patch.object(views,'MAX_BYTES',3 if fault=='bytes' else views.MAX_BYTES),\
                        patch.object(views,'MAX_ENTRIES',2 if fault=='entries' else views.MAX_ENTRIES),\
                        self.assertRaises(views.FreshnessHold):
                    self.capture(source,root/'capture',check=check,owners=(os.getuid()+1,) if fault=='owner' else None)

    def test_overlapping_or_traversing_roots_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,_=self.tree(root)
            for roots in (('out','out'),('out','out/empty'),('../outside',),('/out',)):
                with self.subTest(roots=roots),self.assertRaises(views.FreshnessHold):
                    self.capture(source,root/'capture',roots)

    def test_writable_stdio_cannot_alias_an_original_captured_inode(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data=self.tree(root);record=self.capture(source,root/'capture')
            writer=os.open(data,os.O_WRONLY|os.O_CLOEXEC);null=os.open(os.devnull,os.O_WRONLY|os.O_CLOEXEC)
            read,pipe=os.pipe2(os.O_CLOEXEC)
            try:
                with self.assertRaises(views.FreshnessHold):views.validate_capture_stdio(record,[writer,null])
                views.validate_capture_stdio(record,[pipe,null])
                self.assertEqual(os.read(read,0),b'')
            finally:
                for descriptor in (writer,null,read,pipe):os.close(descriptor)

    def test_child_working_tree_preserves_bytes_modes_and_mtime_without_sharing_writes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data=self.tree(root);sealed=root/'capture'
            capture=self.capture(source,sealed);working=root/'working'
            result=views.materialize_capture(sealed,capture,working,parent_uid=os.getuid(),unit_uid=os.getuid(),check=lambda:None)
            copied=working/data.relative_to(source)
            self.assertEqual(copied.read_bytes(),data.read_bytes())
            self.assertEqual(stat.S_IMODE(copied.stat().st_mode),0o750)
            self.assertEqual(copied.stat().st_mtime_ns,data.stat().st_mtime_ns)
            self.assertNotEqual(copied.stat().st_ino,data.stat().st_ino)
            self.assertNotEqual(copied.stat().st_ino,(sealed/data.relative_to(source)).stat().st_ino)
            copied.write_bytes(b'child effects')
            self.assertEqual(data.read_bytes(),b'binary\x00\xffpayload')
            self.assertEqual((sealed/data.relative_to(source)).read_bytes(),b'binary\x00\xffpayload')
            self.assertTrue((working/'out/empty').is_dir())
            self.assertFalse(result['input_closure_complete']);self.assertFalse(result['shipping_authority'])

    def test_working_tree_refuses_a_changed_sealed_payload_before_execution(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data=self.tree(root);sealed=root/'capture';capture=self.capture(source,sealed)
            original=sealed/data.relative_to(source);original.chmod(0o600);original.write_bytes(b'corrupt');original.chmod(0o555)
            with self.assertRaises(views.FreshnessHold):
                views.materialize_capture(sealed,capture,root/'working',parent_uid=os.getuid(),unit_uid=os.getuid(),check=lambda:None)


if __name__=='__main__':unittest.main()
