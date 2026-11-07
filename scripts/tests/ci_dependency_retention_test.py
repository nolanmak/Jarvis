"""Retirement of known inactive producer payloads, under existing admission."""
import importlib.util
from pathlib import Path
import shutil
import tempfile
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('retention_store_tests', Path(__file__).with_name('ci_dependency_store_test.py'))
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
store = fixtures.store


class RetentionTests(unittest.TestCase):
    def test_another_sessions_known_but_unreleased_producer_is_not_adopted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, producer, _, target, _, _ = self.producer(root)
            self.assertEqual(store.reclaim_producers(root, [],cache_identity='b'*64), [])
            self.assertTrue(target.is_dir())

    def producer(self, root):
        result=fixtures.StoreTests().fixture(root)
        store.cache._write_record(result[3].parent/'cargo-home-seeded.json',{'version':1,'kind':'ci-downloads-seeded'})
        if not hasattr(self,'releases'):self.releases={}
        self.releases[result[1]]=result[-1]
        return result

    def reclaim(self,root,protected):
        return store.reclaim_producers(root,protected,cache_identity='b'*64,eligible=[{'producer':key,'ack':ack} for key,ack in self.releases.items()])

    def test_release_cannot_delete_a_different_physical_cache_or_rewrite_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            binding,producer,_,target,_,ack=self.producer(root)
            releases=[{'producer':producer,'ack':ack}]
            self.assertEqual(store.reclaim_producers(root,[],cache_identity='0'*64,eligible=releases),[])
            self.assertTrue(target.is_dir())
            seed=store.publish(root,producer,ack)
            bad=[{'producer':producer,'ack':ack|{'quiescence_sha256':'9'*64}}]
            self.assertEqual(store.reclaim_producers(root,[],cache_identity='b'*64,eligible=bad),[])
            self.assertTrue(target.parent.is_dir())
            self.assertEqual(self.reclaim(root,[]),[producer])
            self.assertEqual(self.reclaim(root,[]),[producer])
            self.assertEqual(store.read_seed(root,seed,binding)['producer']['quiescence_sha256'],ack['quiescence_sha256'])

    def test_live_producer_is_preserved_and_inactive_payloads_are_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            items = [self.producer(root) for _ in range(4)]
            protected = items[0][1]
            reclaimed = self.reclaim(root, [protected])
            self.assertEqual(len(reclaimed), 2)
            self.assertNotIn(protected, reclaimed)
            self.assertTrue(items[0][3].is_dir())
            for _, producer, _, target, _, _ in items:
                if producer in reclaimed:
                    self.assertFalse(target.parent.exists())
                    record = store._document(root / 'ci-producer-retirements' / 'completed' / (producer + '.json'))
                    self.assertEqual(record['state'], 'reclaimed')
                    self.assertEqual(record['original']['control']['command'], next(x[2] for x in items if x[1] == producer))
                    self.assertEqual(record['original']['produced']['command'], record['original']['control']['command'])
                    self.assertEqual(record['original']['downloads'],{'version':1,'kind':'ci-downloads-seeded'})
            self.releases={key:ack for key,ack in self.releases.items() if key not in reclaimed}
            self.assertEqual(len(self.reclaim(root, [protected])), 1)

    def test_unknown_torn_and_escaped_producers_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryDirectory() as outside:
            root = Path(directory)
            _, producer, _, target, _, _ = self.producer(root)
            target.parent.joinpath('control.json').write_text('{torn')
            namespace = root / 'ci-dependency-producers'
            unknown = namespace / str(uuid.uuid4())
            unknown.mkdir(mode=0o700)
            unknown.joinpath('owner-data').write_text('keep')
            escaped = namespace / str(uuid.uuid4())
            escaped.symlink_to(outside, target_is_directory=True)
            self.assertEqual(self.reclaim(root, []), [])
            self.assertTrue(target.is_dir())
            self.assertEqual(unknown.joinpath('owner-data').read_text(), 'keep')
            self.assertTrue(escaped.is_symlink())

    def test_unknown_download_seed_metadata_preserves_the_released_producer(self):
        for marker in ({'version':2,'kind':'ci-downloads-seeded'},
                       {'version':True,'kind':'ci-downloads-seeded'},
                       {'version':1,'kind':'untrusted'},
                       {'version':1,'kind':'ci-downloads-seeded','extra':True}):
            with self.subTest(marker=marker),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);_,producer,_,target,_,_=self.producer(root)
                store.cache._write_record(target.parent/'cargo-home-seeded.json',marker)
                self.assertEqual(self.reclaim(root,[]),[])
                self.assertTrue(target.is_dir())

    def test_interrupted_deletion_recovers_original_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, producer, command, target, _, _ = self.producer(root)
            def interrupt(path, *args, **kwargs):
                Path(path).joinpath('control.json').unlink()
                raise OSError('interrupted after control deletion')

            with patch.object(store.shutil, 'rmtree', side_effect=interrupt):
                with self.assertRaises(store.FreshnessHold):
                    self.reclaim(root, [])
            pending = root / 'ci-producer-retirements' / 'pending' / (producer + '.json')
            self.assertEqual(store._document(pending)['original']['control']['command'], command)
            self.assertTrue(target.is_dir())
            self.releases={}
            self.assertEqual(self.reclaim(root, []), [producer])
            self.assertFalse(target.parent.exists())
            self.assertFalse(pending.exists())

    def test_deleted_directory_before_completion_is_reconciled(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, producer, _, target, _, _ = self.producer(root)
            real_remove = shutil.rmtree

            def remove_then_interrupt(path, *args, **kwargs):
                real_remove(path, *args, **kwargs)
                raise OSError('interrupted before completion')

            with patch.object(store.shutil, 'rmtree', side_effect=remove_then_interrupt):
                with self.assertRaises(store.FreshnessHold):
                    self.reclaim(root, [])
            self.assertFalse(target.parent.exists())
            self.assertEqual(self.reclaim(root, []), [producer])

    def test_published_seed_and_installed_links_survive_producer_retirement(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binding, producer, _, target, home, ack = self.producer(root)
            seed = store.publish(root, producer, ack)
            store.index_seed(root, seed, binding)
            consumer_target = root / 'consumer-target'
            consumer_home = root / 'consumer-home'
            consumer_target.mkdir()
            consumer_home.mkdir()
            store.install(root, seed, binding, consumer_target, consumer_home)
            self.assertEqual(self.reclaim(root, []), [producer])
            self.assertEqual(store.lookup_seed(root, binding), seed)
            self.assertEqual((consumer_target / 'debug/deps/libleaf-0123456789abcdef.rlib').read_bytes(), b'compiled external library')
            record = store._document(root / 'ci-producer-retirements' / 'completed' / (producer + '.json'))
            self.assertEqual(record['original']['published']['seed'], seed)


if __name__ == '__main__':
    unittest.main()
