"""No VM/model calls: controller binding, real lock inheritance, and mocked launch boundary."""
import fcntl,hashlib,importlib.util,json,os,stat,subprocess,tempfile,unittest,uuid
from pathlib import Path
from unittest.mock import patch
ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('ci_vm',ROOT/'scripts/ci-build-vm.py');adapter=importlib.util.module_from_spec(spec);spec.loader.exec_module(adapter)
vm=adapter.vm
def stopped_workers():
 return {'version':1,'kind':'ci-guest-command-quiescence','worker_free':True,'shipping_authority':False,
  'scopes':[{'version':1,'kind':'ci-guest-workers-quiescent','state':'all_descendants_reaped',
   'scope_id':str(uuid.uuid4()),'receipt_sha256':hashlib.sha256(b'all-descendants-reaped\n').hexdigest(),'shipping_authority':False} for _ in range(3)]}
def observed_inputs(workers):
 return {'format_version':1,'kind':'ci-input-write-observation','changed':False,'changed_roots':[],
  'observed_events':0,'events_sha256':hashlib.sha256(b'').hexdigest(),'before_sha256':'f'*64,'after_sha256':'f'*64,
  'guest_worker_quiescence':workers['scopes'][-1],'shipping_authority':False}
class AdapterTests(unittest.TestCase):
 def test_internal_probe_flag_is_typed_and_cannot_select_a_root_probe_from_other_operations(self):
  with tempfile.TemporaryDirectory() as directory:
   request=self.fixture(Path(directory))
   for value in (True,1,0,'true',None,{}):
    with self.subTest(value=value),patch.object(vm.Runtime,'load') as load:
     with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request|{'planner_protocol_probe':value})
     load.assert_not_called()

 def test_structured_cargo_commands_preserve_original_selection_flags_and_filters(self):
  self.assertEqual(adapter.cargo_commands({'operation':'targeted','packages':['alpha','beta'],'filters':[]}),[
   ['/toolchain/bin/cargo','build','-p','alpha','-p','beta'],
   ['/toolchain/bin/cargo','test','-p','alpha','-p','beta','--no-fail-fast','--','--test-threads=1']])
  self.assertEqual(adapter.cargo_commands({'operation':'baseline','packages':[],'filters':['module::case']}),[
   ['/toolchain/bin/cargo','test','--workspace','--no-fail-fast','--','--test-threads=1','module::case']])

 def test_protected_cargo_entry_rejects_irrelevant_operations_torn_flags_and_predecessors(self):
  with tempfile.TemporaryDirectory() as directory:
   request=self.fixture(Path(directory))
   for change in ({'protected_cargo':True},{'protected_cargo':1},{'protected_cargo':'true'},
    {'protected_cargo':True,'operation':'baseline','predecessor':'invalid'},
    {'protected_cargo':True,'planner_protocol_probe':True,'operation':'repository','command':'true'}):
    with self.subTest(change=change),patch.object(vm.Runtime,'load') as load:
     with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request|change)
     load.assert_not_called()

 def test_protected_execution_replaces_driver_and_binds_receipt_to_original_command_scope(self):
  for borrowed in (False,True):
   with self.subTest(borrowed=borrowed),tempfile.TemporaryDirectory() as directory:
    root=Path(directory);request=self.fixture(root)
    request.update(operation='targeted',packages=['consumer'],filters=[],protected_cargo=True,build_cache=str(root/'cache'))
    def execute(runtime,workspace,command,environment,**options):
     context=options['ci_context'];commands=adapter.cargo_commands(request)
     self.assertEqual(context['cargo_execution'],commands)
     self.assertEqual(json.loads(command[-1]),commands)
     self.assertEqual(set(context['planner_programs']),{'ci-cargo-planner.py','ci-cargo-relay.py','ci-cargo-staging.py',
      'ci-cargo-views.py','ci-cargo-observer.py','ci-lookup-inputs.py','ci-cargo-planner.so'})
     result=self.prepared_result(options,exit_code=101,stdout='original failure',stderr='original Cargo stderr')
     generation=context['generation']
     result['cargo_execution']={'version':1,'kind':'ci-cargo-execution','binding':generation['binding'],
      'command':generation['command'],'commands':[{'argv':argv,'context_sha256':'1'*64,'exit_code':0 if i==0 else 101,
       'compiler_calls':0,'units':[],'original_cargo_cleanup':True} for i,argv in enumerate(commands)],
      'checkpoint':self.execution_checkpoint(generation,result['guest_worker_quiescence']['scopes'][-1]),
      'metadata_sha256':'2'*64,'original_metadata_cleanup':True,'input_closure_complete':False,'full_ci':False,
      'shipping_authority':False,'guest_worker_quiescence':result['guest_worker_quiescence']['scopes'][0 if borrowed else -1]}
     return result
    with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'private_bytes',side_effect=lambda path: b'measured program' if Path(path).name=='ci-cargo-planner.so' else Path(path).read_bytes()),patch.object(adapter,'ensure_cache',
      side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
     if borrowed:
      with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)
     else:
      result=adapter.execute(request,run=execute)
      self.assertEqual((result['exit_code'],result['stdout'],result['stderr']),(101,'original failure','original Cargo stderr'))
      self.assertFalse(result['cargo_execution']['shipping_authority'])

 def test_repository_entry_refuses_mixed_roles_and_predecessor_without_recovery(self):
  with tempfile.TemporaryDirectory() as directory:
   request=self.fixture(Path(directory))
   for change in ({'protected_repository':True},{'protected_repository':1},
    {'protected_repository':True,'operation':'repository','command':'true','predecessor':'invalid'},
    {'protected_repository':True,'operation':'repository','command':'true','planner_protocol_probe':True}):
    with self.subTest(change=change),patch.object(vm.Runtime,'load') as load:
     with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request|change)
     load.assert_not_called()

 def test_repository_entry_preserves_original_shell_and_requires_its_own_worker_scope(self):
  for borrowed in (False,True):
   with self.subTest(borrowed=borrowed),tempfile.TemporaryDirectory() as directory:
    root=Path(directory);request=self.fixture(root)
    request.update(operation='repository',command='python3 -I repository_driver.py',protected_repository=True,build_cache=str(root/'cache'))
    def execute(runtime,workspace,command,environment,**options):
     self.assertEqual(command,['/bin/bash','--noprofile','--norc','-o','pipefail','-c',request['command']])
     self.assertTrue(options['ci_context']['repository_execution'])
     result=self.prepared_result(options,exit_code=0,stdout='original nested stdout',stderr='')
     generation=options['ci_context']['generation']
     result['repository_execution']={'version':1,'kind':'ci-repository-execution','binding':generation['binding'],
      'command':generation['command'],'checkpoint':self.execution_checkpoint(generation,result['guest_worker_quiescence']['scopes'][-1]),
      'worker_units':[],'cargo_commands':[],'original_worker_cleanup':True,
      'input_closure_complete':False,'full_ci':False,'shipping_authority':False,
      'guest_worker_quiescence':result['guest_worker_quiescence']['scopes'][0 if borrowed else -1]}
     return result
    with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'private_bytes',side_effect=lambda path:
      b'measured program' if Path(path).name=='ci-cargo-planner.so' else Path(path).read_bytes()),patch.object(adapter,'ensure_cache',
      side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
     if borrowed:
      with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)
     else:self.assertEqual(adapter.execute(request,run=execute)['stdout'],'original nested stdout')

 def test_source_projection_requires_current_binding_roles_and_independent_worker_cleanup(self):
  for fault in ('missing','version','binding','command','owner','manifest','flags','missing-role','wrong-role','duplicate-probe','borrowed-worker','foreign-scope'):
   with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
    root=Path(directory);request=self.fixture(root)
    request.update(operation='repository',command='true',build_cache=str(root/'cache'))
    def execute(*args,**options):
     result=self.prepared_result(options,exit_code=0,stdout='',stderr='');projected=result['source_projection']
     if fault=='missing':result.pop('source_projection')
     elif fault=='version':projected['version']=True
     elif fault=='binding':projected['binding']['generation']=str(uuid.uuid4())
     elif fault=='command':projected['command']=str(uuid.uuid4())
     elif fault=='owner':projected['owner_uid']=1000
     elif fault=='manifest':projected['manifest_sha256']='unknown'
     elif fault=='flags':projected['input_closure_complete']=True
     elif fault=='missing-role':projected['access'].pop()
     elif fault=='wrong-role':projected['access'][0]['uid']=2000
     elif fault=='duplicate-probe':projected['access'][1]['scope']=projected['access'][0]['scope']
     elif fault=='borrowed-worker':result['input_write_observation']['guest_worker_quiescence']=projected['access'][0]['scope']
     elif fault=='foreign-scope':projected['access'][0]['scope']={**projected['access'][0]['scope'],'scope_id':str(uuid.uuid4())}
     return result
    with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
      side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
     with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)
 def test_seed_cleanup_receipt_is_required_bounded_unique_and_typed(self):
  for reclaimed in (None,{},[[]],['unknown'],['a'*64,'a'*64],['a'*64,'b'*64,'c'*64]):
   with self.subTest(reclaimed=reclaimed),tempfile.TemporaryDirectory() as directory:
    root=Path(directory);request=self.fixture(root)
    request.update(operation='repository',command='true',build_cache=str(root/'registered-image'))
    def execute(*args,**options):
     return self.prepared_result(options,exit_code=0,stdout='',stderr='',seed_reclaimed=reclaimed)
    with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
      side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
     with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)
 def execution_checkpoint(self,generation,scope):
  return {'version':1,'kind':'ci-cargo-execution-checkpoint','binding':generation['binding'],'command':generation['command'],
   'claim':str(uuid.uuid4()),'predecessor':generation['predecessor'],'scope_id':scope['scope_id'],
   'retained_executor':True,'input_closure_complete':False,'shipping_authority':False}
 def prepared_result(self,options,**result):
  result.setdefault('seed_reclaimed',[])
  generation=options['ci_context']['generation']
  result['guest_worker_quiescence']=stopped_workers()
  result['input_write_observation']=observed_inputs(result['guest_worker_quiescence'])
  result['cache_preparation']={'version':1,'binding':dict(generation['binding']),'command':generation['command'],
   'outputs_sha256':hashlib.sha256(json.dumps(generation['outputs'],sort_keys=True,separators=(',',':')).encode()).hexdigest(),
   'dependency_seed':None}
  result['source_projection']={'version':1,'kind':'ci-git-source-projection','binding':dict(generation['binding']),
   'command':generation['command'],'manifest_sha256':'9'*64,'owner_uid':0,
   'access':[{'uid':uid,'scope':result['guest_worker_quiescence']['scopes'][index]} for index,uid in enumerate((1001,2000))],
   'input_closure_complete':False,'shipping_authority':False}
  return result
 def fixture(self,root):
  for name in ['workspace','state','scratch','objects']:(root/name).mkdir(mode=0o700)
  for name,data in [('runner',b'controller runner'),('contract',b'{"version":1}')]:
   (root/name).write_bytes(data);(root/name).chmod(0o600)
  lock=root/'disk-build.lock';fd=os.open(lock,os.O_RDWR|os.O_CREAT,0o600);fcntl.flock(fd,fcntl.LOCK_EX)
  self.addCleanup(os.close,fd);info=os.fstat(fd)
  def git(*args):
   return subprocess.check_output(['git','-c','core.hooksPath=/dev/null','-c','commit.gpgsign=false','-c','user.name=Fixture','-c','user.email=fixture@example.invalid','-C',str(root/'workspace'),*args],text=True).strip()
  git('init','-q');(root/'workspace/source.rs').write_text('literal source');git('add','.');git('commit','-qm','source')
  generation=str(uuid.uuid4())
  return {'version':2,'planner_protocol_probe':False,'source_generation':generation,'retained_generations':[generation],'retained_producers':[],'producer_retirements':[],'command_id':str(uuid.uuid4()),'predecessor':None,
   'tree':git('rev-parse','HEAD^{tree}'),'controller_authority_sha256':'d'*64,
   'operation':'plan','runtime_config':str(root/'runtime.json'),'workspace':str(root/'workspace'),
   'state':str(root/'state'),'scratch':str(root/'scratch'),'git_objects':str(root/'objects'),
   'runner':str(root/'runner'),'contract':str(root/'contract'),'head':git('rev-parse','HEAD'),'base':git('rev-parse','HEAD'),'build_cache':None,
   'storage':{'fd':fd,'fds':[fd],'path':str(lock),'device':info.st_dev,'inode':info.st_ino,'floor_bytes':0}}
 def test_producer_retention_cannot_be_omitted_or_contain_invalid_ownership(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d))
   for field in ('retained_producers','producer_retirements'):
    missing=dict(request);missing.pop(field)
    with patch.object(vm.Runtime,'load') as load:
     with self.assertRaisesRegex(vm.Unavailable,'invalid CI VM request'):adapter.execute(missing)
     load.assert_not_called()
   producer=str(uuid.uuid4())
   for bad in (None,'not-a-list',['bad'],[producer,producer]):
    with self.assertRaises(adapter.cache_generation.FreshnessHold):
     adapter.execute(request|{'retained_producers':bad})

 def test_producer_retirement_requires_original_bound_execution_metadata(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d));producer=str(uuid.uuid4())
   release={'producer':producer,'ack':{'command':str(uuid.uuid4()),'authority_sha256':'d'*64,
    'cache_identity_sha256':'b'*64,'quiescence_sha256':'a'*64}}
   malformed=[None,{},'not-a-list',[{}],[release,release],
    [release|{'ack':release['ack']|{'authority_sha256':'e'*64}}],
    [release|{'ack':release['ack']|{'command':'unknown'}}],
    [release|{'ack':release['ack']|{'quiescence_sha256':None}}]]
   for bad in malformed:
    with patch.object(vm.Runtime,'load') as load:
     with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request|{'producer_retirements':bad})
     load.assert_not_called()
   with self.assertRaises(adapter.cache_generation.FreshnessHold):
    adapter.execute(request|{'producer_retirements':[release],'retained_producers':[producer]})
   request.update(operation='repository',command='true',build_cache=str(Path(d)/'cache'),producer_retirements=[release])
   with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',side_effect=lambda path,*args:Path(path).touch(mode=0o600)),patch.object(vm,'run') as run:
    with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request)
    run.assert_not_called()

 def test_protected_producer_reaches_guest_and_cannot_be_reported_as_reclaimed(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);producer=str(uuid.uuid4());request=self.fixture(root)
   request.update(operation='repository',command='true',build_cache=str(root/'cache'),retained_producers=[producer])
   def run(runtime,workspace,argv,env,**options):
    self.assertEqual(options['ci_context']['generation']['retained_producers'],[producer])
    return self.prepared_result(options,exit_code=0,stdout='',stderr='',producer_reclaimed=[producer])
   with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
    with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=run)

 def test_plan_probe_is_only_dispatched_inside_vm_with_pinned_bundle(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d));request.update(id=None,changed_files=[],packages=[],filters=[],command=None,cache_bytes=None);seen=[]
   def execute(runtime,workspace,argv,environment,**options):
    seen.append((argv,options));self.assertIn('--probe-prerequisites',argv);self.assertEqual(environment,{})
    self.assertEqual(options['ci_context']['runner'],b'controller runner');self.assertIsNone(options['timeout'])
    plan={'head':request['head'],'base':request['base'],'contract_sha256':hashlib.sha256(b'{"version":1}').hexdigest(),'checks':[]}
    return {'exit_code':0,'stdout':json.dumps(plan),'stderr':'','seed_reclaimed':[],'guest_worker_quiescence':stopped_workers()}
   with patch.object(vm.Runtime,'load',return_value=object()):result=adapter.execute(request,run=execute)
   self.assertEqual(result['status'],'planned');self.assertEqual(len(seen),1)

 def test_dependency_production_requires_an_isolated_source_and_cannot_report_passing_ci(self):
  with tempfile.TemporaryDirectory() as directory:
   root=Path(directory);request=self.fixture(root)
   source=root/'producer';source.mkdir(mode=0o700)
   workspace=adapter.dependency_workspace
   metadata={'version':1,'workspace_root':'/workspace','packages':[
    {'id':'leaf','name':'leaf','version':'1.0.0','source':'registry+https://github.com/rust-lang/crates.io-index',
     'manifest_path':'/ci-cargo-home/registry/src/pinned/leaf/Cargo.toml',
     'targets':[{'kind':['lib'],'src_path':'/ci-cargo-home/registry/src/pinned/leaf/src/lib.rs'}]}],
    'resolve':{'nodes':[{'id':'leaf','dependencies':[],'features':[]}]}}
   lock='version=4\n[[package]]\nname="leaf"\nversion="1.0.0"\nsource="registry+https://github.com/rust-lang/crates.io-index"\nchecksum="'+'a'*64+'"\n'
   files=workspace.source_files(metadata,lock);identity=workspace.write_source(source,files)
   dependency={'kind':'produce','producer':str(uuid.uuid4()),'source_sha256':identity,
    'dependency_inputs_sha256':hashlib.sha256(files['seed-inputs.json']).hexdigest()}
   request.update(operation='dependency',workspace=str(source),dependency=dependency,build_cache=str(root/'registered-image'),retained_producers=[dependency['producer']])
   seen=[]
   def execute(*args,**options):
    seen.append(options)
    self.assertEqual(options['ci_context']['dependency_action']['kind'],'produce')
    self.assertIn('ci-dependency-store.py',options['ci_context']['dependency_programs'])
    return {'exit_code':0,'stdout':'','stderr':'','cache_reclaimed':[],'seed_reclaimed':[],'guest_worker_quiescence':stopped_workers(),
     'dependency_result':{'version':1,'kind':'produced','producer':dependency['producer'],
      'command':request['command_id'],'dependency_inputs_sha256':dependency['dependency_inputs_sha256'],
      'shipping_authority':False}}
   with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
    side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
    result=adapter.execute(request,run=execute)
    self.assertEqual(result['status'],'seed_produced')
    (source/'application.rs').write_text('unapproved application code')
    with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)
    self.assertEqual(len(seen),1)
 def test_wrong_guest_binding_never_becomes_authority(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d))
   with patch.object(vm.Runtime,'load',return_value=object()),self.assertRaisesRegex(vm.Unavailable,'binding mismatch'):
    adapter.execute(request,run=lambda *a,**k:{'exit_code':0,'stdout':'{}','stderr':'','seed_reclaimed':[],'guest_worker_quiescence':stopped_workers()})
 def test_real_inherited_lock_not_proc_reopen_and_fd_identity_refusal(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);request=self.fixture(root);lease=adapter.InheritedStorage(request['storage'],root/'scratch');lease.check()
   fcntl.flock(lease.fd,fcntl.LOCK_UN)
   with self.assertRaisesRegex(vm.Unavailable,'not held'):adapter.InheritedStorage(request['storage'],root/'scratch')
 def test_rust_shaped_group_writable_data_directory_is_not_lock_authority(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);request=self.fixture(root);target=root/'target';target.mkdir(mode=0o775);target.chmod(0o775)
   fd=os.open(target,os.O_RDONLY|os.O_DIRECTORY);self.addCleanup(os.close,fd)
   request['storage']['fds'].append(fd)
   adapter.InheritedStorage(request['storage'],root/'scratch').check()
   target.chmod(0o777)
   with self.assertRaisesRegex(vm.Unavailable,'target lease'):adapter.InheritedStorage(request['storage'],root/'scratch')
   target.chmod(0o775);os.fchmod(request['storage']['fd'],0o664)
   with self.assertRaisesRegex(vm.Unavailable,'target lease'):adapter.InheritedStorage(request['storage'],root/'scratch')
 def test_step_exit_is_not_fabricated_from_output_and_requires_managed_cache(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d));request.update(operation='step',id='test/job/1')
   with patch.object(vm.Runtime,'load',return_value=object()),self.assertRaisesRegex(vm.Unavailable,'cache image'):
    adapter.execute(request,run=lambda *a,**k:None)
   request['build_cache']=str(Path(d)/'registered-image')
   with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
    result=adapter.execute(request,run=lambda *a,**k:self.prepared_result(k,exit_code=17,stdout='{"status":"passed"}',stderr=''))
   self.assertEqual(result['status'],'failed');self.assertEqual(result['exit_code'],17)

 def test_declared_outputs_do_not_hide_tracked_or_unexplained_inputs(self):
  for fault in ('none','tracked-output','unexplained-output'):
   with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
    root=Path(directory);request=self.fixture(root);workspace=root/'workspace'
    (workspace/'package.json').write_text('{}');(workspace/'public').mkdir()
    (workspace/'public/original.css').write_text('original tracked CSS')
    def git(*args):
     return subprocess.check_output(['git','-c','core.hooksPath=/dev/null','-c','commit.gpgsign=false',
      '-c','user.name=Fixture','-c','user.email=fixture@example.invalid','-C',str(workspace),*args],text=True).strip()
    if fault=='tracked-output':(workspace/'public/tailwind.css').write_text('must remain original')
    git('add','.');git('commit','-qm','declared Node input')
    request.update(head=git('rev-parse','HEAD'),base=git('rev-parse','HEAD'),tree=git('rev-parse','HEAD^{tree}'),
     operation='repository',command='npm run build',build_cache=str(root/'registered-image'))
    if fault=='unexplained-output':(workspace/'public/tailwind.css').write_text('unbound input')
    seen=[]
    def execute(*args,**options):
     seen.append(options)
     self.assertEqual(options['ci_context']['generation']['outputs'],
      {'node_modules':'directory','dist':'directory','public/tailwind.css':'file'})
     self.assertEqual((workspace/'public/tailwind.css').read_bytes(),b'')
     return self.prepared_result(options,exit_code=0,stdout='build complete',stderr='')
    with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
     side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
     if fault=='none':self.assertEqual(adapter.execute(request,run=execute)['status'],'passed')
     else:
      with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)
      self.assertEqual(seen,[])
      self.assertFalse((workspace/'dist').exists())
    self.assertEqual((workspace/'public/original.css').read_text(),'original tracked CSS')

 def test_source_verdict_requires_original_root_cache_preparation(self):
  for status in (0,17):
   for fault in ('none','missing','command','generation','source','physical-cache','output-policy','seed'):
    with self.subTest(status=status,fault=fault),tempfile.TemporaryDirectory() as directory:
     root=Path(directory);request=self.fixture(root)
     request.update(operation='repository',command='true',build_cache=str(root/'registered-image'))
     expected=[]
     def execute(*args,**options):
      generation=options['ci_context']['generation']
      preparation={'version':1,'binding':dict(generation['binding']),'command':generation['command'],
       'outputs_sha256':hashlib.sha256(json.dumps(generation['outputs'],sort_keys=True,separators=(',',':')).encode()).hexdigest(),
       'dependency_seed':None}
      expected.append(preparation)
      value=json.loads(json.dumps(preparation))
      if fault=='command':value['command']=str(uuid.uuid4())
      if fault=='generation':value['binding']['generation']=str(uuid.uuid4())
      if fault=='source':value['binding']['source_inputs_sha256']='1'*64
      if fault=='physical-cache':value['binding']['cache_identity_sha256']='2'*64
      if fault=='output-policy':value['outputs_sha256']='3'*64
      if fault=='seed':value['dependency_seed']='unknown'
      result=self.prepared_result(options,exit_code=status,stdout='source command output',stderr='')
      if fault=='missing':result.pop('cache_preparation')
      else:result['cache_preparation']=value
      return result
     with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
      side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
      if fault=='none':
       result=adapter.execute(request,run=execute)
       self.assertEqual(result['cache_preparation'],expected[0])
       self.assertEqual(result['status'],'passed' if status==0 else 'failed')
      else:
       with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)

 def test_missing_or_unknown_worker_cleanup_never_becomes_a_source_verdict(self):
  for status in [0,17]:
   for fault in ['missing','empty','still-live','duplicate','unknown-scope']:
    with self.subTest(status=status,fault=fault),tempfile.TemporaryDirectory() as directory:
     root=Path(directory);request=self.fixture(root)
     request.update(operation='repository',command='true',build_cache=str(root/'registered-image'))
     def execute(*args,**options):
      result=self.prepared_result(options,exit_code=status,stdout='candidate output',stderr='')
      if fault=='missing':result.pop('guest_worker_quiescence')
      elif fault=='empty':result['guest_worker_quiescence']['scopes']=[]
      elif fault=='still-live':result['guest_worker_quiescence']['worker_free']=False
      elif fault=='duplicate':result['guest_worker_quiescence']['scopes']*=2
      else:result['guest_worker_quiescence']['scopes'][0]['state']='unknown'
      return result
     with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
      side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
      with self.assertRaises(adapter.cache_generation.FreshnessHold) as refused:adapter.execute(request,run=execute)
      self.assertEqual(refused.exception.reason,'cleanup_uncertain')

 def test_input_observation_must_be_complete_and_bound_to_the_same_worker(self):
  for status in [0,17]:
   for fault in ['missing','legacy','foreign-worker','extra-field','contradiction','unknown-root']:
    with self.subTest(status=status,fault=fault),tempfile.TemporaryDirectory() as directory:
     root=Path(directory);request=self.fixture(root)
     request.update(operation='repository',command='true',build_cache=str(root/'registered-image'))
     def execute(*args,**options):
      result=self.prepared_result(options,exit_code=status,stdout='candidate output',stderr='')
      observed=result['input_write_observation']
      if fault=='missing':result.pop('input_write_observation')
      elif fault=='legacy':observed['format_version']=0
      elif fault=='foreign-worker':observed['guest_worker_quiescence']=stopped_workers()['scopes'][0]
      elif fault=='extra-field':observed['unrecognized']=True
      elif fault=='contradiction':observed['changed']=True
      else:observed['changed_roots']=['output_99'];observed['observed_events']=1;observed['changed']=True
      return result
     with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
      side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
      with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)

 def test_cache_reuse_capacity_and_private_identity(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);image=root/'cache.img';capacity=64*1024*1024
   class Lease:
    fds=[4,5];floor=0
    def check(self):pass
   lease=Lease()
   def format_image(argv,**kwargs):
    fd=int(argv[-1].split('/')[-1]);header=bytearray(1024)
    import struct
    struct.pack_into('<I',header,4,capacity//4096);struct.pack_into('<I',header,24,2);header[56:58]=b'\x53\xef'
    os.pwrite(fd,header,1024)
   with patch.object(adapter.subprocess,'run',side_effect=format_image) as formatter:
    adapter.ensure_cache(image,capacity,lease);inode=image.stat().st_ino
    adapter.ensure_cache(image,capacity,lease)
    self.assertEqual(formatter.call_count,1);self.assertEqual(image.stat().st_ino,inode)
    with self.assertRaisesRegex(vm.Unavailable,'capacity changed'):adapter.ensure_cache(image,capacity*2,lease)
    image.chmod(0o644)
    with self.assertRaisesRegex(vm.Unavailable,'identity'):adapter.ensure_cache(image,capacity,lease)
 def test_cache_refuses_symlink_and_missing_target_admission(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);image=root/'cache.img'
   class Lease:
    fds=[4];floor=0
    def check(self):pass
   lease=Lease()
   with self.assertRaisesRegex(vm.Unavailable,'target lease'):adapter.ensure_cache(image,64*1024*1024,lease)
   lease.fds=[4,5];image.symlink_to(root/'other')
   with self.assertRaises(OSError):adapter.ensure_cache(image,64*1024*1024,lease)

 def test_fixed_targeted_and_baseline_argv_and_validation(self):
  for operation,packages,filters,expected in [
   ('targeted',['core-lib','cli'],[],[['/toolchain/bin/cargo','build','-p','core-lib','-p','cli'],['/toolchain/bin/cargo','test','-p','core-lib','-p','cli','--no-fail-fast','--','--test-threads=1']]),
   ('baseline',[],['tests::red'],[['/toolchain/bin/cargo','test','--workspace','--no-fail-fast','--','--test-threads=1','tests::red']])]:
   with tempfile.TemporaryDirectory() as d:
    request=self.fixture(Path(d));request.update(operation=operation,packages=packages,filters=filters,build_cache=str(Path(d)/'cache'),cache_bytes=67108864)
    seen=[]
    def run(runtime,workspace,argv,env,**kwargs):
     self.assertEqual(argv[:3],['/usr/bin/python3','-I','-c'])
     with patch.object(adapter.subprocess,'run',side_effect=lambda command:seen.append(command) or subprocess.CompletedProcess(command,0)),patch.object(adapter.sys,'argv',['script',argv[4]]):
      exec(argv[3],{})
     return self.prepared_result(kwargs,exit_code=0,stdout='',stderr='')
    with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
     self.assertEqual(adapter.execute(request,run=run)['status'],'passed')
    self.assertEqual(seen,expected)
  for invalid in [dict(operation='targeted',packages=[]),dict(operation='targeted',packages=['--workspace']),dict(operation='baseline',filters=['x;touch bad']),dict(operation='step',packages=['cli'])]:
   with self.assertRaises(vm.Unavailable):adapter.cargo_request(invalid)
 def test_repository_command_only_in_admitted_guest(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d));request.update(operation='repository',command='printf owner-configured',build_cache=str(Path(d)/'cache'),cache_bytes=67108864)
   def run(runtime,workspace,argv,env,**kwargs):
    self.assertEqual(argv,['/bin/bash','--noprofile','--norc','-o','pipefail','-c','printf owner-configured'])
    return self.prepared_result(kwargs,exit_code=12,stdout='',stderr='failed')
   with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
    self.assertEqual(adapter.execute(request,run=run)['status'],'failed')
  with self.assertRaises(vm.Unavailable):adapter.cargo_request({'operation':'repository','command':'x'*65537})

 def test_setup_diagnostics_are_fixed_and_exclude_request_content(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);request=self.fixture(root);request.update(id=None,changed_files=[],packages=[],filters=[],command=None,cache_bytes=None)
   path=root/'request.json';path.write_text(json.dumps(request));path.chmod(0o600)
   # Real Rust-shaped request through parser, actual held descriptors and safe
   # failure at the runtime loader. No VM launch or arbitrary stderr text.
   import io,contextlib
   output=io.StringIO()
   with patch.object(adapter.sys,'argv',['adapter','--spec',str(path)]),patch.object(vm.Runtime,'load',side_effect=PermissionError(13,'SECRET_PATH_AND_PAYLOAD')),contextlib.redirect_stdout(output):
    self.assertEqual(adapter.main(),2)
   result=json.loads(output.getvalue());self.assertEqual(result['stage'],'runtime_load');self.assertEqual(result['category'],'PermissionError');self.assertEqual(result['errno'],13)
   self.assertNotIn('SECRET',output.getvalue());self.assertNotIn(str(root),output.getvalue())

