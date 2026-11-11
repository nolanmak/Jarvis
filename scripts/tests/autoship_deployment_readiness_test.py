import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
spec=importlib.util.spec_from_file_location('deployment',Path(__file__).parents[1]/'autoship-deployment-readiness.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

def birth(pid):return Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[19]
def write(path,value):
    path.write_text(json.dumps(value));path.chmod(0o600)

class DeploymentTests(unittest.TestCase):
    @unittest.skipUnless(Path('/proc/self/stat').exists(),'Linux process-bound evidence')
    def test_workflow_store_uses_effective_worker_receipt_and_rejects_stale_or_changed_process(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);receipts=root/'receipts';receipts.mkdir(mode=0o700)
            child=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(30)'],cwd=root)
            self.addCleanup(lambda:child.poll() is None and (child.terminate(),child.wait()))
            value={'version':1,'role':'worker','pid':child.pid,'birth':birth(child.pid),'boot_id':m.boot(),
                'dry_run':False,'config':{'HOME':str(root/'worker-home'),'XDG_STATE_HOME':str(root/'worker-state'),
                    'AUGMENTAGENT_AUTOPR':'1','AUGMENTAGENT_AUTOPR_WORKFLOW_DB':'worker.db','AUGMENTAGENT_AUTOPR_DAILY_CAP':'7','AUGMENTAGENT_NATIVE_VM_POLICY':'native/policy.json'}}
            path=receipts/'worker.json';write(path,value);before=path.read_bytes()
            expected={'cwd':str(root),'workflow_db':'worker.db','state_home':str(root/'worker-state'),'home':str(root/'worker-home'),'daily_cap':'7','native_policy':'native/policy.json'}
            self.assertEqual(m.workflow_store_configuration(receipts,lambda role:child.pid),expected)
            self.assertEqual(path.read_bytes(),before)
            for version in (0,2,3,True,'2'):
                current=json.loads(before);current['version']=version;write(path,current)
                with self.subTest(version=version),self.assertRaises(m.Refused):m.workflow_store_configuration(receipts,lambda role:child.pid)
            current=json.loads(before)
            for key in ('HOME','XDG_STATE_HOME','AUGMENTAGENT_AUTOPR_WORKFLOW_DB'):current['config'].pop(key,None)
            write(path,current)
            with self.assertRaises(m.Refused):m.workflow_store_configuration(receipts,lambda role:child.pid)
            write(path,value)
            for fault in ('birth','public','secret','changed-process'):
                current=json.loads(before)
                if fault=='birth':current['birth']='stale'
                if fault=='secret':current['config']['API_KEY']='must-not-output'
                write(path,current)
                if fault=='public':path.chmod(0o644)
                calls=[]
                def pid_for(role,calls=calls,fault=fault):
                    calls.append(role)
                    return 0 if fault=='changed-process' and len(calls)>1 else child.pid
                with self.subTest(fault=fault),self.assertRaises(m.Refused):m.workflow_store_configuration(receipts,pid_for)
            child.terminate();child.wait();write(path,value)
            with self.assertRaises(m.Refused):m.workflow_store_configuration(receipts,lambda role:child.pid)

    @unittest.skipUnless(Path('/proc/self/stat').exists(),'Linux process-bound evidence')
    def test_activation_detects_path_and_content_changes_but_refuses_stale_process_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);receipts=root/'receipts';receipts.mkdir(mode=0o700)
            policy=root/'policy.json';write(policy,{'version':1,'providers':{'claude':{}}})
            selection=root/'selection';selection.write_text('AUGMENTAGENT_NATIVE_VM_POLICY="'+str(policy)+'"');selection.chmod(0o600)
            value={'version':1,'role':'daemon','pid':os.getpid(),'birth':birth(os.getpid()),'boot_id':m.boot(),
                   'external':True,'config':{'AUGMENTAGENT_AUTOPR':'1','AUGMENTAGENT_NATIVE_VM_POLICY':str(policy)},'policy_sha256':m.digest(policy.read_bytes())}
            receipt=receipts/'daemon.json';write(receipt,value)
            pid_for=lambda role:os.getpid()
            self.assertFalse(m.activation_needed(selection,receipts,pid_for))
            write(policy,{'version':1,'providers':{'codex':{}}})
            self.assertTrue(m.activation_needed(selection,receipts,pid_for))
            value['policy_sha256']=m.digest(policy.read_bytes());value['config']['AUGMENTAGENT_NATIVE_VM_POLICY']='/other/policy.json';write(receipt,value)
            self.assertTrue(m.activation_needed(selection,receipts,pid_for))
            value['birth']='stale';write(receipt,value)
            with self.assertRaises(m.Refused):m.activation_needed(selection,receipts,pid_for)

    @unittest.skipUnless(Path('/proc/self/stat').exists(),'Linux process-bound evidence')
    def test_another_serve_cannot_replace_a_live_receipt(self):
        with tempfile.TemporaryDirectory() as temporary:
            receipts=Path(temporary)/'receipts';receipts.mkdir(mode=0o700)
            child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'])
            self.addCleanup(lambda:child.poll() is None and (child.terminate(),child.wait()))
            path=receipts/'daemon.json'
            write(path,{'version':1,'role':'daemon','pid':child.pid,'birth':birth(child.pid),'boot_id':m.boot()})
            before=path.read_bytes()
            with self.assertRaises(m.Refused):m.record('daemon',os.getppid(),receipts,False,True)
            self.assertEqual(path.read_bytes(),before)
            child.terminate();child.wait()
            m.record('daemon',os.getppid(),receipts,False,True)
            self.assertEqual(json.loads(path.read_bytes())['pid'],os.getppid())

    @unittest.skipUnless(Path('/proc/self/stat').exists(),'Linux process-bound evidence')
    def test_effective_receipts_detect_missing_worker_drift_and_stale_process_without_writes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);receipts=root/'receipts';receipts.mkdir(mode=0o700)
            policy=root/'policy.json';write(policy,{'version':1,'providers':{'claude':{},'codex':{}}})
            selection=root/'selection.env';selection.write_text('AUGMENTAGENT_NATIVE_VM_POLICY="'+str(policy)+'"\n');selection.chmod(0o600)
            children={role:subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'],env={'PATH':'/usr/bin:/bin'}) for role in ('daemon','worker')}
            self.addCleanup(lambda:[(child.terminate(),child.wait()) for child in children.values()])
            binary=Path(f'/proc/{children["daemon"].pid}/exe').resolve()
            values={role:{'version':1,'role':role,'pid':child.pid,'birth':birth(child.pid),
                'boot_id':Path('/proc/sys/kernel/random/boot_id').read_text().strip(),'policy_sha256':m.digest(policy.read_bytes()),'dry_run':False,'external':True,
                'config':{'AUGMENTAGENT_AUTOPR':'1','HOME':str(root),'PATH':'/usr/bin:/bin','AUGMENTAGENT_NATIVE_VM_POLICY':str(policy),
                          'CLAUDE_CLI':'/usr/bin/true','CODEX_CLI':'/usr/bin/true','AUGMENTAGENT_RUN_RESOURCES':'1'}} for role,child in children.items()}
            for case in ('matched','provider-update','provider-drift','worker-missing-policy','drift','stale','missing-receipt','public','wrong-binary','dry-run','secret-field','changed-policy','worker-stopped','state-drift','route-drift'):
                with self.subTest(case=case):
                    current=json.loads(json.dumps(values))
                    if case=='worker-missing-policy':current['worker']['config'].pop('AUGMENTAGENT_NATIVE_VM_POLICY')
                    if case=='drift':current['worker']['config']['AUGMENTAGENT_RUN_PIDS']='8'
                    if case=='provider-drift':current['worker']['config']['CLAUDE_CLI']='/usr/bin/false'
                    if case=='state-drift':current['worker']['config']['XDG_STATE_HOME']=str(root/'elsewhere')
                    if case=='route-drift':
                        route=root/'router.json';write(route,{'mode':'direct','api_key':'do-not-output-this-value'})
                        current['worker']['config']['AUGMENTAGENT_MODEL_ROUTER_CONFIG']=str(route)
                    if case=='stale':current['worker']['birth']='0'
                    if case=='dry-run':current['worker']['dry_run']=True
                    if case=='secret-field':current['worker']['config']['API_KEY']='do-not-output-this-value'
                    provider = root/'claude'
                    if case=='provider-update':
                        provider.symlink_to('/usr/bin/true')
                        for value in current.values():
                            value['config']['CLAUDE_CLI']=str(provider)
                    for role,value in current.items():write(receipts/(role+'.json'),value)
                    if case=='missing-receipt':(receipts/'worker.json').unlink()
                    if case=='public':(receipts/'worker.json').chmod(0o644)
                    expected=binary if case!='wrong-binary' else Path('/usr/bin/true')
                    before={str(p):p.read_bytes() for p in receipts.iterdir()}
                    calls=[]
                    def pid_for(role,calls=calls,case=case):
                        calls.append(role)
                        if case=='changed-policy' and len(calls)==4:write(policy,{'version':1,'providers':{'claude':{},'codex':{}},'changed':True})
                        return 0 if case=='worker-stopped' and role=='worker' else children[role].pid
                    snapshot=m.snapshot
                    def snapshot_during_update(role,*args):
                        result=snapshot(role,*args)
                        if case=='provider-update' and role=='daemon' and provider.readlink()==Path('/usr/bin/true'):
                            replacement=root/'new-claude'
                            replacement.symlink_to('/usr/bin/false')
                            replacement.replace(provider)
                        return result
                    with patch.object(m,'snapshot',side_effect=snapshot_during_update):
                        report=m.inspect(expected,selection,receipts,pid_for)
                    self.assertEqual(report['matches'],case in ('matched','provider-update'),report)
                    self.assertFalse(report['live_verified'])
                    if case=='matched':self.assertEqual(report['status'],'configuration-matched')
                    if case=='worker-missing-policy':self.assertEqual(report['services']['worker']['status'],'policy-not-selected')
                    if case=='worker-stopped':self.assertTrue(report['expected_active'])
                    if case in ('drift','state-drift','route-drift','provider-drift'):self.assertEqual(report['status'],'effective-configuration-mismatch')
                    if case in ('provider-update','provider-drift'):self.assertEqual(len(calls),8)
                    if case=='stale':self.assertEqual(report['services']['worker']['status'],'process-receipt-stale')
                    if case=='wrong-binary':self.assertEqual(report['services']['worker']['status'],'executable-mismatch')
                    if case=='changed-policy':self.assertEqual(report['status'],'selection-changed')
                    self.assertNotIn('do-not-output-this-value',json.dumps(report))
                    self.assertEqual(before,{str(p):p.read_bytes() for p in receipts.iterdir()})
                    write(policy,{'version':1,'providers':{'claude':{},'codex':{}}})

if __name__=='__main__':unittest.main()
