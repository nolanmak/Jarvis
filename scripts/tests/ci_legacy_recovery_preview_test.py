"""Recovery previews cannot become write-through runs or passing-CI receipts."""
import importlib.util
import os
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest

spec=importlib.util.spec_from_file_location('legacy_recovery_preview',Path(__file__).resolve().parents[1]/'ci-legacy-recovery-preview.py')
preview=importlib.util.module_from_spec(spec)
try:spec.loader.exec_module(preview)
except FileNotFoundError:preview=None


class RecoveryPreviewTests(unittest.TestCase):
    def test_kernel_descriptor_check_rejects_writeable_backing_before_guest_start(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);image=root/'cache.img';image.write_bytes(b'original')
            scratch=root/'scratch';scratch.mkdir()
            program="""import os,sys
base=os.open(sys.argv[1],os.O_RDWR if sys.argv[3]=='write' else os.O_RDONLY)
overlay=os.open(sys.argv[2],os.O_RDWR|os.O_CREAT,0o600);os.ftruncate(overlay,65536)
print('ready',flush=True)
sys.stdin.buffer.read(1)
"""
            for access in ('write','read'):
                child=subprocess.Popen([sys.executable,'-c',program,str(image),str(scratch/'overlay'),access],
                    stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                try:
                    self.assertEqual(child.stdout.readline(),b'ready\n')
                    if access=='write':
                        with self.assertRaisesRegex(preview.PreviewUnavailable,'read-only'):
                            preview.verify_paused_image(child,{'qemu':sys.executable},image,scratch)
                    else:
                        result=preview.verify_paused_image(child,{'qemu':sys.executable},image,scratch)
                        self.assertTrue(result['base_read_only_verified_before_guest'])
                        self.assertEqual(result['overlay_descriptors'][0]['size'],65536)
                finally:
                    child.terminate();child.communicate(timeout=5)

    def test_drive_is_snapshot_only_paused_and_monitor_commit_is_unavailable(self):
        command=preview.preview_command({'qemu':'/trusted/qemu','memory_mb':4096,'data_dir':'/trusted/share',
            'firmware':'/trusted/bios','kernel':'/trusted/kernel'},Path('/owned/initrd'),[],
            Path('/owned/ci-vm/cache.img'),Path('/run/user/1000/private/q.sock'))
        self.assertIn('-S',command)
        self.assertEqual(command[command.index('-monitor')+1],'none')
        drives=[command[i+1] for i,arg in enumerate(command[:-1]) if arg=='-drive']
        self.assertEqual(len(drives),1);self.assertIn('snapshot=on',drives[0])
        self.assertIn('format=raw',drives[0]);self.assertEqual(command[command.index('-nic')+1],'none')

    def test_protected_inventory_detects_artifact_changes_but_excludes_only_retired_namespaces(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'target/debug/deps').mkdir(parents=True)
            artifact=root/'target/debug/deps/libexample.rlib';artifact.write_bytes(b'original artifact')
            (root/'target/debug/incremental').mkdir();incremental=root/'target/debug/incremental/derived'
            incremental.write_bytes(b'derived incremental')
            before=preview.protected_inventory(root)
            incremental.unlink();(root/'ci-legacy-incremental-retirements').mkdir()
            self.assertEqual(before,preview.protected_inventory(root))
            artifact.write_bytes(b'changed artifact')
            self.assertNotEqual(before['sha256'],preview.protected_inventory(root)['sha256'])

    def test_source_image_checksum_keeps_identity_and_avoids_atime_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            image=Path(directory)/'cache.img';image.write_bytes(b'original raw image')
            os.utime(image,ns=(946684800000000000,946684800000000000))
            before=image.stat();observed=preview.hash_image(image)
            self.assertEqual(image.stat().st_atime_ns,before.st_atime_ns)
            self.assertEqual(observed['identity']['inode'],before.st_ino)
            image.write_bytes(b'changed raw image')
            self.assertNotEqual(observed['sha256'],preview.hash_image(image)['sha256'])

    def test_preview_schema_refuses_false_preservation_and_shipping_authority(self):
        valid={'version':1,'kind':'ci-legacy-recovery-preview','view':'temporary-writes',
            'protected_before':{'sha256':'a'*64,'files':1,'bytes':3},
            'protected_after':{'sha256':'a'*64,'files':1,'bytes':3},
            'available_before':1,'available_after':2,'retired':[],
            'input_closure_complete':False,'full_ci':False,'shipping_authority':False}
        preview.validate_guest_result(valid)
        for change in ({'shipping_authority':True},{'full_ci':True},{'input_closure_complete':True},
                {'protected_after':{'sha256':'b'*64,'files':1,'bytes':3}},{'available_after':True}):
            with self.subTest(change=change),self.assertRaises(preview.PreviewUnavailable):
                preview.validate_guest_result(valid|change)
