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


class AdmissionPublicationTests(unittest.TestCase):
    """Exercise the real supervisor without delegated cgroups or a provider."""
    def start_supervisor(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        receipt, effect = root/'cleanup-complete', root/'effect'
        env = {'PATH':'/usr/bin:/bin', 'JARVIS_SUPERVISOR_ADMISSION':'1',
               'JARVIS_SUPERVISOR_PARENT_PID':str(os.getpid())}
        child = subprocess.Popen([
            '/usr/bin/python3', '-I', str(Path(__file__).parents[1]/'provider-supervisor.py'),
            str(receipt), '/usr/bin/python3', '-I', '-c',
            f'from pathlib import Path; Path({str(effect)!r}).write_text("executed")',
        ], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        def reap():
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=5)
        self.addCleanup(reap)
        deadline = time.monotonic()+5
        while not (root/'launch-ready').exists() and child.poll() is None and time.monotonic()<deadline:
            time.sleep(0.005)
        self.assertTrue((root/'launch-ready').exists(), 'supervisor did not reach admission')
        return root, child, receipt, effect

    def test_incomplete_admission_waits_for_complete_bytes_before_dispatch(self):
        for prefix in (b'', b'admit'):
            with self.subTest(prefix=prefix):
                root, child, receipt, effect = self.start_supervisor()
                with (root/'launch-continue').open('xb') as output:
                    output.write(prefix)
                    output.flush()
                    os.fsync(output.fileno())
                    time.sleep(0.06)
                    self.assertIsNone(child.poll(), 'partial publication was treated as invalid admission')
                    self.assertFalse(effect.exists(), 'provider dispatched before full admission')
                    self.assertFalse(receipt.exists())
                    output.write(b'admitted\n'[len(prefix):])
                    output.flush()
                    os.fsync(output.fileno())
                _, error = child.communicate(timeout=5)
                self.assertEqual(child.returncode, 0, error.decode())
                self.assertTrue(effect.exists())
                self.assertEqual(receipt.read_bytes(), b'all-descendants-reaped\n')

    def test_malformed_admission_never_dispatches(self):
        for value in (b'rejected\n', b'admitted\nextra'):
            with self.subTest(value=value):
                root, child, _, effect = self.start_supervisor()
                (root/'launch-continue').write_bytes(value)
                child.communicate(timeout=5)
                self.assertNotEqual(child.returncode, 0)
                self.assertFalse(effect.exists())

    def test_cancel_during_partial_publication_never_dispatches(self):
        root, child, receipt, effect = self.start_supervisor()
        (root/'launch-continue').write_bytes(b'admit')
        time.sleep(0.06)
        self.assertIsNone(child.poll())
        child.terminate()
        child.communicate(timeout=5)
        self.assertNotEqual(child.returncode, 0)
        self.assertFalse(effect.exists())
        self.assertEqual(receipt.read_bytes(), b'all-descendants-reaped\n')


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


class AdmissionDiagnosticTests(unittest.TestCase):
    def test_receipt_uses_only_fixed_codes_and_bounded_numeric_fields(self):
        error=resources.AdmissionRefused('host_headroom','secret payload /private/path',
            requested_bytes=6*1024**3,available_bytes=5*1024**3,margin_bytes=256*1024**2,
            secret='credential',limit_bytes=-1,current_bytes=True,required_bytes=2**65)
        receipt=resources.refusal_receipt(error)
        self.assertEqual(receipt['refusal_code'],'host_headroom')
        self.assertEqual(receipt['facts'],{'requested_bytes':6*1024**3,'available_bytes':5*1024**3,'margin_bytes':256*1024**2})
        self.assertNotIn('private',json.dumps(receipt));self.assertNotIn('credential',json.dumps(receipt))
        for error in [ValueError('private payload'),OSError(13,'secret detail','/private/credential'),
                      resources.AdmissionRefused('secret-code','secret payload')]:
            receipt=resources.refusal_receipt(error)
            self.assertEqual(receipt['refusal_code'],'admission_setup_failed')
            self.assertNotIn('secret',json.dumps(receipt));self.assertNotIn('private',json.dumps(receipt))
        self.assertEqual(resources.refusal_receipt(PermissionError(13,'secret'))['errno'],13)

    def test_real_supervisor_refuses_before_dispatch_and_retains_safe_reason(self):
        import shutil
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);receipt=root/'cleanup-complete';effect=root/'must-not-exist'
            for name in ['provider-supervisor.py','run-resource-scope.py']:
                shutil.copyfile(Path(__file__).parents[1]/name,root/name)
            # Validation precedes cgroup access, so this runs without delegation.
            env={'PATH':'/usr/bin:/bin','JARVIS_SUPERVISOR_ADMISSION':'1',
                 'JARVIS_RUN_RESOURCE_POLICY':json.dumps({'memory_bytes':1,'pids':8,'cpu_percent':20})}
            completed=subprocess.run(['/usr/bin/python3','-I',str(root/'provider-supervisor.py'),str(receipt),
                '/usr/bin/python3','-I','-c',f'from pathlib import Path;Path({str(effect)!r}).write_text("started")'],
                env=env,capture_output=True,timeout=5)
            self.assertNotEqual(completed.returncode,0)
            self.assertFalse(effect.exists(),'no subprocess/model may dispatch on refusal')
            self.assertFalse((root/'launch-ready').exists())
            self.assertEqual(receipt.read_text(),'all-descendants-reaped\n')
            refusal=json.loads((root/'resource-accounting.json').read_text())
            self.assertEqual(refusal['phase'],'admission_failed');self.assertTrue(refusal['exhausted'])
            self.assertEqual(refusal['refusal_code'],'invalid_policy')
            self.assertEqual(refusal['facts']['memory_bytes'],1)
            self.assertEqual(refusal['facts']['minimum'],64*1024**2)
            self.assertEqual((root/'resource-accounting.json').stat().st_mode&0o777,0o600)

