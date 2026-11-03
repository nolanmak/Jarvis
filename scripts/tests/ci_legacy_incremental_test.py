"""Legacy derived-cache retirement; no artifact reuse or passing-CI authority."""
import errno
import copy
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

    def linked_peer(self):
        peer=self.unit/'s-abcdef-lmnop-0123456789';peer.mkdir(mode=0o755)
        lock=self.unit/'s-abcdef-lmnop.lock';lock.touch(mode=0o600)
        os.link(self.session/'0123456789abcdef.o',peer/'0123456789abcdef.o')
        return peer,lock

    def test_closed_link_policy_reclaims_both_sessions_without_touching_artifacts(self):
        peer,_=self.linked_peer();artifact=self.artifact.stat()
        self.assertEqual(self.reclaim(),[])
        self.assertEqual(len(self.reclaim(allow_closed_links=True)),2)
        self.assertFalse(peer.exists());self.assertFalse(self.session.exists())
        self.assertEqual(self.artifact.stat(),artifact)
        records=[json.loads(p.read_text()) for p in (self.root/'ci-legacy-incremental-retirements').glob('*/receipt.json')]
        linked=next(r for r in records if r['version']==2)
        self.assertEqual(linked['state'],'reclaimed');self.assertEqual(linked['endpoint_count'],2)
        self.assertNotIn('links',linked);self.assertFalse(linked['shipping_authority'])

    def test_closed_links_with_foreign_unknown_or_busy_peers_are_retained(self):
        peer,lock=self.linked_peer()
        for fault in ('foreign','unknown','busy'):
            fd=None;foreign=self.artifact.parent/'alias.o';unknown=peer/'unknown'
            if fault=='foreign':os.link(self.session/'0123456789abcdef.o',foreign)
            elif fault=='unknown':unknown.write_bytes(b'unknown')
            else:
                fd=os.open(lock,os.O_RDONLY);fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
            try:
                self.assertEqual(self.reclaim(allow_closed_links=True),[])
                self.assertTrue(self.session.exists());self.assertTrue(peer.exists())
            finally:
                if fd is not None:os.close(fd)
                if foreign.exists():foreign.unlink()
                if unknown.exists():unknown.unlink()

    def test_duplicate_inode_inside_selected_session_is_retained(self):
        os.link(self.session/'0123456789abcdef.o',self.session/'abcdef.o')
        self.assertEqual(self.reclaim(allow_closed_links=True),[])
        self.assertTrue(self.session.exists())

    def test_closed_link_partial_unlink_replays_without_stale_peer_obligations(self):
        peer,_=self.linked_peer()
        def interrupted(path,*args,**kw):
            (Path(path)/'0123456789abcdef.o').unlink()
            raise OSError(errno.EIO,'stop after unlinking shared object')
        with patch.object(cache.shutil,'rmtree',side_effect=interrupted):
            with self.assertRaises(cache.FreshnessHold):self.reclaim(allow_closed_links=True,limit=1)
        self.assertTrue(peer.exists());self.assertFalse(self.session.exists())
        self.assertEqual(len(self.reclaim(limit=1)),1)
        self.assertEqual(len(self.reclaim(allow_closed_links=True)),1)

    def test_closed_link_replay_holds_changed_or_busy_peer(self):
        peer,lock=self.linked_peer()
        with patch.object(cache.os,'rename',side_effect=OSError(errno.EIO,'stop before move')):
            with self.assertRaises(cache.FreshnessHold):self.reclaim(allow_closed_links=True)
        fd=os.open(lock,os.O_RDONLY);fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
        try:
            with self.assertRaises(cache.FreshnessHold):self.reclaim()
        finally:os.close(fd)
        (peer/'0123456789abcdef.o').write_bytes(b'changed shared object')
        with self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertTrue(self.session.exists());self.assertTrue(peer.exists())

    def test_closed_link_policy_requires_boolean(self):
        with self.assertRaises(cache.FreshnessHold):self.reclaim(allow_closed_links=1)

    def test_strict_selection_retains_its_original_lock_through_intent(self):
        original=cache._write_record
        def inspect(path,record):
            if record['state']=='retiring':
                with self.assertRaises(BlockingIOError):
                    descriptor=cache._legacy_lock(self.lock,[os.getuid(),os.getgid()],self.root.stat().st_dev)
                    os.close(descriptor)
            return original(path,record)
        with patch.object(cache,'_write_record',side_effect=inspect):self.assertEqual(len(self.reclaim()),1)

    def linked_intent(self):
        peer,lock=self.linked_peer()
        with patch.object(cache.os,'rename',side_effect=OSError(errno.EIO,'stop before move')):
            with self.assertRaises(cache.FreshnessHold):self.reclaim(allow_closed_links=True,limit=1)
        receipt=next((self.root/'ci-legacy-incremental-retirements').glob('*/receipt.json'))
        return peer,lock,receipt,json.loads(receipt.read_text())

    def test_version_two_schema_rejects_forged_endpoint_indices_and_metadata(self):
        _,_,_,record=self.linked_intent()
        for fault in ('index','duplicate','foreign','owner','link_count','extra','version','count','session_identity'):
            value=copy.deepcopy(record);links=value['links'];name='0123456789abcdef.o'
            if fault=='index':links['files'][name][0][0]=True
            elif fault=='duplicate':links['files'][name][1]=links['files'][name][0]
            elif fault=='foreign':links['sessions'][0]['relative']='target/debug/incremental/foreign-abc/s-abcdef-ghijk-0123456789'
            elif fault=='owner':links['sessions'][0]['directory'][3]+=1
            elif fault=='link_count':value['files'][0][6]=3
            elif fault=='extra':links['unknown']=True
            elif fault=='version':value['version']=True
            elif fault=='count':links['files'][name].append([0,name])
            else:links['sessions'][0]['directory'][1]+=1
            with self.subTest(fault=fault),self.assertRaises(cache.FreshnessHold):cache._validate_legacy_retirement(value,self.binding)

    def test_peer_directory_and_lock_rebinding_hold_even_when_file_rows_match(self):
        peer,lock,_,_=self.linked_intent()
        for path in (peer,lock):
            info=path.stat();os.utime(path,ns=(info.st_atime_ns,info.st_mtime_ns+1))
            with self.subTest(path=path.name),self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertTrue(self.session.exists());self.assertTrue(peer.exists())

    def test_complete_removal_replays_without_obsolete_peer_locks(self):
        peer,lock=self.linked_peer();original=cache._write_record
        def fail_completion(path,record):
            if record['state']=='reclaimed':raise OSError(errno.EIO,'stop before compact receipt')
            return original(path,record)
        with patch.object(cache,'_write_record',side_effect=fail_completion):
            with self.assertRaises(cache.FreshnessHold):self.reclaim(allow_closed_links=True,limit=1)
        self.assertFalse(self.session.exists())
        fd=os.open(lock,os.O_RDONLY);fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
        try:self.assertEqual(len(self.reclaim(limit=1)),1)
        finally:os.close(fd)
        self.assertTrue(peer.exists())

    def test_partial_unlink_with_remaining_shared_file_still_validates_that_peer(self):
        peer,_=self.linked_peer()
        os.link(self.session/'query-cache.bin',peer/'query-cache.bin')
        def interrupted(path,*args,**kw):
            (Path(path)/'0123456789abcdef.o').unlink()
            raise OSError(errno.EIO,'stop after first shared unlink')
        with patch.object(cache.shutil,'rmtree',side_effect=interrupted):
            with self.assertRaises(cache.FreshnessHold):self.reclaim(allow_closed_links=True,limit=1)
        (peer/'query-cache.bin').write_bytes(b'changed remaining shared input')
        with self.assertRaises(cache.FreshnessHold):self.reclaim()

    def test_foreign_alias_in_another_unit_prevents_closed_link_retirement(self):
        peer,_=self.linked_peer();foreign=self.incremental/'other-abcdef';foreign.mkdir()
        os.link(self.session/'0123456789abcdef.o',foreign/'abcdef.o')
        self.assertEqual(self.reclaim(allow_closed_links=True),[])
        self.assertTrue(self.session.exists());self.assertTrue(peer.exists())

    def test_closed_link_intent_write_failure_preserves_source_and_releases_locks(self):
        peer,lock=self.linked_peer()
        with patch.object(cache,'_write_record',side_effect=OSError(errno.ENOSPC,'full')):
            with self.assertRaises(OSError):self.reclaim(allow_closed_links=True)
        self.assertTrue(self.session.exists());self.assertTrue(peer.exists())
        fd=cache._legacy_lock(lock,[os.getuid(),os.getgid()],self.root.stat().st_dev);os.close(fd)
        self.assertEqual(len(self.reclaim(allow_closed_links=True)),2)

    def test_many_shared_aliases_obey_original_per_attempt_limit(self):
        for index in range(40):
            session=self.unit/('s-abcdef-p'+str(index)+'-0123456789');session.mkdir(mode=0o755)
            (self.unit/('s-abcdef-p'+str(index)+'.lock')).touch(mode=0o600)
            os.link(self.session/'0123456789abcdef.o',session/'0123456789abcdef.o')
        self.assertEqual(len(self.reclaim(allow_closed_links=True)),32)
        self.assertEqual(len(self.reclaim(allow_closed_links=True)),9)
        self.assertEqual(self.artifact.read_bytes(),b'compatible compiled artifact')

    def test_oversized_record_and_incomplete_candidate_census_retain_sessions(self):
        peer,_=self.linked_peer()
        with patch.object(cache,'_LEGACY_MAX_RECORD',1):self.assertEqual(self.reclaim(allow_closed_links=True),[])
        for index in range(255):
            session=self.unit/('s-abcdef-p'+str(index)+'-0123456789');session.mkdir(mode=0o755)
            (self.unit/('s-abcdef-p'+str(index)+'.lock')).touch(mode=0o600)
        self.assertEqual(self.reclaim(allow_closed_links=True),[])
        self.assertTrue(self.session.exists());self.assertTrue(peer.exists())

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
