"""Cargo artifact selection only; this does not attest an executed producer."""
import importlib.util
from pathlib import Path
import unittest

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('dependency_artifacts',ROOT/'scripts/ci-dependency-artifacts.py')
artifacts=importlib.util.module_from_spec(spec);spec.loader.exec_module(artifacts)


class ArtifactTests(unittest.TestCase):
    def metadata(self):
        return {'version':1,'workspace_root':'/workspace','packages':[
            {'id':'local','name':'local','source':None,'manifest_path':'/workspace/Cargo.toml'},
            {'id':'external','name':'external','source':'registry+https://github.com/rust-lang/crates.io-index',
             'manifest_path':'/ci-cargo-home/registry/src/pinned/external/Cargo.toml'}],
            'resolve':{'nodes':[{'id':'local','dependencies':['external'],'features':[]},
                                {'id':'external','dependencies':[],'features':['std','alloc']}]}}

    def message(self):
        return {'reason':'compiler-artifact','package_id':'external','features':['std'],
                'profile':{'opt_level':'0','debuginfo':0,'debug_assertions':True,'overflow_checks':True,'test':False},
                'target':{'name':'external','kind':['lib'],'crate_types':['lib'],
                          'src_path':'/ci-cargo-home/registry/src/pinned/external/src/lib.rs'},
                'filenames':['/workspace/target/debug/deps/libexternal-0123456789abcdef.rlib',
                             '/workspace/target/debug/deps/libexternal-0123456789abcdef.rmeta'],
                'executable':None,'fresh':False}

    def test_a_local_artifact_cannot_be_selected_even_when_its_filename_looks_external(self):
        message=self.message();local=message|{'package_id':'local'}
        plan=artifacts.select(self.metadata(),[local,message],target_root='/workspace/target')
        self.assertEqual(len(plan),1)
        self.assertEqual(plan[0]['package_id'],'external')
        self.assertEqual(plan[0]['features'],['std'])
        self.assertEqual(plan[0]['fingerprint_directory'],'debug/.fingerprint/external-0123456789abcdef')
        self.assertEqual(plan[0]['dep_info'],'debug/deps/external-0123456789abcdef.d')

    def test_profile_features_and_target_namespace_are_separate_unit_identities(self):
        first=self.message();second=self.message()
        second['profile']=second['profile']|{'test':True}
        second['features']=['alloc'];second['filenames']=['/workspace/target/x86_64-unknown-linux-gnu/debug/deps/libexternal-fedcba9876543210.rlib']
        plan=artifacts.select(self.metadata(),[first,second],target_root='/workspace/target')
        self.assertEqual(len(plan),2)
        self.assertNotEqual(plan[0]['profile'],plan[1]['profile'])
        self.assertNotEqual(plan[0]['fingerprint_directory'],plan[1]['fingerprint_directory'])

    def test_build_script_binary_and_generated_outputs_remain_distinct_units(self):
        compiled=self.message()
        compiled['target']=compiled['target']|{'name':'build-script-build','kind':['custom-build'],
            'crate_types':['bin'],'src_path':'/ci-cargo-home/registry/src/pinned/external/build.rs'}
        compiled['filenames']=['/workspace/target/debug/build/external-0123456789abcdef/build-script-build']
        compiled['executable']=compiled['filenames'][0]
        ran={'reason':'build-script-executed','package_id':'external',
             'out_dir':'/workspace/target/debug/build/external-fedcba9876543210/out',
             'linked_libs':['static=generated'],'linked_paths':['native=/workspace/target/debug/build/external-fedcba9876543210/out'],
             'cfgs':['generated_config'],'env':[['GENERATED','value']]}
        plan=artifacts.select(self.metadata(),[compiled,ran],target_root='/workspace/target')
        self.assertEqual(len(plan),2)
        self.assertEqual({entry['fingerprint_directory'] for entry in plan},
            {'debug/.fingerprint/external-0123456789abcdef','debug/.fingerprint/external-fedcba9876543210'})
        self.assertIn('debug/build/external-fedcba9876543210',set().union(*(set(entry['directory_trees']) for entry in plan)))

    def test_unknown_units_and_paths_never_become_a_partial_valid_plan(self):
        for fault in ('unknown-package','escaped-output','lexical-escape','unhashed-output','unknown-feature','foreign-source','contradictory-unit','unsafe-package-name'):
            with self.subTest(fault=fault):
                metadata=self.metadata();message=self.message();messages=[message]
                if fault=='unknown-package':message['package_id']='unresolved'
                elif fault=='escaped-output':message['filenames']=['/private/foreign.rlib']
                elif fault=='lexical-escape':message['filenames']=['/workspace/target/../foreign.rlib']
                elif fault=='unhashed-output':message['filenames']=['/workspace/target/debug/deps/libexternal.rlib']
                elif fault=='unknown-feature':message['features']=['not-resolved']
                elif fault=='foreign-source':message['target']=message['target']|{'src_path':'/workspace/local.rs'}
                elif fault=='contradictory-unit':messages.append(message|{'features':['alloc']})
                else:metadata['packages'][1]['name']='../../escape'
                with self.assertRaises(artifacts.InputsUnknown):
                    artifacts.select(metadata,messages,target_root='/workspace/target')


if __name__=='__main__':unittest.main()
