"""An external seed producer must not receive application source or hooks."""
import importlib.util
import json
import tempfile
from pathlib import Path
import tomllib
import unittest

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('seed_workspace',ROOT/'scripts/ci-dependency-workspace.py')
workspace=importlib.util.module_from_spec(spec);spec.loader.exec_module(workspace)


class WorkspaceTests(unittest.TestCase):
    def inputs(self):
        metadata={'version':1,'workspace_root':'/workspace','packages':[
            {'id':'root','name':'application','version':'0.1.0','source':None,'manifest_path':'/workspace/Cargo.toml',
             'targets':[{'kind':['lib'],'src_path':'/workspace/src/lib.rs'}]},
            {'id':'leaf','name':'leaf','version':'1.2.3','source':'registry+https://github.com/rust-lang/crates.io-index',
             'manifest_path':'/ci-cargo-home/registry/src/pinned/leaf/Cargo.toml',
             'targets':[{'kind':['lib'],'src_path':'/ci-cargo-home/registry/src/pinned/leaf/src/lib.rs'}]}],
            'resolve':{'nodes':[{'id':'root','dependencies':['leaf'],'features':[]},
                                {'id':'leaf','dependencies':[],'features':['std']}]}}
        lock='version = 4\n[[package]]\nname="leaf"\nversion="1.2.3"\nsource="registry+https://github.com/rust-lang/crates.io-index"\nchecksum="'+'a'*64+'"\n'
        return metadata,lock

    def test_only_fixed_controller_code_and_pinned_declarations_enter_the_producer(self):
        metadata,lock=self.inputs();metadata['packages'][0]['metadata']={'build':'evil application code'}
        files=workspace.source_files(metadata,lock)
        self.assertEqual(set(files),{'Cargo.toml','src/lib.rs','seed-inputs.json','seed-lock.toml'})
        self.assertEqual(files['src/lib.rs'],b'// Controller-owned dependency producer; no application code.\n')
        manifest=tomllib.loads(files['Cargo.toml'].decode())
        self.assertEqual(manifest['package']['name'],'jarvis-external-dependency-seed')
        dependency=next(iter(manifest['dependencies'].values()))
        self.assertEqual(dependency,{'package':'leaf','version':'=1.2.3','default-features':False,'features':['std']})
        self.assertNotIn('evil',b''.join(files.values()).decode())
        self.assertEqual(files,workspace.source_files(metadata,lock))
        seeded_lock=tomllib.loads(files['seed-lock.toml'].decode())
        self.assertEqual(seeded_lock['package'][0]['checksum'],'a'*64)
        self.assertEqual(seeded_lock['package'][-1]['dependencies'],
            ['leaf 1.2.3 (registry+https://github.com/rust-lang/crates.io-index)'])

    def test_missing_duplicate_or_changed_checksum_never_becomes_an_approved_source(self):
        metadata,lock=self.inputs()
        for bad in (lock.replace('checksum','unbound'),lock.replace('a'*64,'not-a-checksum'),lock+'\n[[package]]\n'+lock.split('[[package]]',1)[1]):
            with self.subTest(lock=bad):
                with self.assertRaises(workspace.InputsUnknown):workspace.source_files(metadata,bad)

    def test_resolved_source_must_match_pinned_versions_checksums_features_and_graph(self):
        metadata,lock=self.inputs();files=workspace.source_files(metadata,lock)
        policy=json.loads(files['seed-inputs.json']);resolved=json.loads(json.dumps(metadata))
        resolved['packages'][0]['name']='jarvis-external-dependency-seed'
        resolved['packages'][0]['version']='0.0.0'
        workspace.verify_resolution(resolved,lock,policy)
        for fault in ('version','checksum','features','edge'):
            value=json.loads(json.dumps(resolved));actual_lock=lock
            if fault=='version':value['packages'][1]['version']='9.0.0'
            elif fault=='checksum':actual_lock=lock.replace('a'*64,'b'*64)
            elif fault=='features':value['resolve']['nodes'][1]['features']=[]
            else:value['resolve']['nodes'][1]['dependencies']=['root']
            with self.subTest(fault=fault),self.assertRaises(workspace.InputsUnknown):
                workspace.verify_resolution(value,actual_lock,policy)

    def test_source_census_refuses_application_files_changed_bytes_and_links(self):
        metadata,lock=self.inputs();files=workspace.source_files(metadata,lock)
        for fault in ('none','changed','extra','symlink'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);identity=workspace.write_source(root,files)
                if fault=='changed':(root/'src/lib.rs').write_text('unapproved source')
                elif fault=='extra':(root/'application.rs').write_text('application source')
                elif fault=='symlink':
                    (root/'src/lib.rs').unlink();(root/'src/lib.rs').symlink_to(root/'Cargo.toml')
                if fault=='none':self.assertEqual(workspace.verify_source(root,identity),identity)
                else:
                    with self.assertRaises(workspace.InputsUnknown):workspace.verify_source(root,identity)


if __name__=='__main__':unittest.main()
