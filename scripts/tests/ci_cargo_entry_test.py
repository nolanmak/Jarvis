"""Real original-supervisor exec stops; no mocked process or credential evidence."""
import json
import fcntl
import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('entry_staging_tests',ROOT/'scripts/ci-cargo-staging.py')
staging=importlib.util.module_from_spec(spec);spec.loader.exec_module(staging)


class SealedEntryDescriptorTests(unittest.TestCase):
    def test_actual_sealed_inode_bytes_without_offset_flags_or_timestamp_changes(self):
        fd=os.memfd_create('entry-content',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            data=b'sealed stdin\0\xff';os.write(fd,data);os.lseek(fd,5,os.SEEK_SET)
            os.utime(fd,ns=(123456789,987654321));staging._seal(fd)
            before=os.fstat(fd);flags=fcntl.fcntl(fd,fcntl.F_GETFL)
            result=staging.observe_sealed_descriptor(os.getpid(),fd)
            after=os.fstat(fd)
            self.assertEqual(result['sha256'],hashlib.sha256(data).hexdigest())
            self.assertEqual(result['size_bytes'],len(data))
            self.assertEqual((result['device'],result['inode']),(before.st_dev,before.st_ino))
            self.assertEqual(os.lseek(fd,0,os.SEEK_CUR),5)
            self.assertEqual(fcntl.fcntl(fd,fcntl.F_GETFL),flags)
            self.assertEqual((before.st_atime_ns,before.st_mtime_ns),(after.st_atime_ns,after.st_mtime_ns))
        finally:os.close(fd)

    def test_unsealed_descriptor_is_refused_before_any_content_read(self):
        fd=os.memfd_create('mutable-entry-content',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            os.write(fd,b'mutable');os.utime(fd,ns=(123456789,987654321));before=os.fstat(fd)
            with self.assertRaises(staging.StagingUnavailable):staging.observe_sealed_descriptor(os.getpid(),fd)
            self.assertEqual(os.lseek(fd,0,os.SEEK_CUR),7)
            self.assertEqual(os.fstat(fd).st_atime_ns,before.st_atime_ns)
        finally:os.close(fd)

    def test_oversize_sealed_descriptor_and_invalid_numbers_are_refused(self):
        fd=os.memfd_create('large-entry-content',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            os.ftruncate(fd,staging.MAX_BYTES+1);staging._seal(fd)
            for number in (fd,-1,65536,True):
                with self.subTest(number=number),self.assertRaises(staging.StagingUnavailable):
                    staging.observe_sealed_descriptor(os.getpid(),number)
        finally:os.close(fd)


@unittest.skipUnless(sys.platform=='linux' and os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),
                     'requires managed Linux unit policy helper')
class EntryTests(unittest.TestCase):
    def run_case(self,mode):
        with tempfile.TemporaryDirectory() as directory:
            result=subprocess.run([sys.executable,'-I',str(Path(__file__).resolve()),'--probe',
                mode,directory,os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']],
                capture_output=True,text=True,timeout=15)
            self.assertEqual(result.returncode,0,result.stderr)
            record=json.loads(result.stdout)
            self.assertIsNone(record['callback_error'])
            self.assertTrue(record['cleanup'])
            self.assertEqual(record['body_started'],mode=='normal')
            self.assertEqual(record['held'],mode!='normal')
            if mode=='normal':
                entry=record['entry']
                self.assertEqual(entry['descriptors'],[0,1,2])
                self.assertEqual(entry['uids'],[os.getuid()]*4)
                self.assertEqual(entry['gids'],[os.getgid()]*4)
                self.assertEqual(entry['groups'],sorted(os.getgroups()))
                self.assertEqual(entry['no_new_privs'],1)
                self.assertEqual(entry['seccomp'],2)
                self.assertFalse(entry['shipping_authority'])
                self.assertFalse(entry['input_closure_complete'])

    def test_actual_exec_stop_observed_before_first_body_instruction(self):self.run_case('normal')
    def test_extra_inherited_descriptor_prevents_body_execution(self):self.run_case('extra')
    def test_running_controller_cannot_be_claimed_as_original_exec_stop(self):self.run_case('running')


def probe(mode,directory,library):
    import importlib.util
    def load(name,path):
        spec=importlib.util.spec_from_file_location(name,path)
        module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module
    staging=load('entry_staging',ROOT/'scripts/ci-cargo-staging.py')
    supervisor=load('entry_supervisor',ROOT/'scripts/provider-supervisor.py')
    root=Path(directory);marker=root/'body';receipt=root/'cleanup';entry=None;held=False;callback_error=None
    extra=os.open(os.devnull,os.O_RDONLY|os.O_CLOEXEC)
    bootstrap='''import ctypes,os,signal,sys
lib=ctypes.CDLL(sys.argv[1],use_errno=True)
assert lib.jarvis_ci_restrict_unit()==0
libc=ctypes.CDLL(None,use_errno=True)
assert libc.ptrace(0,0,None,None)==0
os.kill(os.getpid(),signal.SIGSTOP)
os.execve(sys.executable,[sys.executable,'-I','-c',"from pathlib import Path;Path(__import__('sys').argv[1]).write_text('started')",sys.argv[2]],{})
'''
    def executed(child):
        nonlocal entry,held,callback_error
        try:
            assert not marker.exists()
            entry=staging.observe_exec_entry(os.getpid() if mode=='running' else child.pid)
            staging.require_exec_entry(entry,uid=os.getuid(),gid=os.getgid(),groups=sorted(os.getgroups()),
                                       descriptors=[0,1,2])
        except staging.StagingUnavailable:
            held=True;raise
        except BaseException as error:
            callback_error=type(error).__name__;raise
    try:
        try:
            supervisor.supervise(receipt,[sys.executable,'-I','-c',bootstrap,library,str(marker)],
                                 forward_fds=(extra,) if mode=='extra' else (),on_exec=executed)
        except staging.StagingUnavailable:held=True
    finally:os.close(extra)
    print(json.dumps({'entry':entry,'held':held,'callback_error':callback_error,'body_started':marker.exists(),
                      'cleanup':receipt.read_text()=='all-descendants-reaped\n'}))


if __name__=='__main__':
    if len(sys.argv)>1 and sys.argv[1]=='--probe':probe(*sys.argv[2:])
    else:unittest.main()
