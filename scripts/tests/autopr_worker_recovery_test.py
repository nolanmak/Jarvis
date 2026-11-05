"""Real process/managed-unit fixtures; the service-manager boundary is scripted."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('recovery',ROOT/'scripts/recover-autopr-worker.py')
recovery=importlib.util.module_from_spec(spec);spec.loader.exec_module(recovery)

class WorkerRecoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.state=self.root/'state';self.state.mkdir()
        self.proc=self.root/'proc';self.proc.mkdir()
        self.repository=self.root/'repo';release=self.repository/'target/release';release.mkdir(parents=True)
        self.binary=release/'augmentagent';self.binary.write_text('synthetic executable');self.binary.chmod(0o700)
        self.fragment=self.root/'augmentagent-autopr.service'
        self.fragment.write_text('# Managed by ensure-autopr-worker.sh\n[Service]\n');self.fragment.chmod(0o600)
        for pid,unit in [(31,'augmentagent.service'),(45,'augmentagent-autopr.service')]:
            path=self.proc/str(pid);path.mkdir()
            (path/'stat').write_text(f'{pid} (fixture) '+' '.join(['0']*19+['77']))
            (path/'cgroup').write_text('0::/augmentagent-runtime.slice/'+unit)
            (path/'cmdline').write_bytes(b'augmentagent\0'+(b'serve' if pid==31 else b'auto-pr-worker')+b'\0')
            (path/'exe').symlink_to(self.binary)
        self.marker=self.state/'autopr-host.json';self.host={'pid':31,'birth':'77','external':True}
        self.marker.write_text(json.dumps(self.host))
        launcher=str(self.repository/'scripts/run-rs.sh')
        self.worker={'ActiveState':'failed','Result':'start-limit-hit','MainPID':'0','UnitFileState':'enabled',
            'Job':'','FragmentPath':str(self.fragment),'ExecStart':f'{{ path={launcher} ; argv[]={launcher} auto-pr-worker --dry-run false ; ignore_errors=no ; }}'}
        self.daemon={'ActiveState':'active','MainPID':'31'}
        self.starts=[];self.on_recheck=None;self.fail_start=False;self.after_start=None
        fixture=self
        class Manager:
            def properties(self,unit):
                if unit=='augmentagent.service':return dict(fixture.daemon)
                if fixture.on_recheck and getattr(self,'seen',False):fixture.on_recheck()
                self.seen=True
                return dict(fixture.worker)
            def start(self):
                fixture.starts.append('start')
                if fixture.fail_start:raise RuntimeError('manager refused start')
                fixture.worker.update(ActiveState='active',Result='success',MainPID='45')
                if fixture.after_start:fixture.after_start()
        self.manager=Manager()
    def run_recovery(self):
        return recovery.recover(self.repository,self.state,self.proc,self.manager)
    def test_failed_worker_is_started_once_and_verified_without_restart_or_reset(self):
        self.assertEqual(self.run_recovery()['state'],'recovered')
        self.assertEqual(self.starts,['start'])
        self.assertEqual(self.run_recovery()['state'],'not-eligible')
        self.assertEqual(self.starts,['start'])
    def test_owner_stop_disable_active_worker_and_pending_stop_are_preserved(self):
        for change in [{'ActiveState':'inactive'},{'UnitFileState':'disabled'},
                       {'ActiveState':'active','MainPID':'45'},{'Job':'42 /job/42'}, {'Result':'exit-code'}]:
            with self.subTest(change=change):
                prior=dict(self.worker);self.worker.update(change)
                self.assertEqual(self.run_recovery()['state'],'not-eligible');self.assertEqual(self.starts,[])
                self.worker=prior
    def test_legacy_stale_pid_birth_and_missing_host_attestation_never_start(self):
        for change in [{'external':False},{'birth':'old'},{'pid':999}]:
            with self.subTest(change=change):
                self.marker.write_text(json.dumps(dict(self.host,**change)))
                self.assertEqual(self.run_recovery()['state'],'held');self.assertEqual(self.starts,[])
        self.marker.unlink();self.assertEqual(self.run_recovery()['state'],'held');self.assertEqual(self.starts,[])
    def test_unmanaged_pinned_and_unsafe_units_never_start(self):
        original=self.fragment.read_text()
        self.fragment.write_text('# owner managed\n')
        self.assertEqual(self.run_recovery()['state'],'held')
        self.fragment.write_text(original);self.fragment.chmod(0o666)
        self.assertEqual(self.run_recovery()['state'],'held');self.fragment.chmod(0o600)
        self.worker['ExecStart']='{ path=/frozen/run-rs.sh ; argv[]=/frozen/run-rs.sh auto-pr-worker ; }'
        self.assertEqual(self.run_recovery()['state'],'held');self.assertEqual(self.starts,[])
    def test_unrolled_binary_or_reused_daemon_pid_never_start(self):
        old=self.root/'old-executable';old.write_text('old executable')
        exe=self.proc/'31/exe';exe.unlink();exe.symlink_to(old)
        self.assertEqual(self.run_recovery()['state'],'held');self.assertEqual(self.starts,[])
    def test_owner_stop_between_checks_cannot_be_overridden(self):
        self.on_recheck=lambda:self.worker.update(ActiveState='inactive')
        self.assertEqual(self.run_recovery()['state'],'held');self.assertEqual(self.starts,[])
    def test_daemon_mode_change_between_checks_cannot_start_worker(self):
        self.on_recheck=lambda:self.marker.write_text(json.dumps(dict(self.host,external=False)))
        self.assertEqual(self.run_recovery()['state'],'held');self.assertEqual(self.starts,[])
    def test_manager_failure_is_reported_without_a_success_receipt(self):
        self.fail_start=True
        with self.assertRaisesRegex(RuntimeError,'manager refused'):self.run_recovery()
        self.assertEqual(self.starts,['start'])
    def test_running_state_with_wrong_executable_is_not_recovery_proof(self):
        def changed():
            wrong=self.root/'wrong';wrong.write_text('wrong executable')
            exe=self.proc/'45/exe';exe.unlink();exe.symlink_to(wrong)
        self.after_start=changed
        with patch.object(recovery.time,'sleep'),patch.object(recovery.time,'monotonic',side_effect=[0,0,11]):
            with self.assertRaisesRegex(RuntimeError,'identity is unverified'):self.run_recovery()
        self.assertEqual(self.starts,['start'])
    def test_daemon_mode_change_after_start_cannot_receive_recovery_receipt(self):
        self.after_start=lambda:self.marker.write_text(json.dumps(dict(self.host,external=False)))
        with patch.object(recovery.time,'sleep'),patch.object(recovery.time,'monotonic',side_effect=[0,0,11]):
            with self.assertRaisesRegex(RuntimeError,'identity is unverified'):self.run_recovery()
        self.assertEqual(self.starts,['start'])
    def test_malformed_or_symlinked_host_marker_is_not_authority(self):
        for value in ['null','[]','invalid json']:
            self.marker.write_text(value)
            self.assertEqual(self.run_recovery()['state'],'held')
        target=self.root/'other-marker';target.write_text(json.dumps(self.host))
        self.marker.unlink();self.marker.symlink_to(target)
        self.assertEqual(self.run_recovery()['state'],'held');self.assertEqual(self.starts,[])

if __name__=='__main__':unittest.main()
