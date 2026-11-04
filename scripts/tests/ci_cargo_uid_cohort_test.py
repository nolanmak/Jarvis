"""Actual procfs identities; these observations never prove a freeze or reuse."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import unittest
from ci_cargo_entry_test import staging


class UidCohortTests(unittest.TestCase):
    def setUp(self):
        self.child=subprocess.Popen([sys.executable,'-I','-c','import signal;signal.pause()'])
        self.addCleanup(self.cleanup)
        os.kill(self.child.pid,signal.SIGSTOP)
        deadline=time.monotonic()+2
        while self.state()!='T' and time.monotonic()<deadline:time.sleep(.005)
        self.assertEqual(self.state(),'T')
        self.original=self.identity()

    def cleanup(self):
        if self.child.poll() is None:self.child.kill()
        self.child.wait(timeout=2)

    def state(self):return Path(f'/proc/{self.child.pid}/stat').read_text().rsplit(')',1)[1].split()[0]

    def identity(self):
        fields=Path(f'/proc/{self.child.pid}/stat').read_text().rsplit(')',1)[1].split()
        return {'pid':self.child.pid,'tid':self.child.pid,'birth':int(fields[19])}

    def test_same_uid_tasks_outside_expected_cohort_refuse_observation(self):
        # This actual child is stopped, but this test process has the same UID
        # outside its cohort. A stopped child alone must never authorize it.
        with self.assertRaises(staging.StagingUnavailable) as caught:
            staging.observe_uid_cohort(os.getuid(),[self.original],check=lambda:None)
        self.assertEqual(caught.exception.stage_reason,'uid_cohort_outside')
        self.assertEqual(self.state(),'T')

    def test_wrong_original_birth_is_refused_before_global_census(self):
        with self.assertRaises(staging.StagingUnavailable) as caught:
            staging.observe_uid_cohort(os.getuid(),[{**self.original,'birth':self.original['birth']+1}],check=lambda:None)
        self.assertEqual(caught.exception.stage_reason,'uid_cohort_identity')

    def test_original_task_with_foreign_uid_is_refused(self):
        with self.assertRaises(staging.StagingUnavailable) as caught:
            staging.observe_uid_cohort(2**32-2,[self.original],check=lambda:None)
        self.assertEqual(caught.exception.stage_reason,'uid_cohort_identity')

    def test_disappeared_original_task_is_refused(self):
        self.child.kill();self.child.wait(timeout=2)
        with self.assertRaises(staging.StagingUnavailable):
            staging.observe_uid_cohort(os.getuid(),[self.original],check=lambda:None)

    def test_cancellation_during_census_is_not_converted_to_success(self):
        def cancelled():raise staging.StagingUnavailable('original_cancelled')
        with self.assertRaises(staging.StagingUnavailable) as caught:
            staging.observe_uid_cohort(os.getuid(),[self.original],check=cancelled)
        self.assertEqual(caught.exception.stage_reason,'original_cancelled')

    def test_unused_uid_observation_has_no_cleanup_or_input_authority(self):
        # A genuinely absent UID is a census result, never a paused cohort.
        observed=staging.observe_uid_cohort(2**32-2,[],check=lambda:None)
        self.assertEqual(observed['tasks'],[])
        self.assertFalse(observed['input_closure_complete']);self.assertFalse(observed['shipping_authority'])
        self.assertNotIn('frozen',observed);self.assertNotIn('original_cleanup',observed)

    def test_duplicate_and_unbounded_expected_tasks_are_refused(self):
        for tasks in ([self.original,self.original],[self.original]*513):
            with self.subTest(count=len(tasks)),self.assertRaises(staging.StagingUnavailable):
                staging.observe_uid_cohort(os.getuid(),tasks,check=lambda:None)


if __name__=='__main__':unittest.main()
