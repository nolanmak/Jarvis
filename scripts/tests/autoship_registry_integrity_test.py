import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'autoship-registry-integrity.py'
spec = importlib.util.spec_from_file_location('integrity', SCRIPT)
integrity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(integrity)


class IntegrityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tree = self.root / 'registry/src/index/demo-1.0.0'
        self.tree.mkdir(parents=True)
        self.archive = self.root / 'registry/cache/index/demo-1.0.0.crate'
        self.archive.parent.mkdir(parents=True)
        with tarfile.open(self.archive, 'w:gz') as archive:
            body = b'pub fn ready() -> bool { false }'
            info = tarfile.TarInfo('demo-1.0.0/lib.rs')
            info.size = len(body)
            archive.addfile(info, io.BytesIO(body))
        (self.tree / 'lib.rs').write_bytes(body)
        self.lock = self.root / 'Cargo.lock'
        self.lock.write_text('version = 4\n[[package]]\nname = "demo"\nversion = "1.0.0"\n'
                             'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                             f'checksum = "{integrity.digest_file(self.archive)}"\n')

    def check(self):
        return integrity.attest(self.lock, self.root / 'registry')

    def test_valid_archive_and_extracted_source(self):
        self.assertEqual(self.check()['packages'], 1)

    def test_modified_source_rejected_even_with_forged_checksum_manifest(self):
        (self.tree / 'lib.rs').write_text('forged')
        (self.tree / '.cargo-checksum.json').write_text('{}')
        with self.assertRaises(RuntimeError):
            self.check()

    def test_extra_executable_source_rejected(self):
        (self.tree / 'build.rs').write_text('fn main() {}')
        with self.assertRaises(RuntimeError):
            self.check()

    def test_modified_archive_rejected(self):
        self.archive.write_bytes(b'forged')
        with self.assertRaises(RuntimeError):
            self.check()

    def test_symlinked_source_rejected(self):
        original = self.tree / 'lib.rs'
        contents = original.read_bytes()
        original.unlink()
        outside = self.root / 'outside'
        outside.write_bytes(contents)
        original.symlink_to(outside)
        with self.assertRaises(RuntimeError):
            self.check()

    def test_git_dependency_is_not_falsely_attested(self):
        self.lock.write_text('version=4\n[[package]]\nname="demo"\nversion="1"\nsource="git+https://example.com/repo#deadbeef"\n')
        with self.assertRaises(RuntimeError):
            self.check()


if __name__ == '__main__':
    unittest.main()
