"""Publication protocol/filesystem tests; real producer/UID acceptance is separate."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
import uuid
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('seed_store',ROOT/'scripts/ci-dependency-store.py')
store=importlib.util.module_from_spec(spec);spec.loader.exec_module(store)


class StoreTests(unittest.TestCase):
    def released_seed(self, root):
        binding,producer,command,target,home,ack=self.fixture(root)
        seed=store.publish(root,producer,ack)
        self.assertEqual(store.reclaim_producers(root,[],cache_identity=binding['cache_identity_sha256'],
            eligible=[{'producer':producer,'ack':ack}]),[producer])
        return binding,seed

    def test_orphan_seed_reclamation_preserves_index_producer_and_generation_references(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            old=store.publish(root,producer,ack)
            self.assertEqual(store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256']),[])
            store.reclaim_producers(root,[],cache_identity=binding['cache_identity_sha256'],
                eligible=[{'producer':producer,'ack':ack}])
            store.index_seed(root,old,binding)
            self.assertEqual(store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256']),[])
            generation={key:binding[key] for key in ('authority_sha256','cache_identity_sha256','contract_sha256','source_inputs_sha256')}
            generation.update(generation=str(uuid.uuid4()),head=binding['source_head'],tree='1'*40)
            target=store.cache.prepare(root,generation,str(uuid.uuid4()),os.getuid(),os.getgid())
            home=target.parent/'cargo-home';home.mkdir(mode=0o700)
            store.cache.begin_seed_install(target,generation,old)
            store.install(root,old,binding,target,home)
            store.cache.finish_seed_install(target,generation,old)
            _,new=self.released_seed(root);store.index_seed(root,new,binding)
            self.assertEqual(store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256']),[])
            self.assertEqual(store.cache.reclaim(root,[],os.getuid(),os.getgid()),[generation['generation']])
            self.assertEqual(store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256']),[old])
            self.assertEqual(store.lookup_seed(root,binding),new)
            self.assertFalse((root/'ci-trusted-dependencies'/old).exists())

    def test_orphan_seed_reclamation_is_bounded_and_keeps_other_hard_links(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);released=[self.released_seed(root) for _ in range(3)]
            binding,seed=released[0]
            source=root/'ci-trusted-dependencies'/seed/'files/target/debug/deps/libleaf-0123456789abcdef.rlib'
            sentinel=root/'retained-artifact';os.link(source,sentinel)
            before=sentinel.read_bytes();inode=sentinel.stat().st_ino
            first=store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256'])
            second=store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256'])
            self.assertEqual(len(first),2);self.assertEqual(len(second),1)
            self.assertEqual(set(first+second),{seed for _,seed in released})
            self.assertEqual((sentinel.read_bytes(),sentinel.stat().st_ino),(before,inode))
            self.assertEqual(store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256']),[])

    def test_orphan_seed_reclamation_replays_an_original_interrupted_intent(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,seed=self.released_seed(root)
            def interrupted(path):
                artifact=Path(path)/'files/target/debug/deps/libleaf-0123456789abcdef.rlib'
                artifact.unlink()
                raise OSError('interrupted after first unlink')
            with patch.object(store.shutil,'rmtree',side_effect=interrupted):
                with self.assertRaises(store.FreshnessHold):
                    store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256'])
            self.assertEqual(store.reclaim_seeds(root,cache_identity=binding['cache_identity_sha256']),[seed])
            self.assertFalse((root/'ci-trusted-dependencies'/seed).exists())

    def test_unknown_seed_contents_references_and_foreign_cache_are_preserved(self):
        for case in ('extra-file','symlink','unknown-index','unknown-generation','unknown-producer','foreign-cache','missing-release','protected-install'):
            with self.subTest(case=case),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);binding,seed=self.released_seed(root)
                path=root/'ci-trusted-dependencies'/seed
                if case=='extra-file':(path/'unknown').write_bytes(b'preserve')
                if case=='symlink':(path/'unknown').symlink_to(root/'outside')
                if case=='unknown-index':
                    namespace=root/'ci-dependency-index';namespace.mkdir(mode=0o700)
                    (namespace/('0'*64+'.json')).write_bytes(b'torn')
                if case=='unknown-generation':
                    namespace=root/'ci-source-generations';namespace.mkdir(mode=0o711)
                    (namespace/str(uuid.uuid4())).mkdir(mode=0o711)
                if case=='unknown-producer':
                    namespace=root/'ci-dependency-producers'
                    (namespace/str(uuid.uuid4())).mkdir(mode=0o700)
                if case=='missing-release':
                    for receipt in (root/'ci-producer-retirements'/'completed').iterdir():receipt.unlink()
                identity='0'*64 if case=='foreign-cache' else binding['cache_identity_sha256']
                self.assertEqual(store.reclaim_seeds(root,cache_identity=identity,
                    protected_seeds=[seed] if case=='protected-install' else []),[])
                self.assertTrue(path.is_dir())

    def test_empty_seed_installation_still_has_consistent_sealed_roots(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,_,_,_,_,_=self.fixture(root)
            producer=str(uuid.uuid4());command=str(uuid.uuid4())
            store.prepare_isolated_producer(root,binding,producer,command,os.getuid(),os.getgid())
            metadata={'version':1,'workspace_root':'/workspace','packages':[
                {'id':'root','name':'empty','source':None,'manifest_path':'/workspace/Cargo.toml'}],
                'resolve':{'nodes':[{'id':'root','dependencies':[],'features':[]}]}}
            store.record_isolated_output(root,producer,command,metadata,[])
            ack={'command':command,'authority_sha256':binding['authority_sha256'],
                 'cache_identity_sha256':binding['cache_identity_sha256'],'quiescence_sha256':'1'*64}
            seed=store.publish(root,producer,ack)
            target=root/'empty-target';home=root/'empty-home';target.mkdir();home.mkdir()
            store.install(root,seed,binding,target,home)
            self.assertEqual(target.stat().st_mode&0o1777,0o1777)
            self.assertEqual(home.stat().st_mode&0o1777,0o1777)

    def test_index_lookup_revalidates_seed_bytes_and_survives_a_changed_source_head(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            self.assertIsNone(store.lookup_seed(root,binding))
            seed=store.publish(root,producer,ack);store.index_seed(root,seed,binding)
            self.assertEqual(store.lookup_seed(root,binding|{'source_head':'1'*40,'source_inputs_sha256':'2'*64}),seed)
            self.assertIsNone(store.lookup_seed(root,binding|{'authority_sha256':'3'*64}))
            artifact=root/'ci-trusted-dependencies'/seed/'files/target/debug/deps/libleaf-0123456789abcdef.rlib'
            artifact.unlink()
            with self.assertRaises(store.FreshnessHold):store.lookup_seed(root,binding)

    def test_replacing_a_torn_index_preserves_old_metadata_and_requires_a_valid_seed(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            seed=store.publish(root,producer,ack);path=store._index_path(root,binding)
            path.write_text('torn');path.chmod(0o600)
            with self.assertRaises(store.FreshnessHold):store.lookup_seed(root,binding)
            store.index_seed(root,seed,binding)
            retired=list((path.parent/'superseded').iterdir())
            self.assertEqual(len(retired),1);self.assertEqual(retired[0].read_text(),'torn')
            self.assertEqual(store.lookup_seed(root,binding),seed)

    def fixture(self,root):
        binding={field:char*64 for field,char in [('authority_sha256','a'),('cache_identity_sha256','b'),
            ('contract_sha256','c'),('source_inputs_sha256','d'),('dependency_inputs_sha256','e')]}
        binding['source_head']='f'*40
        producer=str(uuid.uuid4());command=str(uuid.uuid4())
        target,home=store.prepare_isolated_producer(root,binding,producer,command,os.getuid(),os.getgid())
        files={'debug/deps/libleaf-0123456789abcdef.rlib':b'compiled external library',
            'debug/deps/leaf-0123456789abcdef.d':b'/workspace/target/debug/deps/libleaf-0123456789abcdef.rlib: /ci-cargo-home/registry/src/pinned/leaf/src/lib.rs\n',
            'debug/.fingerprint/leaf-0123456789abcdef/lib-leaf.json':json.dumps({'rustc':1,'features':'[]','declared_features':'[]',
                'profile':2,'rustflags':[],'config':3,'target':4,'path':5,'compile_kind':0,'deps':[],'local':[]}).encode(),
            'debug/.fingerprint/leaf-0123456789abcdef/lib-leaf':b'fingerprint'}
        for name,data in files.items():
            path=target/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(data)
        package=home/'registry/src/pinned/leaf';(package/'src').mkdir(parents=True)
        (package/'Cargo.toml').write_text('[package]\nname="leaf"\nversion="1.0.0"')
        (package/'src/lib.rs').write_text('pub fn value()->u8 { 7 }')
        metadata={'version':1,'workspace_root':'/workspace','packages':[
            {'id':'leaf','name':'leaf','source':'registry+https://github.com/rust-lang/crates.io-index',
             'manifest_path':'/ci-cargo-home/registry/src/pinned/leaf/Cargo.toml',
             'targets':[{'kind':['lib'],'src_path':'/ci-cargo-home/registry/src/pinned/leaf/src/lib.rs'}]}],
            'resolve':{'nodes':[{'id':'leaf','dependencies':[],'features':[]}]}}
        message={'reason':'compiler-artifact','package_id':'leaf','features':[],
            'profile':{'opt_level':'0','debuginfo':0,'debug_assertions':True,'overflow_checks':True,'test':False},
            'target':{'name':'leaf','kind':['lib'],'crate_types':['lib'],
                'src_path':'/ci-cargo-home/registry/src/pinned/leaf/src/lib.rs'},
            'filenames':['/workspace/target/debug/deps/libleaf-0123456789abcdef.rlib'],
            'executable':None,'fresh':False}
        store.record_isolated_output(root,producer,command,metadata,[message])
        ack={'command':command,'authority_sha256':binding['authority_sha256'],
            'cache_identity_sha256':binding['cache_identity_sha256'],'quiescence_sha256':'1'*64}
        return binding,producer,command,target,home,ack

    def test_only_the_original_producer_command_can_publish_after_host_acknowledgement(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            for bad in (None,ack|{'command':str(uuid.uuid4())},ack|{'authority_sha256':'2'*64},
                        ack|{'quiescence_sha256':None}):
                with self.assertRaises(store.FreshnessHold):store.publish(root,producer,bad)
            seed=store.publish(root,producer,ack)
            self.assertEqual(store.publish(root,producer,ack),seed)
            manifest=store.read_seed(root,seed,binding)
            self.assertEqual(manifest['producer']['command'],command)
            self.assertEqual(manifest['producer']['quiescence_sha256'],'1'*64)
            self.assertFalse(manifest['shipping_authority'])
            self.assertEqual(manifest['units'][0]['unit_identity_status'],'sealed_producer_fingerprint')
            self.assertFalse(target.exists())
            self.assertFalse(home.exists())
            self.assertTrue((target.parent/'published.json').is_file())

    def test_interrupted_published_payload_cleanup_replays_original_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            with patch.object(store.shutil,'rmtree',side_effect=OSError('interrupted cleanup')):
                with self.assertRaises(store.FreshnessHold):store.publish(root,producer,ack)
            receipt=store._document(target.parent/'published.json')
            seed=store.publish(root,producer,ack)
            self.assertEqual(seed,receipt['seed'])
            self.assertFalse(target.exists());self.assertFalse(home.exists())
            self.assertEqual(store.read_seed(root,seed,binding)['producer']['command'],command)

    def test_candidate_artifacts_cannot_promote_themselves_by_supplying_matching_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            with self.assertRaises(store.FreshnessHold):store.publish(root,str(uuid.uuid4()),{
                'command':str(uuid.uuid4()),'authority_sha256':'a'*64,'cache_identity_sha256':'b'*64,'quiescence_sha256':'c'*64})

    def test_install_preserves_compiled_and_source_bytes_and_rejects_incompatible_authority(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            seed=store.publish(root,producer,ack)
            for field in ('authority_sha256','cache_identity_sha256','contract_sha256','dependency_inputs_sha256'):
                with self.assertRaises(store.FreshnessHold):store.read_seed(root,seed,binding|{field:'9'*64})
            new_target=root/'consumer-target';new_home=root/'consumer-home'
            new_target.mkdir();new_home.mkdir()
            store.install(root,seed,binding,new_target,new_home)
            artifact=new_target/'debug/deps/libleaf-0123456789abcdef.rlib'
            self.assertEqual(artifact.read_bytes(),b'compiled external library')
            self.assertEqual((new_home/'registry/src/pinned/leaf/src/lib.rs').read_text(),'pub fn value()->u8 { 7 }')
            self.assertEqual(artifact.stat().st_mode&0o222,0)
            self.assertEqual(new_target.stat().st_mode&0o1777,0o1777)
            self.assertEqual((new_target/'debug/deps').stat().st_mode&0o1777,0o1777)

    def test_corrupted_artifact_never_installs_even_if_its_unit_input_identity_is_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            seed=store.publish(root,producer,ack)
            manifest=store.read_seed(root,seed,binding)
            path=root/'ci-trusted-dependencies'/seed/'files/target/debug/deps/libleaf-0123456789abcdef.rlib'
            path.chmod(0o600);path.write_bytes(b'corrupted')
            with self.assertRaises(store.FreshnessHold):store.read_seed(root,seed,binding)

    def test_publication_interruption_cannot_create_an_activatable_partial_seed(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            with patch.object(store.os,'rename',side_effect=OSError('publication interrupted')):
                with self.assertRaises(OSError):store.publish(root,producer,ack)
            seeds=root/'ci-trusted-dependencies'
            self.assertFalse(any(len(path.name)==64 for path in seeds.iterdir()))
            seed=store.publish(root,producer,ack)
            self.assertEqual(store.read_seed(root,seed,binding)['version'],1)

    def test_unknown_producer_and_incomplete_fingerprint_cannot_authorize_reuse(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            fingerprint=target/'debug/.fingerprint/leaf-0123456789abcdef/lib-leaf.json'
            value=json.loads(fingerprint.read_text());value.pop('rustflags');fingerprint.write_text(json.dumps(value))
            with self.assertRaises(store.FreshnessHold):store.publish(root,producer,ack)
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            seed=store.publish(root,producer,ack)
            manifest=store.read_seed(root,seed,binding);manifest['producer']['kind']='candidate'
            identity=store._digest(manifest)
            old=root/'ci-trusted-dependencies'/seed;new=old.with_name(identity);old.rename(new)
            store.cache._write_record(new/'manifest.json',manifest)
            with self.assertRaises(store.FreshnessHold):store.read_seed(root,identity,binding)

    def test_first_party_or_volatile_compiler_inputs_cannot_be_published_as_external(self):
        for input_line in ('/workspace/src/private.rs','/other/unbound.rs',
                           '# env-dep:CI_CONTRACT_HEAD=changed','# env-dep:UNDECLARED_INPUT'):
            with self.subTest(input_line=input_line),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
                dep=target/'debug/deps/leaf-0123456789abcdef.d'
                if input_line.startswith('#'):dep.write_text(dep.read_text()+input_line+'\n')
                else:dep.write_text('artifact: '+input_line+'\n')
                with self.assertRaises(store.FreshnessHold):store.publish(root,producer,ack)

    def test_missing_published_artifact_is_a_typed_hold_instead_of_a_source_verdict(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding,producer,command,target,home,ack=self.fixture(root)
            seed=store.publish(root,producer,ack)
            artifact=root/'ci-trusted-dependencies'/seed/'files/target/debug/deps/libleaf-0123456789abcdef.rlib'
            artifact.unlink()
            with self.assertRaises(store.FreshnessHold):store.read_seed(root,seed,binding)


if __name__=='__main__':unittest.main()
