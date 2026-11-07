"""Namespace graph mechanics; no execution, cleanup or CI authority."""
import os
from pathlib import Path
import tempfile
import unittest
import ci_cargo_capture_test as captures
views=captures.views


class NamespaceEdgesTests(unittest.TestCase):
    def fixture(self,root,tree):
        source=root/'source';(source/'out').mkdir(parents=True)
        for name,body in tree.items():
            path=source/'out'/name;path.parent.mkdir(parents=True,exist_ok=True)
            if body is None:path.mkdir(exist_ok=True)
            else:path.write_bytes(body)
        sealed=root/'before';capture=captures.CaptureRootsTests().capture(source,sealed)
        working=root/'working';copied=views.materialize_capture(sealed,capture,working,
            parent_uid=os.getuid(),unit_uid=os.getuid(),check=lambda:None)
        fd=os.open(source,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
        try:held=views.RetainedInodes(fd,capture,working,copied,parent_uid=os.getuid(),unit_uid=os.getuid(),check=lambda:None)
        finally:os.close(fd)
        return source,working,held

    def edges(self,root,held):
        plan=held.prepare_transfer(root/'transfer',root/'before',check=lambda:None)
        return views.namespace_edges(plan)

    def test_directory_move_keeps_descendant_edges_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'sub/file':b'child','sub/nested/leaf':b'leaf'})
            with held:
                (working/'out/sub').rename(working/'out/moved');edges=self.edges(root,held)
                self.assertEqual([e['initial_path'] for e in edges['detach']],[b'out/sub'.hex()])
                self.assertEqual([e['final_path'] for e in edges['attach']],[b'out/moved'.hex()])
                self.assertEqual((source/'out/sub/file').read_bytes(),b'child')

    def test_directory_rename_over_empty_directory_detaches_old_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'source/file':b'child','destination':None})
            with held:
                (working/'out/source').replace(working/'out/destination');edges=self.edges(root,held)
                self.assertEqual({e['initial_path'] for e in edges['detach']},{b'out/source'.hex(),b'out/destination'.hex()})
                self.assertEqual(len(edges['attach']),1);self.assertEqual(edges['attach'][0]['final_path'],b'out/destination'.hex())
                self.assertTrue((source/'out/destination').is_dir())

    def test_swapped_files_are_moves_of_two_old_objects(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'a':b'A','b':b'B'})
            with held:
                (working/'out/a').rename(working/'out/tmp');(working/'out/b').rename(working/'out/a');(working/'out/tmp').rename(working/'out/b')
                edges=self.edges(root,held)
                self.assertEqual(len(edges['detach']),2);self.assertEqual(len(edges['attach']),2)
                self.assertEqual({tuple(e['working_inode']) for e in edges['detach']},{tuple(e['working_inode']) for e in edges['attach']})
                self.assertTrue(all(e['initial_path'] is not None for e in edges['attach']))

    def test_create_order_puts_new_directory_before_its_child(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'existing':b'old'})
            with held:
                (working/'out/new').mkdir();(working/'out/new/file').write_bytes(b'new')
                edges=self.edges(root,held)
                self.assertFalse(edges['detach'])
                self.assertEqual([e['final_path'] for e in edges['attach']],[b'out/new'.hex(),b'out/new/file'.hex()])
                self.assertEqual(edges['attach'][1]['new_parent']['working_inode'],edges['attach'][0]['working_inode'])

    def test_moved_child_detaches_before_its_removed_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source,working,held=self.fixture(root,{'sub/file':b'child'})
            with held:
                (working/'out/sub/file').rename(working/'out/kept');(working/'out/sub').rmdir()
                edges=self.edges(root,held)
                self.assertEqual([e['initial_path'] for e in edges['detach']],[b'out/sub/file'.hex(),b'out/sub'.hex()])
                self.assertEqual([e['final_path'] for e in edges['attach']],[b'out/kept'.hex()])


if __name__=='__main__':unittest.main()
