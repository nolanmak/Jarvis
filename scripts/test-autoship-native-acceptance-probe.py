#!/usr/bin/env python3
"""Pure probe preflight arithmetic: never starts a service, VM, build or model."""
import importlib.util
from pathlib import Path
import unittest
import tempfile
import json
import os
import signal
import subprocess
import sys
import threading
import time

spec=importlib.util.spec_from_file_location('native_probe',Path(__file__).with_name('autoship-native-acceptance-probe.py'))
probe=importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
GIB=1024**3
MIB=1024**2


class Tests(unittest.TestCase):
    def test_claude_guarded_edit_requires_its_own_ignored_test_and_file_only_budget(self):
        expected='self_improve::native_route_acceptance::live_native_guarded_claude_file_edit'
        self.assertEqual(probe.TESTS.get('claude_file_edit'),expected,
                         'Claude must execute its guarded native edit test, not the Codex test or transport helper')
        with tempfile.TemporaryDirectory() as temporary:
            binary=Path(temporary)/'compiled-inventory-fixture'
            binary.write_text('#!/usr/bin/python3\nimport sys\nassert sys.argv[1:]=='+repr(['--list','--ignored','--exact',expected])+"\nprint(sys.argv[-1]+': test')\nprint('1 test, 0 benchmarks')\n")
            binary.chmod(0o700)
            self.assertEqual(len(probe.verify_test_inventory(binary,probe.TESTS['claude_file_edit'])),64)
            with self.assertRaises(ValueError):probe.verify_test_inventory(binary,probe.TESTS['file_edit'])
        self.assertEqual(probe.reachable_build_memory('claude_file_edit',4096),0)
        budget=probe.resource_budget(1024,probe.reachable_build_memory('claude_file_edit',4096),3*GIB)
        self.assertEqual(budget['run_memory_bytes'],2*GIB)
        self.assertEqual(budget['final_admission'],'RunScope')

    def test_service_stop_without_bound_test_completion_is_not_success(self):
        test=probe.TESTS['adoption'];nonce='fixture-generation'
        passed=f"test {test} ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n"
        receipt={'nonce':nonce,'exit_code':0}
        for value,transcript in [(None,'running 1 test\n'),(None,passed),({'nonce':'old','exit_code':0},passed),(receipt,'running 1 test\n'),(receipt,passed.replace('1 passed','0 passed'))]:
            with self.subTest(receipt=value,transcript=transcript):
                self.assertNotEqual(probe.completion_status(0,value,nonce,transcript,test),0)
        self.assertEqual(probe.completion_status(0,receipt,nonce,passed,test),0)
        self.assertNotEqual(probe.completion_status(1,receipt,nonce,passed,test),0)
        self.assertNotEqual(probe.completion_status(0,{'nonce':nonce,'exit_code':1},nonce,passed,test),0)

    def test_actual_wrapper_records_completion_but_not_interrupted_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);test=probe.TESTS['adoption'];nonce='actual-wrapper'
            for interrupted in (False,True):
                receipt=root/('stopped.json' if interrupted else 'completed.json')
                marker=root/'started'
                child="import pathlib,time;pathlib.Path("+repr(str(marker))+").touch();time.sleep(30)" if interrupted else "print("+repr(f"test {test} ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s")+")"
                process=subprocess.Popen([sys.executable,'-I','-c',probe.COMPLETION_WRAPPER,str(receipt),nonce,sys.executable,'-c',child],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
                try:
                    if interrupted:
                        deadline=time.monotonic()+5
                        while not marker.exists() and time.monotonic()<deadline:time.sleep(.01)
                        self.assertTrue(marker.exists(),'child must actually start before interruption')
                        os.killpg(process.pid,signal.SIGTERM)
                    stdout,stderr=process.communicate(timeout=5)
                    value=json.loads(receipt.read_text()) if receipt.exists() else None
                    self.assertEqual(probe.completion_status(0,value,nonce,stdout,test)==0,not interrupted,stderr)
                    self.assertEqual(receipt.exists(),not interrupted)
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid,signal.SIGKILL);process.communicate()

    def test_explicit_deferred_mode_preserves_full_limits_and_requires_controller_floor(self):
        available=6300*MIB
        with self.assertRaises(ValueError):probe.resource_budget(1024,4096,available)
        budget=probe.resource_budget(1024,4096,available,defer_run_admission=True)
        self.assertTrue(budget['deferred_run_admission'])
        self.assertFalse(budget['run_admitted_by_preflight'])
        self.assertEqual(budget['run_memory_bytes'],6*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],8*GIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+512*MIB)
        self.assertEqual(budget['startup_floor_bytes'],512*MIB)
        self.assertEqual(budget['final_admission'],'RunScope')
        self.assertEqual(probe.resource_budget(1024,4096,512*MIB,defer_run_admission=True)['startup_floor_bytes'],512*MIB)
        with self.assertRaises(ValueError):probe.resource_budget(1024,4096,512*MIB-1,defer_run_admission=True)
        with self.assertRaises(ValueError):probe.resource_budget(1024,4096,8*GIB,defer_run_admission='yes')

    def test_exact_ignored_inventory_required_before_launch(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);binary=root/'fixture-binary';mode=root/'mode'
            expected=probe.TESTS['routed_transport']
            binary.write_text("#!/usr/bin/python3\nimport pathlib,sys\nroot=pathlib.Path(__file__).parent\nassert sys.argv[1:]==['--list','--ignored','--exact',"+repr(expected)+"]\nmode=(root/'mode').read_text()\nif mode=='error':raise SystemExit(1)\nif mode=='exact':print(sys.argv[-1]+': test');print('1 test, 0 benchmarks')\nelif mode=='other':print('different_test: test')\nelse:print('0 tests, 0 benchmarks')\n")
            binary.chmod(0o700)
            for value in ['zero','other','error']:
                mode.write_text(value)
                with self.assertRaises(ValueError):probe.verify_test_inventory(binary,expected)
            mode.write_text('exact')
            self.assertEqual(len(probe.verify_test_inventory(binary,expected)),64)

    def test_reviewer_probes_do_not_reserve_unreachable_build_vm(self):
        for name in ('writer','adjudicator','routed_transport','file_edit'):
            build=probe.reachable_build_memory(name,4096)
            self.assertEqual(build,0)
            budget=probe.resource_budget(1024,build,3*GIB)
            self.assertEqual(budget['run_memory_bytes'],2*GIB)
            self.assertEqual(budget['aggregate_ceiling_bytes'],4*GIB)
        for name in ('issue','adoption'):
            build=probe.reachable_build_memory(name,4096)
            self.assertEqual(build,4096)
            self.assertEqual(probe.resource_budget(1024,build,7*GIB)['run_memory_bytes'],6*GIB)
            with self.assertRaises(ValueError):probe.reachable_build_memory(name,0)
        with self.assertRaises(ValueError):probe.reachable_build_memory('unclassified',4096)

    def test_full_guests_fit_at_six_and_half_gib_without_reserving_unit_ceiling(self):
        budget=probe.resource_budget(1024,4096,6*GIB+512*MIB)
        self.assertEqual(budget['run_memory_bytes'],6*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],8*GIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+512*MIB)
        self.assertEqual(budget['final_admission'],'RunScope')
        with self.assertRaisesRegex(ValueError,'insufficient host headroom'):
            probe.resource_budget(1024,4096,6*GIB+512*MIB-1)
        with self.assertRaisesRegex(ValueError,'insufficient host headroom'):
            probe.resource_budget(1024,4096,4400*MIB)

    def test_controller_allowance_is_explicit_and_cannot_reduce_minimum(self):
        budget=probe.resource_budget(1024,4096,7*GIB,512)
        self.assertEqual(budget['controller_allowance_bytes'],512*MIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+768*MIB)
        for value in [0,255,2049,True,'256',256.0]:
            with self.assertRaises(ValueError):
                probe.resource_budget(1024,4096,10*GIB,value)

    def test_review_reservation_and_malformed_inputs(self):
        budget=probe.resource_budget(1024,0,3*GIB)
        self.assertEqual(budget['run_memory_bytes'],2*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],4*GIB)
        for native,build,available in [(True,4096,10*GIB),(511,4096,10*GIB),
                (1024,True,10*GIB),(1024,-1,10*GIB),(1024,4097,10*GIB),
                (1024,4096,-1),(1024,4096,'available'),(1024,4096,True)]:
            with self.assertRaises(ValueError):probe.resource_budget(native,build,available)
        with self.assertRaises(ValueError):probe.resource_budget(4096,4096,20*GIB)