class CompositionHeadroomTests(unittest.TestCase):
    def test_first_admission_enables_child_accounting_before_reading_headroom(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent=Path(tmp)
            controls=parent/'cgroup.subtree_control'
            controls.write_text('')
            events=[]
            original=Path.write_text
            def enable(path, value, *args, **kwargs):
                if path==controls:
                    events.append('enabled')
                    return original(path, 'cpu memory pids')
                return original(path,value,*args,**kwargs)
            def check(path, requested, available):
                self.assertEqual(events,['enabled'])
                self.assertEqual(controls.read_text(),'cpu memory pids')
                events.append('headroom')
                return {'requested_bytes':requested}
            with patch.object(Path,'write_text',enable), patch.object(resources,'headroom',check), patch.object(resources,'available_memory',return_value=1024**3):
                with resources.reserve(parent,64*1024**2) as proof:
                    self.assertEqual(proof['requested_bytes'],64*1024**2)
            self.assertEqual(events,['enabled','headroom'])

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
            with self.assertRaisesRegex(RuntimeError,'host memory') as caught:
                resources.headroom(parent,6*gib,5*gib)
            refusal=resources.refusal_receipt(caught.exception)
            self.assertEqual(refusal['refusal_code'],'host_headroom')
            self.assertEqual(refusal['facts']['available_bytes'],5*gib)
            self.assertEqual(refusal['facts']['required_bytes'],6*gib+256*1024**2)

    def test_reservation_tracks_cleanup_not_idle_rss(self):
        gib=1024**3
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp),8*gib,gib,(6*gib,0))
            with self.assertRaisesRegex(RuntimeError,'reservations exhausted'):
                resources.headroom(parent,2*gib,20*gib)
            import shutil
            shutil.rmtree(parent/('run-'+'a'*32))
            self.assertEqual(resources.headroom(parent,6*gib,20*gib)['requested_bytes'],6*gib)


    def test_reparented_page_cache_is_not_a_reservation(self):
        # 2026-10-28: 3.3 GiB of inactive_file left by removed run groups sat
        # on the service beside a 278 MB daemon leaf and one 4 GiB run, and a
        # 4 GiB text turn was refused as "reservations exhausted".
        gib=1024**3
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp),10*gib,gib,(4*gib,128*1024**2))
            charged=int((parent/'memory.current').read_text())
            (parent/'memory.current').write_text(str(charged+3*gib))
            (parent/'memory.stat').write_text(f'anon {charged}\ninactive_file {3*gib}\nactive_file 0\n')
            proof=resources.headroom(parent,4*gib,20*gib)
            self.assertEqual(proof['reserved_before_bytes'],5*gib)
            # Cache the kernel cannot drop yet (active) still counts.
            (parent/'memory.stat').write_text(f'anon {charged}\ninactive_file {gib}\nactive_file {2*gib}\n')
            with self.assertRaisesRegex(RuntimeError,'reservations exhausted') as caught:
                resources.headroom(parent,4*gib,20*gib)
            facts=resources.refusal_receipt(caught.exception)['facts']
            self.assertEqual(facts['reclaimable_bytes'],gib)
            self.assertEqual(facts['reserved_bytes'],7*gib)

    def test_a_child_s_own_inactive_cache_does_not_reserve_its_usage(self):
        gib=1024**3
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp),8*gib,3*gib)
            (parent/'daemon'/'memory.stat').write_text(f'inactive_file {2*gib}\n')
            (parent/'memory.stat').write_text(f'inactive_file {2*gib}\n')
            self.assertEqual(resources.headroom(parent,6*gib,20*gib)['reserved_before_bytes'],gib)


