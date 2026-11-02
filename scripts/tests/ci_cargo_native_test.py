"""Exercise the actual native exec-denial primitive in supervised children."""
import os
from pathlib import Path
import platform
import signal
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]
PROGRAM=ROOT/'crates/augmentagent-cli/src/ci_cargo_exec_cases.py'


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native helper build')
class NativeExecTests(unittest.TestCase):
    def run_mode(self,mode):
        with tempfile.TemporaryDirectory() as directory:
            receipt=Path(directory)/'quiescent'
            library=str(Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve())
            command=[sys.executable,'-I',str(ROOT/'scripts/provider-supervisor.py'),str(receipt),
                     sys.executable,'-I',str(PROGRAM),library,mode]
            result=subprocess.run(command,capture_output=True,text=True,timeout=15,
                env={key:value for key,value in os.environ.items() if key in ('HOME','PATH','LANG','LC_ALL')})
            self.assertEqual(receipt.read_text(),'all-descendants-reaped\n')
            return result

    def test_direct_exec_is_denied(self):
        result=self.run_mode('execve')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(result.stdout,'DENIED\n')

    def test_fd_relative_exec_is_denied(self):
        result=self.run_mode('execveat')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(result.stdout,'DENIED\n')

    def test_fork_cannot_escape_the_filter(self):
        result=self.run_mode('fork')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertIn('DENIED\n',result.stdout)
        self.assertIn('REAPED\n',result.stdout)

    def test_x32_cannot_alias_native_syscall_numbers(self):
        result=self.run_mode('x32')
        self.assertEqual(result.returncode,128+signal.SIGSYS,result.stderr)


if __name__=='__main__':unittest.main()
