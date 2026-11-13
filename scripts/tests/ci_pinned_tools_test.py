#!/usr/bin/env python3
"""Exercise download failover and fail-closed installation with deterministic transport fixtures."""
import hashlib, importlib.util, io, json, os, tempfile, unittest, urllib.error, zipfile
from pathlib import Path
from unittest.mock import patch
SPEC=importlib.util.spec_from_file_location('pinned_tools',Path(__file__).resolve().parents[1]/'ci-pinned-tools.py')
tools=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(tools)
class DownloadTests(unittest.TestCase):
 def test_gateway_failure_uses_verified_fallback(self):
  payload=b'pinned artifact'; digest=hashlib.sha256(payload).hexdigest()
  with tempfile.TemporaryDirectory() as d, patch.object(tools.urllib.request,'urlopen',side_effect=[urllib.error.HTTPError('first',504,'gateway',{},None),io.BytesIO(payload)]) as request:
   target=Path(d)/'artifact';tools.download(['https://first','https://second'],digest,target)
   self.assertEqual(target.read_bytes(),payload);self.assertEqual(request.call_count,2)
 def test_wrong_digest_never_becomes_installed_artifact(self):
  with tempfile.TemporaryDirectory() as d, patch.object(tools.urllib.request,'urlopen',side_effect=lambda *a,**k:io.BytesIO(b'wrong')):
   target=Path(d)/'artifact'
   with self.assertRaisesRegex(RuntimeError,'checksum'):tools.download(['https://first'],'0'*64,target)
   self.assertFalse(target.exists())
 def test_all_failed_downloads_are_bounded_and_leave_no_artifact(self):
  error=urllib.error.URLError('offline')
  with tempfile.TemporaryDirectory() as d, patch.object(tools.urllib.request,'urlopen',side_effect=error) as request:
   target=Path(d)/'artifact'
   with self.assertRaises(RuntimeError):tools.download(['https://one','https://two'],'0'*64,target)
   self.assertEqual(request.call_count,4);self.assertFalse(target.exists())
 def test_oversize_download_rejected(self):
  with tempfile.TemporaryDirectory() as d, patch.object(tools.urllib.request,'urlopen',side_effect=lambda *a,**k:io.BytesIO(b'12345')):
   with self.assertRaisesRegex(RuntimeError,'size'):tools.download(['https://one'],'0'*64,Path(d)/'artifact',limit=4)
 def test_zip_traversal_rejected_before_extracting_anything(self):
  with tempfile.TemporaryDirectory() as d:
   root=Path(d);archive=root/'bad.zip'
   with zipfile.ZipFile(archive,'w') as z:z.writestr('deno',b'valid');z.writestr('../escaped',b'bad')
   with self.assertRaises(ValueError):tools.extract_deno(archive,root/'out')
   self.assertFalse((root/'escaped').exists());self.assertFalse((root/'out/deno').exists())
