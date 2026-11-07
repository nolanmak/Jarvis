"""Private diagnostics preserve original bytes and never grant CI authority."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import time
import unittest

spec=importlib.util.spec_from_file_location('repository_result_planner',Path(__file__).resolve().parents[1]/'ci-cargo-planner.py')
planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)

class RepositoryResultTests(unittest.TestCase):
    def test_real_failed_command_retains_binary_streams_without_changing_result(self):
        child=subprocess.Popen([sys.executable,'-c',"import os;os.write(1,b'original-out\\x00');os.write(2,b'linker failed\\xff\\n');raise SystemExit(17)"],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        output,error=planner.capture_command(child,time.monotonic()+5)
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'result.json'
            value=planner.retain_repository_result(path,{'head':'original'},'original-command',child.returncode,output,error)
            saved=json.loads(path.read_bytes())
            self.assertEqual(saved,value)
            self.assertEqual(saved['returncode'],17)
            for name,raw in [('stdout',output),('stderr',error)]:
                self.assertEqual(bytes.fromhex(saved[name]['prefix_hex']+saved[name]['suffix_hex']),raw)
                self.assertEqual(saved[name]['sha256'],hashlib.sha256(raw).hexdigest())
                self.assertEqual(saved[name]['size_bytes'],len(raw));self.assertFalse(saved[name]['truncated'])
            self.assertEqual((child.returncode,output,error),(17,b'original-out\x00',b'linker failed\xff\n'))
            self.assertEqual(stat.S_IMODE(path.stat().st_mode),0o600)
            self.assertFalse(saved['input_closure_complete']);self.assertFalse(saved['full_ci']);self.assertFalse(saved['shipping_authority'])

    def test_large_output_keeps_bounded_edges_and_full_digest(self):
        data=b'begin\x00'+b'x'*70000+b'\xffend'
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'result.json'
            value=planner.retain_repository_result(path,{},'command',1,b'',data)
            self.assertTrue(value['stderr']['truncated'])
            self.assertEqual(bytes.fromhex(value['stderr']['prefix_hex']),data[:32768])
            self.assertEqual(bytes.fromhex(value['stderr']['suffix_hex']),data[-32768:])
            self.assertEqual(value['stderr']['sha256'],hashlib.sha256(data).hexdigest())
            self.assertLess(path.stat().st_size,512*1024)

    def test_replay_does_not_replace_original_failure_or_follow_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);path=root/'result.json'
            original=planner.retain_repository_result(path,{},'command',1,b'',b'original')
            before=path.read_bytes()
            self.assertEqual(planner.retain_repository_result(path,{},'command',1,b'',b'original'),original)
            with self.assertRaises(planner.FreshnessHold):planner.retain_repository_result(path,{},'command',0,b'',b'changed')
            self.assertEqual(path.read_bytes(),before)
            alias=root/'alias';alias.symlink_to(path)
            with self.assertRaises(planner.FreshnessHold):planner.retain_repository_result(alias,{},'command',1,b'',b'original')
            self.assertEqual(path.read_bytes(),before)

    def test_oversized_or_nonbinary_output_and_invalid_status_never_create_record(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'result.json'
            for status,output,error in [(True,b'',b''),(256,b'',b''),(0,'decoded',b''),(0,b'',b'x'*(8*1024**2+1))]:
                with self.subTest(status=status),self.assertRaises(planner.FreshnessHold):
                    planner.retain_repository_result(path,{},'command',status,output,error)
                self.assertFalse(path.exists())

    def test_untrusted_json_remains_bytes_and_cannot_change_authority(self):
        with tempfile.TemporaryDirectory() as directory:
            value=planner.retain_repository_result(Path(directory)/'result.json',{},'command',1,b'',b'{"shipping_authority":true,"full_ci":true}')
            self.assertFalse(value['shipping_authority']);self.assertFalse(value['full_ci'])
            self.assertEqual(bytes.fromhex(value['stderr']['prefix_hex']),b'{"shipping_authority":true,"full_ci":true}')

class ContainedPipeDiagnosticTests(unittest.TestCase):
    def test_reaped_child_snapshot_does_not_consume_pipe_bytes_flags_or_fds(self):
        import fcntl
        reader,writer=os.pipe2(os.O_CLOEXEC)
        try:
            child=subprocess.Popen([sys.executable,'-c',"import os;os.write(2,b'actual nested failure\\xff');raise SystemExit(17)"],stderr=writer,stdout=subprocess.DEVNULL)
            self.assertEqual(child.wait(timeout=5),17)
            flags=fcntl.fcntl(writer,fcntl.F_GETFL);descriptors=set(os.listdir('/proc/self/fd'))
            value=planner.nonconsuming_pipe_diagnostic(writer)
            self.assertEqual(value['status'],'sampled');self.assertFalse(value['complete_stream'])
            self.assertEqual(bytes.fromhex(value['sample_hex']),b'actual nested failure\xff')
            self.assertEqual(value['sample_sha256'],hashlib.sha256(b'actual nested failure\xff').hexdigest())
            self.assertEqual(fcntl.fcntl(writer,fcntl.F_GETFL),flags)
            self.assertEqual(set(os.listdir('/proc/self/fd')),descriptors)
            self.assertEqual(os.read(reader,4096),b'actual nested failure\xff')
            os.close(writer);writer=None
            self.assertEqual(os.read(reader,4096),b'')
        finally:
            os.close(reader)
            if writer is not None:os.close(writer)

    def test_empty_pipe_does_not_claim_a_complete_or_unreleased_stream(self):
        reader,writer=os.pipe2(os.O_CLOEXEC)
        try:
            value=planner.nonconsuming_pipe_diagnostic(writer)
            self.assertEqual(value['status'],'empty');self.assertFalse(value['complete_stream'])
            os.write(writer,b'later');self.assertEqual(os.read(reader,5),b'later')
        finally:os.close(reader);os.close(writer)

    def test_regular_file_is_not_read_or_repositioned(self):
        with tempfile.TemporaryFile() as stream:
            stream.write(b'private file');stream.flush();stream.seek(3)
            value=planner.nonconsuming_pipe_diagnostic(stream.fileno())
            self.assertEqual(value['status'],'not_a_pipe');self.assertFalse(value['complete_stream'])
            self.assertEqual(stream.tell(),3)

if __name__=='__main__':unittest.main()
