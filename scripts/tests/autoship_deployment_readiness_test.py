import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
spec=importlib.util.spec_from_file_location('deployment',Path(__file__).parents[1]/'autoship-deployment-readiness.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

def birth(pid):return Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[19]
def write(path,value):
    path.write_text(json.dumps(value));path.chmod(0o600)

class DeploymentTests(unittest.TestCase):
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
                'boot_id':Path('/proc/sys/kernel/random/boot_id').read_text().strip(),'dry_run':False,'external':True,
                'config':{'AUGMENTAGENT_AUTOPR':'1','HOME':str(root),'PATH':'/usr/bin:/bin','AUGMENTAGENT_NATIVE_VM_POLICY':str(policy),
                          'CLAUDE_CLI':'/usr/bin/true','CODEX_CLI':'/usr/bin/true','AUGMENTAGENT_RUN_RESOURCES':'1'}} for role,child in children.items()}
            for case in ('matched','worker-missing-policy','drift','stale','missing-receipt','public','wrong-binary','dry-run','secret-field','changed-policy','worker-stopped','state-drift','route-drift'):
                with self.subTest(case=case):
                    current=json.loads(json.dumps(values))
                    if case=='worker-missing-policy':current['worker']['config'].pop('AUGMENTAGENT_NATIVE_VM_POLICY')
                    if case=='drift':current['worker']['config']['AUGMENTAGENT_RUN_PIDS']='8'
                    if case=='state-drift':current['worker']['config']['XDG_STATE_HOME']=str(root/'elsewhere')
                    if case=='route-drift':
                        route=root/'router.json';write(route,{'mode':'direct','api_key':'do-not-output-this-value'})
                        current['worker']['config']['AUGMENTAGENT_MODEL_ROUTER_CONFIG']=str(route)
                    if case=='stale':current['worker']['birth']='0'
                    if case=='dry-run':current['worker']['dry_run']=True
                    if case=='secret-field':current['worker']['config']['API_KEY']='do-not-output-this-value'
                    for role,value in current.items():write(receipts/(role+'.json'),value)
                    if case=='missing-receipt':(receipts/'worker.json').unlink()
                    if case=='public':(receipts/'worker.json').chmod(0o644)
                    expected=binary if case!='wrong-binary' else Path('/usr/bin/true')
                    before={str(p):p.read_bytes() for p in receipts.iterdir()}
                    calls=[]
                    def pid_for(role):
                        calls.append(role)
                        if case=='changed-policy' and len(calls)==4:write(policy,{'version':1,'providers':{'claude':{},'codex':{}},'changed':True})
                        return 0 if case=='worker-stopped' and role=='worker' else children[role].pid
                    report=m.inspect(expected,selection,receipts,pid_for)
                    self.assertEqual(report['matches'],case=='matched',report)
                    self.assertFalse(report['live_verified'])
                    if case=='matched':self.assertEqual(report['status'],'configuration-matched')
                    if case=='worker-missing-policy':self.assertEqual(report['services']['worker']['status'],'policy-not-selected')
                    if case=='worker-stopped':self.assertTrue(report['expected_active'])
                    if case in ('drift','state-drift','route-drift'):self.assertEqual(report['status'],'effective-configuration-mismatch')
                    if case=='stale':self.assertEqual(report['services']['worker']['status'],'process-receipt-stale')
                    if case=='wrong-binary':self.assertEqual(report['services']['worker']['status'],'executable-mismatch')
                    if case=='changed-policy':self.assertEqual(report['status'],'selection-changed')
                    self.assertNotIn('do-not-output-this-value',json.dumps(report))
                    self.assertEqual(before,{str(p):p.read_bytes() for p in receipts.iterdir()})
                    write(policy,{'version':1,'providers':{'claude':{},'codex':{}}})

if __name__=='__main__':unittest.main()