class WorkflowTests(unittest.TestCase):
 def test_every_affected_runtime_has_pinned_provisioning_and_vm_refusal(self):
  root=Path(__file__).resolve().parents[2]
  contract=json.loads((root/'scripts/ci-contract.json').read_text())
  found=0
  for workflow in contract['workflows'].values():
   for job in workflow.get('jobs',{}).values():
    steps=job['steps']
    for i,step in enumerate(steps):
     action=step.get('uses','')
     tool='deno' if action.startswith('denoland/setup-deno@') else 'python39' if action.startswith('actions/setup-python@') and step.get('with',{}).get('python-version')=='3.9.25' else None
     if tool:
      found+=1
      provision=contract['commands'][steps[i-1]['command_id']]
      self.assertEqual(provision['body'].splitlines()[-1],'python3 scripts/ci-pinned-tools.py '+tool)
      if tool=='python39':self.assertEqual(steps[i-1]['timeout-minutes'],5)
      self.assertEqual(provision['vm_provisioning'],['pinned-tool-release'])
      self.assertEqual(job['runs-on'],'ubuntu-24.04' if tool=='python39' else 'ubuntu-latest')
      self.assertNotIn('continue-on-error',steps[i-1])
  self.assertEqual(found,6)
 def test_python39_source_jobs_provision_required_headers_before_building(self):
  root=Path(__file__).resolve().parents[2]
  contract=json.loads((root/'scripts/ci-contract.json').read_text())
  commands=[row for row in contract['commands'].values() if row['body'].splitlines()[-1]=='python3 scripts/ci-pinned-tools.py python39']
  self.assertEqual(len(commands),3)
  required={'libbz2-dev','libffi-dev','liblzma-dev','libsqlite3-dev','libssl-dev','zlib1g-dev'}
  for row in commands:
   body=row['body']
   install=next(line for line in body.splitlines() if line.startswith('python3 scripts/ci-apt-install.py '))
   self.assertEqual(set(install.split()[2:]),required,install)
   self.assertLess(body.index(install),body.index('python3 scripts/ci-pinned-tools.py python39'))
   self.assertEqual(row['vm_provisioning'],['pinned-tool-release'])
 def test_exporter_checks_the_source_path_with_controlled_provisioning(self):
  root=Path(__file__).resolve().parents[2];contract=json.loads((root/'scripts/ci-contract.json').read_text())
  job=contract['workflows']['.github/workflows/apple-notes.yml']['jobs']['exporter']
  checks=[step for step in job['steps'] if 'command_id' in step and '--verify-source' in contract['commands'][step['command_id']]['body']]
  self.assertEqual(len(checks),1,'hosted source fallback is never exercised')
  step=checks[0];self.assertEqual(step['timeout-minutes'],5)
  self.assertEqual(contract['commands'][step['command_id']]['vm_provisioning'],['pinned-tool-release'])
  self.assertNotIn('continue-on-error',step)
 def test_source_verification_refuses_local_host_setup(self):
  with patch.dict(os.environ,{'GITHUB_ACTIONS':'false'}),patch.object(tools,'build_python39') as build:
   with self.assertRaisesRegex(RuntimeError,'GitHub'):tools.verify_python39_source()
   build.assert_not_called()
 def test_completed_cache_reuses_without_download_or_mutation(self):
  with tempfile.TemporaryDirectory() as d:
   cache=Path(d)/'deno/2.7.14';cache.mkdir(parents=True);(cache/'x64').mkdir();(cache/'x64.complete').touch()
   with patch.dict(os.environ,{'GITHUB_ACTIONS':'true','RUNNER_TEMP':d,'RUNNER_TOOL_CACHE':d}),patch.object(tools.platform,'system',return_value='Linux'),patch.object(tools.platform,'machine',return_value='x86_64'),patch.object(tools,'download') as download:
    tools.provision('deno');download.assert_not_called()
 def test_python_uses_setup_actions_agent_tools_directory(self):
  with tempfile.TemporaryDirectory() as d:
   cache=Path(d)/'agent/Python/3.9.25';cache.mkdir(parents=True);(cache/'x64').mkdir();(cache/'x64.complete').touch()
   with patch.dict(os.environ,{'GITHUB_ACTIONS':'true','RUNNER_TEMP':d,'RUNNER_TOOL_CACHE':d+'/other','AGENT_TOOLSDIRECTORY':d+'/agent'}),patch.object(tools.platform,'system',return_value='Linux'),patch.object(tools.platform,'machine',return_value='x86_64'),patch.object(Path,'read_text',return_value='VERSION_ID="24.04"'),patch.object(tools,'download') as download:
    tools.provision('python39');download.assert_not_called()
   self.assertFalse((Path(d)/'other').exists())
 def test_python_binary_outage_tries_independent_source_but_never_executes_corrupt_bytes(self):
  def response(request,**kwargs):
   if request.full_url.startswith('https://www.python.org/'):return io.BytesIO(b'corrupt source')
   raise urllib.error.HTTPError(request.full_url,504,'gateway',{},None)
  with tempfile.TemporaryDirectory() as d:
   with patch.dict(os.environ,{'GITHUB_ACTIONS':'true','RUNNER_TEMP':d,'RUNNER_TOOL_CACHE':d+'/cache','AGENT_TOOLSDIRECTORY':''}),patch.object(tools.platform,'system',return_value='Linux'),patch.object(tools.platform,'machine',return_value='x86_64'),patch.object(Path,'read_text',return_value='VERSION_ID="24.04"'),patch.object(tools.urllib.request,'urlopen',side_effect=response) as request,patch.object(tools.subprocess,'run') as execute:
    with self.assertRaises(RuntimeError):tools.provision('python39')
    self.assertTrue(any(call.args[0].full_url.startswith('https://www.python.org/') for call in request.call_args_list),'binary outage never reached independent source fallback')
    execute.assert_not_called()
   self.assertFalse((Path(d)/'cache/Python/3.9.25/x64.complete').exists())
 def test_scanner_binary_outage_tries_pinned_module_but_never_executes_corrupt_bytes(self):
  def response(request,**kwargs):
   if request.full_url.startswith('https://proxy.golang.org/'):return io.BytesIO(b'corrupt source')
   raise urllib.error.HTTPError(request.full_url,504,'gateway',{},None)
  with tempfile.TemporaryDirectory() as d:
   with patch.dict(os.environ,{'GITHUB_ACTIONS':'true','RUNNER_TEMP':d}),patch.object(tools.platform,'system',return_value='Linux'),patch.object(tools.platform,'machine',return_value='x86_64'),patch.object(tools.urllib.request,'urlopen',side_effect=response) as request,patch.object(tools.subprocess,'run') as execute:
    with self.assertRaises(RuntimeError):tools.provision('gitleaks')
    self.assertTrue(any(call.args[0].full_url.startswith('https://proxy.golang.org/') for call in request.call_args_list),'binary outage never reached module fallback')
    execute.assert_not_called()
   self.assertFalse((Path(d)/'gitleaks').exists())
 def test_incomplete_cache_is_preserved_and_refused(self):
  with tempfile.TemporaryDirectory() as d:
   cache=Path(d)/'deno/2.7.14/x64';cache.mkdir(parents=True);(cache/'owned').write_text('keep')
   with patch.dict(os.environ,{'GITHUB_ACTIONS':'true','RUNNER_TEMP':d,'RUNNER_TOOL_CACHE':d}),patch.object(tools.platform,'system',return_value='Linux'),patch.object(tools.platform,'machine',return_value='x86_64'):
    with self.assertRaisesRegex(RuntimeError,'Incomplete'):tools.provision('deno')
   self.assertEqual((cache/'owned').read_text(),'keep')
if __name__=='__main__':unittest.main()
