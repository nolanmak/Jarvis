"""Command-wide ordinal ownership across concurrent relay callers."""
from concurrent.futures import ThreadPoolExecutor
import copy
import importlib.util
import os
import socket
import tempfile
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


if __name__=='__main__':unittest.main()
