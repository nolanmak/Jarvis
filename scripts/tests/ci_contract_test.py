#!/usr/bin/env python3
"""Behavior fixtures never run actual CI builds or mutate host packages."""
import argparse,copy,hashlib,shutil,importlib.util,json,os,subprocess,sys,tempfile,unittest
from pathlib import Path
from unittest.mock import patch
ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('shared_ci',ROOT/'scripts/ci-contract.py');ci=importlib.util.module_from_spec(spec);spec.loader.exec_module(ci)
class ContractTests(unittest.TestCase):
 def test_autoship_cli_suites_execute_as_separate_cargo_commands(self):
  body=json.loads((ROOT/'scripts/ci-contract.json').read_text())['commands']['rust/cli/6']['body']
  with tempfile.TemporaryDirectory() as directory:
   root=Path(directory);cargo=root/'cargo';calls=root/'calls'
   cargo.write_text('#!'+sys.executable+'\nimport json,os,sys\nwith open(os.environ["CALLS"],"a") as f:f.write(json.dumps(sys.argv[1:])+"\\n")\n');cargo.chmod(0o700)
   done=subprocess.run(['bash','--noprofile','--norc','-ec',body],cwd=ROOT,env={'PATH':str(root)+':/usr/bin:/bin','CALLS':str(calls)},capture_output=True,text=True)
   self.assertEqual(done.returncode,0,done.stderr)
   rows=[json.loads(line) for line in calls.read_text().splitlines()]
   self.assertEqual(len(rows),2,rows)
   self.assertTrue(all(row[:3]==['test','-p','augmentagent-cli'] for row in rows))
   self.assertIn('--bin',rows[0]);self.assertIn('--test-threads=1',rows[0])
   self.assertEqual({rows[1][i+1] for i,a in enumerate(rows[1]) if a=='--test'},
                    {'autoship_commands_cli','autoship_resolution_cli','autoship_submit_cli'})
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
 def test_ci_apt_helper_is_setup_and_only_probed_locally(self):
  body='python3 scripts/ci-apt-install.py poppler-utils python3-yaml'
  self.assertTrue(ci.host_setup(body))
  requirements=ci.tool_prerequisites([{'_body':body}],'linux')
  self.assertEqual(len(requirements),1)
  with patch.object(ci.subprocess,'run',return_value=subprocess.CompletedProcess([],0,'install ok installed','')) as run:
   self.assertEqual(ci.prerequisite_check(requirements,ROOT),[])
   self.assertEqual([call.args[0][-1] for call in run.call_args_list],['poppler-utils','python3-yaml'])
   self.assertTrue(all(call.args[0][0]=='dpkg-query' for call in run.call_args_list))
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
 @unittest.skipUnless(sys.platform=='linux','Linux runner temporary filesystem placement')
 def test_hosted_linux_temporary_files_use_runner_disk_instead_of_system_tmp(self):
  with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as runtime, tempfile.TemporaryDirectory() as state:
   root=Path(d);c=self.fixture(root,'test "$TMPDIR" = "$RUNNER_TEMP"; python3 -c \'import os,tempfile; assert tempfile.gettempdir() == os.environ["RUNNER_TEMP"]\'')
   env={**os.environ,'RUNNER_TEMP':runtime,'TMPDIR':'/tmp'}
   for mode in [['--hosted'],['--state-dir',state]]:
    done=subprocess.run([sys.executable,'-I',str(ROOT/'scripts/ci-contract.py'),'--contract-json',json.dumps(c),'step',*mode,'--id','test/check/0'],cwd=root,env=env,capture_output=True,text=True)
    self.assertEqual(done.returncode,0,done.stderr)
   for invalid in ['', 'relative', str(root/'missing')]:
    env['RUNNER_TEMP']=invalid
    failed=subprocess.run([sys.executable,'-I',str(ROOT/'scripts/ci-contract.py'),'--contract-json',json.dumps(c),'step','--hosted','--id','test/check/0'],cwd=root,env=env,capture_output=True,text=True)
    self.assertEqual(failed.returncode,2,failed.stderr)
    self.assertIn('runner temporary directory',failed.stderr)
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
 def test_macos_jobs_mirror_their_linux_jobs(self):
  c=json.loads((ROOT/'scripts/ci-contract.json').read_text())
  self.assertEqual(mirror_drift(c),[])
  for job in [j for w in c['workflows'].values() if w['role']=='validation' for j in w['jobs'].values()]:
   for step in job['steps']:
    self.assertIn(step.get('if'),ALLOWED_STEP_IF,step)
 def test_mirror_check_detects_each_kind_of_drift(self):
  c=json.loads((ROOT/'scripts/ci-contract.json').read_text())
  mac=lambda d:d['workflows']['.github/workflows/macos.yml']['jobs']['channels-macos']
  linux=lambda d:d['workflows']['.github/workflows/rust.yml']['jobs']['phone']
  def command_steps(job):return [s for s in job['steps'] if 'command_id' in s]
  cases=[]
  d=copy.deepcopy(c);mac(d)['steps'].remove(command_steps(mac(d))[-1]);cases.append(('dropped macOS step',d))
  d=copy.deepcopy(c);s=command_steps(mac(d));s[0],s[1]=s[1],s[0];i=[mac(d)['steps'].index(x) for x in s[:2]];mac(d)['steps'][min(i)],mac(d)['steps'][max(i)]=mac(d)['steps'][max(i)],mac(d)['steps'][min(i)];cases.append(('reordered macOS steps',d))
  d=copy.deepcopy(c);d['commands'][command_steps(linux(d))[0]['command_id']]['body']+='\necho changed\n';cases.append(('changed Linux body',d))
  d=copy.deepcopy(c);command_steps(linux(d))[0].setdefault('env',{})['NEW']='1';cases.append(('changed Linux step env',d))
  d=copy.deepcopy(c);linux(d)['env']['CARGO_PROFILE_TEST_DEBUG']=2;cases.append(('changed Linux job env',d))
  d=copy.deepcopy(c);linux(d)['steps'].insert(1,{'uses':'actions/setup-node@x','with':{'node-version':'22'}});cases.append(('Linux-only setup action',d))
  d=copy.deepcopy(c);mac(d)['name']='channels (macos-14)';cases.append(('name not derived from runner',d))
  d=copy.deepcopy(c);[x for x in linux(d)['steps'] if x.get('uses','').startswith('actions/setup-python@')][0]['with']['python-version']='3.13';cases.append(('changed setup input',d))
  d=copy.deepcopy(c);linux(d)['timeout-minutes']=10;cases.append(('changed timeout',d))
  for label,case in cases:self.assertNotEqual(mirror_drift(case),[],label)
  # A Linux-only step needs no macOS twin.
  d=copy.deepcopy(c);row=dict(command_steps(linux(d))[0]);row['if']="runner.os == 'Linux'";linux(d)['steps'].insert(3,row)
  self.assertEqual(mirror_drift(d),[])
  # So does a macOS-only step.
  d=copy.deepcopy(c);row=dict(command_steps(mac(d))[0]);row['if']="runner.os == 'macOS'";mac(d)['steps'].insert(3,row)
  self.assertEqual(mirror_drift(d),[])
 def test_validation_rust_jobs_restore_the_warm_cache_without_writing(self):
  # rust-cache.yml is the only writer; a job only hits its cache when the key
  # inputs (runner label, toolchain, CARGO* env) match the warm job's.
  c=json.loads((ROOT/'scripts/ci-contract.json').read_text())
  warm=yaml_load(ROOT/'.github/workflows/rust-cache.yml')['jobs']['warm']
  labels=set(warm['strategy']['matrix']['os']);toolchain=[s for s in warm['steps'] if s.get('uses','').startswith('dtolnay/rust-toolchain@')][0]['with']['toolchain']
  checked=0
  for path,w in c['workflows'].items():
   if w['role']!='validation':continue
   for name,job in w['jobs'].items():
    caches=[s for s in job['steps'] if s.get('uses','').startswith('Swatinem/rust-cache@')]
    if not caches:continue
    checked+=1;where=path+':'+name
    self.assertEqual(caches[0]['with'],{'shared-key':'ci','env-vars':'ImageOS','save-if':False},where)
    self.assertEqual(job.get('env'),warm['env'],where)
    rows=ci.matrices(job)
    self.assertTrue({ci.resolve(job['runs-on'],{'matrix.'+k:v for k,v in r.items()}) for r in rows}<=labels,where)
    for s in job['steps']:
     if s.get('uses','').startswith('dtolnay/rust-toolchain@'):self.assertEqual(s['with']['toolchain'],toolchain,where)
  self.assertGreaterEqual(checked,9)
 def test_platform_no_longer_cancels_main_runs(self):
  syncspec=importlib.util.spec_from_file_location('sync_platform',ROOT/'scripts/ci-contract-sync.py');sync=importlib.util.module_from_spec(syncspec);syncspec.loader.exec_module(sync)
  with self.assertRaisesRegex(ValueError,'PR concurrency policy'):
   sync.validate_concurrency('platform.yml',{'concurrency':{'group':'platform-${{ github.ref }}','cancel-in-progress':True}})
  sync.validate_concurrency('macos.yml',{'concurrency':sync.PR_CONCURRENCY})

