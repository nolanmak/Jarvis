"""Non-consuming observations of actual inherited jobserver pipes/permits."""
import fcntl
import importlib.util
import os
from pathlib import Path
import threading
import unittest
from unittest.mock import patch
import ci_cargo_parent_signals_test as signals
planner=signals.planner
staging=planner.staging


class JobserverObservationTests(unittest.TestCase):
    def setUp(self):
        self.reader,self.writer=os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close,self.reader);self.addCleanup(os.close,self.writer)
        self.env={b'CARGO_MAKEFLAGS':b'-j --jobserver-auth=71,72'}
        self.mapping={71:self.reader,72:self.writer}

    def test_empty_pipe_is_observed_without_waiting_for_or_inserting_a_token(self):
        value=staging.observe_jobserver(self.env,self.mapping)
        self.assertEqual(value['available_bytes'],0);self.assertFalse(value['shipping_authority'])
        import select
        self.assertEqual(select.select([self.reader],[],[],0)[0],[])

    def test_filled_pipe_bytes_and_original_descriptor_flags_are_unchanged(self):
        os.write(self.writer,b'abc');before=[fcntl.fcntl(fd,fcntl.F_GETFL) for fd in (self.reader,self.writer)]
        value=staging.observe_jobserver(self.env,self.mapping)
        self.assertEqual(value['available_bytes'],3)
        self.assertEqual([value['reader_flags'],value['writer_flags']],before)
        self.assertEqual([fcntl.fcntl(fd,fcntl.F_GETFL) for fd in (self.reader,self.writer)],before)
        self.assertEqual(os.read(self.reader,3),b'abc')
        job=staging.normalize_jobserver(self.env,self.mapping)[2]
        self.assertEqual(value['kernel_pipe_sha256'],job['kernel_pipe_sha256'])

    def test_missing_or_cross_pipe_model_is_not_a_zero_token_observation(self):
        other,writer=os.pipe2(os.O_CLOEXEC)
        try:
            for env,mapping in (({},{}),(self.env,{71:self.reader,72:writer})):
                with self.assertRaises(staging.StagingUnavailable):staging.observe_jobserver(env,mapping)
        finally:os.close(other);os.close(writer)

    def test_ioctl_failure_refuses_observation_without_changing_tokens(self):
        os.write(self.writer,b'+')
        with patch.object(staging.fcntl,'ioctl',side_effect=OSError('injected ioctl failure')):
            with self.assertRaises(OSError):staging.observe_jobserver(self.env,self.mapping)
        self.assertEqual(os.read(self.reader,1),b'+')

    def test_root_observation_reads_the_existing_slots_without_acquiring_one(self):
        root=object.__new__(planner.RootRelay);root._slots=threading.BoundedSemaphore(4)
        root._lock=threading.Lock();root._children=set()
        self.assertEqual(root.runnable_observation()['available'],4)
        for _ in range(4):self.assertTrue(root._slots.acquire(blocking=False))
        root._children.update((object(),object(),object(),object()))
        value=root.runnable_observation()
        self.assertEqual(value['available'],0);self.assertEqual(value['original_supervisors'],4)
        self.assertFalse(root._slots.acquire(blocking=False));self.assertFalse(value['shipping_authority'])
        root._slots.release();self.assertEqual(root.runnable_observation()['available'],1)
        self.assertTrue(root._slots.acquire(blocking=False))


if __name__=='__main__':unittest.main()
