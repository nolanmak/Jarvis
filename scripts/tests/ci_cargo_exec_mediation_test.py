"""First-exec kernel feasibility against actual constructor/static/raw execs."""
import json
import os
from pathlib import Path
import platform
import subprocess
import struct
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


@unittest.skipUnless(sys.platform == 'linux' and platform.machine() == 'x86_64', 'requires Linux x86-64')
@unittest.skipUnless(os.environ.get('AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY'), 'requires managed native helper build')
class ExecMediationTests(unittest.TestCase):
    def run_case(self, mode, expected):
        library = Path(os.environ['AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY']).resolve()
        early = Path(os.environ.get('AUGMENTAGENT_CARGO_EXEC_EARLY_LIBRARY', library.parent / 'libci_cargo_exec_early.so'))
        executable = Path(os.environ.get('AUGMENTAGENT_CARGO_EXEC_STATIC_BINARY', library.parent / 'ci_cargo_exec_static'))
        if mode == 'static':
            data = executable.read_bytes()
            self.assertEqual(data[:6], b'\x7fELF\x02\x01')
            offset, = struct.unpack_from('<Q', data, 32)
            size, count = struct.unpack_from('<HH', data, 54)
            self.assertTrue(size >= 56 and 0 < count < 4096 and offset + size * count <= len(data))
            self.assertNotIn(3, [struct.unpack_from('<I', data, offset + size * i)[0] for i in range(count)])
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / 'cleanup'
            result = subprocess.run([sys.executable, '-I', str(ROOT / 'crates/augmentagent-cli/src/ci_cargo_exec_mediation_cases.py'),
                str(ROOT / 'scripts/provider-supervisor.py'), str(library), str(early), str(executable), mode, str(receipt)],
                capture_output=True, text=True, timeout=15,
                env={key: value for key, value in os.environ.items() if key in ('HOME', 'PATH', 'LANG', 'LC_ALL')})
            self.assertEqual(result.returncode, 0, result.stderr)
            lines = result.stdout.splitlines()
            self.assertEqual(lines[:-1], expected)
            record = json.loads(lines[-1])
            self.assertTrue(record['original_cleanup'])
            self.assertFalse(record['shipping_authority'])
            self.assertEqual(receipt.read_text(), 'all-descendants-reaped\n')

    def test_early_constructor_and_program_body_execs_are_denied(self):
        self.run_case('early', ['EARLY_DENIED', 'BODY_DENIED'])

    def test_static_program_execs_are_denied_without_a_preload(self):
        self.run_case('static', ['STATIC_DENIED'])

    def test_failed_initial_path_candidate_preserves_authorized_search(self):
        self.run_case('search', ['BODY_DENIED'])

    def test_listener_loss_fails_closed_after_original_exec_ack(self):
        self.run_case('listener-loss', ['BODY_DENIED'])

    def test_initial_cancellation_never_acknowledges_exec(self):
        self.run_case('cancel', [])

    def test_forked_descendant_inherits_exec_mediation(self):
        self.run_case('fork', ['BODY_DENIED'])


if __name__ == '__main__':
    unittest.main()
