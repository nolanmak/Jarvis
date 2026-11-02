"""No VM/model calls: controller binding, real lock inheritance, and mocked launch boundary."""
import fcntl,hashlib,importlib.util,json,os,stat,subprocess,tempfile,unittest,uuid
from pathlib import Path
from unittest.mock import patch
ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('ci_vm',ROOT/'scripts/ci-build-vm.py');adapter=importlib.util.module_from_spec(spec);spec.loader.exec_module(adapter)
vm=adapter.vm
class AdapterTests(unittest.TestCase):
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
  return {'version':2,'source_generation':generation,'retained_generations':[generation],'command_id':str(uuid.uuid4()),'predecessor':None,
   'tree':git('rev-parse','HEAD^{tree}'),'controller_authority_sha256':'d'*64,
   'operation':'plan','runtime_config':str(root/'runtime.json'),'workspace':str(root/'workspace'),
   'state':str(root/'state'),'scratch':str(root/'scratch'),'git_objects':str(root/'objects'),
   'runner':str(root/'runner'),'contract':str(root/'contract'),'head':git('rev-parse','HEAD'),'base':git('rev-parse','HEAD'),'build_cache':None,
   'storage':{'fd':fd,'fds':[fd],'path':str(lock),'device':info.st_dev,'inode':info.st_ino,'floor_bytes':0}}
 def test_plan_probe_is_only_dispatched_inside_vm_with_pinned_bundle(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d));request.update(id=None,changed_files=[],packages=[],filters=[],command=None,cache_bytes=None);seen=[]
   def execute(runtime,workspace,argv,environment,**options):
    seen.append((argv,options));self.assertIn('--probe-prerequisites',argv);self.assertEqual(environment,{})
    self.assertEqual(options['ci_context']['runner'],b'controller runner');self.assertIsNone(options['timeout'])
    plan={'head':request['head'],'base':request['base'],'contract_sha256':hashlib.sha256(b'{"version":1}').hexdigest(),'checks':[]}
    return {'exit_code':0,'stdout':json.dumps(plan),'stderr':''}
   with patch.object(vm.Runtime,'load',return_value=object()):result=adapter.execute(request,run=execute)
   self.assertEqual(result['status'],'planned');self.assertEqual(len(seen),1)
 def test_wrong_guest_binding_never_becomes_authority(self):
  with tempfile.TemporaryDirectory() as d:
   request=self.fixture(Path(d))
   with patch.object(vm.Runtime,'load',return_value=object()),self.assertRaisesRegex(vm.Unavailable,'binding mismatch'):
    adapter.execute(request,run=lambda *a,**k:{'exit_code':0,'stdout':'{}','stderr':''})
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
    result=adapter.execute(request,run=lambda *a,**k:{'exit_code':17,'stdout':'{"status":"passed"}','stderr':''})
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
     return {'exit_code':0,'stdout':'build complete','stderr':''}
    with patch.object(vm.Runtime,'load',return_value=object()),patch.object(adapter,'ensure_cache',
     side_effect=lambda path,*args:Path(path).touch(mode=0o600)):
     if fault=='none':self.assertEqual(adapter.execute(request,run=execute)['status'],'passed')
     else:
      with self.assertRaises(adapter.cache_generation.FreshnessHold):adapter.execute(request,run=execute)
      self.assertEqual(seen,[])
      self.assertFalse((workspace/'dist').exists())
    self.assertEqual((workspace/'public/original.css').read_text(),'original tracked CSS')

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
     return {'exit_code':0,'stdout':'','stderr':''}
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
    return {'exit_code':12,'stdout':'','stderr':'failed'}
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
  return {'state':root/'state','git_objects':root/'objects','runner':b'pinned runner','contract':b'pinned contract','cache_program':adapter.CACHE_PROGRAM,'generation':None}
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
     result=cleanup.parent/'control/outcome.json';result.write_text(json.dumps({'exit_code':0,'stdout':'finished','stderr':'','cache_reclaimed':receipt_reclaimed}));result.chmod(0o600)
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
