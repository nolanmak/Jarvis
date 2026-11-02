"""External unit eligibility, not a producer/quiescence or reuse proof."""
import importlib.util
from pathlib import Path
import unittest

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('ci_dependency_seed',ROOT/'scripts/ci-dependency-seed.py')
seed=importlib.util.module_from_spec(spec);spec.loader.exec_module(seed)


class EligibilityTests(unittest.TestCase):
    def metadata(self):
        value={'version':1,'workspace_root':'/workspace','packages':[
            {'id':'workspace','name':'workspace','source':None},
            {'id':'excluded-path','name':'excluded-path','source':None},
            {'id':'vendor','name':'vendor','source':'registry+https://github.com/rust-lang/crates.io-index'},
            {'id':'registry-a','name':'registry-a','source':'registry+https://github.com/rust-lang/crates.io-index'},
            {'id':'registry-b','name':'registry-b','source':'registry+https://github.com/rust-lang/crates.io-index'},
            {'id':'registry-with-path','name':'registry-with-path','source':'registry+https://github.com/rust-lang/crates.io-index'},
            {'id':'git','name':'git','source':'git+https://example.invalid/dependency#'+'a'*40},
        ],'resolve':{'nodes':[
            {'id':'workspace','dependencies':['registry-a','registry-with-path','git'],'features':['default']},
            {'id':'excluded-path','dependencies':[],'features':[]},
            {'id':'vendor','dependencies':[],'features':[]},
            {'id':'registry-a','dependencies':['registry-b'],'features':['std']},
            {'id':'registry-b','dependencies':[],'features':[]},
            {'id':'registry-with-path','dependencies':['vendor'],'features':[]},
            {'id':'git','dependencies':['registry-a'],'features':[]},
        ]}}
        for package in value['packages']:
            prefix='/workspace/vendor' if package['id']=='vendor' else ('/workspace/crates' if package['source'] is None else '/ci-cargo-home/registry/src/pinned')
            package['manifest_path']=prefix+'/'+package['name']+'/Cargo.toml'
        return value

    def test_only_external_units_without_any_transitive_path_input_are_eligible(self):
        value=self.metadata()
        self.assertEqual(set(seed.eligible_packages(value)),{'registry-a','registry-b','git'})
        value['resolve']['nodes'][4]['dependencies']=['excluded-path']
        self.assertEqual(seed.eligible_packages(value),{})

    def test_renaming_a_local_package_cannot_turn_it_into_a_registry_package(self):
        value=self.metadata();value['packages'][2]['name']='registry-a'
        self.assertNotIn('vendor',seed.eligible_packages(value))
        self.assertNotIn('registry-with-path',seed.eligible_packages(value))

    def test_registry_labels_cannot_certify_workspace_vendored_or_excluded_source(self):
        value=self.metadata();value['workspace_root']='/workspace/subproject'
        value['packages'][3]['manifest_path']='/workspace/excluded/local/Cargo.toml'
        eligible=seed.eligible_packages(value)
        self.assertEqual(set(eligible),{'registry-b'})

    def test_unsupported_missing_duplicate_and_cyclic_graphs_fail_closed(self):
        for fault in ('missing-resolve','missing-node','missing-package','duplicate-package','duplicate-node',
                      'missing-features','unknown-feature','unresolved-dependency','cycle','unpinned-git','unknown-source'):
            with self.subTest(fault=fault):
                value=self.metadata();nodes=value['resolve']['nodes'];packages=value['packages']
                if fault=='missing-resolve':value['resolve']=None
                elif fault=='missing-node':nodes.pop()
                elif fault=='missing-package':packages.pop()
                elif fault=='duplicate-package':packages.append(packages[0])
                elif fault=='duplicate-node':nodes.append(nodes[0])
                elif fault=='missing-features':nodes[0].pop('features')
                elif fault=='unknown-feature':nodes[0]['features']=[{'unbound':'input'}]
                elif fault=='unresolved-dependency':nodes[0]['dependencies'].append('unknown')
                elif fault=='cycle':nodes[4]['dependencies']=['registry-a']
                elif fault=='unpinned-git':packages[-1]['source']='git+https://example.invalid/dependency?branch=main'
                else:packages[-1]['source']='unbound-source'
                with self.assertRaises(seed.InputsUnknown):seed.eligible_packages(value)


if __name__=='__main__':unittest.main()
