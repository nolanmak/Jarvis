"""Owned per-transfer creation; no alias, materialization or CI authority."""
import errno
import hashlib
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch
import ci_cargo_capture_test as captures
views=captures.views


class TransferObjectsTests(unittest.TestCase):
    def fixture(self,root):
        (root/'journal').mkdir(mode=0o700);(root/'out').mkdir()
        payload=root/'payload';payload.write_bytes(b'verified prepared bytes');payload.chmod(0o444)
        parent=os.open(root/'out',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC);self.addCleanup(os.close,parent)
        expected={'size':payload.stat().st_size,'sha256':hashlib.sha256(payload.read_bytes()).hexdigest(),
            'mode':0o750,'mtime_ns':946684800000000001,'uid':os.getuid(),'gid':os.getgid()}
        plan=hashlib.sha256(b'owned-transfer-test-plan').hexdigest()
        return payload,parent,expected,plan

    def holder(self,root,plan):return views.PreparedTransferObjects(root/'journal',plan_sha256=plan,check=lambda:None)

    def test_file_is_verified_anonymous_and_keeps_its_recorded_capability(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            with self.holder(root,plan) as holder:
                fd=holder.prepare_file('new-file',parent,payload,expected)
                self.assertEqual(os.fstat(fd).st_nlink,0);self.assertEqual(os.pread(fd,100,0),payload.read_bytes())
                self.assertEqual(stat.S_IMODE(os.fstat(fd).st_mode),0o750);self.assertEqual(os.fstat(fd).st_mtime_ns,expected['mtime_ns'])
                self.assertFalse(os.get_inheritable(fd));self.assertEqual(list((root/'out').iterdir()),[])
                self.assertEqual(holder.prepare_file('new-file',parent,payload,expected),fd)

    def test_directories_bind_identity_and_remain_private_while_children_are_prepared(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            with self.holder(root,plan) as holder:
                stage=holder.prepare_directory('stage',parent,b'private-stage')
                child=holder.prepare_directory('new-dir',stage,b'new-dir')
                self.assertEqual(holder.prepare_directory('stage',parent,b'private-stage'),stage)
                self.assertEqual((root/'out/private-stage/new-dir').stat().st_ino,os.fstat(child).st_ino)
                self.assertEqual(stat.S_IMODE(os.fstat(stage).st_mode),0o700);self.assertFalse(os.get_inheritable(child))

    def test_lost_directory_commit_recovers_through_the_still_held_created_inode(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root);real=views.cache._write_record;failed=[]
            def interrupted(path,value):
                if value.get('kind')=='ci-prepared-directory' and value.get('state')=='prepared' and not failed:
                    failed.append(True);raise OSError(errno.ENOSPC,'prepared identity commit failed')
                return real(path,value)
            with self.holder(root,plan) as holder:
                with patch.object(views.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):
                    holder.prepare_directory('stage',parent,b'stage')
                before=(root/'out/stage').stat()
                fd=holder.prepare_directory('stage',parent,b'stage')
                self.assertEqual(os.fstat(fd).st_ino,before.st_ino)

    def test_unknown_directory_after_lost_mkdir_acknowledgement_is_never_adopted_by_name(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root);real=views.os.mkdir
            def interrupted(path,*args,**kwargs):
                result=real(path,*args,**kwargs)
                if path==b'stage':raise OSError(errno.EIO,'lost directory creation acknowledgement')
                return result
            with self.holder(root,plan) as holder:
                with patch.object(views.os,'mkdir',side_effect=interrupted),self.assertRaises(OSError):
                    holder.prepare_directory('stage',parent,b'stage')
                (root/'out/stage/unknown').write_bytes(b'preserve unknown bytes')
                with self.assertRaises(views.FreshnessHold):holder.prepare_directory('stage',parent,b'stage')
                self.assertEqual((root/'out/stage/unknown').read_bytes(),b'preserve unknown bytes')

    def test_existing_foreign_directory_cannot_receive_a_prepared_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            (root/'out/stage').mkdir();(root/'out/stage/foreign').write_bytes(b'foreign')
            with self.holder(root,plan) as holder:
                with self.assertRaises(views.FreshnessHold):holder.prepare_directory('stage',parent,b'stage')
                self.assertEqual((root/'out/stage/foreign').read_bytes(),b'foreign')

    def test_corrupted_copy_refuses_preparation_and_closes_the_anonymous_inode(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root);real=views.os.write
            def corrupted(fd,body):return real(fd,b'X'*len(body))
            with self.holder(root,plan) as holder:
                before=set(os.listdir('/proc/self/fd'))
                with patch.object(views.os,'write',side_effect=corrupted),self.assertRaises(views.FreshnessHold):
                    holder.prepare_file('file',parent,payload,expected)
                self.assertEqual(set(os.listdir('/proc/self/fd')),before)
                self.assertEqual(list((root/'out').iterdir()),[])

    def test_unsafe_metadata_is_refused_without_creating_a_namespace_entry(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root);expected['mode']=0o4755
            with self.holder(root,plan) as holder:
                with self.assertRaises(views.FreshnessHold):holder.prepare_file('file',parent,payload,expected)
                self.assertEqual(list((root/'out').iterdir()),[])

    def test_reopened_prepared_directory_requires_the_recorded_inode(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            with self.holder(root,plan) as holder:original=holder.prepare_directory('stage',parent,b'stage');identity=os.fstat(original).st_ino
            with self.holder(root,plan) as holder:self.assertEqual(os.fstat(holder.prepare_directory('stage',parent,b'stage')).st_ino,identity)
            (root/'out/stage').rename(root/'out/old-stage');(root/'out/stage').mkdir(mode=0o700)
            with self.holder(root,plan) as holder:
                with self.assertRaises(views.FreshnessHold):holder.prepare_directory('stage',parent,b'stage')
                self.assertTrue((root/'out/stage').exists());self.assertTrue((root/'out/old-stage').exists())

    def test_anonymous_file_lost_with_its_holder_cannot_be_recreated_as_prepared(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            with self.holder(root,plan) as holder:holder.prepare_file('file',parent,payload,expected)
            with self.holder(root,plan) as holder:
                with self.assertRaises(views.FreshnessHold):holder.prepare_file('file',parent,payload,expected)
            self.assertEqual(list((root/'out').iterdir()),[])

    def test_forked_holder_cannot_allocate_from_inherited_parent_capabilities(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            with self.holder(root,plan) as holder:
                pid=os.fork()
                if pid==0:
                    try:holder.prepare_directory('stage',parent,b'stage')
                    except views.FreshnessHold:os._exit(0)
                    except BaseException:os._exit(2)
                    os._exit(1)
                self.assertEqual(os.waitpid(pid,0)[1],0);self.assertEqual(list((root/'out').iterdir()),[])


    def test_file_byte_reservations_are_shared_across_keys_and_retries(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            with self.holder(root,plan) as holder,patch.object(views,'MAX_BYTES',expected['size']):
                first=holder.prepare_file('first',parent,payload,expected)
                self.assertEqual(holder.prepare_file('first',parent,payload,expected),first)
                with self.assertRaises(views.FreshnessHold):holder.prepare_file('second',parent,payload,expected)
                self.assertEqual(os.pread(first,100,0),payload.read_bytes())

    def test_directory_entry_reservations_do_not_reset_on_a_new_key(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root)
            with self.holder(root,plan) as holder,patch.object(views,'MAX_ENTRIES',1):
                first=holder.prepare_directory('first',parent,b'first')
                self.assertEqual(holder.prepare_directory('first',parent,b'first'),first)
                with self.assertRaises(views.FreshnessHold):holder.prepare_directory('second',first,b'second')
                self.assertEqual(list((root/'out/first').iterdir()),[])

    def test_new_holder_cannot_replace_an_unknown_preparing_anonymous_capability(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root);real=views.cache._write_record;failed=[]
            def interrupted(path,value):
                if value.get('kind')=='ci-prepared-file' and value.get('state')=='prepared' and not failed:
                    failed.append(True);raise OSError(errno.ENOSPC,'prepared file identity not committed')
                return real(path,value)
            with self.holder(root,plan) as original:
                with patch.object(views.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):
                    original.prepare_file('file',parent,payload,expected)
                with self.holder(root,plan) as fresh:
                    with self.assertRaises(views.FreshnessHold):fresh.prepare_file('file',parent,payload,expected)
                self.assertEqual(os.pread(original.prepare_file('file',parent,payload,expected),100,0),payload.read_bytes())

    def test_clean_failed_copy_can_retry_its_original_reservation_without_double_charge(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);payload,parent,expected,plan=self.fixture(root);real=views.os.write
            def corrupted(fd,body):return real(fd,b'X'*len(body))
            with self.holder(root,plan) as holder,patch.object(views,'MAX_BYTES',expected['size']):
                with patch.object(views.os,'write',side_effect=corrupted),self.assertRaises(views.FreshnessHold):
                    holder.prepare_file('file',parent,payload,expected)
                descriptor=holder.prepare_file('file',parent,payload,expected)
                self.assertEqual(os.pread(descriptor,100,0),payload.read_bytes())
                with self.assertRaises(views.FreshnessHold):holder.prepare_file('other',parent,payload,expected)


if __name__=='__main__':unittest.main()
