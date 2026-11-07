"""Sealed executable bytes are physical capture, never execution authority."""
import copy
import errno
import fcntl
import hashlib
import importlib.util
import mmap
import os
from pathlib import Path
import pickle
import platform
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('executable_staging',ROOT/'scripts/ci-cargo-staging.py')
staging=importlib.util.module_from_spec(spec);spec.loader.exec_module(staging)
EXEC_SEALS=fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL|0x20


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native helper build')
class ExecutableCaptureTests(unittest.TestCase):
    def setUp(self):
        library=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        self.body=(library.parent/'ci_cargo_status_proxy').read_bytes()
        self.directory=self.enterContext(tempfile.TemporaryDirectory())
        self.path=Path(self.directory)/'original-image';self.path.write_bytes(self.body);self.path.chmod(0o700)
        self.source=os.open(self.path,os.O_RDONLY|os.O_CLOEXEC);self.addCleanup(os.close,self.source)

    def capture(self):return staging.capture_executable(self.source)

    def test_exact_copy_is_sealed_without_changing_source_offset_or_flags(self):
        os.lseek(self.source,17,os.SEEK_SET);flags=fcntl.fcntl(self.source,fcntl.F_GETFL)
        descriptors=set(os.listdir('/proc/self/fd'))
        with self.capture() as image:
            fd=image.fileno();info=os.fstat(fd);observed=image.observation()
            self.assertEqual(os.pread(fd,len(self.body)+1,0),self.body)
            self.assertEqual(stat.S_IMODE(info.st_mode),0o555);self.assertEqual(info.st_nlink,0)
            self.assertEqual(fcntl.fcntl(fd,fcntl.F_GET_SEALS)&EXEC_SEALS,EXEC_SEALS)
            self.assertTrue(fcntl.fcntl(fd,fcntl.F_GETFD)&fcntl.FD_CLOEXEC)
            self.assertEqual(observed['sha256'],hashlib.sha256(self.body).hexdigest())
            self.assertEqual(observed['size_bytes'],len(self.body))
            for field in ('source_authority','execution_authority','input_closure_complete','full_ci','shipping_authority'):
                self.assertIs(observed[field],False,field)
        self.assertEqual(os.lseek(self.source,0,os.SEEK_CUR),17)
        self.assertEqual(fcntl.fcntl(self.source,fcntl.F_GETFL),flags)
        self.assertEqual(set(os.listdir('/proc/self/fd')),descriptors)

    def test_dup_alias_cannot_write_resize_map_or_change_execute_bits(self):
        with self.capture() as image:
            alias=os.dup(image.fileno())
            try:
                for operation in (lambda:os.pwrite(alias,b'changed',0),
                        lambda:os.ftruncate(alias,0),lambda:os.ftruncate(alias,len(self.body)+1),
                        lambda:os.fchmod(alias,0o400),lambda:os.fchmod(alias,0o510),
                        lambda:mmap.mmap(alias,len(self.body),flags=mmap.MAP_SHARED,prot=mmap.PROT_READ|mmap.PROT_WRITE)):
                    with self.assertRaises(OSError) as error:operation()
                    self.assertEqual(error.exception.errno,errno.EPERM)
                self.assertEqual(os.pread(alias,len(self.body),0),self.body)
            finally:os.close(alias)

    def test_captured_descriptor_executes_original_image_after_source_replaced(self):
        with self.capture() as image:
            self.path.unlink();self.path.write_bytes(b'#!/bin/sh\nexit 77\n');self.path.chmod(0o700)
            result=subprocess.run([sys.executable,'-I','-c',
                'import os,sys;os.execve(int(sys.argv[1]),["status-proxy","invalid"],{})',str(image.fileno())],
                pass_fds=(image.fileno(),),capture_output=True,timeout=5,
                env={k:v for k,v in os.environ.items() if k in ('HOME','PATH','LANG','LC_ALL')})
            # Actual static proxy body refuses its deliberately invalid config.
            self.assertEqual((result.returncode,result.stdout,result.stderr),(125,b'',b''))

    def test_nonregular_oversize_dynamic_and_invalid_images_are_refused(self):
        reader,writer=os.pipe2(os.O_CLOEXEC)
        try:
            with self.assertRaises(staging.StagingUnavailable):staging.capture_executable(reader)
        finally:os.close(reader);os.close(writer)
        for body in (b'',b'not ELF',Path('/bin/true').read_bytes()):
            self.path.write_bytes(body)
            with self.assertRaises(staging.StagingUnavailable):self.capture()
        with self.path.open('wb') as stream:stream.truncate(2*1024**2+1)
        with self.assertRaises(staging.StagingUnavailable):self.capture()
        for descriptor in (-1,True,65536):
            with self.assertRaises(staging.StagingUnavailable):staging.capture_executable(descriptor)

    def test_existing_writable_mapping_prevents_successful_capture_and_closes_temporary(self):
        before=set(os.listdir('/proc/self/fd'));original=os.memfd_create;maps=[];created=[]
        def exposed(*args,**kwargs):
            fd=original(*args,**kwargs);created.append(fd);os.ftruncate(fd,len(self.body))
            maps.append(mmap.mmap(fd,len(self.body),flags=mmap.MAP_SHARED,prot=mmap.PROT_READ|mmap.PROT_WRITE))
            return fd
        try:
            with patch.object(os,'memfd_create',side_effect=exposed),self.assertRaises(staging.StagingUnavailable):self.capture()
            self.assertEqual(len(created),1)
            with self.assertRaises(OSError):os.fstat(created[0])
            maps[0][0:1]=b'X' # Real retained mapping remains writable; no sealed result escaped.
        finally:
            for mapping in maps:mapping.close()
        self.assertEqual(set(os.listdir('/proc/self/fd')),before)

    def test_observed_source_drift_refuses_and_preserves_borrowed_descriptor(self):
        original=os.pread;changed=[];before=set(os.listdir('/proc/self/fd'))
        with self.path.open('r+b') as writer:
            def drifting(fd,size,offset):
                data=original(fd,size,offset)
                if fd==self.source and not changed:
                    changed.append(True);self.assertEqual(os.pwrite(writer.fileno(),b'X',0),1)
                    self.assertEqual(original(self.source,1,0),b'X')
                return data
            with patch.object(os,'pread',side_effect=drifting),self.assertRaises(staging.StagingUnavailable):self.capture()
        self.assertTrue(changed);self.assertEqual(set(os.listdir('/proc/self/fd')),before)
        self.assertEqual(os.pread(self.source,1,0),b'X')

    def test_preseal_descriptor_alias_tamper_cannot_return_mislabeled_bytes(self):
        original=fcntl.fcntl;tampered=[];unexpected=None;before=set(os.listdir('/proc/self/fd'))
        def interfere(fd,command,*arguments):
            if command==fcntl.F_ADD_SEALS:
                alias=os.dup(fd)
                try:
                    value=bytes([self.body[-1]^1])
                    self.assertEqual(os.pwrite(alias,value,len(self.body)-1),1)
                    self.assertEqual(os.pread(fd,1,len(self.body)-1),value)
                    tampered.append(True)
                finally:os.close(alias)
            return original(fd,command,*arguments)
        try:
            with patch.object(fcntl,'fcntl',side_effect=interfere),self.assertRaises(staging.StagingUnavailable):
                unexpected=self.capture()
        finally:
            if unexpected is not None:unexpected.close()
        self.assertTrue(tampered);self.assertEqual(set(os.listdir('/proc/self/fd')),before)

    def test_capture_cannot_be_copied_pickled_or_used_after_close(self):
        image=self.capture()
        try:
            for operation in (lambda:copy.copy(image),lambda:copy.deepcopy(image),lambda:pickle.dumps(image)):
                with self.assertRaises(staging.StagingUnavailable):operation()
        finally:image.close()
        with self.assertRaises(staging.StagingUnavailable):image.fileno()

    def test_forked_process_cannot_use_the_original_local_capture_object(self):
        with self.capture() as image:
            child=os.fork()
            if child==0:
                try:image.fileno()
                except staging.StagingUnavailable:os._exit(0)
                except BaseException:os._exit(2)
                os._exit(1)
            _,status=os.waitpid(child,0)
            self.assertEqual(status,0)
            self.assertEqual(os.pread(image.fileno(),len(self.body),0),self.body)

    def test_invalid_mode_refuses_use_but_cleanup_still_closes_its_owned_inode(self):
        image=self.capture();descriptor=image.fileno()
        try:
            os.fchmod(descriptor,0o755) # Execute bits are unchanged; content stays sealed.
            with self.assertRaises(staging.StagingUnavailable):image.fileno()
            image.close()
            with self.assertRaises(OSError):os.fstat(descriptor)
        finally:
            try:os.close(descriptor)
            except OSError as error:self.assertEqual(error.errno,errno.EBADF)

    def test_replaced_descriptor_is_refused_without_closing_the_foreign_pipe(self):
        image=self.capture();descriptor=image.fileno();original=os.dup(descriptor)
        reader,writer=os.pipe2(os.O_CLOEXEC)
        try:
            os.dup2(writer,descriptor)
            with self.assertRaises(staging.StagingUnavailable):image.fileno()
            with self.assertRaises(staging.StagingUnavailable):image.close()
            os.write(descriptor,b'+');self.assertEqual(os.read(reader,1),b'+')
        finally:
            os.dup2(original,descriptor);image.close()
            os.close(original);os.close(reader);os.close(writer)

    def test_unsupported_kernel_does_not_fall_back_to_unsealed_image(self):
        before=set(os.listdir('/proc/self/fd'))
        with patch.object(os,'memfd_create',side_effect=OSError(errno.EINVAL,'unsupported')):
            with self.assertRaises(staging.StagingUnavailable):self.capture()
        self.assertEqual(set(os.listdir('/proc/self/fd')),before)


if __name__=='__main__':unittest.main()
