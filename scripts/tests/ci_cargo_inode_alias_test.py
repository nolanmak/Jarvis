"""Actual task FD/mapping observations; no writer, pause or reuse authority."""
import json
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
spec=importlib.util.spec_from_file_location('task_alias_staging',Path(__file__).resolve().parents[1]/'ci-cargo-staging.py')
staging=importlib.util.module_from_spec(spec);spec.loader.exec_module(staging)


PROGRAM='''import ctypes,fcntl,json,mmap,os,sys,threading
fd=int(sys.argv[1]);mode=sys.argv[2]
def run():
 global mapped
 if mode in ('shared','private'):
  # CPython mmap duplicates its FD; use the kernel mapping directly so this
  # case really retains a mapping after every file description is closed.
  libc=ctypes.CDLL(None,use_errno=True);libc.mmap.restype=ctypes.c_void_p
  libc.mmap.argtypes=[ctypes.c_void_p,ctypes.c_size_t,ctypes.c_int,ctypes.c_int,ctypes.c_int,ctypes.c_long]
  mapped=libc.mmap(None,10,mmap.PROT_READ|mmap.PROT_WRITE,mmap.MAP_SHARED if mode=='shared' else mmap.MAP_PRIVATE,fd,0)
  assert mapped not in (None,ctypes.c_void_p(-1).value),ctypes.get_errno()
  os.close(fd)
 elif mode=='cwd':os.fchdir(fd);os.close(fd)
 else:
  os.lseek(fd,3,os.SEEK_SET)
  if mode=='flock':fcntl.flock(fd,fcntl.LOCK_EX)
  if mode=='posix':
   fcntl.lockf(fd,fcntl.LOCK_EX|fcntl.LOCK_NB,2,0,os.SEEK_SET)
   fcntl.lockf(fd,fcntl.LOCK_EX|fcntl.LOCK_NB,2,6,os.SEEK_SET)
  if mode=='ofd':
   class Lock(ctypes.Structure):
    _fields_=[('kind',ctypes.c_short),('whence',ctypes.c_short),('start',ctypes.c_longlong),('length',ctypes.c_longlong),('pid',ctypes.c_int)]
   fcntl.fcntl(fd,fcntl.F_OFD_SETLK,bytes(Lock(fcntl.F_WRLCK,os.SEEK_SET,2,4,0)))
 tid=threading.get_native_id();pid=os.getpid()
 birth=int(Path('/proc/self/task/'+str(tid)+'/stat').read_text().rsplit(')',1)[1].split()[19])
 print(json.dumps({'pid':pid,'tid':tid,'birth':birth}),flush=True)
 sys.stdin.buffer.read(1)
from pathlib import Path
if mode=='thread':
 t=threading.Thread(target=run);t.start();t.join()
else:run()
'''


