"""Actual inherited FDs against declared worker outputs; no projection proof."""
import fcntl
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from ci_cargo_capture_test import views


class ProjectionStdioTests(unittest.TestCase):
    def setUp(self):
        self.root=Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.workspace=self.root/'workspace';self.workspace.mkdir()
        (self.workspace/'generated').mkdir();(self.workspace/'generated/data').write_bytes(b'original')
        self.null=os.open(os.devnull,os.O_WRONLY|os.O_CLOEXEC);self.addCleanup(os.close,self.null)

    def descriptor(self,path,flags=os.O_WRONLY):
        fd=os.open(path,flags|os.O_CLOEXEC);self.addCleanup(os.close,fd);return fd

    def guard(self,fd,outputs=None,check=lambda:None):
        return views.reject_projection_stdio([fd,self.null],self.workspace,
            {'generated':'directory'} if outputs is None else outputs,owner=os.getuid(),check=check)

    def test_worker_owned_destination_alias_is_not_a_parent_owned_stdio_alias(self):
        fd=self.descriptor(self.workspace/'generated/data')
        # The actual FD belongs to this worker, not the distinct parent unit.
        # An empty W inode set isolates the missing projection-side check.
        views.validate_capture_stdio({'entries':[]},[fd,self.null],parent_uid=os.getuid()+1000)
        with self.assertRaises(views.FreshnessHold):self.guard(fd)
        self.assertEqual((self.workspace/'generated/data').read_bytes(),b'original')

    def test_declared_file_alias_through_another_name_is_refused(self):
        path=self.workspace/'style.css';path.write_bytes(b'css')
        os.link(path,self.root/'alias');fd=self.descriptor(self.root/'alias')
        with self.assertRaises(views.FreshnessHold):self.guard(fd,{'style.css':'file'})

    def test_readonly_destination_alias_is_also_refused(self):
        fd=self.descriptor(self.workspace/'generated/data',os.O_RDONLY)
        with self.assertRaises(views.FreshnessHold):self.guard(fd)

    def test_external_regular_output_retains_original_bytes_offset_flags_and_times(self):
        path=self.root/'external';path.write_bytes(b'external output')
        fd=self.descriptor(path,os.O_RDWR);os.lseek(fd,4,os.SEEK_SET)
        before=os.fstat(fd);flags=fcntl.fcntl(fd,fcntl.F_GETFL)
        result=self.guard(fd)
        self.assertFalse(result['alias_closure_complete']);self.assertFalse(result['shipping_authority'])
        self.assertEqual(os.lseek(fd,0,os.SEEK_CUR),4);self.assertEqual(fcntl.fcntl(fd,fcntl.F_GETFL),flags)
        after=os.fstat(fd);self.assertEqual((after.st_atime_ns,after.st_mtime_ns),(before.st_atime_ns,before.st_mtime_ns))
        self.assertEqual(os.pread(fd,99,0),b'external output')

    def test_missing_identity_and_ambiguous_hardlinks_hold(self):
        for fault in ('unlinked','hardlink'):
            with self.subTest(fault=fault):
                path=self.root/fault;path.write_bytes(b'external');fd=self.descriptor(path)
                if fault=='unlinked':path.unlink()
                else:os.link(path,self.workspace/'generated'/fault)
                with self.assertRaises(views.FreshnessHold):self.guard(fd)

    def test_changed_path_and_untrusted_output_root_hold_without_writes(self):
        path=self.root/'external';path.write_bytes(b'unchanged');fd=self.descriptor(path)
        readlink=os.readlink
        def moved(name,*a,**kw):
            result=readlink(name,*a,**kw)
            if str(name)==f'/proc/self/fd/{fd}':path.rename(self.root/'moved')
            return result
        with patch.object(views.os,'readlink',side_effect=moved),self.assertRaises(views.FreshnessHold):self.guard(fd)
        (self.workspace/'generated').rename(self.workspace/'elsewhere')
        (self.workspace/'generated').symlink_to('elsewhere')
        with self.assertRaises(views.FreshnessHold):self.guard(fd)
        self.assertEqual((self.root/'moved').read_bytes(),b'unchanged')

    def test_pipe_null_and_cancellation(self):
        reader,writer=os.pipe2(os.O_CLOEXEC);self.addCleanup(os.close,reader);self.addCleanup(os.close,writer)
        self.guard(writer);os.write(writer,b'original');self.assertEqual(os.read(reader,99),b'original')
        def cancelled():raise views.FreshnessHold('cleanup_uncertain')
        with self.assertRaises(views.FreshnessHold):self.guard(writer,check=cancelled)


if __name__=='__main__':unittest.main()
