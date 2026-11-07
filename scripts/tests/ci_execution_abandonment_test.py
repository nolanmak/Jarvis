"""Actual original descendant supervision; no VM/artifact authority claimed."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid
from unittest.mock import patch
import ci_cargo_planner_protocol_test as fixtures
planner=fixtures.planner


class AbandonmentTests(unittest.TestCase):
    def setUp(self):
        self.root=Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.generation=self.root/'generation';self.generation.mkdir(mode=0o711)
        self.target=self.generation/'target';self.target.mkdir(mode=0o700)
        self.control=self.root/'control';self.control.mkdir(mode=0o700)
        self.command=str(uuid.uuid4());self.binding=fixtures.PlannerProtocolTests().publication_binding()
        self.enterContext(patch.object(planner,'_source',return_value=self.target))
        self.workers=planner._load('abandonment_original_workers','ci-worker-scope.py')
        self.lease=planner.ExecutionLease(self.target,self.binding,self.command,None)
        self.scope=self.workers.WorkerScope(self.control,os.getuid(),os.getgid(),['/usr/bin/false'])
        self.lease.bind_scope(self.scope)

    def stopped(self):
        process=subprocess.Popen(self.scope.argv);self.scope.attach(process)
        self.assertEqual(process.wait(timeout=5),1)
        return self.scope.verify()

    def test_original_cleanup_authorizes_nonreusable_disposition_but_never_completion(self):
        stopped=self.stopped();record=self.lease.abandon_nonreusable(self.scope,stopped)
        state=planner.ExecutionLease._read(self.lease.path,self.binding)
        self.assertEqual(state['version'],3);self.assertEqual(state['phase'],'abandoned')
        self.assertIsNone(state['completion_sha256']);self.assertFalse(record['shipping_authority'])
        self.assertFalse((self.generation/'completed-ci-commands').exists())
        self.assertEqual(self.lease.abandon_nonreusable(self.scope,stopped),record)
        with self.assertRaises(planner.FreshnessHold):self.lease.retained()
        with self.assertRaises(planner.FreshnessHold):planner.ExecutionLease(
            self.target,self.binding,str(uuid.uuid4()),self.command)

    def test_running_or_foreign_scope_cannot_authorize_abandonment(self):
        with self.assertRaises(planner.FreshnessHold):self.lease.abandon_nonreusable(self.scope,None)
        stopped=self.stopped()
        other=self.workers.WorkerScope(self.control,os.getuid(),os.getgid(),['/usr/bin/false'])
        with self.assertRaises(planner.FreshnessHold):self.lease.abandon_nonreusable(other,stopped)
        self.assertEqual(planner.cache._read_record(self.lease.path)['phase'],'active')

    def test_lost_state_ack_replays_only_same_original_disposition(self):
        stopped=self.stopped();write=planner.cache._write_record
        def lost_state(path,value):
            if path==self.lease.path and value.get('phase')=='abandoned':raise OSError('state ack lost')
            return write(path,value)
        with patch.object(planner.cache,'_write_record',side_effect=lost_state),self.assertRaises(OSError):
            self.lease.abandon_nonreusable(self.scope,stopped)
        self.assertEqual(planner.cache._read_record(self.lease.path)['phase'],'active')
        with self.assertRaises(planner.FreshnessHold):self.lease.retained()
        with self.assertRaises(planner.FreshnessHold):self.lease.finish(self.scope,stopped)
        record=self.lease.abandon_nonreusable(self.scope,stopped)
        self.assertEqual(record['scope']['cleanup_sha256'],planner.ExecutionLease.cleanup_digest())


if __name__=='__main__':unittest.main()
