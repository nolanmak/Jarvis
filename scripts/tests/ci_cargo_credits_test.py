"""Jobserver lineage mechanics; original supervision owns execution/cleanup proof."""
import errno
import fcntl
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
import uuid
from unittest.mock import patch
import ci_cargo_parent_signals_test as signals
planner=signals.planner


class JobserverCreditTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name);self.addCleanup(self.temp.cleanup)
        self.binding={'generation':str(uuid.uuid4()),'head':'a'*40,'tree':'b'*40,
            **{key:'c'*64 for key in ('contract_sha256','authority_sha256','cache_identity_sha256','source_inputs_sha256')}}
        self.command=str(uuid.uuid4());self.lock=threading.RLock();self.reader,self.writer=os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close,self.reader);self.addCleanup(os.close,self.writer)
        self.environment={b'CARGO_MAKEFLAGS':b'-j --jobserver-auth=71,72'}
        self.job=planner.staging.normalize_jobserver(self.environment,{71:self.reader,72:self.writer})[2]
        self.credits=planner.NestedJobserverCredits(self.root/'credits.json',binding=self.binding,command=self.command,lock=self.lock)
        self.scopes={}

    def enter(self,ordinal,parent=None):
        scope=str(uuid.uuid4());self.scopes[ordinal]=scope
        return self.credits.enter(ordinal,scope,self.job,parent=parent)

    def pause(self,parent):
        return {'version':1,'kind':'ci-unit-freeze-observation','scope':self.scopes[parent],
            'original_pid':os.getpid(),'frozen':True,'processes':[],
            'input_closure_complete':False,'shipping_authority':False}

    def lend(self,parent,child):
        self.credits.prepare(parent,child,self.job)
        return self.credits.lend(parent,child,self.pause(parent))

    def test_four_exhausted_parent_slots_lend_without_touching_pipe_bytes_or_flags(self):
        slots=threading.BoundedSemaphore(4);flags=[fcntl.fcntl(fd,fcntl.F_GETFL) for fd in (self.reader,self.writer)]
        # Three explicit tokens plus the jobserver owner's implicit slot.
        os.write(self.writer,b'abc');self.assertEqual(os.read(self.reader,3),b'abc')
        roots=[]
        for parent in range(4):
            self.assertTrue(slots.acquire(blocking=False));roots.append(self.enter(parent))
        self.assertFalse(slots.acquire(blocking=False));self.assertEqual(len({r['credit_id'] for r in roots}),4)
        for parent in range(4):
            child=parent+4;loan=self.lend(parent,child);leaf=self.enter(child,parent)
            self.assertEqual(loan['credit_id'],roots[parent]['credit_id']);self.assertEqual(leaf['credit_id'],roots[parent]['credit_id'])
            with self.assertRaises(planner.FreshnessHold):self.credits.require_owned(parent)
        self.assertEqual(select_readable(self.reader),False)
        self.assertEqual([fcntl.fcntl(fd,fcntl.F_GETFL) for fd in (self.reader,self.writer)],flags)
        for parent in range(4):
            self.credits.return_child(parent,parent+4);self.credits.require_owned(parent);slots.release()
        self.assertEqual(select_readable(self.reader),False)
        os.write(self.writer,b'abc');self.assertEqual(os.read(self.reader,3),b'abc')
        for _ in range(4):self.assertTrue(slots.acquire(blocking=False))
        self.assertFalse(slots.acquire(blocking=False))

    def test_nested_chain_has_one_leaf_and_no_fresh_credit(self):
        root=self.enter(8);self.lend(8,9);a=self.enter(9,8);self.lend(9,10);b=self.enter(10,9)
        self.assertEqual(root['credit_id'],a['credit_id']);self.assertEqual(a['credit_id'],b['credit_id'])
        for parent in (8,9):
            with self.assertRaises(planner.FreshnessHold):self.credits.require_owned(parent)
        with self.assertRaises(planner.FreshnessHold):self.credits.return_child(8,9)
        self.credits.return_child(9,10);self.credits.return_child(8,9);self.credits.require_owned(8)
        with self.assertRaises(planner.FreshnessHold):self.credits.require_owned(10)

    def test_foreign_pipe_and_missing_model_refuse_before_pause(self):
        self.enter(8);wrong=dict(self.job,kernel_pipe_sha256='d'*64)
        for job in (wrong,None,dict(self.job,worker_numbers=[7,8])):
            with self.assertRaises(planner.FreshnessHold):self.credits.prepare(8,9,job)
        self.credits.require_owned(8)
        self.assertIsNone(self.credits.observation()['units']['8']['child'])

    def test_wrong_scope_or_unverified_pause_cannot_lend(self):
        self.enter(8);self.credits.prepare(8,9,self.job)
        for pause in (dict(self.pause(8),scope=str(uuid.uuid4())),dict(self.pause(8),frozen=False)):
            with self.assertRaises(planner.FreshnessHold):self.credits.lend(8,9,pause)
        with self.assertRaises(planner.FreshnessHold):self.enter(9,8)

    def test_second_child_duplicate_return_and_ordinal_reuse_are_refused(self):
        self.enter(8);self.lend(8,9)
        with self.assertRaises(planner.FreshnessHold):self.credits.prepare(8,10,self.job)
        self.enter(9,8);self.credits.return_child(8,9)
        with self.assertRaises(planner.FreshnessHold):self.credits.return_child(8,9)
        with self.assertRaises(planner.FreshnessHold):self.credits.prepare(8,9,self.job)
        with self.assertRaises(planner.FreshnessHold):self.enter(8)

    def test_prepared_but_never_dispatched_loan_can_be_cancelled_without_token_injection(self):
        self.enter(8);self.credits.prepare(8,9,self.job);self.credits.abort_prepared(8,9)
        self.credits.require_owned(8)
        with self.assertRaises(planner.FreshnessHold):self.credits.lend(8,9,self.pause(8))
        self.assertFalse(select_readable(self.reader))

    def test_lent_or_unknown_subtree_keeps_the_original_credit_fenced(self):
        self.enter(8);self.lend(8,9)
        with self.assertRaises(planner.FreshnessHold):self.credits.abort_prepared(8,9)
        with self.assertRaises(planner.FreshnessHold):self.credits.return_child(8,9)
        with self.assertRaises(planner.FreshnessHold):self.credits.require_owned(8)
        self.assertFalse(select_readable(self.reader))

    def test_lost_journal_ack_replays_known_state_without_duplicate_credit(self):
        self.enter(8);write=planner.cache._write_record;failed=[]
        def interrupted(path,value):
            if path.name=='credits.json' and value.get('units',{}).get('8',{}).get('child')==9 and not failed:
                failed.append(True);write(path,value);raise OSError(errno.ENOSPC,'lost journal ack')
            return write(path,value)
        with patch.object(planner.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):
            self.credits.prepare(8,9,self.job)
        self.credits.prepare(8,9,self.job);self.credits.lend(8,9,self.pause(8));self.enter(9,8)
        self.assertEqual(len({r['credit_id'] for r in self.credits.observation()['units'].values()}),1)
        self.assertFalse(select_readable(self.reader))

    def test_foreign_journal_cannot_be_resampled_or_adopted_by_another_holder(self):
        self.enter(8)
        with self.assertRaises(planner.FreshnessHold):
            planner.NestedJobserverCredits(self.root/'credits.json',binding=self.binding,command=self.command,lock=self.lock)
        record=json.loads((self.root/'credits.json').read_text());record['units']['8']['state']='lent'
        planner.cache._write_record(self.root/'credits.json',record)
        with self.assertRaises(planner.FreshnessHold):self.credits.prepare(8,9,self.job)

    def test_forked_holder_cannot_lend_or_return_parent_credit(self):
        self.enter(8);pid=os.fork()
        if pid==0:
            try:self.credits.prepare(8,9,self.job)
            except planner.FreshnessHold:os._exit(0)
            except BaseException:os._exit(2)
            os._exit(1)
        self.assertEqual(os.waitpid(pid,0)[1],0);self.credits.require_owned(8)


    def test_root_cleanup_closes_the_original_credit_without_lending_it_again(self):
        self.enter(8);self.credits.finish_root(8)
        self.assertEqual(self.credits.observation()['units']['8']['state'],'closed')
        with self.assertRaises(planner.FreshnessHold):self.credits.prepare(8,9,self.job)
        with self.assertRaises(planner.FreshnessHold):self.credits.finish_root(8)
        self.assertFalse(select_readable(self.reader))

    def test_original_cleanup_hook_returns_only_the_bound_child_credit(self):
        self.enter(8);self.lend(8,9);self.enter(9,8)
        relay=object.__new__(planner.UnitPeerRelay);relay.parent_ordinal=8;relay.root=self.root
        relay.ordinals=type('CreditOwner',(),{'credits':self.credits})()
        relay.active_record={'last_request_ordinal':9,'jobserver_loan':{'state':'lent'}}
        relay.unit_credit_drained({'ordinal':9},None,False)
        with self.assertRaises(planner.FreshnessHold):self.credits.require_owned(8)
        relay.unit_credit_drained({'ordinal':9},None,True);self.credits.require_owned(8)
        self.assertEqual(json.loads((self.root/'admission.json').read_text())['jobserver_loan']['state'],'returned')
        self.assertFalse(select_readable(self.reader))

    def test_lost_lend_ack_does_not_create_a_second_credit_or_change_the_pipe(self):
        self.enter(8);self.credits.prepare(8,9,self.job);write=planner.cache._write_record;failed=[]
        def interrupted(path,value):
            if path.name=='credits.json' and value['loans']['9']['state']=='lent' and not failed:
                failed.append(True);write(path,value);raise OSError(errno.ENOSPC,'lost lend ack')
            return write(path,value)
        with patch.object(planner.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):
            self.credits.lend(8,9,self.pause(8))
        self.credits.lend(8,9,self.pause(8));self.enter(9,8)
        with self.assertRaises(planner.FreshnessHold):self.credits.require_owned(8)
        self.assertFalse(select_readable(self.reader))


def select_readable(fd):
    import select
    return bool(select.select([fd],[],[],0)[0])


if __name__=='__main__':unittest.main()
