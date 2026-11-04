#!/usr/bin/env python3
"""Hosted CI policy tests; no credentials, builds, or network."""
import base64, copy, hashlib, importlib.util, json, subprocess, unittest
from unittest.mock import patch
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('hosted',ROOT/'scripts/autoship-hosted-ci.py')
hosted=importlib.util.module_from_spec(spec);spec.loader.exec_module(hosted)
class PolicyTests(unittest.TestCase):
 def contract(self):
  return {'workflows':{'.github/workflows/test.yml':{'role':'validation','events':{'pull_request':None,'push':{'branches':['main']}},'jobs':{'tests':{'name':'tests (${{ matrix.os }})','runs-on':'${{ matrix.os }}','strategy':{'matrix':{'os':['ubuntu-latest','macos-latest']}}},'privacy':{'runs-on':'ubuntu-latest'},'node-release':{'runs-on':'ubuntu-latest'}}}}}
 def test_required_linux_matrix_only(self):
  self.assertEqual(hosted.expected_jobs(self.contract(),['src/a.rs'],'pull_request'),{'.github/workflows/test.yml':{'tests (ubuntu-latest)','privacy','node-release'}})
 def test_paths_select_missing_workflow_before_it_exists_remotely(self):
  c=self.contract();c['workflows']['.github/workflows/test.yml']['events']['pull_request']={'paths':['src/**','!src/docs/**']}
  self.assertTrue(hosted.expected_jobs(c,['src/a.rs'],'pull_request'))
  self.assertFalse(hosted.expected_jobs(c,['src/docs/a.md'],'pull_request'))
 def test_main_only_uses_push_workflows(self):
  c=self.contract();del c['workflows']['.github/workflows/test.yml']['events']['push']
  self.assertFalse(hosted.expected_jobs(c,[],'push'))
 def test_unknown_runner_or_expression_holds(self):
  for runner in ['self-hosted','windows-latest','${{ inputs.runner }}']:
   c=self.contract();c['workflows']['.github/workflows/test.yml']['jobs']['privacy']['runs-on']=runner
   with self.assertRaises(ValueError):hosted.expected_jobs(c,[],'pull_request')
 def test_missing_pending_skipped_cancelled_and_neutral_never_pass(self):
  for status,conclusion in [('queued',None),('in_progress',None),('completed','skipped'),('completed','cancelled'),('completed','neutral')]:
   state,failures=hosted.job_verdict({'tests'},[{'id':1,'name':'tests','status':status,'conclusion':conclusion}])
   self.assertEqual(state,'pending',(status,conclusion))
  self.assertEqual(hosted.job_verdict({'tests'},[])[0],'pending')
 def test_success_and_real_failure(self):
  for conclusion,state in [('success','passed'),('failure','failed'),('timed_out','failed'),('action_required','pending')]:
   self.assertEqual(hosted.job_verdict({'tests'},[{'id':1,'name':'tests','status':'completed','conclusion':conclusion}])[0],state)
 def test_rerun_does_not_reuse_old_success(self):
  jobs=[{'id':1,'name':'tests','status':'completed','conclusion':'success'},{'id':2,'name':'tests','status':'queued','conclusion':None}]
  self.assertEqual(hosted.job_verdict({'tests'},jobs)[0],'pending')
 def test_mac_failure_does_not_gate_linux(self):
  jobs=[{'id':1,'name':'tests','status':'completed','conclusion':'success'},{'id':2,'name':'mac','status':'completed','conclusion':'failure'}]
  self.assertEqual(hosted.job_verdict({'tests'},jobs)[0],'passed')
 def test_run_must_match_repository_head_event_and_workflow(self):
  run={'id':1,'head_sha':'a'*40,'event':'pull_request','path':'.github/workflows/test.yml','repository':{'full_name':'o/r'}}
  self.assertTrue(hosted.run_matches(run,'o/r','a'*40,'pull_request','.github/workflows/test.yml'))
  for key,value in [('head_sha','b'*40),('event','push'),('path','.github/workflows/spoof.yml'),('repository',{'full_name':'evil/r'})]:
   changed=copy.deepcopy(run);changed[key]=value
   self.assertFalse(hosted.run_matches(changed,'o/r','a'*40,'pull_request','.github/workflows/test.yml'))
 def test_repository_identity_is_case_insensitive(self):
  run={'id':1,'head_sha':'a'*40,'event':'pull_request','path':'.github/workflows/test.yml','repository':{'full_name':'NolanMak/Jarvis'}}
  self.assertTrue(hosted.run_matches(run,'nolanmak/jarvis','a'*40,'pull_request','.github/workflows/test.yml'))
 def test_run_path_ref_suffix_is_not_a_workflow_identity(self):
  self.assertFalse(hosted.run_matches({'id':1,'head_sha':'a'*40,'event':'pull_request','path':'.github/workflows/test.yml@evil','repository':{'full_name':'o/r'}},'o/r','a'*40,'pull_request','.github/workflows/test.yml'))
