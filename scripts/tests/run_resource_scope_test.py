"""Real bounded descendants; run under an isolated delegated transient service.

systemd-run --user --wait --pipe --collect -p Delegate='cpu memory pids' \
 -p DelegateSubgroup=daemon -p MemoryMax=256M -p TasksMax=64 -p CPUQuota=100% \
 /usr/bin/python3 /absolute/path/to/run_resource_scope_test.py
"""
import importlib.util
import json
import tempfile
import time
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('resources', Path(__file__).parents[1]/'run-resource-scope.py')
resources = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resources)


class ResourceTests(unittest.TestCase):
    def setUp(self):
        if resources.current_group().name != 'daemon':
            self.skipTest('requires isolated DelegateSubgroup=daemon transient service')
        self.scope = resources.RunScope({'memory_bytes':64*1024*1024,'pids':8,'cpu_percent':20})
        self.addCleanup(self.scope.close)

    def run_child(self, code):
        return subprocess.run(['/usr/bin/python3','-I','-c',code],
                              preexec_fn=self.scope.attach_child,
                              capture_output=True,text=True,timeout=10)

    def test_memory_exhaustion_is_local_and_observed(self):
        result=self.run_child('x=bytearray(128*1024*1024); print(len(x))')
        self.assertNotEqual(result.returncode,0)
        observation=self.scope.observation()
        self.assertTrue(observation['exhausted'])
        self.assertGreater(observation['memory_events']['oom_kill'],0)
        self.assertEqual(resources.current_group().name,'daemon')

    def test_process_exhaustion_is_local_and_observed(self):
        result=self.run_child('''import subprocess
children=[]
try:
 for _ in range(32): children.append(subprocess.Popen(['/bin/sleep','10']))
 raise SystemExit('PID ceiling failed')
except BlockingIOError:
 print('PIDS_LIMIT_OBSERVED')
finally:
 for child in children: child.kill()
 for child in children: child.wait()
''')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertIn('PIDS_LIMIT_OBSERVED',result.stdout)
        self.assertTrue(self.scope.observation()['exhausted'])

    def test_cpu_throttling_and_detached_descendants_keep_membership(self):
        result=self.run_child('''import os,time,subprocess
os.setsid()
end=time.monotonic()+0.6
while time.monotonic()<end: pass
print(open('/proc/self/cgroup').read().strip())
subprocess.Popen(['/bin/sleep','10'],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
''')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertIn(self.scope.path.name,result.stdout)
        observation=self.scope.observation()
        self.assertGreater(observation['cpu_stat']['nr_throttled'],0)
        self.assertFalse(observation['exhausted'])
        path=self.scope.path
        self.scope.close()
        self.assertFalse(path.exists())

    def test_worker_command_cannot_migrate_to_controller_or_rewrite_limits(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);policy=root/'sandbox-policy.json'
            policy.write_text(json.dumps({'cwd':str(root),'read_roots':[str(root)],'write_roots':[]}));policy.chmod(0o600)
            destinations=[str(self.scope.path.parent/'daemon'/'cgroup.procs'),str(self.scope.path/'memory.max')]
            code=f"import os; paths={destinations!r}\nfor path in paths:\n try:\n  open(path,'w').write(str(os.getpid()))\n  raise SystemExit('resource control write escaped')\n except PermissionError: pass\n"
            result=subprocess.run(['/usr/bin/python3','-I',str(Path(__file__).parents[1]/'codex-command-sandbox.py'),str(policy),'/usr/bin/python3','-I','-c',code],preexec_fn=self.scope.attach_child,capture_output=True,text=True,timeout=5)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual((self.scope.path/'memory.max').read_text().strip(),str(64*1024*1024))

    def test_supervisor_waits_for_controller_admission_and_records_effective_limits(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);receipt=root/'cleanup-complete';effect=root/'effect'
            env=dict(os.environ,JARVIS_SUPERVISOR_ADMISSION='1',
                     JARVIS_RUN_RESOURCE_POLICY=json.dumps({'memory_bytes':64*1024*1024,'pids':8,'cpu_percent':20}))
            child=subprocess.Popen(['/usr/bin/python3','-I',str(Path(__file__).parents[1]/'provider-supervisor.py'),str(receipt),
                                    '/usr/bin/python3','-I','-c',f'from pathlib import Path; Path({str(effect)!r}).write_text("executed")'],env=env)
            try:
                deadline=time.monotonic()+3
                while not (root/'launch-ready').exists() and time.monotonic()<deadline: time.sleep(0.01)
                self.assertTrue((root/'launch-ready').exists())
                self.assertFalse(effect.exists())
                admitted=json.loads((root/'resource-accounting.json').read_text())
                self.assertEqual(admitted['phase'],'admitted')
                self.assertEqual(admitted['identity']['limits']['memory.max'],str(64*1024*1024))
                (root/'launch-continue').write_text('admitted\n')
                self.assertEqual(child.wait(timeout=5),0)
                self.assertTrue(effect.exists())
                completed=json.loads((root/'resource-accounting.json').read_text())
                self.assertEqual(completed['phase'],'completed')
                self.assertFalse(Path(completed['identity']['path']).exists())
            finally:
                if child.poll() is None: child.terminate();child.wait(timeout=5)

    def test_dead_supervisor_descendants_are_reconciled_by_cgroup_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);receipt=root/'cleanup-complete';effect=root/'started'
            env=dict(os.environ,JARVIS_RUN_RESOURCE_POLICY=json.dumps({'memory_bytes':64*1024*1024,'pids':8,'cpu_percent':20}))
            code=f"import subprocess,time;from pathlib import Path;subprocess.Popen(['/bin/sleep','30'],start_new_session=True);Path({str(effect)!r}).touch();time.sleep(30)"
            child=subprocess.Popen(['/usr/bin/python3','-I',str(Path(__file__).parents[1]/'provider-supervisor.py'),str(receipt),'/usr/bin/python3','-I','-c',code],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            deadline=time.monotonic()+3
            while not effect.exists() and time.monotonic()<deadline:time.sleep(0.01)
            self.assertTrue(effect.exists())
            child.kill();child.wait(timeout=3)
            proof=root/'resource-accounting.json'
            scope=Path(json.loads(proof.read_text())['identity']['path'])
            self.assertTrue(scope.exists())
            resources.cleanup_receipt(proof)
            self.assertFalse(scope.exists())
            self.assertFalse(receipt.exists(),'a dead supervisor did not personally reap descendants')

    def test_denied_admission_never_dispatches_provider(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);receipt=root/'cleanup-complete';effect=root/'effect'
            env=dict(os.environ,JARVIS_SUPERVISOR_ADMISSION='1',
                     JARVIS_RUN_RESOURCE_POLICY=json.dumps({'memory_bytes':1024**3,'pids':8,'cpu_percent':20}))
            child=subprocess.run(['/usr/bin/python3','-I',str(Path(__file__).parents[1]/'provider-supervisor.py'),str(receipt),
                                    '/usr/bin/python3','-I','-c',f'from pathlib import Path; Path({str(effect)!r}).touch()'],env=env,capture_output=True,timeout=5)
            self.assertNotEqual(child.returncode,0)
            self.assertFalse(effect.exists())
            self.assertFalse((root/'launch-ready').exists())
            self.assertEqual(json.loads((root/'resource-accounting.json').read_text())['phase'],'admission_failed')
            self.assertEqual(receipt.read_text(),'all-descendants-reaped\n')

    def test_failed_limit_readback_removes_only_new_empty_cgroup(self):
        parent=self.scope.path.parent
        before={path.name for path in parent.glob('run-*')}
        original=Path.read_text
        def failed_readback(path,*args,**kwargs):
            if path.name=='pids.max' and path.parent.name.startswith('run-'):
                return '999999\n'
            return original(path,*args,**kwargs)
        with patch.object(Path,'read_text',failed_readback):
            with self.assertRaisesRegex(RuntimeError,'readback'):
                resources.RunScope({'memory_bytes':64*1024*1024,'pids':8,'cpu_percent':20})
        self.assertEqual({path.name for path in parent.glob('run-*')},before)
        self.assertTrue(self.scope.path.exists(),'unrelated active run must survive cleanup')

    def test_ceiling_cannot_exceed_original_aggregate(self):
        with self.assertRaises(RuntimeError):
            resources.RunScope({'memory_bytes':1024**3,'pids':8,'cpu_percent':20})


class CompositionHeadroomTests(unittest.TestCase):
    def fixture(self, root, total, daemon, run=None):
        parent=root/'fixture.service'; parent.mkdir()
        (parent/'memory.max').write_text(str(total))
        (parent/'memory.current').write_text(str(daemon+(run[1] if run else 0)))
        child=parent/'daemon';child.mkdir();(child/'memory.current').write_text(str(daemon))
        if run:
            child=parent/('run-'+'a'*32);child.mkdir()
            (child/'memory.max').write_text(str(run[0]));(child/'memory.current').write_text(str(run[1]))
        return parent

    def test_full_composed_build_fits_but_second_lazy_guest_is_not_overcommitted(self):
        gib=1024**3
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp),10*gib,gib,(2*gib,128*1024**2))
            proof=resources.headroom(parent,6*gib,20*gib)
            self.assertEqual(proof['reserved_before_bytes'],3*gib)
            with self.assertRaisesRegex(RuntimeError,'reservations exhausted'):
                resources.headroom(parent,8*gib,20*gib)

    def test_insufficient_host_headroom_refuses_before_guest_admission(self):
        gib=1024**3
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp),10*gib,gib)
            with self.assertRaisesRegex(RuntimeError,'host memory'):
                resources.headroom(parent,6*gib,5*gib)

    def test_reservation_tracks_cleanup_not_idle_rss(self):
        gib=1024**3
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp),8*gib,gib,(6*gib,0))
            with self.assertRaisesRegex(RuntimeError,'reservations exhausted'):
                resources.headroom(parent,2*gib,20*gib)
            import shutil
            shutil.rmtree(parent/('run-'+'a'*32))
            self.assertEqual(resources.headroom(parent,6*gib,20*gib)['requested_bytes'],6*gib)


if __name__ == '__main__': unittest.main(verbosity=2)
