"""Black-box Deno header policy, independent of registry access or Rust build."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import unittest

RUNNER = Path(__file__).resolve().parents[2] / 'sidecars/code-mode-runner/runner.ts'
DENO = os.environ.get('AUGMENTAGENT_DENO_BIN') or shutil.which('deno')


@unittest.skipUnless(DENO, 'Deno runtime is required')
class RunnerPolicyTests(unittest.TestCase):
    def run_program(self, **header):
        result = subprocess.run([DENO, 'run', '--no-config', '--no-npm', '--no-remote',
                                 '--deny-import', '--no-prompt', str(RUNNER)],
                                input=json.dumps({'manifest': [], **header}) + '\n',
                                capture_output=True, text=True, timeout=3)
        return [json.loads(line) for line in result.stdout.splitlines() if line]

    def test_host_header_selects_short_deadline(self):
        frames = self.run_program(timeoutMs=25, program='async function main() { await new Promise(r => setTimeout(r, 250)); return "too late"; } main();')
        self.assertEqual(frames[-1].get('error', {}).get('kind'), 'timeout', frames)

    def test_invalid_budget_rejected_before_user_code(self):
        for value in (0, -1, True, '20', 3600001, 1.5, None):
            with self.subTest(value=value):
                frames = self.run_program(timeoutMs=value, program='async function main() { return "executed"; } main();')
                self.assertIn('error', frames[-1])
                self.assertIn('timeoutMs', frames[-1]['error']['message'])

    def test_input_bindings_are_frozen_and_nonreplaceable(self):
        frames = self.run_program(computeInputs={'sheet': 'opaque-id'}, program='''
async function main() {
  const value = computeInputs.sheet;
  try { computeInputs.sheet = 'wrong'; } catch (_) {}
  try { globalThis.computeInputs = {}; } catch (_) {}
  return [value, computeInputs.sheet, Object.isFrozen(computeInputs)];
} main();''')
        self.assertEqual(frames[-1].get('final'), ['opaque-id', 'opaque-id', True], frames)

    def test_invalid_input_bindings_rejected(self):
        for value in ([], None, {'sheet': 1}, {'sheet': ''}):
            with self.subTest(value=value):
                frames = self.run_program(computeInputs=value, program='async function main() { return 1; } main();')
                self.assertIn('error', frames[-1])

    def test_default_header_still_executes_typescript(self):
        frames = self.run_program(program='async function main(): Promise<number> { return 42; } main();')
        self.assertEqual(frames[-1], {'final': 42, 'localRefusal': False})


if __name__ == '__main__':
    unittest.main()
