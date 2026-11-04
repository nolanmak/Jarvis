"""Real original-supervisor exec stops; no mocked process or credential evidence."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]


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
