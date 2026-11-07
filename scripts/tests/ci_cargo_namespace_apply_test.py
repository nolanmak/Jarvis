"""Actual namespace syscalls and recovery, without publication authority."""
import errno
import hashlib
import json
import mmap
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import ci_cargo_capture_test as captures
views=captures.views


class NamespaceApplicationTests(unittest.TestCase):
    def setup_tree(self,root,directory=False):
        (root/'source').mkdir();(root/'destination').mkdir()
        node=root/'source/old'
        if directory:node.mkdir();(node/'child').write_bytes(b'unchanged child')
        else:node.write_bytes(b'original inode bytes')
        source=os.open(root/'source',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        destination=os.open(root/'destination',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        inode=os.open(node,os.O_RDONLY|os.O_CLOEXEC|(os.O_DIRECTORY if directory else 0))
        self.addCleanup(os.close,source);self.addCleanup(os.close,destination);self.addCleanup(os.close,inode)
        digest=hashlib.sha256(json.dumps({'original_inode':[node.stat().st_dev,node.stat().st_ino],'action':'move'},sort_keys=True).encode()).hexdigest()
        return node,source,destination,inode,digest

    def move(self,root,source,destination,inode,digest):
        return views.apply_namespace_step(root/'move.json',plan_sha256=digest,action='move',inode_fd=inode,
            source_parent=source,source_name=b'old',destination_parent=destination,destination_name=b'new',check=lambda:None)

    def test_move_preserves_open_inode_mapping_and_does_not_clobber_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root)
            mapping=mmap.mmap(inode,0,access=mmap.ACCESS_READ)
            try:
                result=self.move(root,source,destination,inode,digest)
                self.assertFalse(node.exists());new=root/'destination/new'
                self.assertEqual(new.stat().st_ino,os.fstat(inode).st_ino)
                self.assertEqual(mapping[:],b'original inode bytes')
                writer=os.open(new,os.O_RDWR|os.O_CLOEXEC)
                try:os.pwrite(writer,b'changed!',0)
                finally:os.close(writer)
                self.assertEqual(mapping[:8],b'changed!');self.assertFalse(result['effects_materialized'])
                self.assertEqual(result['state'],'applied')
            finally:mapping.close()

    def test_directory_move_preserves_descendant_inode_and_ctime(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root,directory=True)
            before=(node/'child').stat();self.move(root,source,destination,inode,digest)
            after=(root/'destination/new/child').stat()
            self.assertEqual((after.st_dev,after.st_ino,after.st_ctime_ns),(before.st_dev,before.st_ino,before.st_ctime_ns))

    def test_occupied_destination_is_preserved_before_any_move(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root)
            (root/'destination/new').write_bytes(b'foreign')
            with self.assertRaises(views.FreshnessHold):self.move(root,source,destination,inode,digest)
            self.assertEqual(node.read_bytes(),b'original inode bytes');self.assertEqual((root/'destination/new').read_bytes(),b'foreign')

    def test_move_completed_before_error_replays_from_held_inode_and_whole_parent_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root);real=views._rename_noreplace
            def interrupted(*args):real(*args);raise OSError(errno.EIO,'lost move acknowledgement')
            with patch.object(views,'_rename_noreplace',side_effect=interrupted),self.assertRaises(OSError):self.move(root,source,destination,inode,digest)
            with patch.object(views,'_rename_noreplace',side_effect=AssertionError('must not repeat completed move')):
                result=self.move(root,source,destination,inode,digest)
            self.assertEqual(result['state'],'applied');self.assertEqual((root/'destination/new').stat().st_ino,os.fstat(inode).st_ino)

    def test_move_completed_before_progress_sync_replays_without_another_rename(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root);real=views.cache._write_record;failed=[]
            def interrupted(path,value):
                if value.get('state')=='applied' and not failed:failed.append(True);raise OSError(errno.ENOSPC,'progress sync failed')
                return real(path,value)
            with patch.object(views.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):self.move(root,source,destination,inode,digest)
            before=os.fstat(inode)
            result=self.move(root,source,destination,inode,digest)
            self.assertEqual(os.fstat(inode).st_ctime_ns,before.st_ctime_ns);self.assertEqual(result['state'],'applied')

    def test_foreign_replacement_or_other_parent_change_cannot_be_adopted_as_a_move(self):
        for fault in ('replacement','extra'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root);real=views._rename_noreplace
                def interrupted(*args):real(*args);raise OSError(errno.EIO,'lost acknowledgement')
                with patch.object(views,'_rename_noreplace',side_effect=interrupted),self.assertRaises(OSError):self.move(root,source,destination,inode,digest)
                if fault=='replacement':
                    (root/'foreign').write_bytes(b'foreign');(root/'foreign').replace(root/'destination/new')
                else:(root/'destination/extra').write_bytes(b'foreign')
                with self.assertRaises(views.FreshnessHold):self.move(root,source,destination,inode,digest)
                if fault=='replacement':self.assertEqual((root/'destination/new').read_bytes(),b'foreign')
                else:self.assertEqual((root/'destination/extra').read_bytes(),b'foreign')

    def test_unlink_preserves_open_old_inode_and_replays_acknowledgement_loss(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root);real=views.os.unlink;failed=[]
            def interrupted(path,*args,**kwargs):
                result=real(path,*args,**kwargs)
                if path==b'old' and not failed:failed.append(True);raise OSError(errno.EIO,'lost unlink acknowledgement')
                return result
            def remove():return views.apply_namespace_step(root/'remove.json',plan_sha256=digest,action='remove',inode_fd=inode,source_parent=source,source_name=b'old',check=lambda:None)
            with patch.object(views.os,'unlink',side_effect=interrupted),self.assertRaises(OSError):remove()
            result=remove();self.assertEqual(result['state'],'applied');self.assertEqual(os.fstat(inode).st_nlink,0)
            self.assertEqual(os.pread(inode,100,0),b'original inode bytes')

    def test_prepared_anonymous_file_is_attached_once_with_its_recorded_inode(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root)
            prepared=os.open('.',os.O_TMPFILE|os.O_RDWR|os.O_CLOEXEC,0o600,dir_fd=destination)
            try:
                os.write(prepared,b'prepared bytes');os.fsync(prepared)
                result=views.apply_namespace_step(root/'attach.json',plan_sha256=digest,action='attach',inode_fd=prepared,
                    destination_parent=destination,destination_name=b'new',check=lambda:None)
                self.assertEqual(result['state'],'applied');self.assertEqual(os.fstat(prepared).st_nlink,1)
                self.assertEqual((root/'destination/new').stat().st_ino,os.fstat(prepared).st_ino)
                self.assertEqual((root/'destination/new').read_bytes(),b'prepared bytes')
            finally:os.close(prepared)


    def test_same_parent_move_and_move_into_child_directory(self):
        for nested in (False,True):
            with self.subTest(nested=nested),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root)
                if nested:
                    (root/'source/child').mkdir();target=os.open(root/'source/child',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
                else:target=os.dup(source)
                try:
                    result=views.apply_namespace_step(root/'same.json',plan_sha256=digest,action='move',inode_fd=inode,
                        source_parent=source,source_name=b'old',destination_parent=target,destination_name=b'new',check=lambda:None)
                    final=root/('source/child/new' if nested else 'source/new')
                    self.assertEqual(final.stat().st_ino,os.fstat(inode).st_ino);self.assertEqual(result['state'],'applied')
                finally:os.close(target)

    def test_rmdir_replay_keeps_original_directory_capability(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root,directory=True)
            (node/'child').unlink();real=views.os.rmdir;failed=[]
            def interrupted(path,*args,**kwargs):
                result=real(path,*args,**kwargs)
                if path==b'old' and not failed:failed.append(True);raise OSError(errno.EIO,'lost rmdir acknowledgement')
                return result
            def remove():return views.apply_namespace_step(root/'rmdir.json',plan_sha256=digest,action='remove',inode_fd=inode,
                source_parent=source,source_name=b'old',check=lambda:None)
            with patch.object(views.os,'rmdir',side_effect=interrupted),self.assertRaises(OSError):remove()
            result=remove();self.assertFalse(node.exists());self.assertEqual(os.fstat(inode).st_nlink,0)
            self.assertFalse(result['shipping_authority'])

    def test_kernel_refuses_destination_race_without_overwriting_foreign_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root);real=views._rename_noreplace
            def raced(*args):
                (root/'destination/new').write_bytes(b'foreign race');return real(*args)
            with patch.object(views,'_rename_noreplace',side_effect=raced),self.assertRaises(OSError) as caught:
                self.move(root,source,destination,inode,digest)
            self.assertEqual(caught.exception.errno,errno.EEXIST)
            self.assertEqual(node.read_bytes(),b'original inode bytes');self.assertEqual((root/'destination/new').read_bytes(),b'foreign race')
            with self.assertRaises(views.FreshnessHold):self.move(root,source,destination,inode,digest)
            self.assertEqual((root/'destination/new').read_bytes(),b'foreign race')

    def test_no_move_is_dispatched_without_a_durable_prepared_record(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root)
            with patch.object(views.cache,'_write_record',side_effect=OSError(errno.ENOSPC,'intent storage full')):
                with patch.object(views,'_rename_noreplace',side_effect=AssertionError('move before intent')),self.assertRaises(OSError):
                    self.move(root,source,destination,inode,digest)
            self.assertTrue(node.exists());self.assertFalse((root/'destination/new').exists())

    def test_wrong_plan_or_forked_controller_cannot_replay_a_prepared_move(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root)
            with patch.object(views,'_rename_noreplace',side_effect=OSError(errno.EINTR,'not yet dispatched')),self.assertRaises(OSError):
                self.move(root,source,destination,inode,digest)
            with self.assertRaises(views.FreshnessHold):self.move(root,source,destination,inode,hashlib.sha256(b'other plan').hexdigest())
            pid=os.fork()
            if pid==0:
                try:self.move(root,source,destination,inode,digest)
                except views.FreshnessHold:os._exit(0)
                except BaseException:os._exit(2)
                os._exit(1)
            self.assertEqual(os.waitpid(pid,0)[1],0);self.assertTrue(node.exists())

    def test_attachment_acknowledgement_loss_replays_without_linking_twice(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);node,source,destination,inode,digest=self.setup_tree(root)
            prepared=os.open('.',os.O_TMPFILE|os.O_RDWR|os.O_CLOEXEC,0o600,dir_fd=destination)
            try:
                os.write(prepared,b'prepared bytes');os.fsync(prepared);real=views._link_prepared_inode
                def interrupted(*args):real(*args);raise OSError(errno.EIO,'lost link acknowledgement')
                def attach():return views.apply_namespace_step(root/'attach-replay.json',plan_sha256=digest,action='attach',inode_fd=prepared,
                    destination_parent=destination,destination_name=b'new',check=lambda:None)
                with patch.object(views,'_link_prepared_inode',side_effect=interrupted),self.assertRaises(OSError):attach()
                with patch.object(views,'_link_prepared_inode',side_effect=AssertionError('must not link twice')):result=attach()
                self.assertEqual(os.fstat(prepared).st_nlink,1);self.assertEqual(result['state'],'applied')
            finally:os.close(prepared)


if __name__=='__main__':unittest.main()
