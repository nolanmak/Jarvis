#!/usr/bin/env python3
"""Behavior fixtures never run actual CI builds or mutate host packages."""
import argparse,copy,hashlib,shutil,importlib.util,json,os,subprocess,sys,tempfile,unittest
from pathlib import Path
from unittest.mock import patch
ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('shared_ci',ROOT/'scripts/ci-contract.py');ci=importlib.util.module_from_spec(spec);spec.loader.exec_module(ci)
class ContractTests(unittest.TestCase):
 def fixture(self,root,body='printf ok',matrix=None):
  workflows=root/'.github/workflows';workflows.mkdir(parents=True);source=workflows/'test.yml';source.write_text('fixture')
  job={'runs-on':'${{ matrix.os }}' if matrix else 'ubuntu-latest','steps':[{'command_id':'test/check/0'}]}
  if matrix:job['strategy']={'matrix':{'os':matrix}}
  return {'version':1,'commands':{'test/check/0':{'body':body,'shell':'bash'}},'workflows':{'.github/workflows/test.yml':{'role':'validation','sha256':hashlib.sha256(b'fixture').hexdigest(),'events':{'pull_request':{'paths':['src/**']}},'env':{},'jobs':{'check':job}}}}
 def args(self,root,**changes):
  return argparse.Namespace(**dict({'root':str(root),'head':'head','base':'base','platform':'linux','changed_file':['src/a.rs'],'probe_prerequisites':False},**changes))
 def test_vm_provisioning_is_exact_and_does_not_skip_unrelated_tests(self):
  c=json.loads((ROOT/'scripts/ci-contract.json').read_text())
  with patch.object(ci,'prerequisite_check',return_value=[]):
   plan=ci.plan(c,self.args(ROOT,changed_file=['scripts/ci-contract.json'],probe_prerequisites=True,vm_runtime=True))
  rows={row['command_id']:row for row in plan['checks'] if row.get('platform')=='linux' and 'command_id' in row}
  for identifier in ['browser-sidecar/synthetic-page/2','browser-sidecar/synthetic-page/3','rust/pdf/5','rust/pdf/6','rust/pdf/10','rust/pdf/14','model-router/adapter-compose/1','model-router/pinned-gateway/2','model-router/codex-redirect/2','model-router/codex-redirect/4','computer-use/browser/6','platform/platform/36','privacy/privacy/4']:
   self.assertEqual(rows[identifier]['status'],'remote_required',identifier)
  for identifier in ['rust/pdf/7','rust/pdf/8','rust/pdf/9','model-router/codex-redirect/3','model-router/installer/3','computer-use/browser/8']:
   self.assertEqual(rows[identifier]['status'],'ready',identifier)
  with self.assertRaisesRegex(ValueError,'unsupported CI provisioning'):ci.vm_provisioning({'vm_provisioning':['invented-exemption']})
 def test_vm_step_refuses_unprovisioned_dependency_before_execution(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root,body='touch must-not-run');c['commands']['test/check/0']['vm_provisioning']=['pdf-python-runtime']
   result=subprocess.run([sys.executable,str(ROOT/'scripts/ci-contract.py'),'--contract-json',json.dumps(c),'step','--vm-runtime','--id','test/check/0'],cwd=root,capture_output=True,text=True)
   self.assertEqual(result.returncode,2);self.assertIn('shared CI prerequisite unavailable',result.stderr);self.assertFalse((root/'must-not-run').exists())
 def test_validation_concurrency_rejects_omission_and_push_cancellation(self):
  syncspec=importlib.util.spec_from_file_location('sync_concurrency',ROOT/'scripts/ci-contract-sync.py');sync=importlib.util.module_from_spec(syncspec);syncspec.loader.exec_module(sync)
  import yaml
  contract=json.loads((ROOT/'scripts/ci-contract.json').read_text())
  for path,row in contract['workflows'].items():
   if row['role']=='validation':sync.validate_concurrency(Path(path).name,yaml.safe_load((ROOT/path).read_text()))
  for policy in [None,{'group':'${{ github.workflow }}-${{ github.ref }}','cancel-in-progress':True},
                 {**sync.PR_CONCURRENCY,'group':'${{ github.workflow }}-${{ github.ref }}'},
                 {**sync.PR_CONCURRENCY,'cancel-in-progress':True}]:
   with self.assertRaisesRegex(ValueError,'PR concurrency policy'):sync.validate_concurrency('future-validation.yaml',{'concurrency':policy})
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);shutil.copytree(ROOT/'.github/workflows',root/'.github/workflows');(root/'scripts').mkdir()
   shutil.copyfile(ROOT/'scripts/ci-contract.json',root/'scripts/ci-contract.json')
   (root/'.github/workflows/future-validation.yaml').write_text('name: Future\non:\n  pull_request:\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n')
   with self.assertRaisesRegex(ValueError,'PR concurrency policy'):sync.generate(root)
 def test_inventory_all_workflows_and_command_wrappers_match(self):
  c=json.loads((ROOT/'scripts/ci-contract.json').read_text());self.assertEqual(ci.verify(c,ROOT),[])
  self.assertEqual(len(c['workflows']),len(list((ROOT/'.github/workflows').glob('*.yml'))))
  for path,w in c['workflows'].items():
   if w['role']=='validation':
    raw=(ROOT/path).read_text()
    for j in w['jobs'].values():
     for s in j['steps']:
      if 'command_id'in s:self.assertIn('--id '+s['command_id'],raw);self.assertIn(s['command_id'],c['commands'])
 def test_generated_inventory_is_reproducible_and_new_workflow_fails_closed(self):
  syncspec=importlib.util.spec_from_file_location('sync',ROOT/'scripts/ci-contract-sync.py');sync=importlib.util.module_from_spec(syncspec);syncspec.loader.exec_module(sync)
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);shutil.copytree(ROOT/'.github/workflows',root/'.github/workflows');(root/'scripts').mkdir()
   shutil.copyfile(ROOT/'scripts/ci-contract.json',root/'scripts/ci-contract.json')
   before=(root/'scripts/ci-contract.json').read_bytes();sync.generate(root)
   self.assertEqual((root/'scripts/ci-contract.json').read_bytes(),before)
   (root/'.github/workflows/undeclared.yml').write_text('on: workflow_dispatch\njobs: {}\n')
   with self.assertRaisesRegex(ValueError,'unclassified workflow'):sync.generate(root)
 def test_paths_matrix_and_unsupported_platform_do_not_claim_pass(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root,matrix=['ubuntu-latest','macos-latest']);p=ci.plan(c,self.args(root))
   self.assertEqual([x['status'] for x in p['checks']],['ready','remote_required']);self.assertTrue(p['remote_pending'])
   self.assertEqual(len(set(x['id'] for x in p['checks'])),2)
   self.assertTrue(all(x['status']=='not_applicable' for x in ci.plan(c,self.args(root,changed_file=['README.md']))['checks']))
 def test_changed_workflow_needs_external_authority_but_retains_local_checks(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root);(root/'.github/workflows/new.yml').write_text('new')
   checks=ci.plan(c,self.args(root))['checks'];self.assertEqual(checks[0]['status'],'remote_required');self.assertEqual(checks[1]['status'],'ready')
 def test_missing_version_and_setup_are_never_green_or_executed(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root);c['workflows']['.github/workflows/test.yml']['jobs']['check']['steps'].insert(0,{'uses':'actions/setup-node@pinned','with':{'node-version':'22'}})
   with patch.object(ci.subprocess,'run',return_value=subprocess.CompletedProcess([],0,'v18.0.0','')):
    self.assertEqual(ci.plan(c,self.args(root,probe_prerequisites=True))['checks'][0]['status'],'prerequisite_hold')
   c=self.fixture_body(c,'sudo apt-get install -y private-package')
   self.assertEqual(ci.plan(c,self.args(root))['checks'][0]['status'],'remote_required')
 def fixture_body(self,c,body):c['commands']['test/check/0']['body']=body;return c
 def test_shared_step_executes_same_body_env_and_directory(self):
  with tempfile.TemporaryDirectory() as state, tempfile.TemporaryDirectory() as d:
   root=Path(d);(root/'sub').mkdir();c=self.fixture(root,'test "$DECLARED" = yes; test "${PWD##*/}" = sub; printf exact')
   job=c['workflows']['.github/workflows/test.yml']['jobs']['check'];job['env']={'DECLARED':'yes'};job['steps'][0]['working-directory']='sub'
   done=subprocess.run([sys.executable,'-I',str(ROOT/'scripts/ci-contract.py'),'--contract-json',json.dumps(c),'step','--id','test/check/0','--state-dir',state],cwd=root,capture_output=True,text=True)
   self.assertEqual(done.returncode,0,done.stderr);self.assertEqual(done.stdout,'exact')
   c['commands']['test/check/0']['body']='exit 23'
   failed=subprocess.run([sys.executable,'-I',str(ROOT/'scripts/ci-contract.py'),'--contract-json',json.dumps(c),'step','--id','test/check/0','--state-dir',state],cwd=root,capture_output=True,text=True)
   self.assertEqual(failed.returncode,23)
 def test_raw_contract_bytes_bind_plan(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root);raw=json.dumps(c,indent=2)
   out=subprocess.check_output([sys.executable,'-I',str(ROOT/'scripts/ci-contract.py'),'--contract-json',raw,'plan','--head','h','--base','b','--root',str(root)],text=True)
   self.assertEqual(json.loads(out)['contract_sha256'],hashlib.sha256(raw.encode()).hexdigest())
 def test_embedded_controller_runner_needs_no_checkout_script(self):
  with tempfile.TemporaryDirectory() as state, tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root);source=(ROOT/'scripts/ci-contract.py').read_text()
   done=subprocess.run([sys.executable,'-I','-c',source,'--contract-json',json.dumps(c),'step','--id','test/check/0','--state-dir',state],cwd=root,capture_output=True,text=True)
   self.assertEqual(done.returncode,0,done.stderr);self.assertEqual(done.stdout,'ok')
 def test_github_globs_match_root_nested_and_matrix_include_refuses(self):
  self.assertTrue(ci.match_path('Cargo.toml','**/Cargo.toml'))
  self.assertTrue(ci.match_path('a/b/Cargo.toml','**/Cargo.toml'))
  self.assertFalse(ci.match_path('a/b/Cargo.toml','*/Cargo.toml'))
  with self.assertRaisesRegex(ValueError,'matrix include'):ci.matrices({'strategy':{'matrix':{'os':['ubuntu-latest'],'include':[{'os':'ubuntu-latest','extra':1}]}}})
 def test_yaml_workflow_cannot_escape_inventory(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root);(root/'.github/workflows/new.yaml').write_text('new')
   self.assertEqual(ci.verify(c,root),['.github/workflows/new.yaml'])
 def test_host_browser_install_never_runs_locally_and_foreign_condition_is_preserved(self):
  self.assertTrue(ci.host_setup('npx playwright install --with-deps chrome'))
  self.assertTrue(ci.host_setup('npx playwright install-deps'))
  requirements=ci.tool_prerequisites([{'if':"runner.os == 'macOS'",'_body':'brew install x'}],'linux')
  self.assertEqual(requirements,[])
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root,matrix=['ubuntu-latest','macos-latest'])
   c['workflows']['.github/workflows/test.yml']['jobs']['check']['steps'][0]['if']="runner.os == 'macOS'"
   self.assertEqual([x['status'] for x in ci.plan(c,self.args(root))['checks']],['not_applicable','remote_required'])
 def test_all_jobs_share_private_runtime_across_steps_but_not_heads(self):
  with tempfile.TemporaryDirectory() as state, tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root,'printf marker > "$RUNNER_TEMP/marker"')
   job=c['workflows']['.github/workflows/test.yml']['jobs']['check'];job['steps'].append({'command_id':'test/check/1'})
   c['commands']['test/check/1']={'body':'test "$(cat "$RUNNER_TEMP/marker")" = marker','shell':'bash'}
   def call(index,head):return subprocess.run([sys.executable,'-I',str(ROOT/'scripts/ci-contract.py'),'--contract-json',json.dumps(c),'step','--id','test/check/'+str(index),'--state-dir',state,'--input-head',head,'--input-base','base'],cwd=root,capture_output=True,text=True)
   self.assertEqual(call(0,'head').returncode,0);self.assertEqual(call(1,'head').returncode,0)
   self.assertNotEqual(call(1,'new-head').returncode,0)
 def test_github_declared_and_default_deadlines_are_explicit(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root);job=c['workflows']['.github/workflows/test.yml']['jobs']['check']
   row=ci.plan(c,self.args(root))['checks'][0];self.assertEqual(row['job_timeout_seconds'],21600);self.assertIsNone(row['step_timeout_seconds'])
   job['timeout-minutes']=45;job['steps'][0]['timeout-minutes']=7
   row=ci.plan(c,self.args(root))['checks'][0];self.assertEqual(row['job_timeout_seconds'],2700);self.assertEqual(row['step_timeout_seconds'],420)
   job['timeout-minutes']=False
   with self.assertRaisesRegex(ValueError,'timeout'):ci.plan(c,self.args(root))
 def test_unknown_expression_or_action_is_not_silent_pass(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);c=self.fixture(root);job=c['workflows']['.github/workflows/test.yml']['jobs']['check'];job['env']={'X':'${{ secrets.UNDECLARED }}'}
   self.assertEqual(ci.plan(c,self.args(root))['checks'][0]['status'],'remote_required')
if __name__=='__main__':unittest.main()
