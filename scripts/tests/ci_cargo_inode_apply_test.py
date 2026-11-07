"""Real inode mutation mechanics, without execution or publication authority."""
import errno
import json
import mmap
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch

import ci_cargo_capture_test as captures
views=captures.views


class InodeEffectsTests(unittest.TestCase):
    def fixture(self,root,body=b'abcdefghij',extra=False):
        source=root/'source';(source/'out').mkdir(parents=True)
        data=source/'out/data';data.write_bytes(body);data.chmod(0o640)
        if extra:(source/'out/other').write_bytes(b'other original')
        sealed=root/'before';capture=captures.CaptureRootsTests().capture(source,sealed)
        working=root/'working';copied=views.materialize_capture(sealed,capture,working,
            parent_uid=os.getuid(),unit_uid=os.getuid(),check=lambda:None)
        descriptor=os.open(source,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:held=views.RetainedInodes(descriptor,capture,working,copied,
            parent_uid=os.getuid(),unit_uid=os.getuid(),check=lambda:None)
        finally:os.close(descriptor)
        return source,data,working,held

    def prepare(self,root,held):return held.prepare_transfer(root/'transfer',root/'before',check=lambda:None)
    def apply(self,root,held,plan):return held.apply_inode_effects(root/'transfer',plan,check=lambda:None)

    def test_inplace_effects_preserve_inode_offsets_shared_and_private_mappings(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data,working,held=self.fixture(root,b'A'*4096)
            descriptor=os.open(data,os.O_RDWR|os.O_CLOEXEC);os.lseek(descriptor,17,os.SEEK_SET)
            shared=mmap.mmap(descriptor,4096,flags=mmap.MAP_SHARED)
            private=mmap.mmap(descriptor,4096,flags=mmap.MAP_PRIVATE);private[0:1]=b'B'
            original=data.stat()
            try:
                with held:
                    child=working/'out/data';child.write_bytes(b'C'*4096);child.chmod(0o750)
                    os.utime(child,ns=(946684800000000000,946684800000000001))
                    result=self.apply(root,held,self.prepare(root,held))
                    self.assertEqual(result['state'],'inodes_applied');self.assertFalse(result['effects_materialized'])
                    self.assertEqual(data.stat().st_ino,original.st_ino);self.assertEqual(os.lseek(descriptor,0,os.SEEK_CUR),17)
                    self.assertEqual(shared[:],b'C'*4096);self.assertEqual(private[:],b'B'+b'A'*4095)
                    self.assertEqual(stat.S_IMODE(data.stat().st_mode),0o750)
                    self.assertEqual(data.stat().st_mtime_ns,946684800000000001)
                    os.pwrite(descriptor,b'D',2);self.assertEqual(data.read_bytes()[:4],b'CCDC')
            finally:shared.close();private.close();os.close(descriptor);held.close()

    def test_old_inode_changes_apply_before_later_unlink_or_replacement(self):
        for action in ('unlink','replace'):
            with self.subTest(action=action),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);source,data,working,held=self.fixture(root)
                descriptor=os.open(data,os.O_RDONLY|os.O_CLOEXEC)
                try:
                    with held:
                        child=working/'out/data';child.write_bytes(b'old inode changed')
                        if action=='unlink':child.unlink()
                        else:
                            replacement=working/'out/new';replacement.write_bytes(b'new inode');replacement.replace(child)
                        result=self.apply(root,held,self.prepare(root,held))
                        self.assertEqual(os.pread(descriptor,100,0),b'old inode changed')
                        self.assertEqual(data.read_bytes(),b'old inode changed')
                        self.assertFalse(result['effects_materialized']);self.assertTrue(data.exists())
                finally:os.close(descriptor)

    def test_every_before_image_and_old_payload_is_validated_before_any_write(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data,working,held=self.fixture(root,extra=True)
            with held:
                (working/'out/data').write_bytes(b'changed first');(working/'out/other').write_bytes(b'changed second')
                plan=self.prepare(root,held);row=next(r for r in plan['old_effects']['entries'] if r['path']==b'out/other'.hex())
                payload=root/'transfer/old'/row['payload'];payload.chmod(0o600);payload.write_bytes(b'corrupt');payload.chmod(0o444)
                with self.assertRaises(views.FreshnessHold):self.apply(root,held,plan)
                self.assertEqual(data.read_bytes(),b'abcdefghij');self.assertEqual((source/'out/other').read_bytes(),b'other original')

    def test_unrecognized_partial_write_stays_fenced_and_is_not_repaired_by_guessing(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data,working,held=self.fixture(root)
            with held:
                (working/'out/data').write_bytes(b'ZYXWVUTSRQ');plan=self.prepare(root,held);real_write=views.os.pwrite
                def partial(descriptor,body,offset):
                    real_write(descriptor,body[:3],offset);raise OSError(errno.EIO,'interrupted write')
                with patch.object(views.os,'pwrite',side_effect=partial),self.assertRaises(OSError):self.apply(root,held,plan)
                self.assertEqual(data.read_bytes(),b'ZYXdefghij')
                progress=json.loads((root/'transfer/inode-progress.json').read_text())
                self.assertEqual(progress['state'],'applying');self.assertFalse(progress['effects_materialized'])
                with self.assertRaises(views.FreshnessHold):self.apply(root,held,plan)
                self.assertEqual(data.read_bytes(),b'ZYXdefghij')

    def test_completed_write_with_lost_progress_commit_is_recognized_without_rewriting(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data,working,held=self.fixture(root)
            with held:
                (working/'out/data').write_bytes(b'new content');plan=self.prepare(root,held);real_record=views.cache._write_record;failed=[]
                def interrupted(path,value):
                    if path.name=='inode-progress.json' and value.get('completed') and not failed:
                        failed.append(True);raise OSError(errno.ENOSPC,'progress sync failed')
                    return real_record(path,value)
                with patch.object(views.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):self.apply(root,held,plan)
                self.assertEqual(data.read_bytes(),b'new content');after=data.stat()
                with patch.object(views.os,'pwrite',side_effect=AssertionError('must not rewrite completed image')):
                    result=self.apply(root,held,plan)
                self.assertEqual(result['state'],'inodes_applied');self.assertFalse(result['shipping_authority'])
                self.assertEqual(data.stat().st_ctime_ns,after.st_ctime_ns)

    def test_metadata_only_effects_preserve_bytes_without_a_data_write(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data,working,held=self.fixture(root)
            with held:
                child=working/'out/data';child.chmod(0o400);os.utime(child,ns=(946684800000000000,946684800000000001))
                plan=self.prepare(root,held);atime=data.stat().st_atime_ns
                with patch.object(views.os,'pwrite',side_effect=AssertionError('metadata-only effect rewrote data')):
                    result=self.apply(root,held,plan)
                self.assertEqual(data.stat().st_atime_ns,atime)
                self.assertEqual(data.read_bytes(),b'abcdefghij');self.assertEqual(stat.S_IMODE(data.stat().st_mode),0o400)
                self.assertEqual(data.stat().st_mtime_ns,946684800000000001);self.assertFalse(result['full_ci'])

    def test_corrupt_final_image_or_late_child_write_refuses_all_parent_changes(self):
        for fault in ('after','child'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);source,data,working,held=self.fixture(root)
                with held:
                    (working/'out/data').write_bytes(b'child effects');plan=self.prepare(root,held)
                    if fault=='after':
                        payload=root/'transfer/after/out/data';payload.chmod(0o600);payload.write_bytes(b'corrupt');payload.chmod(0o444)
                    else:(working/'out/data').write_bytes(b'late child write')
                    with self.assertRaises(views.FreshnessHold):self.apply(root,held,plan)
                    self.assertEqual(data.read_bytes(),b'abcdefghij')

    def test_forked_controller_cannot_apply_inherited_inode_capabilities(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data,working,held=self.fixture(root)
            with held:
                (working/'out/data').write_bytes(b'child effects');plan=self.prepare(root,held)
                pid=os.fork()
                if pid==0:
                    try:self.apply(root,held,plan)
                    except views.FreshnessHold:os._exit(0)
                    except BaseException:os._exit(2)
                    os._exit(1)
                self.assertEqual(os.waitpid(pid,0)[1],0);self.assertEqual(data.read_bytes(),b'abcdefghij')

    def test_foreign_write_after_completed_phase_is_preserved_and_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,data,working,held=self.fixture(root)
            with held:
                (working/'out/data').write_bytes(b'child effect');plan=self.prepare(root,held);self.apply(root,held,plan)
                data.write_bytes(b'foreign later write')
                with self.assertRaises(views.FreshnessHold):self.apply(root,held,plan)
                self.assertEqual(data.read_bytes(),b'foreign later write')


if __name__=='__main__':unittest.main()
