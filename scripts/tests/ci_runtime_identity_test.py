"""Trusted runtime input identity; candidate source uses its separate Git census."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('identity',ROOT/'scripts/ci-runtime-identity.py')
identity=importlib.util.module_from_spec(spec);spec.loader.exec_module(identity)


class IdentityTests(unittest.TestCase):
    def test_content_identity_detects_same_size_same_mtime_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);file=root/'compiler';file.write_bytes(b'first');file.chmod(0o600)
            before=identity.census({'toolchain':root},content=True)
            stamp=file.stat().st_mtime_ns
            file.write_bytes(b'other');os.utime(file,ns=(stamp,stamp))
            after=identity.census({'toolchain':root},content=True)
            self.assertNotEqual(before['content_sha256'],after['content_sha256'])
            self.assertNotEqual(before['metadata_sha256'],after['metadata_sha256'])
            file.write_bytes(b'first');os.utime(file,ns=(stamp,stamp))
            restored=identity.census({'toolchain':root},content=True)
            self.assertEqual(before['content_sha256'],restored['content_sha256'])
            self.assertNotEqual(before['metadata_sha256'],restored['metadata_sha256'])

    def test_equivalent_reads_keep_identity_and_metadata_only_matches_full_read(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'lib').mkdir(mode=0o700);(root/'lib/std').write_bytes(b'library');(root/'lib/std').chmod(0o600)
            (root/'compiler').symlink_to('lib/std')
            before=identity.census({'toolchain':root},content=True)
            self.assertEqual(before,identity.census({'toolchain':root},content=True))
            metadata=identity.census({'toolchain':root},content=False)
            self.assertEqual(before['metadata_sha256'],metadata['metadata_sha256'])
            self.assertIsNone(metadata['content_sha256'])

    def test_runtime_links_cannot_resolve_into_guest_worker_writes(self):
        for destination in ('/workspace/code','/ci-cargo-home/source','../../workspace/code'):
            with self.subTest(destination=destination),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);(root/'link').symlink_to(destination)
                with self.assertRaises(identity.IdentityUnknown):
                    identity.census({'guest-runtime':root},content=True)

    def test_world_writable_runtime_inputs_and_special_files_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);file=root/'compiler';file.write_bytes(b'code');file.chmod(0o666)
            with self.assertRaises(identity.IdentityUnknown):identity.census({'toolchain':root},content=True)
            file.unlink();os.mkfifo(file)
            with self.assertRaises(identity.IdentityUnknown):identity.census({'toolchain':root},content=True)


if __name__=='__main__':unittest.main()