HOLDER="""
import fcntl,os,sys,time
fd=os.open(sys.argv[1],os.O_RDWR|os.O_CREAT,0o600);fcntl.flock(fd,fcntl.LOCK_EX)
print('locked',flush=True)
seconds=float(sys.argv[2])
if seconds<0:sys.stdin.read()
else:time.sleep(seconds)
"""


def hold(path,seconds):
    """Another process holds the flock, as a managed build would."""
    process=subprocess.Popen([sys.executable,'-I','-c',HOLDER,str(path),str(seconds)],
                             stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
    assert process.stdout.readline().strip()=='locked'
    return process


def lockable(path):
    """Whether a separate process can take the flock right now."""
    code="import fcntl,os,sys\nfd=os.open(sys.argv[1],os.O_RDWR|os.O_CREAT,0o600)\ntry:fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)\nexcept BlockingIOError:raise SystemExit(1)"
    return subprocess.run([sys.executable,'-I','-c',code,str(path)]).returncode==0


class HostBuildLeaseTests(unittest.TestCase):
    """VM runs wait in line on the managed-build lease (never polled RAM)."""
    def setUp(self):
        self.temporary=tempfile.TemporaryDirectory();self.addCleanup(self.temporary.cleanup)
        self.state=Path(self.temporary.name)/'augmentagent';self.state.mkdir(mode=0o700)
        self.build=self.state/'disk-build.lock';self.update=self.state/'disk-update.lock'
        self.lines=[]

    def reap(self,process):
        def stop():
            if process.poll() is None:process.kill()
            process.wait();process.stdout.close();process.stdin.close()
        self.addCleanup(stop)
        return process

    def test_host_state_dir_matches_rust_resolution(self):
        self.assertEqual(probe.host_state_dir({'XDG_STATE_HOME':'/x/state','HOME':'/h'}),Path('/x/state/augmentagent'))
        self.assertEqual(probe.host_state_dir({'XDG_STATE_HOME':'relative','HOME':'/h'}),Path('/h/.local/state/augmentagent'))
        self.assertEqual(probe.host_state_dir({'HOME':'/h'}),Path('/h/.local/state/augmentagent'))

    def test_waits_in_line_while_managed_build_holds_lease(self):
        self.reap(hold(self.build,0.6))
        started=time.monotonic()
        lease=probe.acquire_host_build_lease(self.state,10,self.lines.append)
        self.addCleanup(lease.release)
        self.assertGreater(time.monotonic()-started,0.3)
        self.assertGreater(lease.waited_seconds,0.3)
        self.assertTrue(any(line.startswith('waiting in line for host build lease') and 'disk-build.lock' in line for line in self.lines),self.lines)
        self.assertTrue(any(line.startswith('host build lease acquired after') for line in self.lines),self.lines)

    def test_waits_for_deployment_build_on_update_lock(self):
        self.reap(hold(self.update,0.6))
        lease=probe.acquire_host_build_lease(self.state,10,self.lines.append)
        self.addCleanup(lease.release)
        self.assertGreater(lease.waited_seconds,0.3)
        self.assertTrue(any('disk-update.lock' in line for line in self.lines),self.lines)

    def test_held_lease_excludes_builds_and_deploys_until_released(self):
        lease=probe.acquire_host_build_lease(self.state,1,self.lines.append)
        self.assertEqual(lease.waited_seconds,0.0)
        self.assertEqual(self.lines,[])
        self.assertFalse(lockable(self.build));self.assertFalse(lockable(self.update))
        lease.release()
        self.assertTrue(lockable(self.build));self.assertTrue(lockable(self.update))

    def test_bounded_wait_names_holder_and_holds_nothing(self):
        holder=self.reap(hold(self.build,-1))
        with self.assertRaises(probe.LeaseUnavailable) as raised:
            probe.acquire_host_build_lease(self.state,0.5,self.lines.append)
        message=str(raised.exception)
        self.assertIn('disk-build.lock',message);self.assertIn(f'pid {holder.pid}',message)
        self.assertRegex(message,r'after 0\.\ds')
        self.assertTrue(lockable(self.update))

    def test_waiting_never_sits_on_the_update_lock(self):
        self.reap(hold(self.build,-1))
        waiter=threading.Thread(target=lambda:self.assertRaises(probe.LeaseUnavailable,
            probe.acquire_host_build_lease,self.state,1.5,self.lines.append))
        waiter.start()
        try:
            time.sleep(0.3)
            self.assertTrue(any(lockable(self.update) for _ in range(20)),'a waiting VM run must not block deploys')
        finally:
            waiter.join()

    def test_refuses_linked_lock_files(self):
        other=Path(self.temporary.name)/'elsewhere';other.write_text('')
        os.symlink(other,self.build)
        with self.assertRaises(probe.LeaseUnavailable):probe.acquire_host_build_lease(self.state,0.2,self.lines.append)
        os.unlink(self.build);os.link(other,self.build)
        with self.assertRaises(probe.LeaseUnavailable):probe.acquire_host_build_lease(self.state,0.2,self.lines.append)
        self.assertTrue(lockable(self.update))

    def test_holder_parsed_from_proc_locks(self):
        self.build.write_text('')
        info=self.build.stat()
        text=(f"1: POSIX  ADVISORY  READ 77 00:00:1 0 EOF\n"
              f"2: FLOCK  ADVISORY  WRITE 4242 {os.major(info.st_dev):02x}:{os.minor(info.st_dev):02x}:{info.st_ino} 0 EOF\n")
        self.assertEqual(probe.holder_pid(self.build,text),4242)
        self.assertIsNone(probe.holder_pid(self.build,'1: FLOCK  ADVISORY  WRITE 9 00:00:1 0 EOF\n'))

    def test_memory_is_read_only_after_lease_then_settles(self):
        self.reap(hold(self.build,0.5))
        seen=[]
        readings=iter([2*GIB,3*GIB,7*GIB])
        def read():
            seen.append(lockable(self.build))
            return next(readings)
        lease,budget=probe.admit_host(self.state,1024,4096,256,False,10,10,read,self.lines.append,sleep=lambda _:None)
        self.addCleanup(lease.release)
        self.assertEqual(seen,[False,False,False],'memory must be read only once the lease is ours')
        self.assertEqual(budget['observed_available_bytes'],7*GIB)
        self.assertIn('settle_seconds',budget)

    def test_invalid_declaration_fails_before_queueing(self):
        holder=self.reap(hold(self.build,-1))
        started=time.monotonic()
        with self.assertRaisesRegex(ValueError,'invalid declared guest memory'):
            probe.admit_host(self.state,1024,4097,256,False,30,30,lambda:64*GIB,self.lines.append)
        self.assertLess(time.monotonic()-started,5,'a bad declaration must not wait for the lease')
        self.assertEqual(self.lines,[])
        self.assertIsNone(holder.poll())

    def test_still_waiting_progress_line_on_cadence(self):
        self.reap(hold(self.build,-1))
        now=[0.0]
        def sleep(seconds):now[0]+=seconds
        with self.assertRaises(probe.LeaseUnavailable):
            probe.acquire_host_build_lease(self.state,130,self.lines.append,poll=5,progress_every=60,
                                           clock=lambda:now[0],sleep=sleep)
        progress=[line for line in self.lines if line.startswith('still waiting for host build lease')]
        self.assertEqual(len(progress),2,self.lines)
        self.assertEqual(sum(line.startswith('waiting in line') for line in self.lines),1)

    def run_main(self,root,extra,lock_until=None):
        """Drive main() end to end with a fake systemd-run that observes the host lock."""
        fake=root/'bin';fake.mkdir()
        observed=root/'observed.json'
        (fake/'systemd-run').write_text('#!/usr/bin/python3\n'
            'import fcntl,json,os,sys\n'
            'fd=os.open(os.environ["FAKE_LOCK"],os.O_RDWR|os.O_CREAT,0o600)\n'
            'try:\n fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB);held=False\n'
            'except BlockingIOError:held=True\n'
            'json.dump({"held":held,"argv":sys.argv[1:]},open(os.environ["FAKE_OBSERVED"],"w"))\n')
        (fake/'systemd-run').chmod(0o700)
        binary=root/'test-binary'
        expected=probe.TESTS['writer']
        binary.write_text('#!/usr/bin/python3\nprint('+repr(expected+': test')+')\n');binary.chmod(0o700)
        build=root/'build.json';build.write_text(json.dumps({'memory_mb':4096}));build.chmod(0o600)
        policy=root/'policy.json';policy.write_text(json.dumps({'memory_mb':1024,'runtime_config':str(build)}));policy.chmod(0o600)
        state=root/'state'
        environment=dict(os.environ,PATH=f"{fake}:{os.environ['PATH']}",HOME=str(root),XDG_STATE_HOME=str(state),
                         XDG_CONFIG_HOME=str(root/'config'),FAKE_LOCK=str(state/'augmentagent/disk-build.lock'),
                         FAKE_OBSERVED=str(observed))
        environment.pop('AUGMENTAGENT_MODEL_ROUTER_CONFIG',None);environment.pop('AUGMENTAGENT_MODEL_SELECTION_CONFIG',None)
        script=Path(__file__).with_name('autoship-native-acceptance-probe.py')
        result=subprocess.run([sys.executable,str(script),'writer','--binary',str(binary),'--vm-policy',str(policy),
                               '--defer-run-admission','--capacity-settle-secs','0',*extra],
                              env=environment,capture_output=True,text=True,timeout=60)
        evidence=next((line.split(': ',1)[1] for line in result.stdout.splitlines() if line.startswith('Evidence: ')),None)
        if evidence:self.addCleanup(subprocess.run,['rm','-rf',evidence])
        return result,observed,evidence,state/'augmentagent'

    def test_main_holds_host_lease_for_the_whole_unit_run(self):
        root=Path(self.temporary.name)/'main';root.mkdir()
        result,observed,evidence,state=self.run_main(root,['--max-run-secs','600'])
        self.assertTrue(observed.exists(),result.stderr)
        seen=json.loads(observed.read_text())
        self.assertTrue(seen['held'],'the unit ran without the host build lease')
        self.assertIn('RuntimeMaxSec=600',seen['argv'])
        report=json.loads((Path(evidence)/'result.json').read_text())
        self.assertEqual(report['host_build_lease']['locks'],['disk-update.lock','disk-build.lock'])
        self.assertIn('settle_seconds',report['resource_preflight'])
        self.assertTrue(lockable(state/'disk-build.lock'));self.assertTrue(lockable(state/'disk-update.lock'))

    def test_main_lease_timeout_defers_without_starting_unit(self):
        root=Path(self.temporary.name)/'busy';root.mkdir()
        state=root/'state/augmentagent';state.mkdir(parents=True)
        self.reap(hold(state/'disk-build.lock',-1))
        result,observed,evidence,_=self.run_main(root,['--lease-wait-secs','0.3'])
        self.assertEqual(result.returncode,75,result.stderr)
        self.assertIn('host build lease still busy',result.stderr)
        self.assertFalse(observed.exists(),'unit started without the lease')
        self.assertIsNone(evidence)

    def test_help_lists_queue_flags(self):
        script=Path(__file__).with_name('autoship-native-acceptance-probe.py')
        text=subprocess.run([sys.executable,str(script),'--help'],capture_output=True,text=True).stdout
        for flag in ('--lease-wait-secs','--capacity-settle-secs','--max-run-secs'):self.assertIn(flag,text)

    def test_memory_that_never_fits_releases_the_lease(self):
        with self.assertRaisesRegex(ValueError,'insufficient host headroom') as raised:
            probe.admit_host(self.state,1024,4096,256,False,1,0,lambda:2*GIB,self.lines.append)
        self.assertIn(str(2*GIB),str(raised.exception))
        self.assertTrue(lockable(self.build));self.assertTrue(lockable(self.update))


if __name__=='__main__':unittest.main()
