"""Real filesystem retirement mechanics; original privileged R proof is separate."""
import copy
import errno
import hashlib
import json
import os
from pathlib import Path
import unittest
import uuid
from unittest.mock import patch
import ci_source_projection_test as fixtures
cache=fixtures.cache


class ExecutionRetirementTests(unittest.TestCase):
    setUp=fixtures.SourceProjectionTests.setUp
    git=fixtures.SourceProjectionTests.git
    project=fixtures.SourceProjectionTests.project

    def closed(self):
        self.project();self.generation=self.target.parent
        (self.generation/'generated-forests').mkdir(mode=0o700)
        self.completed=self.generation/'completed-ci-commands';self.completed.mkdir(mode=0o700)
        scope={'id':str(uuid.uuid4()),'receipt':str(self.root/'old-worker/cleanup'),
               'cleanup_sha256':hashlib.sha256(b'all-descendants-reaped\n').hexdigest()}
        self.completion={'version':1,'binding':self.binding,'command':self.command,'claim':str(uuid.uuid4()),
            'predecessor':None,'scope':scope,'forests':[],'input_closure_complete':False,'shipping_authority':False}
        self.state={'version':2,'binding':self.binding,'command':self.command,'claim':self.completion['claim'],
            'predecessor':None,'phase':'closed','scope':scope,'root':None,'checkpoint':None,'forests':[],
            'completion_sha256':self.digest(self.completion),'input_closure_complete':False,'shipping_authority':False}
        self.save();(self.target/'artifact').write_bytes(b'owned derived bytes')

    @staticmethod
    def digest(value):return hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':')).encode()).hexdigest()

    def save(self):
        cache._write_record(self.generation/'cargo-execution-state.json',self.state)
        cache._write_record(self.completed/(self.command+'.json'),self.completion)

    def reclaim(self,protected=()):return cache.reclaim(self.image,list(protected),os.getuid(),os.getgid())

    def abandoned(self):
        self.closed();(self.completed/(self.command+'.json')).unlink();self.completed.rmdir()
        prior={**self.state,'phase':'active','scope':{**self.state['scope'],'cleanup_sha256':None},'completion_sha256':None}
        self.disposition={'version':1,'kind':'ci-execution-abandoned',
            **{key:prior[key] for key in ('binding','command','claim','predecessor')},
            'scope':self.state['scope'],'state_sha256':self.digest(prior),
            'input_closure_complete':False,'shipping_authority':False}
        self.dispositions=self.generation/'execution-dispositions';self.dispositions.mkdir(mode=0o700)
        cache._write_record(self.dispositions/(self.command+'.json'),self.disposition)
        self.state={**prior,'version':3,'phase':'abandoned','scope':self.disposition['scope'],
            'abandonment_sha256':self.digest(self.disposition)}
        cache._write_record(self.generation/'cargo-execution-state.json',self.state)

    def test_verified_abandonment_retires_without_any_successful_completion(self):
        self.abandoned();self.assertEqual(self.reclaim([self.binding['generation']]),[])
        self.assertEqual(self.reclaim(),[self.binding['generation']]);self.assertFalse(self.generation.exists())

    def test_abandonment_replay_preserves_original_disposition_after_partial_removal(self):
        self.abandoned()
        def interrupted(path):
            (self.generation/'cargo-execution-state.json').unlink()
            (self.dispositions/(self.command+'.json')).unlink()
            raise OSError(errno.EIO,'disposition already removed')
        with patch.object(cache.shutil,'rmtree',side_effect=interrupted),self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertEqual(self.reclaim(),[self.binding['generation']])

    def test_foreign_or_modified_abandonment_is_never_retired(self):
        self.abandoned()
        for field in ('claim','state_sha256'):
            changed={**self.disposition,field:str(uuid.uuid4()) if field=='claim' else 'f'*64}
            cache._write_record(self.dispositions/(self.command+'.json'),changed)
            cache._write_record(self.generation/'cargo-execution-state.json',
                {**self.state,'abandonment_sha256':self.digest(changed)})
            self.assertEqual(self.reclaim(),[]);self.assertTrue(self.generation.exists())

    def test_closed_execution_retires_only_after_matching_cleanup_and_completion(self):
        self.closed();self.assertEqual(self.reclaim([self.binding['generation']]),[])
        self.assertEqual(self.reclaim(),[self.binding['generation']]);self.assertFalse(self.generation.exists())
        receipt=cache._read_record(self.image/'ci-generation-retirements'/(self.binding['generation']+'.json'))
        self.assertEqual(receipt['version'],2);self.assertEqual(receipt['state'],'reclaimed')
        self.assertEqual(receipt['original']['binding'],self.binding)
        self.assertTrue((self.source/'src/main.rs').is_file())

    def test_active_unknown_and_foreign_completion_evidence_never_authorize_retirement(self):
        self.closed();original=copy.deepcopy(self.state);completion=copy.deepcopy(self.completion)
        for fault in ('active','legacy','claim','cleanup','digest','command','missing','extra'):
            with self.subTest(fault=fault):
                self.state=copy.deepcopy(original);self.completion=copy.deepcopy(completion)
                if fault=='active':self.state['phase']='active'
                elif fault=='legacy':self.state['version']=1
                elif fault=='claim':self.state['claim']=str(uuid.uuid4())
                elif fault=='cleanup':self.state['scope']['cleanup_sha256']=None
                elif fault=='digest':self.state['completion_sha256']='a'*64
                elif fault=='command':self.state['command']=str(uuid.uuid4())
                self.save()
                if fault=='missing':(self.completed/(self.command+'.json')).unlink()
                if fault=='extra':(self.generation/'unknown').write_bytes(b'keep')
                self.assertEqual(self.reclaim(),[])
                self.assertEqual((self.target/'artifact').read_bytes(),b'owned derived bytes')
                if fault=='extra':(self.generation/'unknown').unlink()

    def test_partial_deletion_replays_without_recreating_or_adopting_missing_state(self):
        self.closed()
        def interrupted(path):
            (self.generation/'cargo-execution-state.json').unlink()
            (self.completed/(self.command+'.json')).unlink()
            raise OSError(errno.EIO,'after original metadata removal')
        with patch.object(cache.shutil,'rmtree',side_effect=interrupted),self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertEqual(self.reclaim(),[self.binding['generation']])
        self.assertTrue((self.source/'src/main.rs').is_file())

    def test_changed_completion_after_retirement_intent_is_not_resampled(self):
        self.closed()
        with patch.object(cache.shutil,'rmtree',side_effect=OSError(errno.EIO,'before deletion')),self.assertRaises(cache.FreshnessHold):
            self.reclaim()
        self.completion['claim']=str(uuid.uuid4());self.save()
        self.assertEqual(self.reclaim(),[]);self.assertTrue(self.generation.exists())

    def test_lost_final_ack_replays_only_original_journal(self):
        self.closed();write=cache._write_record
        def lost_ack(path,value):
            if path.parent.name=='ci-generation-retirements' and value.get('state')=='reclaimed':
                raise OSError(errno.EIO,'completion ack lost')
            return write(path,value)
        with patch.object(cache,'_write_record',side_effect=lost_ack),self.assertRaises(cache.FreshnessHold):self.reclaim()
        self.assertFalse(self.generation.exists());self.assertEqual(self.reclaim(),[self.binding['generation']])

    def test_unknown_or_cyclic_completion_history_is_preserved(self):
        self.closed()
        for predecessor in (str(uuid.uuid4()),self.command):
            self.completion['predecessor']=predecessor;self.state['predecessor']=predecessor
            self.state['completion_sha256']=self.digest(self.completion);self.save()
            self.assertEqual(self.reclaim(),[]);self.assertTrue(self.generation.exists())

    def test_corrupt_saved_evidence_cannot_be_rebuilt_from_current_files(self):
        self.closed()
        with patch.object(cache.shutil,'rmtree',side_effect=OSError(errno.EIO,'before deletion')),self.assertRaises(cache.FreshnessHold):
            self.reclaim()
        receipt=cache._read_record(self.image/'ci-generation-retirements'/(self.binding['generation']+'.json'))
        evidence=self.image/'ci-generation-retirement-evidence'/(receipt['evidence']['execution']+'.json')
        value=cache._read_record(evidence,max_bytes=16*1024**2);value['value']['claim']=str(uuid.uuid4())
        cache._write_record(evidence,value)
        self.assertEqual(self.reclaim(),[]);self.assertTrue(self.generation.exists())

    def test_lost_master_publication_preserves_data_and_reuses_only_exact_evidence(self):
        self.closed();write=cache._write_record
        def failed_publish(path,value):
            if path.parent.name=='ci-generation-retirements':raise OSError(errno.EIO,'master not published')
            return write(path,value)
        with patch.object(cache,'_write_record',side_effect=failed_publish),self.assertRaises(OSError):self.reclaim()
        self.assertEqual((self.target/'artifact').read_bytes(),b'owned derived bytes')
        self.assertEqual(self.reclaim(),[self.binding['generation']])

    def test_large_projection_evidence_keeps_existing_record_limits(self):
        for index in range(120):(self.source/('extra-'+str(index)+'.rs')).write_text('source '+str(index))
        self.git('add','.');self.git('commit','-qm','large source')
        self.binding.update(head=self.git('rev-parse','HEAD'),tree=self.git('rev-parse','HEAD^{tree}'),
            source_inputs_sha256=cache.source_census(self.source,self.objects,self.git('rev-parse','HEAD')))
        cache._write_record(self.target.parent/'control.json',{'version':1,'binding':self.binding,'active_command':self.command})
        self.closed();projection=cache._read_record(self.generation/'source-projection.json',max_bytes=16*1024**2)
        self.assertGreater((self.generation/'source-projection.json').stat().st_size,8192)
        self.assertEqual(self.reclaim(),[self.binding['generation']])
        master=cache._read_record(self.image/'ci-generation-retirements'/(self.binding['generation']+'.json'))
        evidence=cache._read_record(self.image/'ci-generation-retirement-evidence'/(master['evidence']['projection']+'.json'),
                                   max_bytes=16*1024**2)
        self.assertEqual(evidence['value'],projection)


if __name__=='__main__':unittest.main()
