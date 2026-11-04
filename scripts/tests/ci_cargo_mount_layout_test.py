"""Bounded kernel mount-layout parsing and actual proc observations."""
import os
import unittest
from ci_cargo_capture_test import views


class MountLayoutTests(unittest.TestCase):
    def test_mount_paths_preserve_kernel_escaped_and_non_utf8_bytes(self):
        body=(b'1 9 8:1 /private/root / rw,relatime - ext4 /dev/vda rw\n'
              b'2 1 8:1 /input\\040space/\\134raw\xff /workspace/with\\011tab ro,nosuid,nodev - ext4 /dev/vda rw\n')
        rows=views.parse_mount_layout(body)
        self.assertEqual(len(rows),2)
        self.assertEqual(bytes.fromhex(rows[1]['root']),b'/input space/\\raw\xff')
        self.assertEqual(bytes.fromhex(rows[1]['mountpoint']),b'/workspace/with\ttab')
        self.assertEqual(rows[1]['options'],['nodev','nosuid','ro'])
        self.assertEqual(rows[1]['super_options'],['rw'])

    def test_malformed_mount_rows_and_duplicate_ids_are_refused(self):
        good=b'1 9 8:1 /private/root / rw,relatime - ext4 /dev/vda rw\n'
        faults=[b'',good+good,good.replace(b'1 9',b'-1 9'),good.replace(b'8:1',b'8:x'),
                good.replace(b' - ',b' '),good.replace(b' rw,relatime ',b' rw,ro '),
                good.replace(b'/private/root',b'/private/../root'),
                good.replace(b'/private/root',b'/private\\041root'),good.replace(b'/dev/vda',b'/dev/\0vda')]
        for body in faults:
            with self.subTest(body=body),self.assertRaises(views.FreshnessHold):views.parse_mount_layout(body)

    def test_nsfs_root_labels_and_vertical_tab_path_bytes_are_not_path_aliases(self):
        rows=views.parse_mount_layout(b'1 9 0:5 mnt:[123] /run/ns rw - nsfs nsfs rw\n'
            b'2 9 8:1 /private/vertical\x0btab /tmp/vertical\x0btab rw - ext4 /dev/vda rw\n')
        self.assertEqual(bytes.fromhex(rows[0]['root']),b'mnt:[123]')
        self.assertEqual(bytes.fromhex(rows[1]['mountpoint']),b'/tmp/vertical\x0btab')

    def test_actual_proc_observation_binds_namespace_root_and_current_layout(self):
        result=views.observe_mount_layout(os.getpid())
        info=os.stat('/');self.assertEqual(result['root_identity'],[info.st_dev,info.st_ino])
        self.assertEqual(result['mount_namespace'],os.readlink('/proc/self/ns/mnt'))
        self.assertGreater(len(result['mounts']),0)
        self.assertEqual(len(result['sha256']),64)
        self.assertFalse(result['input_closure_complete']);self.assertFalse(result['shipping_authority'])


if __name__=='__main__':unittest.main()