class EvidenceTests(unittest.TestCase):
 def setUp(self):
  self.head='a'*40;self.base='b'*40
  self.path='.github/workflows/test.yml'
  self.contract=PolicyTests().contract()
  self.contract['workflows'][self.path]['sha256']=hashlib.sha256(b'policy').hexdigest()
  self.request={'repository':'o/r','pr':7,'contract':self.contract,'policy_files':{}}
  self.pull={'head':{'sha':self.head},'base':{'sha':self.base,'ref':'main','repo':{'full_name':'o/r'}},'state':'open','changed_files':1}
  self.runs=[{'id':10,'run_attempt':1,'status':'in_progress','head_sha':self.head,'head_branch':'branch','event':'pull_request','path':self.path,'repository':{'full_name':'o/r'},'pull_requests':[{'number':7}]}]
  self.jobs=[{'id':i,'run_id':10,'run_attempt':1,'head_sha':self.head,'name':name,'status':'completed','conclusion':'success'} for i,name in enumerate(['tests (ubuntu-latest)','privacy','node-release'],1)]
  self.required=[{'name':'privacy','state':'SUCCESS'},{'name':'node-release','state':'SUCCESS'}]
  self.after=None;self.policy=b'policy';self.files=[{'filename':'src/a.rs'}]
  self.calls=[]
  fixture=self
  class API:
   binary='fake-gh'
   def api(self,endpoint):
    fixture.calls.append(endpoint)
    if '/pulls/' in endpoint and not endpoint.endswith('/files'):
     n=sum('/pulls/' in e and not e.endswith('/files') for e in fixture.calls)
     return fixture.after if n>1 and fixture.after is not None else fixture.pull
    if '/contents/' in endpoint:return {'type':'file','encoding':'base64','content':base64.b64encode(fixture.policy).decode()}
    if endpoint.endswith('/branches/main'):return {'commit':{'sha':fixture.head}}
    raise AssertionError(endpoint)
   def pages(self,endpoint,key=None):
    fixture.calls.append(endpoint)
    if endpoint.endswith('/files'):return fixture.files
    if '/commits/' in endpoint:return fixture.files
    if '/workflows/' in endpoint:return fixture.runs
    if '/jobs?' in endpoint:return fixture.jobs
    raise AssertionError(endpoint)
  self.api=API()
 def evaluate(self):
  with patch.object(hosted.subprocess,'run',return_value=subprocess.CompletedProcess([],0,json.dumps(self.required),'')):
   return hosted.evaluate(self.request,self.api)
 def test_complete_current_head_passes(self):
  evidence=self.evaluate();self.assertEqual(evidence['state'],'passed');self.assertEqual(evidence['head'],self.head)
  self.assertEqual(len(evidence['receipts']),1)
 def test_absent_workflow_or_job_holds(self):
  self.runs=[];self.assertEqual(self.evaluate()['state'],'pending')
  self.setUp();self.jobs.pop();self.assertEqual(self.evaluate()['state'],'pending')
 def test_wrong_pr_or_head_never_adopts_green(self):
  for field,value in [('head_sha','c'*40),('pull_requests',[{'number':8}])]:
   self.setUp();self.runs[0][field]=value;self.assertEqual(self.evaluate()['state'],'pending')
 def test_newest_run_pending_supersedes_old_green(self):
  self.runs.append({**self.runs[0],'id':11});self.jobs=[]
  self.assertEqual(self.evaluate()['state'],'pending')
 def test_failure_retains_job_id_for_repair_logs(self):
  self.jobs[0]['conclusion']='failure'
  evidence=self.evaluate();self.assertEqual(evidence['state'],'failed');self.assertEqual(evidence['failures'][0]['job'],1)
 def test_explicitly_required_macos_still_blocks(self):
  self.required.append({'name':'platform (macos-latest)','state':'PENDING'})
  self.assertEqual(self.evaluate()['state'],'pending')
 def test_head_or_base_change_during_observation_holds(self):
  for side in ['head','base']:
   self.setUp();self.after=copy.deepcopy(self.pull);self.after[side]['sha']='c'*40
   with self.assertRaisesRegex(ValueError,'moved'):self.evaluate()
 def test_policy_changes_in_either_merge_parent_hold(self):
  self.policy=b'weakened'
  with self.assertRaisesRegex(ValueError,'policy changed'):self.evaluate()
 def test_truncated_changed_paths_hold(self):
  self.pull['changed_files']=2
  with self.assertRaisesRegex(ValueError,'incomplete'):self.evaluate()
 def test_renamed_old_path_is_in_applicability(self):
  self.files=[{'filename':'docs/a.md','previous_filename':'src/a.rs'}]
  self.contract['workflows'][self.path]['events']['pull_request']={'paths':['src/**']}
  self.assertEqual(self.evaluate()['state'],'passed')
 def test_main_uses_exact_push_head_without_local_build(self):
  self.request.update(pr=None,head=self.head)
  self.runs[0].update(event='push',head_branch='main')
  self.assertEqual(self.evaluate()['state'],'passed')
 def test_main_path_filtered_workflow_can_be_absent(self):
  self.request.update(pr=None,head=self.head)
  self.runs[0].update(event='push',head_branch='main')
  c=self.contract['workflows'];c['.github/workflows/conditional.yml']={'sha256':hashlib.sha256(b'policy').hexdigest(),'role':'validation','events':{'push':{'paths':['other/**']}},'jobs':{'extra':{'runs-on':'ubuntu-latest'}}}
  original=self.api.pages
  self.api.pages=lambda endpoint,key=None: [] if '/conditional.yml/' in endpoint else original(endpoint,key)
  self.assertEqual(self.evaluate()['state'],'passed')
 def test_main_ignores_no_failure_from_an_actual_conditional_run(self):
  self.request.update(pr=None,head=self.head)
  self.runs[0].update(event='push',head_branch='main')
  self.contract['workflows'][self.path]['events']['push']['paths']=['other/**']
  # Mandatory checks remain in a separate unconditional workflow.
  self.contract['workflows']['.github/workflows/release.yml']=copy.deepcopy(self.contract['workflows'][self.path])
  self.contract['workflows']['.github/workflows/release.yml']['events']['push']={}
  self.jobs[0]['conclusion']='failure'
  self.assertEqual(self.evaluate()['state'],'failed')
 def test_queued_rerun_cannot_reuse_previous_green_jobs(self):
  self.runs[0].update(run_attempt=2,status='queued')
  self.assertEqual(self.evaluate()['state'],'pending')
 def test_job_head_and_attempt_are_bound(self):
  for key,value in [('head_sha','c'*40),('run_attempt',2)]:
   self.setUp();self.jobs[0][key]=value
   with self.assertRaisesRegex(ValueError,'identity'):self.evaluate()
 def test_mismatched_job_run_holds(self):
  self.jobs[0]['run_id']=99
  with self.assertRaisesRegex(ValueError,'identity'):self.evaluate()

class WorkflowSecurityTests(unittest.TestCase):
 def test_all_pr_and_push_validation_jobs_are_read_only_and_secret_free(self):
  import yaml
  contract=json.loads((ROOT/'scripts/ci-contract.json').read_text())
  for path,row in contract['workflows'].items():
   workflow=yaml.safe_load((ROOT/path).read_text())
   events=workflow.get('on',workflow.get(True,{}))
   if not any(e in events for e in ['pull_request','push','pull_request_target']):continue
   raw=(ROOT/path).read_text()
   self.assertNotRegex(raw,r'\$\{\{\s*secrets(?:\.|\[)',path)
   self.assertNotIn('pull_request_target',events,path)
   self.assertTrue(all(v=='read' or v=='none' for v in workflow.get('permissions',{}).values()),path)
   for job in workflow['jobs'].values():
    self.assertTrue(all(v=='read' or v=='none' for v in job.get('permissions',{}).values()),path)
    for step in job.get('steps',[]):
     if step.get('uses','').startswith('actions/checkout@'):
      self.assertIs(step['with']['persist-credentials'],False,path)
if __name__=='__main__':unittest.main()
