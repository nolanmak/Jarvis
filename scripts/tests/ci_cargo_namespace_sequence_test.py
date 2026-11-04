"""Multiple actual namespace steps with a bound, durable completion prefix."""
import errno
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import ci_cargo_capture_test as captures
views=captures.views


class NamespaceSequenceTests(unittest.TestCase):
    def fixture(self,root):
        (root/'out').mkdir();(root/'out/stage').mkdir(mode=0o700)
        (root/'out/a').write_bytes(b'A');(root/'out/b').write_bytes(b'B')
        (root/'journal').mkdir(mode=0o700)
        opened=[]
        def descriptor(path,directory=False):
            fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC|(os.O_DIRECTORY if directory else 0));opened.append(fd);return fd
        nodes={'a':descriptor(root/'out/a'),'b':descriptor(root/'out/b')}
        parents={'out':descriptor(root/'out',True),'stage':descriptor(root/'out/stage',True)}
        steps=[{'action':'move','inode':'a','source_parent':'out','source_name':b'a'.hex(),'destination_parent':'stage','destination_name':b'a'.hex()},
               {'action':'move','inode':'b','source_parent':'out','source_name':b'b'.hex(),'destination_parent':'out','destination_name':b'a'.hex()},
               {'action':'move','inode':'a','source_parent':'stage','source_name':b'a'.hex(),'destination_parent':'out','destination_name':b'b'.hex()}]
        self.addCleanup(lambda:[os.close(fd) for fd in opened])
        digest=hashlib.sha256(json.dumps(steps,sort_keys=True).encode()).hexdigest()
        return nodes,parents,steps,digest

    def run_sequence(self,root,nodes,parents,steps,digest):
        return views.apply_namespace_sequence(root/'journal',plan_sha256=digest,
            steps=steps,inodes=nodes,parents=parents,check=lambda:None)

    def assert_swap(self,root,nodes):
        self.assertEqual((root/'out/a').read_bytes(),b'B');self.assertEqual((root/'out/b').read_bytes(),b'A')
        self.assertEqual((root/'out/a').stat().st_ino,os.fstat(nodes['b']).st_ino)
        self.assertEqual((root/'out/b').stat().st_ino,os.fstat(nodes['a']).st_ino)
        self.assertEqual(list((root/'out/stage').iterdir()),[])

    def test_three_step_swap_commits_the_actual_original_inodes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            result=self.run_sequence(root,nodes,parents,steps,digest);self.assert_swap(root,nodes)
            self.assertEqual(result['next_step'],3);self.assertEqual(result['state'],'namespace_sequence_applied')
            self.assertFalse(result['effects_materialized']);self.assertFalse(result['shipping_authority'])

    def test_each_lost_master_commit_recovers_only_the_active_step(self):
        for completed in (1,2,3):
            with self.subTest(completed=completed),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);nodes,parents,steps,digest=self.fixture(root);real=views.cache._write_record;failed=[]
                def interrupted(path,value):
                    if path.name=='sequence.json' and value.get('next_step')==completed and not failed:
                        failed.append(True);raise OSError(errno.ENOSPC,'master sync failed')
                    return real(path,value)
                with patch.object(views.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):
                    self.run_sequence(root,nodes,parents,steps,digest)
                result=self.run_sequence(root,nodes,parents,steps,digest)
                self.assert_swap(root,nodes);self.assertEqual(result['next_step'],3)

    def test_completed_prefix_is_not_replayed_after_later_parents_have_changed(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root);real=views.apply_namespace_step;calls=[]
            def interrupted(path,**kwargs):
                calls.append(path.name)
                if path.name=='step-2.json':raise OSError(errno.EINTR,'before final operation')
                return real(path,**kwargs)
            with patch.object(views,'apply_namespace_step',side_effect=interrupted),self.assertRaises(OSError):
                self.run_sequence(root,nodes,parents,steps,digest)
            calls=[]
            def resumed(path,**kwargs):calls.append(path.name);return real(path,**kwargs)
            with patch.object(views,'apply_namespace_step',side_effect=resumed):self.run_sequence(root,nodes,parents,steps,digest)
            self.assertEqual(calls,['step-2.json']);self.assert_swap(root,nodes)

    def test_invalid_later_operation_is_refused_before_the_first_move(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            steps[-1]['destination_name']=b'a'.hex()
            with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
            self.assertEqual((root/'out/a').read_bytes(),b'A');self.assertEqual((root/'out/b').read_bytes(),b'B')
            self.assertEqual(list((root/'out/stage').iterdir()),[])

    def test_unrelated_foreign_change_during_active_step_cannot_be_adopted(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root);real=views.apply_namespace_step
            def interrupted(path,**kwargs):
                if path.name=='step-2.json':raise OSError(errno.EINTR,'before final operation')
                return real(path,**kwargs)
            with patch.object(views,'apply_namespace_step',side_effect=interrupted),self.assertRaises(OSError):
                self.run_sequence(root,nodes,parents,steps,digest)
            (root/'out/foreign').write_bytes(b'foreign')
            with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
            self.assertEqual((root/'out/foreign').read_bytes(),b'foreign')
            self.assertFalse((root/'out/b').exists())

    def test_after_completion_graph_drift_cannot_be_hidden_by_the_finished_prefix(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            self.run_sequence(root,nodes,parents,steps,digest)
            (root/'out/a').write_bytes(b'foreign later write')
            with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
            self.assertEqual((root/'out/a').read_bytes(),b'foreign later write')

    def test_bound_operation_list_cannot_change_during_recovery(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            with patch.object(views,'apply_namespace_step',side_effect=OSError(errno.EINTR,'before first operation')),self.assertRaises(OSError):
                self.run_sequence(root,nodes,parents,steps,digest)
            steps[0]['destination_name']=b'other'.hex()
            with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
            self.assertTrue((root/'out/a').exists())


    def test_each_lost_syscall_acknowledgement_recovers_without_repeating_the_prefix(self):
        for number in (1,2,3):
            with self.subTest(number=number),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);nodes,parents,steps,digest=self.fixture(root);real=views._rename_noreplace;calls=[]
                def interrupted(*args):
                    real(*args);calls.append(True)
                    if len(calls)==number:raise OSError(errno.EIO,'lost kernel acknowledgement')
                with patch.object(views,'_rename_noreplace',side_effect=interrupted),self.assertRaises(OSError):
                    self.run_sequence(root,nodes,parents,steps,digest)
                self.run_sequence(root,nodes,parents,steps,digest);self.assert_swap(root,nodes)

    def test_directory_rename_over_existing_keeps_descendant_inode_and_ctime(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,unused,digest=self.fixture(root)
            (root/'out/source').mkdir();(root/'out/source/child').write_bytes(b'child');(root/'out/destination').mkdir()
            before=(root/'out/source/child').stat();extra=[]
            def cap(path,directory=False):
                fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC|(os.O_DIRECTORY if directory else 0));extra.append(fd);return fd
            nodes.update(source=cap(root/'out/source',True),destination=cap(root/'out/destination',True),child=cap(root/'out/source/child'))
            steps=[{'action':'remove','inode':'destination','source_parent':'out','source_name':b'destination'.hex()},
                {'action':'move','inode':'source','source_parent':'out','source_name':b'source'.hex(),'destination_parent':'stage','destination_name':b'directory'.hex()},
                {'action':'move','inode':'source','source_parent':'stage','source_name':b'directory'.hex(),'destination_parent':'out','destination_name':b'destination'.hex()}]
            try:
                result=self.run_sequence(root,nodes,parents,steps,digest);after=(root/'out/destination/child').stat()
                self.assertEqual((after.st_dev,after.st_ino,after.st_ctime_ns),(before.st_dev,before.st_ino,before.st_ctime_ns))
                self.assertEqual(os.fstat(nodes['destination']).st_nlink,0);self.assertEqual(result['next_step'],3)
            finally:
                for fd in extra:os.close(fd)

    def test_deleted_old_inode_and_attached_anonymous_replacement_remain_distinct(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,unused,digest=self.fixture(root)
            prepared=os.open('.',os.O_TMPFILE|os.O_RDWR|os.O_CLOEXEC,0o600,dir_fd=parents['out'])
            try:
                os.write(prepared,b'new inode bytes');os.fsync(prepared);nodes['new']=prepared
                steps=[{'action':'remove','inode':'a','source_parent':'out','source_name':b'a'.hex()},
                    {'action':'attach','inode':'new','destination_parent':'out','destination_name':b'a'.hex()}]
                self.run_sequence(root,nodes,parents,steps,digest)
                self.assertEqual((root/'out/a').read_bytes(),b'new inode bytes')
                self.assertEqual(os.pread(nodes['a'],100,0),b'A');self.assertEqual(os.fstat(nodes['a']).st_nlink,0)
                self.assertEqual((root/'out/a').stat().st_ino,os.fstat(prepared).st_ino)
            finally:os.close(prepared)

    def test_move_child_before_removing_its_parent_updates_all_aliasing_parent_views(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,unused,digest=self.fixture(root)
            (root/'out/sub').mkdir();(root/'out/sub/child').write_bytes(b'child')
            sub=os.open(root/'out/sub',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC);child=os.open(root/'out/sub/child',os.O_RDONLY|os.O_CLOEXEC)
            try:
                nodes.update(sub=sub,child=child);parents['sub']=sub
                steps=[{'action':'move','inode':'child','source_parent':'sub','source_name':b'child'.hex(),'destination_parent':'out','destination_name':b'kept'.hex()},
                    {'action':'remove','inode':'sub','source_parent':'out','source_name':b'sub'.hex()}]
                result=self.run_sequence(root,nodes,parents,steps,digest)
                self.assertEqual(result['next_step'],2);self.assertEqual((root/'out/kept').read_bytes(),b'child')
                self.assertFalse((root/'out/sub').exists())
            finally:os.close(child);os.close(sub)

    def test_unrelated_bound_inode_change_is_refused_before_recovering_active_move(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            (root/'elsewhere').mkdir();other=root/'elsewhere/other';other.write_bytes(b'unrelated bound bytes')
            descriptor=os.open(other,os.O_RDONLY|os.O_CLOEXEC);nodes['other']=descriptor;real=views._rename_noreplace
            def interrupted(*args):real(*args);raise OSError(errno.EIO,'lost acknowledgement')
            try:
                with patch.object(views,'_rename_noreplace',side_effect=interrupted),self.assertRaises(OSError):
                    self.run_sequence(root,nodes,parents,steps,digest)
                other.write_bytes(b'foreign changed bytes')
                with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
                self.assertTrue((root/'out/stage/a').exists());self.assertEqual((root/'out/b').read_bytes(),b'B')
                self.assertFalse((root/'out/a').exists());self.assertEqual(other.read_bytes(),b'foreign changed bytes')
            finally:os.close(descriptor)


    def test_nonempty_directory_removal_is_rejected_before_earlier_valid_moves(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            (root/'out/nonempty').mkdir();(root/'out/nonempty/child').write_bytes(b'protected child')
            descriptor=os.open(root/'out/nonempty',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
            try:
                nodes['nonempty']=descriptor;steps.append({'action':'remove','inode':'nonempty','source_parent':'out','source_name':b'nonempty'.hex()})
                with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
                self.assertEqual((root/'out/a').read_bytes(),b'A');self.assertEqual((root/'out/b').read_bytes(),b'B')
                self.assertEqual((root/'out/nonempty/child').read_bytes(),b'protected child')
            finally:os.close(descriptor)

    def test_directory_cycle_is_rejected_before_earlier_valid_moves(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            (root/'out/sub/inner').mkdir(parents=True)
            sub=os.open(root/'out/sub',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC);inner=os.open(root/'out/sub/inner',os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
            try:
                nodes['sub']=sub;parents['inner']=inner
                steps.append({'action':'move','inode':'sub','source_parent':'out','source_name':b'sub'.hex(),
                    'destination_parent':'inner','destination_name':b'cycle'.hex()})
                with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
                self.assertEqual((root/'out/a').read_bytes(),b'A');self.assertEqual((root/'out/b').read_bytes(),b'B')
            finally:os.close(inner);os.close(sub)

    def test_no_sequence_operation_precedes_durable_master_preparation(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            with patch.object(views.cache,'_write_record',side_effect=OSError(errno.ENOSPC,'master storage full')):
                with patch.object(views,'apply_namespace_step',side_effect=AssertionError('operation before master')),self.assertRaises(OSError):
                    self.run_sequence(root,nodes,parents,steps,digest)
            self.assertEqual((root/'out/a').read_bytes(),b'A');self.assertEqual((root/'out/b').read_bytes(),b'B')

    def test_lost_final_completion_record_still_revalidates_the_entire_current_graph(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root);real=views.cache._write_record
            def interrupted(path,value):
                if path.name=='sequence.json' and value.get('state')=='namespace_sequence_applied':
                    raise OSError(errno.ENOSPC,'final commit unavailable')
                return real(path,value)
            with patch.object(views.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):
                self.run_sequence(root,nodes,parents,steps,digest)
            (root/'out/a').write_bytes(b'foreign after last step')
            with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
            self.assertEqual((root/'out/a').read_bytes(),b'foreign after last step')

    def test_lost_final_completion_record_can_commit_without_repeating_any_move(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root);real=views.cache._write_record;failed=[]
            def interrupted(path,value):
                if path.name=='sequence.json' and value.get('state')=='namespace_sequence_applied' and not failed:
                    failed.append(True);raise OSError(errno.ENOSPC,'final commit unavailable')
                return real(path,value)
            with patch.object(views.cache,'_write_record',side_effect=interrupted),self.assertRaises(OSError):
                self.run_sequence(root,nodes,parents,steps,digest)
            with patch.object(views,'apply_namespace_step',side_effect=AssertionError('prefix must not repeat')):
                result=self.run_sequence(root,nodes,parents,steps,digest)
            self.assertEqual(result['state'],'namespace_sequence_applied');self.assert_swap(root,nodes)


    def test_unsupported_later_inode_mode_is_refused_before_earlier_moves(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);nodes,parents,steps,digest=self.fixture(root)
            (root/'out/b').chmod(0o4644)
            with self.assertRaises(views.FreshnessHold):self.run_sequence(root,nodes,parents,steps,digest)
            self.assertTrue((root/'out/a').exists());self.assertEqual((root/'out/a').read_bytes(),b'A')
            self.assertEqual(list((root/'out/stage').iterdir()),[])


if __name__=='__main__':unittest.main()
