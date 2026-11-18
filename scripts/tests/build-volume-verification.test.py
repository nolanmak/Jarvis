#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('verify', Path(__file__).resolve().parents[1] / 'verify-build-volume.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class Verification(unittest.TestCase):
    def status(self):
        return [{'dev': 2, 'labels': [f'{role} (/mnt/store/{name})' for role, name in [
            ('gate output', 'augmentagent-gate-target'),
            ('session output parent', 'augmentagent-session-targets'),
            ('verifier output parent', 'augmentagent-verifier-targets')]]}]

    def test_all_destinations_and_reclaimed_space(self):
        self.assertEqual(module.verify(self.status(), Path('/mnt/store'), 2, 200, 100)['reclaimed_net_bytes'], 100)

    def test_missing_role_wrong_device_and_no_reclamation_refuse(self):
        missing = self.status(); missing[0]['labels'].pop()
        for status, device, after in [(missing, 2, 200), (self.status(), 3, 200), (self.status(), 2, 100)]:
            with self.subTest(status=status, device=device, after=after), self.assertRaises(ValueError):
                module.verify(status, Path('/mnt/store'), device, after, 100)

if __name__ == '__main__':
    unittest.main()
