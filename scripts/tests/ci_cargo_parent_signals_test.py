"""Actual pidfd signals across a stopped parent; no execution authority."""
import importlib.util
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import threading
import tempfile
import time
import unittest
import uuid

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('staged_signal_planner',ROOT/'ci-cargo-planner.py')
planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)


class StagedParentSignalTests(unittest.TestCase):
    def setUp(self):
        program=('import os,signal\n'
            'signal.signal(signal.SIGUSR1,lambda *_:os.write(1,b"USR1\\n"))\n'
            'signal.signal(signal.SIGUSR2,lambda *_:os.write(1,b"USR2\\n"))\n'
            'os.write(1,b"READY\\n")\n'
            'while True:signal.pause()\n')
        self.child=subprocess.Popen([sys.executable,'-I','-u','-c',program],stdout=subprocess.PIPE,bufsize=0)
        self.pidfd=os.pidfd_open(self.child.pid);self.gate=None
        self.assertEqual(self.line(),b'READY\n')

    def tearDown(self):
        if self.child.poll() is None:self.child.kill()
        self.child.wait(timeout=2)
        if self.gate is not None:self.gate.close()
        os.close(self.pidfd);self.child.stdout.close()

    def line(self):
        self.assertTrue(select.select([self.child.stdout],[],[],2)[0],'signal receiver did not respond')
        data=b''
        while not data.endswith(b'\n'):
            block=os.read(self.child.stdout.fileno(),1)
            self.assertTrue(block);data+=block
        return data

    def stopped(self):
        return Path(f'/proc/{self.child.pid}/stat').read_text().rsplit(')',1)[1].split()[0]=='T'

    def stop(self):
        signal.pidfd_send_signal(self.pidfd,signal.SIGSTOP)
        deadline=time.monotonic()+2
        while not self.stopped() and time.monotonic()<deadline:time.sleep(.005)
        self.assertTrue(self.stopped())

    def thaw(self):signal.pidfd_send_signal(self.pidfd,signal.SIGCONT)

    def new_gate(self):
        self.gate=planner.StagedParentSignals(self.pidfd);return self.gate

    def test_running_parent_receives_original_signal(self):
        gate=self.new_gate();self.assertEqual(gate.forward(signal.SIGUSR1),'forwarded')
        self.assertEqual(self.line(),b'USR1\n')
        self.assertEqual(gate.observation()['delivered'],1)

    def test_frozen_parent_receives_nothing_until_checked_thaw(self):
        gate=self.new_gate();gate.pause(7);self.stop()
        self.assertEqual(gate.forward(signal.SIGUSR1),'queued');self.assertTrue(gate.cancelled.is_set())
        self.assertEqual(select.select([self.child.stdout],[],[],.03)[0],[])
        self.assertTrue(self.stopped());gate.resume(7,self.thaw)
        self.assertEqual(self.line(),b'USR1\n')
        self.assertEqual(gate.observation()['queued'],0)

    def test_wrong_or_duplicate_epoch_cannot_thaw_or_deliver_twice(self):
        gate=self.new_gate();gate.pause(7);self.stop();gate.forward(signal.SIGUSR1)
        with self.assertRaises(planner.FreshnessHold):gate.resume(8,self.thaw)
        self.assertTrue(self.stopped());gate.resume(7,self.thaw)
        self.assertEqual(self.line(),b'USR1\n')
        with self.assertRaises(planner.FreshnessHold):gate.resume(7,self.thaw)
        self.assertEqual(select.select([self.child.stdout],[],[],.03)[0],[])

    def test_failed_thaw_keeps_the_queue_and_cancelled_state(self):
        gate=self.new_gate();gate.pause(7);self.stop();gate.forward(signal.SIGUSR1)
        def failed():raise OSError('injected thaw failure')
        with self.assertRaises(OSError):gate.resume(7,failed)
        self.assertTrue(self.stopped());self.assertTrue(gate.cancelled.is_set())
        self.assertEqual(gate.observation()['queued'],1)
        gate.resume(7,self.thaw);self.assertEqual(self.line(),b'USR1\n')

    def test_signal_racing_thaw_cannot_pass_the_resume_barrier(self):
        gate=self.new_gate();gate.pause(7);self.stop();gate.forward(signal.SIGUSR1)
        entered=threading.Event();release=threading.Event();outcome=[]
        def thaw():
            entered.set()
            if not release.wait(2):raise RuntimeError('test thaw barrier timed out')
            self.thaw()
        def resume():
            try:gate.resume(7,thaw)
            except BaseException as error:outcome.append(error)
        def forward():
            try:outcome.append(gate.forward(signal.SIGUSR2))
            except BaseException as error:outcome.append(error)
        resumer=threading.Thread(target=resume);sender=threading.Thread(target=forward)
        resumer.start()
        try:
            self.assertTrue(entered.wait(2));sender.start();sender.join(.03)
            self.assertTrue(sender.is_alive());self.assertTrue(self.stopped())
        finally:
            release.set();resumer.join(2)
            if sender.ident is not None:sender.join(2)
        self.assertFalse(resumer.is_alive());self.assertFalse(sender.is_alive())
        self.assertEqual(outcome,['forwarded'])
        self.assertEqual({self.line(),self.line()},{b'USR1\n',b'USR2\n'})

    def test_late_signals_and_the_original_4096_bound_are_explicit(self):
        gate=self.new_gate();self.child.terminate();self.child.wait(timeout=2)
        for _ in range(4096):self.assertEqual(gate.forward(signal.SIGUSR1),'late')
        with self.assertRaises(planner.FreshnessHold):gate.forward(signal.SIGUSR1)
        self.assertEqual(gate.observation()['late'],4096)
        self.assertTrue(gate.cancelled.is_set())

    def test_revoked_request_cannot_start_another_capture(self):
        gate=self.new_gate();gate.pause(7);self.stop();gate.cancel();gate.resume(7,self.thaw)
        self.assertTrue(gate.observation()['revoked'])
        with self.assertRaises(planner.FreshnessHold):gate.pause(8)

    def test_busy_unit_socket_revokes_staging_without_starting_another_handler(self):
        gate=self.new_gate();gate.pause(7);self.stop()
        with tempfile.TemporaryDirectory() as directory,\
                socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET) as server,\
                socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET) as client:
            path=str(Path(directory)/'relay.sock');server.bind(path);server.listen(1);server.settimeout(.03)
            # Exercise pre-auth rejection only. This fixture cannot authenticate
            # or execute a request; the existing active slot is already held.
            relay=object.__new__(planner.UnitPeerRelay)
            relay.listener=server;relay._stop=threading.Event();relay._slots=threading.BoundedSemaphore(1)
            relay._slots.acquire();relay._threads=[];relay.errors=[];relay.staged_signals=gate
            binding={'generation':str(uuid.uuid4()),'head':'a'*40,'tree':'b'*40,
                **{key:'c'*64 for key in ('contract_sha256','authority_sha256','cache_identity_sha256','source_inputs_sha256')}}
            relay.ordinals=planner.DispatchOrdinals(binding,str(uuid.uuid4()))
            thread=threading.Thread(target=relay._accept);thread.start();client.settimeout(2)
            try:
                client.connect(path)
                self.assertEqual(planner.decode_event(client.recv(16)),('error',16))
                thread.join(2);self.assertFalse(thread.is_alive())
                self.assertTrue(relay._stop.is_set());self.assertTrue(gate.observation()['revoked'])
                self.assertTrue(gate.cancelled.is_set());self.assertEqual(relay._threads,[])
                self.assertEqual(relay.errors,[{'stage':'planner_request_receive','category':'FreshnessHold','errno':16}])
            finally:relay._stop.set();thread.join(2)


if __name__=='__main__':unittest.main()
