"""Filesystem mechanics only; root namespace/credential evidence needs the VM."""
import importlib.util
import os
from pathlib import Path
import socket
import stat
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('unit_views',ROOT/'ci-cargo-views.py')
views=importlib.util.module_from_spec(spec);spec.loader.exec_module(views)


class UnitViewTests(unittest.TestCase):
    def test_toolchain_view_exposes_children_without_changing_private_source_root(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';source.mkdir(mode=0o700)
            for name in ('bin','lib','libexec','share','etc'):(source/name).mkdir(mode=0o755)
            before=source.stat();destination=root/'runtime';destination.mkdir()
            with patch.object(views,'_readonly') as mount:
                views.toolchain_view(source,destination)
            after=source.stat()
            self.assertEqual((after.st_mode,after.st_ino,after.st_ctime_ns),
                (before.st_mode,before.st_ino,before.st_ctime_ns))
            self.assertEqual(stat.S_IMODE(destination.stat().st_mode),0o555)
            self.assertEqual({(str(call.args[0]),str(call.args[1])) for call in mount.call_args_list},
                {(str(source/name),str(destination/name)) for name in ('bin','lib','libexec','share','etc')})

    def tree(self,root):
        root.mkdir(mode=0o700);(root/'deps').mkdir(mode=0o700)
        (root/'deps/input').write_bytes(b'OLD');(root/'deps/input').chmod(0o444)
        (root/'deps').chmod(0o555);root.chmod(0o555)

    def test_mount_plan_preserves_original_input_inode_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'sealed';self.tree(source)
            manifest=views.inventory(source,owner=os.getuid(),sealed=True)
            before=(source/'deps/input').stat()
            view=root/'view';bindings=views.clone_inputs(source,view,manifest,owner=os.getuid(),unit=os.getuid())
            after=(source/'deps/input').stat()
            self.assertEqual((after.st_ino,after.st_nlink,after.st_ctime_ns,after.st_mtime_ns,after.st_atime_ns),
                (before.st_ino,before.st_nlink,before.st_ctime_ns,before.st_mtime_ns,before.st_atime_ns))
            self.assertEqual(bindings,[[os.fsencode('deps/input').hex(),str(source/'deps/input'),manifest[-1]]])
            self.assertEqual((view/'deps/input').read_bytes(),b'')
            # The next publication is a new immutable version; an active view
            # keeps its original source path until its original cleanup.
            newer=root/'next-version';newer.mkdir();(newer/'input').write_bytes(b'NEW')
            self.assertEqual(Path(bindings[0][1]).read_bytes(),b'OLD')
            self.assertEqual((newer/'input').read_bytes(),b'NEW')

    def test_source_change_or_nonsealed_input_cannot_clone(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'sealed';self.tree(source)
            manifest=views.inventory(source,owner=os.getuid(),sealed=True)
            (source/'deps/input').chmod(0o644)
            with self.assertRaises(views.FreshnessHold):views.clone_inputs(source,root/'view',manifest,owner=os.getuid(),unit=os.getuid())
            self.assertFalse((root/'view').exists())

    def test_sealed_input_with_an_external_inode_alias_has_no_admission(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'sealed';self.tree(source)
            os.link(source/'deps/input',root/'external-alias')
            with self.assertRaises(views.FreshnessHold):views.inventory(source,owner=os.getuid(),sealed=True)

    def test_inventory_is_lossless_and_refuses_symlinks_sockets_and_setid(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'sealed';self.tree(source);source.chmod(0o700)
            raw=source/os.fsdecode(b'raw-\xff');raw.write_bytes(b'bytes');raw.chmod(0o444);source.chmod(0o555)
            manifest=views.inventory(source,owner=os.getuid(),sealed=True)
            self.assertIn(os.fsencode(raw.name).hex(),[row[0] for row in manifest])
            source.chmod(0o700);raw.unlink();raw.symlink_to('deps/input');source.chmod(0o555)
            with self.assertRaises(views.FreshnessHold):views.inventory(source,owner=os.getuid(),sealed=True)
            source.chmod(0o700);raw.unlink()
            endpoint=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
            try:
                endpoint.bind(str(raw));source.chmod(0o555)
                with self.assertRaises(views.FreshnessHold):views.inventory(source,owner=os.getuid(),sealed=True)
            finally:endpoint.close()
            source.chmod(0o700);raw.unlink();(source/'deps/input').chmod(0o4444);source.chmod(0o555)
            with self.assertRaises(views.FreshnessHold):views.inventory(source,owner=os.getuid(),sealed=True)

    def test_output_copy_detaches_unit_writable_inode_and_seals_exact_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);output=root/'unit';output.mkdir(mode=0o700)
            (output/'result').write_bytes(b'compiled bytes')
            sealed=root/'published';manifest=views.copy_outputs(output,sealed,['result'],owner=os.getuid(),controller=os.getuid())
            self.assertEqual((sealed/'result').read_bytes(),b'compiled bytes')
            self.assertNotEqual((sealed/'result').stat().st_ino,(output/'result').stat().st_ino)
            self.assertEqual(stat.S_IMODE((sealed/'result').stat().st_mode),0o444)
            (output/'result').write_bytes(b'modified later')
            self.assertEqual((sealed/'result').read_bytes(),b'compiled bytes')
            self.assertEqual(views.inventory(sealed,owner=os.getuid(),sealed=True),manifest)

    def test_output_alias_or_escaping_path_cannot_publish(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);output=root/'unit';output.mkdir(mode=0o700)
            outside=root/'outside';outside.write_bytes(b'private')
            for kind in ('symlink','hardlink','parent','absolute'):
                with self.subTest(kind=kind):
                    path=output/'result'
                    if path.exists() or path.is_symlink():path.unlink()
                    if kind=='symlink':path.symlink_to(outside)
                    elif kind=='hardlink':os.link(outside,path)
                    name='../outside' if kind=='parent' else str(outside) if kind=='absolute' else 'result'
                    with self.assertRaises(views.FreshnessHold):views.copy_outputs(output,root/kind,[name],owner=os.getuid(),controller=os.getuid())
            self.assertEqual(outside.read_bytes(),b'private')

    def test_output_mutation_during_copy_leaves_no_sealed_result(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);output=root/'unit';output.mkdir(mode=0o700);file=output/'result';file.write_bytes(b'original')
            original=os.read
            def changed(fd,size):
                data=original(fd,size)
                if data:file.write_bytes(b'replaced')
                return data
            with patch.object(views.os,'read',side_effect=changed),self.assertRaises(views.FreshnessHold):
                views.copy_outputs(output,root/'published',['result'],owner=os.getuid(),controller=os.getuid())
            self.assertFalse((root/'published/manifest.json').exists())

    @unittest.skipUnless(hasattr(os,'setxattr'),'requires filesystem extended attributes')
    def test_unmodeled_extended_attributes_cannot_be_dropped_from_inputs_or_outputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);source=root/'sealed';self.tree(source)
            (source/'deps/input').chmod(0o644)
            try:os.setxattr(source/'deps/input',b'user.unit-probe',b'unmodeled input')
            except OSError as error:
                if error.errno in (95,1):self.skipTest('host filesystem disallows user attributes')
                raise
            (source/'deps/input').chmod(0o444)
            with self.assertRaises(views.FreshnessHold):views.inventory(source,owner=os.getuid(),sealed=True)
            (source/'deps/input').chmod(0o644);os.removexattr(source/'deps/input',b'user.unit-probe');(source/'deps/input').chmod(0o444)
            source.chmod(0o755);os.setxattr(source,b'user.unit-probe',b'unmodeled directory input');source.chmod(0o555)
            with self.assertRaises(views.FreshnessHold):views.inventory(source,owner=os.getuid(),sealed=True)
            output=root/'unit';output.mkdir(mode=0o700);(output/'result').write_bytes(b'output')
            os.setxattr(output/'result',b'user.unit-probe',b'unmodeled output')
            with self.assertRaises(views.FreshnessHold):views.copy_outputs(output,root/'published',['result'],
                owner=os.getuid(),controller=os.getuid())

    def test_socket_capabilities_are_never_valid_stdio(self):
        reader,writer=os.pipe2(os.O_CLOEXEC);left,right=socket.socketpair()
        try:
            views.validate_stdio([reader,writer,writer])
            with self.assertRaises(views.FreshnessHold):views.validate_stdio([reader,left.fileno(),writer])
        finally:os.close(reader);os.close(writer);left.close();right.close()

    @unittest.skipIf(os.geteuid()==0,'requires a normal non-root caller')
    def test_no_host_caller_can_enter_a_privileged_namespace(self):
        with self.assertRaises(views.FreshnessHold):views.enter('/candidate/root',2000,'/candidate/source','/candidate/artifacts')


if __name__=='__main__':unittest.main()