class TaskInodeAliasTests(unittest.TestCase):
    def child(self,mode):
        root=Path(self.enterContext(tempfile.TemporaryDirectory()))
        path=root/'data';path.write_bytes(b'abcdefghij')
        if mode=='cwd':path=root
        flags=os.O_RDONLY if mode in ('readonly','cwd') else os.O_RDWR
        fd=os.open(path,flags|os.O_CLOEXEC);info=os.fstat(fd)
        child=subprocess.Popen([sys.executable,'-I','-c',PROGRAM,str(fd),mode],pass_fds=(fd,),
            stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        os.close(fd)
        def finish():
            if child.poll() is None:child.stdin.write('x');child.stdin.flush()
            out,err=child.communicate(timeout=3)
            self.assertEqual(child.returncode,0,err)
        self.addCleanup(finish)
        task=json.loads(child.stdout.readline())
        return task,{(info.st_dev,info.st_ino)},path

    def observe(self,task,inodes):
        return staging.observe_task_inode_aliases(task,inodes,check=lambda:None)

    def test_writable_fd_is_reported_without_touching_bytes_or_offset(self):
        task,keys,path=self.child('writable');before=path.stat()
        value=self.observe(task,keys);fds=[row for row in value['aliases'] if row['kind']=='fd']
        self.assertEqual(len(fds),1);self.assertEqual(fds[0]['flags']&os.O_ACCMODE,os.O_RDWR)
        self.assertEqual(fds[0]['position'],3);self.assertEqual(path.read_bytes(),b'abcdefghij')
        self.assertEqual((path.stat().st_mtime_ns,path.stat().st_ctime_ns),(before.st_mtime_ns,before.st_ctime_ns))
        self.assertFalse(value['alias_closure_complete']);self.assertFalse(value['shipping_authority'])
        self.assertNotIn('frozen',value);self.assertNotIn('original_cleanup',value)

    def test_readonly_fd_still_reports_an_inode_capability(self):
        task,keys,_=self.child('readonly')
        rows=self.observe(task,keys)['aliases'];self.assertEqual(len(rows),1)
        self.assertEqual(rows[0]['flags']&os.O_ACCMODE,os.O_RDONLY)

    def test_held_kernel_lock_is_reported_without_releasing_or_reacquiring_it(self):
        import fcntl
        task,keys,path=self.child('flock');rows=self.observe(task,keys)['aliases']
        self.assertEqual(len(rows),1);self.assertTrue(rows[0]['locks'])
        self.assertEqual(rows[0]['locks'][0]['kind'],'FLOCK')
        fd=os.open(path,os.O_RDWR|os.O_CLOEXEC)
        try:
            with self.assertRaises(BlockingIOError):fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
        finally:os.close(fd)

    def test_shared_writable_mapping_survives_descriptor_close(self):
        task,keys,_=self.child('shared');rows=self.observe(task,keys)['aliases']
        self.assertEqual(len(rows),1);self.assertEqual(rows[0]['kind'],'mapping')
        self.assertTrue(rows[0]['shared']);self.assertTrue(rows[0]['writable'])

    def test_multiple_posix_ranges_are_not_collapsed_into_one_fdinfo_field(self):
        task,keys,_=self.child('posix');locks=self.observe(task,keys)['aliases'][0]['locks']
        self.assertEqual(len(locks),2)
        self.assertEqual({(r['start'],r['end']) for r in locks},{(0,1),(6,7)})
        self.assertTrue(all(r['kind']=='POSIX' and r['owner']==task['pid'] for r in locks))

    def test_open_file_description_lock_has_no_process_owner(self):
        task,keys,_=self.child('ofd');locks=self.observe(task,keys)['aliases'][0]['locks']
        self.assertEqual(len(locks),1);self.assertEqual(locks[0]['kind'],'OFDLCK')
        self.assertEqual((locks[0]['owner'],locks[0]['start'],locks[0]['end']),(-1,2,5))

    def test_private_mapping_is_distinguished_after_descriptor_close(self):
        task,keys,_=self.child('private');rows=self.observe(task,keys)['aliases']
        self.assertEqual(len(rows),1);self.assertEqual(rows[0]['kind'],'mapping')
        self.assertFalse(rows[0]['shared']);self.assertTrue(rows[0]['writable'])

    def test_directory_cwd_capability_survives_descriptor_close(self):
        task,keys,_=self.child('cwd');rows=self.observe(task,keys)['aliases']
        self.assertEqual([row['kind'] for row in rows],['cwd'])

    def test_nonleader_task_is_observed_by_original_tid_and_birth(self):
        task,keys,_=self.child('thread');self.assertNotEqual(task['pid'],task['tid'])
        self.assertEqual(self.observe(task,keys)['task'],task)
        self.assertTrue(self.observe(task,keys)['aliases'])

    def test_wrong_original_birth_is_refused(self):
        task,keys,_=self.child('writable');task['birth']+=1
        with self.assertRaises(staging.StagingUnavailable):self.observe(task,keys)

    def test_unrelated_inode_does_not_create_a_reported_alias(self):
        task,keys,_=self.child('shared');device,inode=next(iter(keys))
        self.assertEqual(self.observe(task,{(device,inode+1)})['aliases'],[])

    def test_original_cancellation_is_preserved(self):
        task,keys,_=self.child('shared')
        def cancelled():raise staging.StagingUnavailable('original_cancelled')
        with self.assertRaises(staging.StagingUnavailable) as caught:
            staging.observe_task_inode_aliases(task,keys,check=cancelled)
        self.assertEqual(caught.exception.stage_reason,'original_cancelled')


if __name__=='__main__':unittest.main()
