"""Whole filesystem transfers; no caller pause, alias or CI authority."""
import errno
import json
import mmap
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch
import ci_cargo_namespace_test as namespace
views=namespace.views


class TransferTests(unittest.TestCase):
    def fixture(self,root,tree):return namespace.NamespaceEdgesTests().fixture(root,tree)

    def transfer(self,root,held):
        plan=held.prepare_transfer(root/'transfer',root/'before',check=lambda:None)
        return views.RetainedTransfer(held,root/'transfer',plan,check=lambda:None)

    def test_replace_write_unlink_move_create_and_metadata_in_one_transfer(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{
                'replace':b'original','remove':b'removed','sub/child':b'child','keep':b'unchanged'})
            descriptors={name:os.open(source/'out'/name,os.O_RDONLY) for name in ('replace','remove','sub/child')}
            original={name:os.fstat(fd) for name,fd in descriptors.items()}
            try:
                with held:
                    (working/'out/replace').write_bytes(b'old inode changed')
                    (working/'out/new').write_bytes(b'replacement');(working/'out/new').replace(working/'out/replace')
                    (working/'out/remove').write_bytes(b'changed then removed');(working/'out/remove').unlink()
                    (working/'out/sub').rename(working/'out/moved')
                    (working/'out/fresh').mkdir();(working/'out/fresh/new').write_bytes(b'created child')
                    (working/'out/fresh').chmod(0o750);os.utime(working/'out',ns=(9,123456789))
                    with self.transfer(root,held) as transfer:result=transfer.apply()
                    self.assertEqual(result['state'],'transfer_applied')
                    self.assertFalse(result['shipping_authority']);self.assertFalse(result['effects_materialized'])
                    self.assertFalse(result['input_closure_complete']);self.assertFalse(result['full_ci'])
                    self.assertEqual((source/'out/replace').read_bytes(),b'replacement')
                    self.assertNotEqual((source/'out/replace').stat().st_ino,original['replace'].st_ino)
                    self.assertEqual(os.pread(descriptors['replace'],100,0),b'old inode changed')
                    self.assertEqual(os.pread(descriptors['remove'],100,0),b'changed then removed')
                    self.assertEqual(os.fstat(descriptors['remove']).st_nlink,0)
                    self.assertEqual((source/'out/moved/child').stat().st_ino,original['sub/child'].st_ino)
                    self.assertEqual((source/'out/fresh/new').read_bytes(),b'created child')
                    self.assertEqual(stat.S_IMODE((source/'out/fresh').stat().st_mode),0o750)
                    self.assertEqual((source/'out').stat().st_mtime_ns,123456789)
                    self.assertEqual(set(p.name for p in (source/'out').iterdir()),{'replace','moved','fresh','keep'})
            finally:
                for fd in descriptors.values():os.close(fd)

    def test_swaps_preserve_both_old_inodes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'a':b'A','b':b'B'})
            with held:
                identities={n:(source/'out'/n).stat().st_ino for n in ('a','b')}
                (working/'out/a').rename(working/'out/temp');(working/'out/b').rename(working/'out/a');(working/'out/temp').rename(working/'out/b')
                with self.transfer(root,held) as transfer:transfer.apply()
                self.assertEqual((source/'out/a').stat().st_ino,identities['b'])
                self.assertEqual((source/'out/b').stat().st_ino,identities['a'])

    def test_ancestor_move_does_not_relink_unchanged_descendant(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'sub/child':b'C'})
            with held:
                before=(source/'out/sub/child').stat()
                (working/'out/sub').rename(working/'out/moved')
                with self.transfer(root,held) as transfer:transfer.apply()
                after=(source/'out/moved/child').stat()
                self.assertEqual((after.st_ino,after.st_ctime_ns),(before.st_ino,before.st_ctime_ns))

    def test_unlinked_parent_and_moved_child_are_applied(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'sub/child':b'C'})
            with held:
                inode=(source/'out/sub/child').stat().st_ino
                (working/'out/sub/child').rename(working/'out/kept');(working/'out/sub').rmdir()
                with self.transfer(root,held) as transfer:transfer.apply()
                self.assertEqual(set(p.name for p in (source/'out').iterdir()),{'kept'})
                self.assertEqual((source/'out/kept').stat().st_ino,inode)

    def test_inplace_writes_keep_parent_offsets_and_mappings(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'A'*4096})
            fd=os.open(source/'out/data',os.O_RDWR);shared=mmap.mmap(fd,4096,flags=mmap.MAP_SHARED)
            try:
                os.lseek(fd,23,os.SEEK_SET)
                with held:
                    (working/'out/data').write_bytes(b'B'*4096)
                    with self.transfer(root,held) as transfer:transfer.apply()
                self.assertEqual(shared[:],b'B'*4096);self.assertEqual(os.lseek(fd,0,os.SEEK_CUR),23)
            finally:shared.close();os.close(fd)

    def test_corrupt_new_payload_refuses_old_inode_writes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                (working/'out/data').write_bytes(b'changed');(working/'out/new').write_bytes(b'new')
                with self.transfer(root,held) as transfer:
                    payload=root/'transfer/after/out/new';payload.chmod(0o600);payload.write_bytes(b'bad');payload.chmod(0o444)
                    with self.assertRaises(views.FreshnessHold):transfer.apply()
                self.assertEqual((source/'out/data').read_bytes(),b'original')

    def test_completed_transfer_revalidates_foreign_bytes_and_membership(self):
        for fault in ('bytes','extra','replace'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
                with held:
                    (working/'out/new').write_bytes(b'new')
                    with self.transfer(root,held) as transfer:
                        transfer.apply()
                        if fault=='bytes':(source/'out/data').write_bytes(b'foreign')
                        elif fault=='extra':(source/'out/extra').write_bytes(b'foreign')
                        else:
                            (source/'out/new').unlink();(source/'out/new').write_bytes(b'new')
                        with self.assertRaises(views.FreshnessHold):transfer.apply()

    def test_lost_phase_commit_replays_only_current_phase(self):
        for phase in ('objects','namespace','directories','transfer_applied'):
            with self.subTest(phase=phase),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
                with held:
                    (working/'out/data').write_bytes(b'changed');(working/'out/newdir').mkdir()
                    (working/'out/newdir/file').write_bytes(b'new')
                    with self.transfer(root,held) as transfer:
                        write=views.cache._write_record;failed=[]
                        def interrupt(path,value):
                            if path.name=='transfer-progress.json' and value.get('state')==phase and not failed:
                                failed.append(True);raise OSError(errno.ENOSPC,'lost phase commit')
                            return write(path,value)
                        with patch.object(views.cache,'_write_record',side_effect=interrupt),self.assertRaises(OSError):transfer.apply()
                        self.assertTrue(failed)
                        with patch.object(held,'apply_inode_effects',side_effect=AssertionError('replayed old completed phase')):
                            result=transfer.apply()
                        self.assertEqual(result['state'],'transfer_applied')
                        self.assertEqual((source/'out/newdir/file').read_bytes(),b'new')

    def test_cancel_before_mutation_preserves_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                (working/'out/data').write_bytes(b'changed')
                with self.transfer(root,held) as transfer:
                    def cancelled():raise views.FreshnessHold('cleanup_uncertain')
                    transfer.check=cancelled
                    with self.assertRaises(views.FreshnessHold):transfer.apply()
                self.assertEqual((source/'out/data').read_bytes(),b'original')


    def test_late_new_child_write_is_rejected_before_parent_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                (working/'out/data').write_bytes(b'changed');(working/'out/new').write_bytes(b'new')
                with self.transfer(root,held) as transfer:
                    (working/'out/new').write_bytes(b'late write')
                    with self.assertRaises(views.FreshnessHold):transfer.apply()
                self.assertEqual((source/'out/data').read_bytes(),b'original')

    def test_foreign_change_after_object_failure_is_not_absorbed_as_a_new_baseline(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                (working/'out/new').write_bytes(b'new')
                with self.transfer(root,held) as transfer:
                    with patch.object(views.PreparedTransferObjects,'prepare_file',side_effect=OSError(errno.ENOSPC,'allocation failed')):
                        with self.assertRaises(OSError):transfer.apply()
                    (source/'out/foreign').write_bytes(b'foreign')
                    with self.assertRaises(views.FreshnessHold):transfer.apply()
                    self.assertFalse((source/'out/new').exists())
                    self.assertEqual((source/'out/foreign').read_bytes(),b'foreign')

    def test_foreign_change_before_first_namespace_record_prevents_dispatch(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                (working/'out/new').write_bytes(b'new')
                with self.transfer(root,held) as transfer:
                    with patch.object(views,'apply_namespace_sequence',side_effect=OSError(errno.EIO,'before sequence')):
                        with self.assertRaises(OSError):transfer.apply()
                    (source/'out/data').write_bytes(b'foreign')
                    with patch.object(views,'apply_namespace_sequence',side_effect=AssertionError('must not dispatch')):
                        with self.assertRaises(views.FreshnessHold):transfer.apply()
                    self.assertFalse((source/'out/new').exists())

    def test_new_anonymous_file_is_allocated_on_the_mutable_root_not_its_prefix(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                (working/'out/new').write_bytes(b'new')
                with self.transfer(root,held) as transfer:
                    prepare=views.PreparedTransferObjects.prepare_file;parents=[]
                    def record(holder,key,parent,payload,expected):
                        parents.append(os.fstat(parent).st_ino)
                        return prepare(holder,key,parent,payload,expected)
                    with patch.object(views.PreparedTransferObjects,'prepare_file',record):transfer.apply()
                    self.assertEqual(parents,[(source/'out').stat().st_ino])

    def test_removed_root_is_an_explicit_tombstone(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                fd=os.open(source/'out/data',os.O_RDONLY)
                try:
                    (working/'out/data').write_bytes(b'final bytes');(working/'out/data').unlink();(working/'out').rmdir()
                    with self.transfer(root,held) as transfer:transfer.apply()
                    self.assertFalse((source/'out').exists());self.assertEqual(os.pread(fd,100,0),b'final bytes')
                finally:os.close(fd)

    def test_lost_holder_cannot_adopt_a_partial_transfer(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                (working/'out/new').write_bytes(b'new')
                with self.transfer(root,held) as transfer:
                    with patch.object(views,'apply_namespace_sequence',side_effect=OSError(errno.EIO,'before sequence')):
                        with self.assertRaises(OSError):transfer.apply()
                    with self.assertRaises(views.FreshnessHold):
                        views.RetainedTransfer(held,root/'transfer',transfer.plan,check=lambda:None)
                    self.assertFalse((source/'out/new').exists())


    def test_lost_directory_sync_ack_recovers_only_the_exact_metadata_effect(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                os.utime(working/'out',ns=(17,123456789))
                with self.transfer(root,held) as transfer:
                    sync=views.os.fsync;failed=[];inode=(source/'out').stat().st_ino
                    def interrupt(fd):
                        info=os.fstat(fd)
                        if info.st_ino==inode and info.st_mtime_ns==123456789 and not failed:
                            failed.append(True);raise OSError(errno.EIO,'lost directory sync ack')
                        return sync(fd)
                    with patch.object(views.os,'fsync',side_effect=interrupt),self.assertRaises(OSError):transfer.apply()
                    self.assertTrue(failed);after=(source/'out').stat()
                    with patch.object(views.os,'utime',side_effect=AssertionError('rewrote completed metadata')):
                        result=transfer.apply()
                    self.assertEqual(result['state'],'transfer_applied')
                    self.assertEqual((source/'out').stat().st_ctime_ns,after.st_ctime_ns)

    def test_foreign_write_during_directory_recovery_is_retained_and_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'data':b'original'})
            with held:
                os.utime(working/'out',ns=(17,123456789))
                with self.transfer(root,held) as transfer:
                    sync=views.os.fsync;failed=[];inode=(source/'out').stat().st_ino
                    def interrupt(fd):
                        info=os.fstat(fd)
                        if info.st_ino==inode and info.st_mtime_ns==123456789 and not failed:
                            failed.append(True);raise OSError(errno.EIO,'lost directory sync ack')
                        return sync(fd)
                    with patch.object(views.os,'fsync',side_effect=interrupt),self.assertRaises(OSError):transfer.apply()
                    (source/'out/data').write_bytes(b'foreign')
                    with self.assertRaises(views.FreshnessHold):transfer.apply()
                    self.assertEqual((source/'out/data').read_bytes(),b'foreign')


if __name__=='__main__':unittest.main()
