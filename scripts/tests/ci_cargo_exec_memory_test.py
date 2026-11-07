"""Actual pre/post-exec memory-description lifetime, not writer closure."""
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]


@unittest.skipUnless(sys.platform=='linux' and platform.machine()=='x86_64','requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'),'requires managed native helper build')
class ExecMemoryLifetimeTests(unittest.TestCase):
    def test_actual_vfork_memory_fd_keeps_original_mm_after_native_exec(self):
        library=Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        with tempfile.TemporaryDirectory() as directory:
            receipt=Path(directory)/'cleanup'
            result=subprocess.run([sys.executable,'-I',str(ROOT/'crates/augmentagent-cli/src/ci_cargo_exec_memory_cases.py'),
                str(ROOT/'scripts/ci-cargo-planner.py'),str(ROOT/'scripts/provider-supervisor.py'),str(library),
                str(library.parent/'ci_cargo_vfork_producer'),str(library.parent/'ci_cargo_vfork_proxy'),
                str(library.parent/'ci_cargo_exec_static'),str(receipt)],capture_output=True,text=True,timeout=15,
                env={k:v for k,v in os.environ.items() if k in ('HOME','PATH','LANG','LC_ALL')})
            self.assertEqual(result.returncode,0,result.stderr)
            lines=result.stdout.splitlines();self.assertEqual(lines[:-1],['PROXY_OUTPUT'])
            self.assertEqual(result.stderr,'PROXY_ERROR\n')
            row=json.loads(lines[-1])
            for field in ('pre_exec_fd_mutated_shared_parent','post_exec_old_fd_mutated_original_parent',
                    'old_parent_value_restored','original_cleanup','native_exec_stop',
                    'parent_released_before_proxy_termination'):self.assertIs(row[field],True,field)
            self.assertEqual(row['post_exec_reopened_child_fd_old_address_errno'],5)
            self.assertFalse(row['complete_writer_closure']);self.assertFalse(row['full_ci_pass'])
            self.assertFalse(row['shipping_authority'])
            self.assertEqual(receipt.read_bytes(),b'all-descendants-reaped\n')


if __name__=='__main__':unittest.main()
