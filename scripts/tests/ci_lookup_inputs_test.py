"""Selective input identity from captured pathname queries and content censuses."""
import importlib.util
import json
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
    def journal_binding(self):
        return {'generation':'12345678-1234-4234-8234-123456789abc','head':'a'*40,'tree':'b'*40,
            **{key:'c'*64 for key in ('contract_sha256','authority_sha256','cache_identity_sha256','source_inputs_sha256')}}

    def test_changed_unit_marks_dependents_durably_without_claiming_revalidation(self):
        root=self.root/'journal';root.mkdir(mode=0o700)
        binding=self.journal_binding()
        journal=self.inputs.UnitInvalidations(root/'units.json',binding=binding,command='original-command')
        journal.register('library',dependencies=[],content_sha256='1'*64,fingerprints=['debug/.fingerprint/example-0123456789abcdef'])
        journal.register('binary',dependencies=['library'],content_sha256='2'*64,fingerprints=['debug/.fingerprint/example-fedcba9876543210'])
        journal.register('unrelated',dependencies=[],content_sha256='3'*64,fingerprints=['debug/.fingerprint/other-0123456789abcdef'])
        self.assertEqual(journal.changed({'library':'1'*64}),[])
        self.assertEqual(journal.changed({'library':'4'*64}),['binary','library'])
        restored=self.inputs.UnitInvalidations(root/'units.json',binding=binding,command='original-command')
        self.assertEqual(restored.pending(),['binary','library'])
        self.assertEqual(restored.changed({'library':'1'*64}),['binary','library'])
        self.assertFalse(restored.state()['shipping_authority'])
        with self.assertRaises(self.inputs.FreshnessHold):
            self.inputs.UnitInvalidations(root/'units.json',binding=binding,command='different-command')

    def test_invalidation_journal_refuses_unknown_graph_paths_and_stale_writers(self):
        root=self.root/'journal';root.mkdir(mode=0o700);path=root/'units.json'
        args={'binding':self.journal_binding(),'command':'original-command'}
        journal=self.inputs.UnitInvalidations(path,**args)
        stale=self.inputs.UnitInvalidations(path,**args)
        journal.register('library',dependencies=[],content_sha256='1'*64,fingerprints=['debug/.fingerprint/library-abcd'])
        committed=path.read_bytes()
        for name,deps,paths in (
                ('unknown',['missing'],['debug/.fingerprint/unknown-abcd']),
                ('self',['self'],['debug/.fingerprint/self-abcd']),
                ('escape',[],['debug/.fingerprint/../../source']),
                ('alias',[],['debug/.fingerprint/library-abcd'])):
            with self.subTest(name=name),self.assertRaises(self.inputs.FreshnessHold):
                journal.register(name,dependencies=deps,content_sha256='2'*64,fingerprints=paths)
            self.assertEqual(path.read_bytes(),committed)
        with self.assertRaises(self.inputs.FreshnessHold):stale.changed({})
        with self.assertRaises(self.inputs.FreshnessHold):journal.changed({'missing':'2'*64})
        detached=journal.state();detached['units'].clear()
        self.assertEqual(journal.changed({'library':'2'*64}),['library'])
        journal.register('later',dependencies=['library'],content_sha256='3'*64,fingerprints=['debug/.fingerprint/later-abcd'])
        self.assertEqual(journal.pending(),['later','library'])
        self.assertEqual(path.stat().st_mode&0o777,0o600)

    def test_invalidation_journal_refuses_corrupt_rebound_and_unprotected_records(self):
        root=self.root/'journal';root.mkdir(mode=0o700);path=root/'units.json'
        args={'binding':self.journal_binding(),'command':'original-command'}
        journal=self.inputs.UnitInvalidations(path,**args)
        journal.register('library',dependencies=[],content_sha256='1'*64,fingerprints=['debug/.fingerprint/library-abcd'])
        original=path.read_bytes()
        with self.assertRaises(self.inputs.FreshnessHold):
            self.inputs.UnitInvalidations(path,**(args|{'binding':args['binding']|{'head':'d'*40}}))
        for fault in ('torn','cycle','authority','public','symlink','public-parent'):
            path.write_bytes(original);path.chmod(0o600);root.chmod(0o700)
            if fault=='torn':path.write_bytes(b'{')
            elif fault in ('cycle','authority'):
                value=json.loads(original)
                if fault=='cycle':value['units']['library']['dependencies']=['library']
                else:value['shipping_authority']=True
                path.write_text(json.dumps(value))
            elif fault=='public':path.chmod(0o644)
            elif fault=='public-parent':root.chmod(0o755)
            else:
                backing=root/'backing';path.rename(backing);path.symlink_to(backing)
            with self.subTest(fault=fault),self.assertRaises(self.inputs.FreshnessHold):
                self.inputs.UnitInvalidations(path,**args)
            if path.is_symlink():path.unlink()

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

    def test_frozen_kernel_base_selects_negative_content_and_refreshes_only_relevant_paths(self):
        views=load('ci_cargo_views')
        def tree(name,*,introduced=False,unread=b'one'):
            root=self.root/name;root.mkdir();(root/'src').mkdir()
            (root/'src/lib.rs').write_bytes(b'old');(root/'unread').write_bytes(unread)
            if introduced:(root/'src/main.rs').write_bytes(b'new')
            for path in root.rglob('*'):path.chmod(0o555 if path.is_dir() else 0o444)
            root.chmod(0o555);return root
        original=tree('original');unrelated=tree('unrelated',unread=b'two');changed=tree('changed',introduced=True)
        def index(root):
            return self.inputs.FrozenLookupIndex({'generated':('/workspace/target/deps',root,
                views.inventory(root,owner=os.geteuid(),sealed=True))},owner=os.geteuid())
        info=original.stat();base={'device':(os.major(info.st_dev)<<20)|os.minor(info.st_dev),
            'inode':info.st_ino,'mode':info.st_mode,'flags':256,'path_seq':2,'dentry_seq':2}
        event={'uid':2004,'pid':101,'tid':101,'syscall':262,'sequence':1,'path':b'src/main.rs','result':-2,
            'lookup_attempts':[{'dfd':-100,'user_namespace':100,'mount_namespace':50,'init_result':0,'base':base}]}
        metadata={'uid':2004,'pid':101,'tid':101,'syscall':262,'sequence':1,'path':b'src/main.rs',
            'fd':-100,'flags':256,'mask':0,'user_namespace':100,'result':-2,'stat':None,'copy_result':None}
        selected=index(original).select([event],[metadata],pid=101,uid=2004,user_namespace=100,mount_namespace=50)
        self.assertEqual(selected.observation['sha256'],selected.refresh(index(unrelated))['sha256'])
        self.assertNotEqual(selected.observation['sha256'],selected.refresh(index(changed))['sha256'])
        self.assertFalse(selected.observation['input_closure_complete']);self.assertFalse(selected.observation['shipping_authority'])
        for fault in ('base','namespace','outcome','flags','duplicate'):
            import copy
            e=copy.deepcopy(event);m=copy.deepcopy(metadata)
            if fault=='base':e['lookup_attempts'][0]['base']['inode']+=1<<48
            elif fault=='namespace':e['lookup_attempts'][0]['mount_namespace']+=1
            elif fault=='outcome':m['result']=0
            elif fault=='flags':m['flags']=4096
            with self.subTest(fault=fault),self.assertRaises(self.inputs.FreshnessHold):
                index(original).select([e],[m,m] if fault=='duplicate' else [m],pid=101,uid=2004,user_namespace=100,mount_namespace=50)

    def test_absolute_compiler_lookup_requires_original_kernel_root(self):
        views=load('ci_cargo_views');root=self.root/'generated';root.mkdir()
        (root/'mod.rs').write_bytes(b'mod chosen;');(root/'mod.rs').chmod(0o444);root.chmod(0o555)
        prefix='/workspace/target/debug/build/probe/out'
        namespaces={'generated':(prefix,root,views.inventory(root,owner=os.geteuid(),sealed=True))}
        original={'device':12,'inode':34,'mode':0o40755}
        event={'uid':2005,'pid':101,'tid':101,'syscall':262,'sequence':1,'path':(prefix+'/chosen.rs').encode(),'result':-2,
            'lookup_attempts':[{'dfd':-100,'user_namespace':100,'mount_namespace':50,'init_result':0,
                'base':dict(original,flags=256,path_seq=2,dentry_seq=2)}]}
        metadata={key:event[key] for key in ('uid','pid','tid','syscall','sequence','path','result')}
        metadata.update(fd=-100,flags=256,mask=0,user_namespace=100,stat=None,copy_result=None)
        def select(index,*,threaded=False):return index.select([event],[metadata],pid=101,uid=2005,
            user_namespace=100,mount_namespace=50,threaded=threaded)
        with self.assertRaises(self.inputs.FreshnessHold):
            select(self.inputs.FrozenLookupIndex(namespaces,owner=os.geteuid()))
        selected=select(self.inputs.FrozenLookupIndex(namespaces,kernel_roots={'/':original},owner=os.geteuid()))
        self.assertFalse(selected.observation['shipping_authority'])
        # rustc resolves modules in a worker thread. Its kernel TGID remains the
        # original supervised PID; namespace and selected base still must match.
        event['tid']=metadata['tid']=103
        with self.assertRaises(self.inputs.FreshnessHold):
            select(self.inputs.FrozenLookupIndex(namespaces,kernel_roots={'/':original},owner=os.geteuid()))
        selected=select(self.inputs.FrozenLookupIndex(namespaces,kernel_roots={'/':original},owner=os.geteuid()),threaded=True)
        self.assertFalse(selected.observation['shipping_authority'])
        for field in ('pid','user_namespace','mount_namespace'):
            if field=='pid':event['pid']=metadata['pid']=102
            else:event['lookup_attempts'][0][field]+=1
            with self.subTest(field=field),self.assertRaises(self.inputs.FreshnessHold):
                select(self.inputs.FrozenLookupIndex(namespaces,kernel_roots={'/':original},owner=os.geteuid()),threaded=True)
            if field=='pid':event['pid']=metadata['pid']=101
            else:event['lookup_attempts'][0][field]-=1
        # A valid root anchor only routes to declared frozen trees.
        event['path']=metadata['path']=b'/etc/unknown'
        with self.assertRaises(self.inputs.FreshnessHold):
            select(self.inputs.FrozenLookupIndex(namespaces,kernel_roots={'/':original},owner=os.geteuid()),threaded=True)
        for fault in (dict(original,inode=True),dict(original,mode=0o100444),dict(original,extra=1)):
            with self.subTest(fault=fault),self.assertRaises(self.inputs.FreshnessHold):
                self.inputs.FrozenLookupIndex(namespaces,kernel_roots={'/':fault},owner=os.geteuid())

    def test_frozen_index_keeps_binary_names_detached_records_and_refuses_aliases(self):
        views=load('ci_cargo_views');root=self.root/'tree';root.mkdir()
        raw=root/os.fsdecode(b'raw-\xff');raw.write_bytes(b'content');raw.chmod(0o444);root.chmod(0o555)
        rows=views.inventory(root,owner=os.geteuid(),sealed=True)
        index=self.inputs.FrozenLookupIndex({'tree':('/workspace/tree',root,rows)},owner=os.geteuid())
        rows.clear()  # Caller mutation does not replace the captured inventory.
        info=root.stat();item=raw.stat()
        event={'uid':2004,'pid':101,'tid':101,'syscall':262,'sequence':1,'path':b'raw-\xff','result':0,
            'lookup_attempts':[{'dfd':3,'user_namespace':100,'mount_namespace':50,'init_result':0,
                'base':{'device':(os.major(info.st_dev)<<20)|os.minor(info.st_dev),'inode':info.st_ino,
                    'mode':info.st_mode,'flags':256,'path_seq':2,'dentry_seq':2}}]}
        metadata={'uid':2004,'pid':101,'tid':101,'syscall':262,'sequence':1,'path':b'raw-\xff','fd':3,
            'flags':256,'mask':0,'user_namespace':100,'result':0,'copy_result':0,
            'stat':{'device':(os.major(item.st_dev)<<20)|os.minor(item.st_dev),'inode':item.st_ino,'mode':item.st_mode}}
        selected=index.select([event],[metadata],pid=101,uid=2004,user_namespace=100,mount_namespace=50)
        observed=selected.observation;observed['facts'].clear()
        self.assertTrue(selected.observation['facts']);self.assertEqual(len(selected.binding_sha256),64)
        with self.assertRaises(self.inputs.FreshnessHold):
            self.inputs.FrozenLookupIndex({'tree':('/workspace/tree',root,views.inventory(root,owner=os.geteuid(),sealed=True))},
                anchors={'/different':root},owner=os.geteuid())

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