def yaml_load(path):
 import yaml
 return yaml.safe_load(path.read_text())
# macOS job -> the Linux jobs whose steps it runs, in this order. The macOS
# job runs every step of its twins except those marked Linux-only; shared
# checkout/toolchain/cache setup appears once. It may add `runner.os ==
# 'macOS'` steps of its own.
MIRRORS={('.github/workflows/macos.yml','channels-macos'):[('.github/workflows/rust.yml','channels'),('.github/workflows/rust.yml','phone')],
         ('.github/workflows/macos.yml','platform-macos'):[('.github/workflows/platform.yml','platform')]}
LINUX_ONLY="runner.os == 'Linux'";MACOS_ONLY="runner.os == 'macOS'"
ALLOWED_STEP_IF={None,'always()','success()',LINUX_ONLY,MACOS_ONLY}
def mirror_drift(contract):
 """Differences between each macOS job and its Linux twin(s); [] when they agree."""
 workflows=contract['workflows'];drift=[]
 def row(step):
  if 'command_id' in step:
   body=contract['commands'][step['command_id']]
   key=('run',body['body'],body['shell'])
  else:key=('uses',step['uses'],json.dumps(step.get('with',{}),sort_keys=True).replace('ubuntu-latest','macos-latest'))
  return key+(json.dumps(step.get('env',{}),sort_keys=True),step.get('working-directory','.'),step.get('if'))
 for (mac_path,mac_name),twins in MIRRORS.items():
  mac=workflows[mac_path]['jobs'][mac_name];linux=[workflows[p]['jobs'][n] for p,n in twins]
  def runner(job):
   # A Linux twin may keep a one-entry matrix so its check ids keep their suffix.
   rows=ci.matrices(job)
   return ci.resolve(job['runs-on'],{'matrix.'+k:v for k,v in rows[0].items()}) if len(rows)==1 else None
  def display(job):return ci.resolve(job.get('name',''),{'matrix.'+k:v for k,v in ci.matrices(job)[0].items()})
  for job,os_name in [(mac,'macos-latest')]+[(j,'ubuntu-latest') for j in linux]:
   if job.get('if') or runner(job)!=os_name:drift.append('job must run only on '+os_name+': '+str(job.get('name')))
  expected=[]
  for j in linux:
   for s in j['steps']:
    if s.get('if')==LINUX_ONLY:continue
    r=row(s)
    if r[0]=='uses' and r[1].split('@')[0] in ('actions/checkout','dtolnay/rust-toolchain','Swatinem/rust-cache') and r in expected:continue
    expected.append(r)
  actual=[row(s) for s in mac['steps'] if s.get('if')!=MACOS_ONLY]
  if actual!=expected:
   first=next((i for i,(a,b) in enumerate(zip(actual,expected)) if a!=b),min(len(actual),len(expected)))
   drift.append(mac_name+': steps differ from '+', '.join(n for _,n in twins)+' at step '+str(first))
  for j in linux:
   for key in ('env','timeout-minutes'):
    if j.get(key)!=mac.get(key):drift.append(mac_name+': job '+key+' differs')
  prefix=mac['name'].split(' (')[0]
  if display(mac)!=prefix+' (macos-latest)' or display(linux[0])!=prefix+' (ubuntu-latest)':drift.append(mac_name+': check names must be <job> (<runner>)')
 return drift
if __name__=='__main__':unittest.main()
