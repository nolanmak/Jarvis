"""Bounded kernel mount-layout parsing and actual proc observations."""
import os
from pathlib import Path
import tempfile
import unittest
from ci_cargo_capture_test import views


class MountLayoutTests(unittest.TestCase):
    def test_binding_checks_require_exact_paths_modes_and_actual_source_inodes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';source.write_bytes(b'immutable')
            tree=root/'tree';tree.mkdir();os.link(source,tree/'input')
            descriptor=os.open(tree,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
            layout={'root_identity':[tree.stat().st_dev,tree.stat().st_ino],
                'sha256':'a'*64,'mounts':views.parse_mount_layout(
                    b'1 9 8:1 /source /input ro,nosuid,nodev - ext4 /dev/vda rw\n')}
            rules={b'/input':{'source':str(source),'kind':'file','access':'ro'}}
            try:
                result=views.validate_mount_bindings(descriptor,layout,rules)
                self.assertEqual(result['verified_bindings'],1);self.assertFalse(result['shipping_authority'])
                original=layout['mounts'][0]
                for fault in ('writable','unknown','duplicate','propagation','missing'):
                    row=dict(original)
                    if fault=='writable':row['options']=['rw','nosuid','nodev']
                    elif fault=='unknown':row['mountpoint']=b'/outside'.hex()
                    elif fault=='propagation':row['optional']=['shared:7']
                    layout['mounts']=[] if fault=='missing' else [row,row] if fault=='duplicate' else [row]
                    with self.subTest(fault=fault),self.assertRaises(views.FreshnessHold):
                        views.validate_mount_bindings(descriptor,layout,rules)
                layout['mounts']=[original];source.unlink();source.write_bytes(b'foreign replacement')
                with self.assertRaises(views.FreshnessHold):views.validate_mount_bindings(descriptor,layout,rules)
            finally:os.close(descriptor)

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

    def proc_delta(self):
        reference={'root_identity':[1,2],'mount_namespace':'mnt:[10]','mounts':views.parse_mount_layout(
            b'1 9 8:1 / / rw - ext4 /dev/vda rw\n'
            b'2 1 0:5 / /proc rw,nosuid,nodev,noexec - proc proc rw\n'
            b'3 1 8:1 /source /workspace ro,nosuid,nodev - ext4 /dev/vda rw\n')}
        actual={'root_identity':[1,2],'mount_namespace':'mnt:[11]','mounts':views.parse_mount_layout(
            b'11 11 8:1 / / rw - ext4 /dev/vda rw\n'
            b'12 11 0:5 / /proc rw,nosuid,nodev,noexec - proc proc rw\n'
            b'13 11 8:1 /source /workspace ro,nosuid,nodev - ext4 /dev/vda rw\n'
            b'14 12 0:7 / /proc ro,nosuid,nodev,noexec - proc proc rw,hidepid=2\n')}
        return reference,actual

    def test_worker_proc_delta_preserves_root_source_mounts_and_parent_graph(self):
        # Shape-only fixtures; they are not live namespace/source admission.
        reference,actual=self.proc_delta()
        views.require_worker_proc_delta(reference,actual,14)

    def test_worker_proc_delta_refuses_wider_namespace_or_mount_changes(self):
        for fault in ('same-namespace','root','source','source-options','parent','extra-proc','shared','writable','hidepid','missing-inherited'):
            reference,actual=self.proc_delta();proc=actual['mounts'][-1]
            if fault=='same-namespace':actual['mount_namespace']=reference['mount_namespace']
            elif fault=='root':actual['root_identity']=[1,3]
            elif fault=='source':actual['mounts'][2]['root']=b'/foreign'.hex()
            elif fault=='source-options':actual['mounts'][2]['options']=['rw','nosuid','nodev']
            elif fault=='parent':actual['mounts'][2]['parent']=12
            elif fault=='extra-proc':actual['mounts'].append(dict(proc,id=15,mountpoint=b'/elsewhere'.hex()))
            elif fault=='shared':actual['mounts'][0]['optional']=['shared:7']
            elif fault=='writable':proc['options']=['rw','nosuid','nodev','noexec']
            elif fault=='hidepid':proc['super_options']=['rw']
            else:actual['mounts'].pop(1)
            with self.subTest(fault=fault),self.assertRaises(views.FreshnessHold):
                views.require_worker_proc_delta(reference,actual,14)

    def test_actual_proc_observation_binds_namespace_root_and_current_layout(self):
        result=views.observe_mount_layout(os.getpid())
        info=os.stat('/');self.assertEqual(result['root_identity'],[info.st_dev,info.st_ino])
        self.assertEqual(result['mount_namespace'],os.readlink('/proc/self/ns/mnt'))
        self.assertGreater(len(result['mounts']),0)
        self.assertEqual(len(result['sha256']),64)
        self.assertFalse(result['input_closure_complete']);self.assertFalse(result['shipping_authority'])


if __name__=='__main__':unittest.main()
