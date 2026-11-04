"""Command-wide ordinal ownership across concurrent relay callers."""
from concurrent.futures import ThreadPoolExecutor
import copy
import importlib.util
import os
import socket
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
import unittest
import uuid

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('dispatch_planner',ROOT/'ci-cargo-planner.py')
planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)


class DispatchOrdinalsTests(unittest.TestCase):
    def binding(self):
        return {'generation':str(uuid.uuid4()),'head':'a'*40,'tree':'b'*40,
            **{key:'c'*64 for key in ('contract_sha256','authority_sha256','cache_identity_sha256','source_inputs_sha256')}}

    def test_concurrent_relays_share_the_existing_total_and_never_reuse_a_uid(self):
        binding=self.binding();command=str(uuid.uuid4());budget=planner.DispatchOrdinals(binding,command)
        def caller(_):
            budget.require(binding,command)
            return [budget.allocate() for _ in range(1024)]
        with ThreadPoolExecutor(max_workers=4) as threads:
            allocated=[value for batch in threads.map(caller,range(4)) for value in batch]
        self.assertEqual(sorted(allocated),list(range(4096)))
        self.assertEqual(len({2000+value for value in allocated}),4096)
        self.assertEqual(max(2000+value for value in allocated),6095)
        for _ in range(3):
            with self.assertRaises(planner.FreshnessHold):budget.allocate()

    def test_foreign_command_or_binding_cannot_borrow_an_existing_counter(self):
        binding=self.binding();command=str(uuid.uuid4());budget=planner.DispatchOrdinals(binding,command)
        for key in binding:
            wrong=copy.deepcopy(binding);wrong[key]=str(uuid.uuid4()) if key=='generation' else 'd'*len(wrong[key])
            with self.subTest(key=key),self.assertRaises(planner.FreshnessHold):budget.require(wrong,command)
        with self.assertRaises(planner.FreshnessHold):budget.require(binding,str(uuid.uuid4()))
        budget.require(binding,command);self.assertEqual(budget.allocate(),0)

    def test_mutating_the_original_dictionary_does_not_change_counter_ownership(self):
        binding=self.binding();original=copy.deepcopy(binding);command=str(uuid.uuid4())
        budget=planner.DispatchOrdinals(binding,command);binding['head']='d'*40
        with self.assertRaises(planner.FreshnessHold):budget.require(binding,command)
        budget.require(original,command);self.assertEqual(budget.allocate(),0)

    def test_forked_copy_cannot_allocate_an_unobserved_duplicate_ordinal(self):
        budget=planner.DispatchOrdinals(self.binding(),str(uuid.uuid4()))
        read,write=os.pipe();child=os.fork()
        if child==0:
            os.close(read)
            try:
                budget.allocate();message=b'accepted'
            except planner.FreshnessHold:message=b'refused'
            os.write(write,message);os.close(write);os._exit(0)
        os.close(write)
        try:self.assertEqual(os.read(read,16),b'refused')
        finally:os.close(read);self.assertEqual(os.waitpid(child,0)[1],0)
        self.assertEqual(budget.allocate(),0)

    def test_fenced_command_cancels_registered_controllers_and_cannot_allocate_again(self):
        import threading
        budget=planner.DispatchOrdinals(self.binding(),str(uuid.uuid4()))
        first=threading.Event();second=threading.Event();budget.register_stop(first);budget.register_stop(second)
        self.assertEqual(budget.allocate(),0);budget.fence()
        self.assertTrue(first.is_set());self.assertTrue(second.is_set());self.assertTrue(budget.fenced())
        with self.assertRaises(planner.FreshnessHold):budget.allocate()
        late=threading.Event();budget.register_stop(late);self.assertTrue(late.is_set())

    def test_fencing_does_not_reset_or_replace_the_original_command(self):
        budget=planner.DispatchOrdinals(self.binding(),str(uuid.uuid4()));budget.fence();budget.fence()
        self.assertTrue(budget.fenced())
        with self.assertRaises(planner.FreshnessHold):budget.allocate()


class PathnameSocketTests(unittest.TestCase):
    def test_long_private_path_binds_via_directory_fd_and_connects_after_fd_close(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)/('a'*70)/('b'*70);root.mkdir(parents=True,mode=0o700)
            path=root/'relay.sock';self.assertGreater(len(os.fsencode(path)),108)
            with socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET) as server:
                planner.bind_controller_socket(server,path);server.listen(1)
                # A short alias exercises the same inode via a different name.
                alias=Path(directory)/'alias';alias.symlink_to(root,target_is_directory=True)
                with socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET) as client:
                    client.connect(str(alias/'relay.sock'))
                    connection,_=server.accept()
                    with connection:connection.sendall(b'bound')
                    self.assertEqual(client.recv(16),b'bound')

    def test_writable_or_symlinked_parent_is_refused_before_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)/'root';root.mkdir(mode=0o700)
            alias=Path(directory)/'alias';alias.symlink_to(root,target_is_directory=True)
            for path in (alias/'relay.sock',root/'relay.sock'):
                if path.parent==root:root.chmod(0o777)
                with socket.socket(socket.AF_UNIX,socket.SOCK_SEQPACKET) as server:
                    with self.assertRaises(planner.FreshnessHold):planner.bind_controller_socket(server,path)
                self.assertFalse((root/'relay.sock').exists())