class TransientAdmissionTests(unittest.TestCase):
    def fixture(self, root):
        parent=root/'fixture.service'; parent.mkdir()
        (parent/'cgroup.subtree_control').write_text('')
        return parent

    def refusal(self):
        return resources.AdmissionRefused('aggregate_reservations','aggregate memory reservations exhausted',
            requested_bytes=4,reserved_bytes=8,current_bytes=8,limit_bytes=10)

    def run_reserve(self, parent, outcomes, wait=60):
        calls=[];sleeps=[]
        def headroom(_parent, requested, _available):
            calls.append(requested)
            outcome=outcomes[min(len(calls),len(outcomes))-1]
            if isinstance(outcome,Exception): raise outcome
            return outcome
        original=Path.write_text
        def write(path, value, *args, **kwargs):
            if path.name=='cgroup.subtree_control': value='cpu memory pids'
            return original(path,value,*args,**kwargs)
        with patch.object(resources,'headroom',headroom), patch.object(resources,'available_memory',return_value=1), \
             patch.object(Path,'write_text',write):
            with resources.reserve(parent,4,wait_seconds=wait,sleep=sleeps.append) as proof:
                pass
        return proof,calls,sleeps

    def test_a_transient_refusal_reclaims_cache_then_admits_without_waiting(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp))
            proof,calls,sleeps=self.run_reserve(parent,[self.refusal(),{'ok':1}])
            self.assertEqual(proof,{'ok':1})
            self.assertEqual(len(calls),2)
            self.assertEqual(sleeps,[],'reclaim retries immediately')
            self.assertEqual((parent/'memory.reclaim').read_text(),'2 swappiness=0',
                'reclaims only the shortfall, file pages only')

    def test_capacity_held_by_a_sibling_waits_with_backoff_until_it_frees(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp))
            outcomes=[self.refusal()]*4+[{'ok':1}]
            proof,calls,sleeps=self.run_reserve(parent,outcomes)
            self.assertEqual(proof,{'ok':1})
            self.assertEqual(sleeps,[0.25,0.5,1.0])

    def test_a_refusal_that_outlasts_the_wait_still_refuses(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp))
            with self.assertRaisesRegex(RuntimeError,'reservations exhausted'):
                self.run_reserve(parent,[self.refusal()],wait=0)

    def test_a_ceiling_that_can_never_fit_refuses_immediately(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent=self.fixture(Path(tmp))
            never=resources.AdmissionRefused('declared_ceiling','run ceiling exceeds aggregate service ceiling')
            with self.assertRaisesRegex(RuntimeError,'ceiling'):
                self.run_reserve(parent,[never])
            self.assertFalse((parent/'memory.reclaim').exists())


if __name__ == '__main__': unittest.main(verbosity=2)
