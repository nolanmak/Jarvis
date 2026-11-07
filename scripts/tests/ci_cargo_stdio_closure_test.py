"""Actual standard-stream aliases omitted by a pathname-only W inventory."""
import mmap
import os
from pathlib import Path
import tempfile
import unittest
import ci_cargo_capture_test as captures
views=captures.views


class StdioClosureTests(unittest.TestCase):
    def capture(self,root,source):return captures.CaptureRootsTests().capture(source,root/'capture')

    def test_unlinked_parent_owned_writer_is_refused_without_touching_fd_or_mapping(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';(source/'out').mkdir(parents=True)
            path=source/'out/data';path.write_bytes(b'P'*4096)
            fd=os.open(path,os.O_RDWR|os.O_CLOEXEC);null=os.open(os.devnull,os.O_WRONLY|os.O_CLOEXEC)
            shared=mmap.mmap(fd,4096,flags=mmap.MAP_SHARED)
            try:
                os.lseek(fd,17,os.SEEK_SET);path.unlink();capture=self.capture(root,source)
                self.assertEqual(os.fstat(fd).st_nlink,0)
                self.assertTrue(all(row[1]!='file' for row in capture['entries']))
                with self.assertRaises(views.FreshnessHold):views.validate_capture_stdio(capture,[fd,null])
                self.assertEqual(shared[:],b'P'*4096);self.assertEqual(os.lseek(fd,0,os.SEEK_CUR),17)
            finally:shared.close();os.close(fd);os.close(null)

    def test_anonymous_parent_owned_file_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';(source/'out').mkdir(parents=True)
            fd=os.open(source/'out',os.O_TMPFILE|os.O_RDWR|os.O_CLOEXEC,0o600)
            null=os.open(os.devnull,os.O_WRONLY|os.O_CLOEXEC)
            try:
                os.write(fd,b'anonymous');capture=self.capture(root,source)
                with self.assertRaises(views.FreshnessHold):views.validate_capture_stdio(capture,[null,fd])
                self.assertEqual(os.pread(fd,99,0),b'anonymous')
            finally:os.close(fd);os.close(null)

    def test_parent_owned_file_moved_outside_named_roots_is_not_an_external_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';(source/'out').mkdir(parents=True)
            path=source/'out/data';path.write_bytes(b'held parent inode')
            fd=os.open(path,os.O_WRONLY|os.O_CLOEXEC);null=os.open(os.devnull,os.O_WRONLY|os.O_CLOEXEC)
            try:
                moved=root/'moved';path.rename(moved);capture=self.capture(root,source)
                with self.assertRaises(views.FreshnessHold):views.validate_capture_stdio(capture,[fd,null])
                self.assertEqual(moved.read_bytes(),b'held parent inode')
            finally:os.close(fd);os.close(null)

    def test_readonly_alias_of_mutable_W_is_refused_for_shared_inode_semantics(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';(source/'out').mkdir(parents=True)
            path=source/'out/data';path.write_bytes(b'original');path.chmod(0o666)
            fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC);null=os.open(os.devnull,os.O_WRONLY|os.O_CLOEXEC)
            try:
                capture=self.capture(root,source)
                with self.assertRaises(views.FreshnessHold):views.validate_capture_stdio(capture,[fd,null])
                self.assertEqual(os.pread(fd,99,0),b'original')
            finally:os.close(fd);os.close(null)

    def test_original_pipe_and_null_outputs_remain_usable(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';(source/'out').mkdir(parents=True)
            capture=self.capture(root,source);reader,writer=os.pipe2(os.O_CLOEXEC)
            null=os.open(os.devnull,os.O_WRONLY|os.O_CLOEXEC)
            try:
                views.validate_capture_stdio(capture,[writer,null]);os.write(writer,b'original bytes')
                self.assertEqual(os.read(reader,99),b'original bytes')
            finally:os.close(reader);os.close(writer);os.close(null)


if __name__=='__main__':unittest.main()