class AdmissionFenceTests(unittest.TestCase):
    def test_controller_refusal_fences_the_command_and_does_not_release_its_slot(self):
        for failure in (planner.FreshnessHold('metadata_torn'),MemoryError()):
            with self.subTest(failure=type(failure).__name__):
                # Rejection only: no admission or execution can occur in this
                # fixture. A held slot represents the original request.
                server,client=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
                relay=object.__new__(planner.RootRelay);relay.errors=[];relay._stop=threading.Event();relay.timeout=.5
                relay.ordinals=planner.DispatchOrdinals(DispatchOrdinalsTests().binding(),str(uuid.uuid4()))
                relay.ordinals.register_stop(relay._stop)
                relay._slots=threading.BoundedSemaphore(1);relay._slots.acquire()
                def refused(*_args):raise failure
                relay._receive=refused
                try:
                    relay._handle(server,0)
                    self.assertEqual(planner.decode_event(client.recv(16)),('error',22))
                    self.assertTrue(relay.ordinals.fenced());self.assertTrue(relay._stop.is_set())
                    self.assertFalse(relay._slots.acquire(blocking=False))
                finally:server.close();client.close()


class RequestStdinTests(unittest.TestCase):
    def stage(self,source,client,server,*,owner=None,stop=None):
        return planner.stage_request_stdin(server,source,deadline=time.monotonic()+3,
            stop=threading.Event() if stop is None else stop,owner_pidfd=owner)

    def test_original_stream_is_read_after_ready_and_sealed_without_tool_start(self):
        reader,writer=os.pipe2(os.O_CLOEXEC)
        client,server=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
        client.settimeout(2)
        body=b'original\x00stdin\xff'*4096
        outcome=[]
        def controller():
            try:outcome.append(self.stage(reader,client,server))
            except BaseException as error:outcome.append(error)
        thread=threading.Thread(target=controller);thread.start()
        try:
            self.assertEqual(planner.decode_event(client.recv(16)),('staging_ready',0))
            remaining=memoryview(body)
            while remaining:
                count=os.write(writer,remaining[:4096]);remaining=remaining[count:]
            os.close(writer);writer=None;thread.join(timeout=4)
            self.assertFalse(thread.is_alive());self.assertEqual(len(outcome),1)
            captured=outcome[0]
            if isinstance(captured,BaseException):raise captured
            with captured:
                self.assertEqual(os.read(captured.fileno(),len(body)+1),body)
                self.assertEqual(captured.observation()['size_bytes'],len(body))
                self.assertFalse(captured.observation()['shipping_authority'])
                with self.assertRaises(OSError):os.write(captured.fileno(),b'change')
            self.assertEqual(select.select([client],[],[],0)[0],[])
        finally:
            client.close();server.close();os.close(reader)
            if writer is not None:os.close(writer)
            thread.join(timeout=4)

    def test_transport_loss_cancels_an_open_stdin_instead_of_waiting_for_eof(self):
        self.cancel_case('transport')

    def test_original_owner_death_cancels_an_open_stdin(self):
        self.cancel_case('owner')

    def test_command_stop_cancels_an_open_stdin(self):
        self.cancel_case('stop')

    def test_original_signal_before_tool_start_cancels_staging(self):
        self.cancel_case('signal')

    def cancel_case(self,mode):
        reader,writer=os.pipe2(os.O_CLOEXEC)
        client,server=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
        client.settimeout(2)
        owner=subprocess.Popen([sys.executable,'-I','-c','import time;time.sleep(10)']) if mode=='owner' else None
        pidfd=os.pidfd_open(owner.pid) if owner is not None else None
        stop=threading.Event();outcome=[]
        def controller():
            try:
                with self.stage(reader,client,server,owner=pidfd,stop=stop):outcome.append('captured')
            except BaseException as error:outcome.append(error)
        thread=threading.Thread(target=controller);thread.start()
        try:
            self.assertEqual(planner.decode_event(client.recv(16)),('staging_ready',0))
            started=time.monotonic()
            if mode=='transport':client.close()
            elif mode=='owner':owner.terminate();owner.wait(timeout=2)
            elif mode=='signal':client.sendall(planner.legacy.RESPONSE.pack(b'JARVSIG3',signal.SIGTERM,0))
            else:stop.set()
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive());self.assertLess(time.monotonic()-started,2)
            self.assertEqual(len(outcome),1)
            self.assertIsInstance(outcome[0],planner.staging.StagingUnavailable)
        finally:
            client.close();server.close();os.close(reader);os.close(writer)
            if owner is not None:
                if owner.poll() is None:owner.terminate();owner.wait(timeout=2)
                os.close(pidfd)
            thread.join(timeout=4)


if __name__=='__main__':unittest.main()