class BuildVMExtensionTests(unittest.TestCase):
 def runtime(self,root):
  artifact=root/'artifact';artifact.write_bytes(b'synthetic');artifact.chmod(0o600)
  runtime=vm.Runtime();runtime.config={key:str(artifact) for key in ['qemu','kernel','busybox','firmware','data_dir','library_dir','module_dir']};runtime.config.update(memory_mb=512,modules=[str(artifact)]);return runtime
 def setup(self,root):
  for name in ['workspace','state','objects','scratch']:(root/name).mkdir(mode=0o700)
  return {'state':root/'state','git_objects':root/'objects','runner':b'pinned runner','contract':b'pinned contract',
   'cache_program':adapter.CACHE_PROGRAM,'generation':None,'worker_programs':adapter.WORKER_PROGRAMS}
 def openssl(self,argv,**kwargs):
  Path(argv[argv.index('-keyout')+1]).write_bytes(b'fixture-key');Path(argv[argv.index('-out')+1]).write_bytes(b'fixture-cert');return subprocess.CompletedProcess(argv,0)
 def test_ci_mounts_are_explicit_and_controller_bundle_is_readonly(self):
  class Stop(Exception):pass
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);context=self.setup(root);(root/'objects').chmod(0o775);captured={}
   def archive(entries):captured['entries']=entries;return b'fixture'
   def qemu(config,image,shares,cache):captured['shares']=shares;raise Stop()
   with patch.object(vm.subprocess,'run',side_effect=self.openssl),patch.object(vm,'initrd',side_effect=archive),patch.object(vm,'qemu_command',side_effect=qemu),self.assertRaises(Stop):
    vm.run(self.runtime(root),root/'workspace',['true'],{},scratch_dir=root/'scratch',ci_context=context)
   self.assertIn(('ci_objects',root/'objects',True),captured['shares']);self.assertIn(('ci_runtime',root/'state',False),captured['shares'])
   self.assertIn(('workspace',root/'workspace',True),captured['shares'])
   entries={name:(mode,data) for name,mode,data,_,_ in captured['entries']};self.assertEqual(entries['ci-tools/runner.py'],(stat.S_IFREG|0o444,b'pinned runner'))
   self.assertIn(b'ro,nosuid,nodev ci_objects /ci-git-objects',entries['init'][1])
   self.assertIn(b'ro,nosuid,nodev workspace /workspace',entries['init'][1])
   self.assertEqual(entries['ci-tools/cache-generation.py'],(stat.S_IFREG|0o444,adapter.CACHE_PROGRAM))
   (root/'objects').chmod(0o777)
   with self.assertRaisesRegex(vm.Unavailable,'separate owned'):vm.run(self.runtime(root),root/'workspace',['true'],{},ci_context=context)
   (root/'objects').chmod(0o775)
   context['state']=root/'workspace'
   with self.assertRaisesRegex(vm.Unavailable,'separate owned'):vm.run(self.runtime(root),root/'workspace',['true'],{},ci_context=context)
 def test_supervised_ci_has_no_900_second_wall_kill(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);context=self.setup(root);polls=[];clock=[0]
   receipt_reclaimed=['12345678-1234-4234-8234-123456789abc']
   class Broker:
    requests=0;bytes=0
    def __init__(self,control):self.control=control
    def poll(self,remaining):polls.append(remaining);clock[0]+=1000
   class Process:
    pid=999999
    def __init__(self,argv,**kwargs):
     self.calls=0;self.returncode=None;cleanup=Path(argv[3]);cleanup.write_text('all-descendants-reaped\n')
     result=cleanup.parent/'control/outcome.json';result.write_text(json.dumps({'exit_code':0,'stdout':'finished','stderr':'',
      'cache_reclaimed':receipt_reclaimed,'producer_reclaimed':[],'seed_reclaimed':[],'guest_worker_quiescence':stopped_workers()}));result.chmod(0o600)
    def wait(self,timeout):
     self.calls+=1
     if self.calls<3:raise subprocess.TimeoutExpired('fixture',timeout)
     self.returncode=0;return 0
    def poll(self):return self.returncode
   with patch.object(vm.subprocess,'run',side_effect=self.openssl),patch.object(vm.subprocess,'Popen',Process),patch.object(vm.dependency_proxy,'Broker',Broker),patch.object(vm.time,'monotonic',side_effect=lambda:clock[0]):
    result=vm.run(self.runtime(root),root/'workspace',['true'],{},scratch_dir=root/'scratch',timeout=None,ci_context=context)
   self.assertEqual(result['stdout'],'finished');self.assertEqual(polls,[1,1,1]);self.assertEqual(clock[0],3000)
   for invalid in (None,'unknown',['not-a-generation'],[True],receipt_reclaimed*2,receipt_reclaimed*3,
                   ['12345678-1234-1234-8234-123456789abc']):
    receipt_reclaimed=invalid
    with patch.object(vm.subprocess,'run',side_effect=self.openssl),patch.object(vm.subprocess,'Popen',Process),patch.object(vm.dependency_proxy,'Broker',Broker),patch.object(vm.time,'monotonic',side_effect=lambda:clock[0]):
     with self.assertRaisesRegex(vm.Unavailable,'invalid CI cache cleanup receipt'):
      vm.run(self.runtime(root),root/'workspace',['true'],{},scratch_dir=root/'scratch',timeout=None,ci_context=context)
if __name__=='__main__':unittest.main()
