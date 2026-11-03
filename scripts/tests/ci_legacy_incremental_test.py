"""Legacy derived-cache retirement; no artifact reuse or passing-CI authority."""
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
import uuid
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('legacy_incremental_cache',Path(__file__).resolve().parents[1]/'ci-cache-generation.py')
cache=importlib.util.module_from_spec(spec);spec.loader.exec_module(cache)


class LegacyIncrementalTests(unittest.TestCase):
    def setUp(self):
        self.root=Path(self.enterContext(tempfile.TemporaryDirectory()));self.root.chmod(0o700)
        self.binding={'generation':str(uuid.uuid4()),'head':'a'*40,'tree':'b'*40,
            **{k:'c'*64 for k in ('contract_sha256','authority_sha256','cache_identity_sha256','source_inputs_sha256')}}
        self.command=str(uuid.uuid4())
        self.incremental=self.root/'target/debug/incremental';self.incremental.mkdir(parents=True)
        self.unit=self.incremental/'example-abcdef';self.unit.mkdir()
        self.session=self.unit/'s-abcdef-ghijk-0123456789';self.session.mkdir()
        self.lock=self.unit/'s-abcdef-ghijk.lock';self.lock.touch(mode=0o600)
        for name in ('dep-graph.bin','query-cache.bin','work-products.bin','0123456789abcdef.o'):
            (self.session/name).write_bytes(b'original incremental '+name.encode())
        (self.root/'target/debug/deps').mkdir();self.artifact=self.root/'target/debug/deps/libexample-abcdef.rlib'
        self.artifact.write_bytes(b'compatible compiled artifact')
        (self.root/'cargo-home').mkdir();(self.root/'cargo-home/download').write_bytes(b'original download')
        for path in self.root.rglob('*'):
            path.chmod(0o755 if path.is_dir() else 0o644)
        self.lock.chmod(0o600)

    def reclaim(self,**kw):
        return cache.reclaim_legacy_incremental(self.root,self.binding,self.command,os.getuid(),os.getgid(),**kw)

    def test_retires_only_recognized_unlocked_sessions_and_preserves_compiled_units(self):
        self.assertEqual(len(self.reclaim()),1)
        self.assertFalse(self.session.exists());self.assertTrue(self.lock.exists())
        self.assertEqual(self.artifact.read_bytes(),b'compatible compiled artifact')
        self.assertEqual((self.root/'cargo-home/download').read_bytes(),b'original download')
        receipts=list((self.root/'ci-legacy-incremental-retirements').glob('*/receipt.json'))
        self.assertEqual(len(receipts),1);record=json.loads(receipts[0].read_text())
        self.assertEqual(record['state'],'reclaimed');self.assertEqual(record['binding'],self.binding)
        self.assertFalse(record['shipping_authority']);self.assertNotIn('files',record)
        self.assertEqual(self.reclaim(),[])

    def test_unknown_entries_links_unsafe_permissions_and_busy_locks_are_retained(self):
        for fault in ('unknown','symlink','hardlink','permissions','busy'):
            with self.subTest(fault=fault):
                path=self.session/'foreign';descriptor=None
                if fault=='unknown':path.write_bytes(b'keep unknown data')
                elif fault=='symlink':path.symlink_to(self.artifact)
                elif fault=='hardlink':os.link(self.artifact,self.session/'abcdef.o')
                elif fault=='permissions':self.session.chmod(0o777)
                else:
                    descriptor=os.open(self.lock,os.O_RDONLY);fcntl.flock(descriptor,fcntl.LOCK_EX|fcntl.LOCK_NB)
                try:
                    self.assertEqual(self.reclaim(),[]);self.assertTrue(self.session.is_dir())
                    self.assertEqual(self.artifact.read_bytes(),b'compatible compiled artifact')
                finally:
                    if descriptor is not None:os.close(descriptor)
                    if path.is_symlink() or path.exists():path.unlink()
                    if (self.session/'abcdef.o').exists():(self.session/'abcdef.o').unlink()
                    self.session.chmod(0o755)

    def test_interrupted_removal_replays_protected_intent_without_touching_artifacts(self):
        original=cache.shutil.rmtree
        def interrupted(path,*args,**kw):
            next(Path(path).iterdir()).unlink()
            raise OSError(errno.EIO,'interrupted derived-cache removal')
        with patch.object(cache.shutil,'rmtree',side_effect=interrupted):
            with self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertFalse(self.session.exists())
        receipts=list((self.root/'ci-legacy-incremental-retirements').glob('*/receipt.json'))
        self.assertEqual(json.loads(receipts[0].read_text())['state'],'retiring')
        self.assertEqual(len(self.reclaim()),1)
        self.assertEqual(self.artifact.read_bytes(),b'compatible compiled artifact')
        self.assertEqual(json.loads(receipts[0].read_text())['state'],'reclaimed')

    def test_failed_intent_write_cannot_move_or_remove_session(self):
        with patch.object(cache,'_write_record',side_effect=OSError(errno.ENOSPC,'full')):
            with self.assertRaises(OSError):self.reclaim()
        self.assertTrue(self.session.exists());self.assertEqual(len(list(self.session.iterdir())),4)
        self.assertEqual(len(self.reclaim()),1)

    def test_rebound_recovery_metadata_is_a_hold(self):
        def interrupted(path,*args,**kw):raise OSError(errno.EIO,'stop')
        with patch.object(cache.shutil,'rmtree',side_effect=interrupted):
            with self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.binding['cache_identity_sha256']='d'*64
        with self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertEqual(self.artifact.read_bytes(),b'compatible compiled artifact')

    def test_completed_history_stays_historical_after_physical_cache_replacement(self):
        self.assertEqual(len(self.reclaim()),1)
        receipt=next((self.root/'ci-legacy-incremental-retirements').glob('*/receipt.json'))
        original=receipt.read_bytes()
        self.binding['cache_identity_sha256']='d'*64
        self.assertEqual(self.reclaim(),[])
        self.assertEqual(receipt.read_bytes(),original)

    def test_interrupted_move_cannot_retire_replaced_session_contents(self):
        with patch.object(cache.os,'rename',side_effect=OSError(errno.EIO,'interrupted move')):
            with self.assertRaises(cache.FreshnessHold):self.reclaim()
        original=self.session/'dep-graph.bin';original.write_bytes(b'changed after original intent')
        with self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertEqual(original.read_bytes(),b'changed after original intent')
        self.assertEqual(self.artifact.read_bytes(),b'compatible compiled artifact')

    def test_unknown_incomplete_envelope_is_retained(self):
        with patch.object(cache,'_write_record',side_effect=OSError(errno.ENOSPC,'full')):
            with self.assertRaises(OSError):self.reclaim()
        allocation=next((self.root/'ci-legacy-incremental-retirements').iterdir())
        unknown=allocation/'unknown';unknown.write_bytes(b'keep unknown evidence')
        with self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertTrue(self.session.exists());self.assertEqual(unknown.read_bytes(),b'keep unknown evidence')

    def test_retirement_limit_and_invalid_limits_do_not_remove_extra_sessions(self):
        other=self.unit/'s-abcdef-lmnop-0123456789';other.mkdir(mode=0o755)
        (other/'dep-graph.bin').write_bytes(b'other incremental');(other/'dep-graph.bin').chmod(0o644)
        (self.unit/'s-abcdef-lmnop.lock').touch(mode=0o600)
        for limit in (0,33,True):
            with self.subTest(limit=limit),self.assertRaises(cache.FreshnessHold):self.reclaim(limit=limit)
            self.assertTrue(self.session.exists());self.assertTrue(other.exists())
        self.assertEqual(len(self.reclaim(limit=1)),1)
        self.assertEqual(sum(path.exists() for path in (self.session,other)),1)
        self.assertEqual(len(self.reclaim(limit=1)),1)


if __name__=='__main__':unittest.main()
