"""Real private-tree permission checks; no pause or reuse authority."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from ci_cargo_capture_test import views


class PrivateBaseTests(unittest.TestCase):
    def fixture(self):
        root=Path(self.enterContext(tempfile.TemporaryDirectory()))
        for name in ('etc','tmp','usr'):(root/name).mkdir(mode=0o755)
        (root/'etc/config').write_bytes(b'root controlled');(root/'etc/config').chmod(0o644)
        (root/'bin').symlink_to('usr/bin')
        (root/'usr/mutable-looking').write_bytes(b'readonly mount');(root/'usr/mutable-looking').chmod(0o666)
        fd=os.open(root,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC);self.addCleanup(os.close,fd)
        layout={'root_identity':[root.stat().st_dev,root.stat().st_ino],
                'mounts':[{'mountpoint':b'/usr'.hex()}]}
        return root,fd,layout

    def observe(self,fd,layout,check=lambda:None):
        return views.validate_private_base(fd,layout,['tmp'],unit_uid=2000,check=check)

    def test_only_declared_mutable_roots_and_verified_mounts_are_excluded(self):
        root,fd,layout=self.fixture();before=(root/'etc/config').stat()
        first=self.observe(fd,layout)
        (root/'tmp/created').write_bytes(b'admitted mutable effect')
        self.assertEqual(self.observe(fd,layout),first)
        self.assertFalse(first['shipping_authority']);self.assertFalse(first['input_closure_complete'])
        self.assertEqual((root/'etc/config').stat().st_atime_ns,before.st_atime_ns)

    def test_world_writable_file_or_parent_outside_W_is_refused(self):
        for name in ('etc/config','etc','.'):
            with self.subTest(name=name):
                root,fd,layout=self.fixture();(root/name).chmod(0o777)
                with self.assertRaises(views.FreshnessHold):self.observe(fd,layout)

    def test_acl_and_unaccounted_alias_are_refused(self):
        root,fd,layout=self.fixture()
        with patch.object(views.os,'listxattr',return_value=['system.posix_acl_access']):
            with self.assertRaises(views.FreshnessHold):self.observe(fd,layout)
        os.link(root/'etc/config',root/'alias')
        with self.assertRaises(views.FreshnessHold):self.observe(fd,layout)

    def test_foreign_owner_is_refused(self):
        root,fd,layout=self.fixture()
        # Actual ownership cannot be changed by this unprivileged mechanics
        # test. A different controller identity must reject the same tree.
        with patch.object(views.os,'geteuid',return_value=os.geteuid()+1):
            with self.assertRaises(views.FreshnessHold):self.observe(fd,layout)

    def test_mutable_root_symlink_and_unaccounted_special_file_are_refused(self):
        for fault in ('symlink','fifo'):
            with self.subTest(fault=fault):
                root,fd,layout=self.fixture()
                if fault=='symlink':(root/'tmp').rmdir();(root/'tmp').symlink_to('etc')
                else:os.mkfifo(root/'etc/pipe')
                with self.assertRaises(views.FreshnessHold):self.observe(fd,layout)

    def test_cancellation_and_foreign_change_cannot_publish_a_snapshot(self):
        root,fd,layout=self.fixture()
        def cancelled():raise views.FreshnessHold('cleanup_uncertain')
        with self.assertRaises(views.FreshnessHold):self.observe(fd,layout,cancelled)
        observed=[];listdir=views.os.listdir
        def mutate(descriptor):
            if os.fstat(descriptor).st_ino==root.stat().st_ino:
                observed.append(True)
                if len(observed)==2:(root/'etc/config').write_bytes(b'foreign change')
            return listdir(descriptor)
        with patch.object(views.os,'listdir',side_effect=mutate),self.assertRaises(views.FreshnessHold):
            self.observe(fd,layout)


if __name__=='__main__':unittest.main()
