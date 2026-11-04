"""Actual signals through controller-owned stopped-parent/active-child chains."""
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import threading
import time
import unittest
from unittest.mock import patch
import ci_cargo_parent_signals_test as staging
planner=staging.planner


class ActiveSignalsTests(unittest.TestCase):
    def setUp(self):self.children=[];self.gates=[]
    def tearDown(self):
        for child,fd in self.children:
            if child.poll() is None:child.kill()
            child.wait(timeout=2)
        for gate in reversed(self.gates):gate.close()
        for child,fd in self.children:os.close(fd);child.stdout.close()

    def line(self,child):
        self.assertTrue(select.select([child.stdout],[],[],2)[0]);data=b''
        while not data.endswith(b'\n'):
            block=os.read(child.stdout.fileno(),1);self.assertTrue(block);data+=block
        return data

    def process(self,lock=None):
        body=('import os,signal\n'
            'signal.signal(signal.SIGUSR1,lambda *_:os.write(1,b"USR1\\n"))\n'
            'signal.signal(signal.SIGUSR2,lambda *_:os.write(1,b"USR2\\n"))\n'
            'os.write(1,b"READY\\n")\nwhile True:signal.pause()\n')
        child=subprocess.Popen([sys.executable,'-I','-u','-c',body],stdout=subprocess.PIPE,bufsize=0)
        fd=os.pidfd_open(child.pid);self.children.append((child,fd));self.assertEqual(self.line(child),b'READY\n')
        gate=planner.StagedParentSignals(fd,lock=lock);self.gates.append(gate)
        return child,gate

    def stop(self,child,gate,request):
        gate.pause(request);signal.pidfd_send_signal(self.children[[c for c,_ in self.children].index(child)][1],signal.SIGSTOP)
        deadline=time.monotonic()+2
        while time.monotonic()<deadline:
            if Path(f'/proc/{child.pid}/stat').read_text().rsplit(')',1)[1].split()[0]=='T':return
            time.sleep(.005)
        self.fail('parent did not stop')

    def activate(self,parent,request,child):parent.activate_child(request,child,lambda:None)

    def test_stopped_parent_routes_signal_to_active_child_only(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7)
        self.activate(p,7,c)
        self.assertEqual(p.forward(signal.SIGUSR1),'forwarded');self.assertEqual(self.line(child),b'USR1\n')
        self.assertEqual(select.select([parent.stdout],[],[],.03)[0],[])
        self.assertFalse(p.cancelled.is_set());self.assertEqual(p.observation()['queued'],0)

    def test_nested_chain_routes_to_leaf_without_waking_either_parent(self):
        parent,p=self.process();middle,m=self.process(p.lock);leaf,l=self.process(p.lock)
        self.stop(parent,p,7);self.activate(p,7,m);self.stop(middle,m,8);self.activate(m,8,l)
        self.assertEqual(p.forward(signal.SIGUSR1),'forwarded');self.assertEqual(self.line(leaf),b'USR1\n')
        self.assertEqual(p.forward(signal.SIGUSR2),'forwarded');self.assertEqual(self.line(leaf),b'USR2\n')
        self.assertFalse(p.cancelled.is_set());self.assertFalse(m.cancelled.is_set())
        self.assertEqual(select.select([parent.stdout,middle.stdout],[],[],.03)[0],[])

    def test_live_child_prevents_detach_and_parent_resume(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7);self.activate(p,7,c)
        with self.assertRaises(planner.FreshnessHold):p.child_drained(7,c)
        with self.assertRaises(planner.FreshnessHold):p.resume(7,lambda:self.fail('must not thaw'))
        child.terminate();child.wait(timeout=2);p.child_drained(7,c)
        p.resume(7,lambda:os.kill(parent.pid,signal.SIGCONT))
        self.assertEqual(p.forward(signal.SIGUSR2),'forwarded');self.assertEqual(self.line(parent),b'USR2\n')

    def test_queued_staging_signal_prevents_active_child_grant(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7)
        self.assertEqual(p.forward(signal.SIGUSR1),'queued')
        with self.assertRaises(planner.FreshnessHold):p.activate_child(7,c,lambda:self.fail('must not grant'))
        self.assertEqual(select.select([child.stdout],[],[],.03)[0],[])

    def test_late_signal_is_not_redirected_to_paused_parent(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7);self.activate(p,7,c)
        child.terminate();child.wait(timeout=2)
        self.assertEqual(p.forward(signal.SIGUSR1),'late');self.assertEqual(p.observation()['queued'],0)
        self.assertEqual(select.select([parent.stdout],[],[],.03)[0],[])
        p.child_drained(7,c)
        self.assertEqual(p.forward(signal.SIGUSR2),'queued')
        self.assertTrue(p.cancelled.is_set())

    def test_signal_racing_grant_waits_for_grant_ack(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7)
        entered=threading.Event();release=threading.Event();results=[]
        def grant():entered.set();self.assertTrue(release.wait(2))
        def activate():
            try:p.activate_child(7,c,grant)
            except BaseException as error:results.append(error)
        def send():
            try:results.append(p.forward(signal.SIGUSR1))
            except BaseException as error:results.append(error)
        activating=threading.Thread(target=activate);sender=threading.Thread(target=send);activating.start()
        try:
            self.assertTrue(entered.wait(2));sender.start();sender.join(.03);self.assertTrue(sender.is_alive())
            self.assertEqual(select.select([child.stdout],[],[],.03)[0],[])
        finally:
            release.set();activating.join(2)
            if sender.ident is not None:sender.join(2)
        self.assertEqual(results,['forwarded']);self.assertEqual(self.line(child),b'USR1\n')

    def test_failed_grant_keeps_chain_fenced_and_blocks_resume(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7)
        def failed():raise OSError('unknown exec grant')
        with self.assertRaises(OSError):p.activate_child(7,c,failed)
        self.assertTrue(p.cancelled.is_set());self.assertTrue(p.observation()['revoked'])
        with self.assertRaises(planner.FreshnessHold):p.resume(7,lambda:self.fail('must not thaw'))

    def test_wrong_scope_independent_lock_and_cycle_are_refused(self):
        parent,p=self.process();child,c=self.process(p.lock);other,o=self.process();self.stop(parent,p,7)
        for request,gate in ((8,c),(7,o),(7,p)):
            with self.subTest(request=request,gate=gate):
                with self.assertRaises(planner.FreshnessHold):self.activate(p,request,gate)
        self.activate(p,7,c)
        with self.assertRaises(planner.FreshnessHold):self.activate(p,7,c)
        self.stop(child,c,8)
        with self.assertRaises(planner.FreshnessHold):self.activate(c,8,p)

    def test_delivery_failure_fences_the_entire_route_without_retry(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7);self.activate(p,7,c)
        with patch.object(planner.signal,'pidfd_send_signal',side_effect=OSError('unknown signal delivery')):
            with self.assertRaises(OSError):p.forward(signal.SIGUSR1)
        self.assertTrue(p.cancelled.is_set());self.assertTrue(c.cancelled.is_set())
        with self.assertRaises(planner.FreshnessHold):p.forward(signal.SIGUSR1)

    def test_original_signal_budget_is_enforced_across_routed_frames(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7);self.activate(p,7,c)
        child.terminate();child.wait(timeout=2)
        for _ in range(4096):self.assertEqual(p.forward(signal.SIGUSR1),'late')
        with self.assertRaises(planner.FreshnessHold):p.forward(signal.SIGUSR1)
        self.assertEqual(p.observation()['received'],4096)
        self.assertTrue(p.cancelled.is_set())


    def test_root_exec_and_cleanup_hooks_bind_and_release_the_actual_pidfds(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7)
        relay=object.__new__(planner.UnitPeerRelay);relay.staged_signals=p
        relay.active_record={'last_request_ordinal':7}
        transport=object.__new__(planner.UnitPeerRelay);transport.staged_signals=c
        grants=[]
        relay.unit_signal_grant({'ordinal':7},transport,lambda:grants.append('original grant'))
        self.assertEqual(grants,['original grant'])
        self.assertEqual(relay.forward_signal(signal.SIGUSR1),'forwarded')
        self.assertEqual(self.line(child),b'USR1\n')
        with self.assertRaises(planner.FreshnessHold):relay.unit_signal_drained({'ordinal':7},transport,True)
        child.terminate();child.wait(timeout=2);relay.unit_signal_drained({'ordinal':7},transport,True)
        self.assertFalse(p.observation()['active_child'])

    def test_wrong_original_request_cannot_release_an_exec_grant(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7)
        relay=object.__new__(planner.UnitPeerRelay);relay.staged_signals=p
        relay.active_record={'last_request_ordinal':8}
        transport=object.__new__(planner.UnitPeerRelay);transport.staged_signals=c
        with self.assertRaises(planner.FreshnessHold):
            relay.unit_signal_grant({'ordinal':7},transport,lambda:self.fail('must not grant'))
        self.assertFalse(p.observation()['active_child'])


    def test_ancestor_cancellation_revokes_leaf_before_another_grant(self):
        parent,p=self.process();child,c=self.process(p.lock);next_child,n=self.process(p.lock)
        self.stop(parent,p,7);self.activate(p,7,c);self.stop(child,c,8);p.cancel()
        self.assertTrue(c.cancelled.is_set());self.assertTrue(c.observation()['revoked'])
        with self.assertRaises(planner.FreshnessHold):
            c.activate_child(8,n,lambda:self.fail('must not grant after ancestor cancellation'))


    def test_cancellation_after_target_close_still_revokes_and_preserves_the_hold(self):
        parent,p=self.process();child,c=self.process(p.lock);self.stop(parent,p,7);self.activate(p,7,c)
        child.kill();child.wait(timeout=2);c.close()
        p.cancel();p.cancel()
        self.assertTrue(p.observation()['revoked']);self.assertTrue(p.observation()['active_child'])
        with self.assertRaises(planner.FreshnessHold):p.forward(signal.SIGUSR1)
        with self.assertRaises(planner.FreshnessHold):p.resume(7,lambda:self.fail('must not thaw'))


if __name__=='__main__':unittest.main()
