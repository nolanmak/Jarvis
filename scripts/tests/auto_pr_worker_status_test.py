import fcntl
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

MODULE=Path(__file__).resolve().parents[1]/'auto-pr-worker-status.py'
spec=importlib.util.spec_from_file_location('worker_status',MODULE)
worker_status=importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker_status)

class WorkerStatusTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.state=self.root/'state';self.state.mkdir()
        self.proc=self.root/'proc';self.proc.mkdir()
        for pid,group in [(31,'augmentagent.service'),(45,'augmentagent-autopr.service')]:
            p=self.proc/str(pid);p.mkdir();(p/'fd').mkdir();(p/'fdinfo').mkdir()
            (p/'stat').write_text(f'{pid} (synthetic) '+' '.join(['0']*19+['77']))
            (p/'cgroup').write_text('0::/augmentagent-runtime.slice/'+group)
            (p/'cmdline').write_bytes(b'augmentagent\0auto-pr-worker\0')
        (self.state/'autopr-host.json').write_text(json.dumps({'pid':31,'birth':'77','external':True}))
        self.lock=(self.state/'self-improve.lock').open('w+')
        self.addCleanup(self.lock.close)
        fcntl.flock(self.lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        (self.proc/'45/fd/6').symlink_to(self.state/'self-improve.lock')
        (self.proc/'45/fdinfo/6').write_text('lock:\t1: FLOCK ADVISORY WRITE 45 0:0:1 0 EOF\n')
    def safe(self):
        return worker_status.isolated(self.state,self.proc,31,45)
    def test_external_worker_can_keep_its_active_run_across_daemon_restart(self):
        self.assertTrue(self.safe())
        # The active worker and its lock do not change when the daemon PID
        # changes. The replacement daemon must attest its own mode/birth.
        new=self.proc/'32';new.mkdir()
        (new/'stat').write_text('32 (new daemon) '+' '.join(['0']*19+['88']))
        (new/'cgroup').write_text('0::/augmentagent-runtime.slice/augmentagent.service')
        (self.state/'autopr-host.json').write_text(json.dumps({'pid':32,'birth':'88','external':True}))
        self.assertTrue(worker_status.isolated(self.state,self.proc,32,45))
    def test_legacy_daemon_and_stale_pid_are_not_safe(self):
        marker=self.state/'autopr-host.json'
        marker.write_text(json.dumps({'pid':31,'birth':'77','external':False}))
        self.assertFalse(self.safe())
        marker.write_text(json.dumps({'pid':31,'birth':'old','external':True}))
        self.assertFalse(self.safe())
    def test_other_lane_owner_or_shared_cgroup_prevents_bypass(self):
        fdinfo=self.proc/'45/fdinfo/6'
        fdinfo.write_text('lock:\t1: FLOCK ADVISORY WRITE 99 0:0:1 0 EOF\n')
        self.assertFalse(self.safe())
        fdinfo.write_text('lock:\t1: FLOCK ADVISORY WRITE 45 0:0:1 0 EOF\n')
        (self.proc/'45/cgroup').write_text((self.proc/'31/cgroup').read_text())
        self.assertFalse(self.safe())
    def test_unreadable_missing_or_unattested_worker_is_not_safe(self):
        (self.proc/'45/fdinfo/6').unlink()
        self.assertFalse(self.safe())
        self.assertFalse(worker_status.isolated(self.state,self.proc,31,31))
    def test_custom_run_lock_must_also_belong_to_worker(self):
        custom=self.root/'custom-lane.lock'
        with custom.open('w+') as handle:
            fcntl.flock(handle,fcntl.LOCK_EX|fcntl.LOCK_NB)
            self.assertFalse(worker_status.isolated(self.state,self.proc,31,45,[custom]))
            (self.proc/'45/fd/7').symlink_to(custom)
            (self.proc/'45/fdinfo/7').write_text('lock: 1: FLOCK ADVISORY WRITE 45 0:0:1 0 EOF\n')
            self.assertTrue(worker_status.isolated(self.state,self.proc,31,45,[custom]))

if __name__=='__main__':unittest.main()
