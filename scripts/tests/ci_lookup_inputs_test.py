"""Selective input identity from captured pathname queries and content censuses."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest


ROOT=Path(__file__).resolve().parents[1]


def load(name):
    spec=importlib.util.spec_from_file_location(name,ROOT/(name.replace('_','-')+'.py'))
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    return module


class LookupInputTests(unittest.TestCase):
    def setUp(self):
        self.inputs=load('ci_lookup_inputs');self.manifest=load('ci_input_manifest')
        self.directory=self.enterContext(tempfile.TemporaryDirectory())
        self.root=Path(self.directory)

    def census(self):
        return self.inputs.LookupCensus({'source':('/workspace',self.manifest.snapshot({'source':self.root}))})

    def query(self,path,*,syscall='openat',outcome='success',errno=None):
        return {'pid':12,'syscall':syscall,'directory':b'/workspace','path':path,
            'outcome':outcome,'errno':errno,'intent':'read' if syscall=='openat' else 'metadata',
            'arguments':'','result':3 if syscall=='openat' else 0}

    def test_content_change_at_identical_old_mtime_changes_only_a_relevant_identity(self):
        used=self.root/'used.rs';other=self.root/'other.rs'
        used.write_bytes(b'OLD');other.write_bytes(b'other');os.utime(used,ns=(1,1))
        query=[self.query(b'used.rs')];before=self.census().observe(query)
        other.write_bytes(b'unrelated change')
        self.assertEqual(before,self.census().observe(query))
        used.write_bytes(b'NEW');os.utime(used,ns=(1,1))
        self.assertNotEqual(before['sha256'],self.census().observe(query)['sha256'])
        self.assertFalse(before['input_closure_complete']);self.assertFalse(before['shipping_authority'])

    def test_formerly_absent_module_invalidates_without_changing_any_read_file(self):
        chosen=self.root/'chosen';chosen.mkdir();(chosen/'mod.rs').write_bytes(b'OLD')
        queries=[self.query(b'chosen/mod.rs'),self.query(b'chosen.rs',syscall='statx',outcome='failure',errno='ENOENT')]
        original=self.census().observe(queries)
        (self.root/'unrelated.rs').write_bytes(b'unrelated')
        self.assertEqual(original,self.census().observe(queries))
        (self.root/'chosen.rs').write_bytes(b'conflict');os.utime(self.root/'chosen.rs',ns=(1,1))
        changed=self.census().observe(queries,require_original_results=False)
        self.assertNotEqual(original['sha256'],changed['sha256'])
        with self.assertRaises(self.inputs.FreshnessHold):self.census().observe(queries)

    def test_missing_ancestor_and_not_a_directory_are_distinct_inputs(self):
        queries=[self.query(b'missing/input.rs',outcome='failure',errno='ENOENT')]
        original=self.census().observe(queries)
        (self.root/'unrelated').write_bytes(b'other')
        self.assertEqual(original,self.census().observe(queries))
        (self.root/'missing').write_bytes(b'now a file')
        changed=self.census().observe(queries,require_original_results=False)
        self.assertNotEqual(original['sha256'],changed['sha256'])

    def test_symlink_is_followed_before_parent_components_and_its_hops_are_inputs(self):
        a=self.root/'a';a.mkdir();b=self.root/'b';b.mkdir();(b/'nested').mkdir()
        (self.root/'input').write_bytes(b'WRONG');(b/'input').write_bytes(b'RIGHT')
        (a/'link').symlink_to('../b/nested')
        query=[self.query(b'a/link/../input')];original=self.census().observe(query)
        (self.root/'input').write_bytes(b'irrelevant')
        self.assertEqual(original,self.census().observe(query))
        (b/'input').write_bytes(b'changed')
        self.assertNotEqual(original['sha256'],self.census().observe(query)['sha256'])
        (a/'link').unlink();(a/'link').symlink_to('../b')
        self.assertNotEqual(original['sha256'],self.census().observe(query)['sha256'])

    def test_directory_reads_depend_on_membership_and_types_not_unread_file_bytes(self):
        (self.root/'one').write_bytes(b'one')
        query=[{'pid':12,'syscall':'getdents64','directory':b'/workspace','intent':'directory',
            'outcome':'success','errno':None,'result':24,'entries':[{'name':b'one','type':'DT_REG'}]},
            {'pid':12,'syscall':'getdents64','directory':b'/workspace','intent':'directory',
            'outcome':'success','errno':None,'result':0,'entries':[]}]
        original=self.census().observe(query)
        (self.root/'one').write_bytes(b'unread bytes changed')
        self.assertEqual(original,self.census().observe(query))
        (self.root/'two').mkdir()
        self.assertNotEqual(original['sha256'],self.census().observe(query,require_original_results=False)['sha256'])

    def test_unknown_paths_devices_writes_and_inconsistent_results_remain_prerequisites(self):
        (self.root/'input').write_bytes(b'input')
        census=self.census()
        for query in [self.query(b'/outside/input'),self.query(b'../outside/input'),
            self.query(b'absent'),{**self.query(b'input'),'intent':'write'},
            {**self.query(b'input'),'descriptor_kind':'character-device'},
            self.query(b'input',outcome='failure',errno='ENOENT')]:
            with self.subTest(query=query),self.assertRaises(self.inputs.FreshnessHold):census.observe([query])

    def test_census_is_detached_from_caller_mutation_and_rejects_torn_snapshot(self):
        (self.root/'input').write_bytes(b'input');snapshot=self.manifest.snapshot({'source':self.root})
        census=self.inputs.LookupCensus({'source':('/workspace',snapshot)});query=[self.query(b'input')]
        before=census.observe(query)
        snapshot['entries'].clear()
        self.assertEqual(before,census.observe(query))
        with self.assertRaises(self.inputs.FreshnessHold):self.inputs.LookupCensus({'source':('/workspace',snapshot)})

    def test_declared_namespaces_share_literal_routes_without_attesting_unknown_neighbors(self):
        source=self.root/'src';source.mkdir();generated=self.root/'generated';generated.mkdir()
        (source/'main.rs').write_bytes(b'source');(generated/'input.rs').write_bytes(b'generated')
        census=self.inputs.LookupCensus({'source':('/workspace/src',self.manifest.snapshot({'source':source})),
            'generated':('/workspace/generated',self.manifest.snapshot({'generated':generated}))})
        observed=census.observe([self.query(b'src/../generated/input.rs')])
        self.assertFalse(observed['input_closure_complete'])
        with self.assertRaises(self.inputs.FreshnessHold):
            census.observe([self.query(b'unknown',outcome='failure',errno='ENOENT')])


if __name__=='__main__':unittest.main()
