"""Required voice packages must install on every declared host target."""

import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
VOICE = ROOT / 'sidecars/discord-voice'


class VoicePackagePlatformTests(unittest.TestCase):
    def test_required_dependencies_support_linux_and_mac_targets(self):
        manifest = json.loads((VOICE / 'package.json').read_text())
        lock = json.loads((VOICE / 'package-lock.json').read_text())
        required = {**manifest.get('dependencies', {}), **manifest.get('devDependencies', {})}
        targets = (('linux', 'x64'), ('darwin', 'arm64'), ('darwin', 'x64'))
        for name in required:
            package = lock['packages'][f'node_modules/{name}']
            for os_name, cpu in targets:
                with self.subTest(package=name, os=os_name, cpu=cpu):
                    supported_os = package.get('os', [])
                    supported_cpu = package.get('cpu', [])
                    self.assertTrue(not supported_os or os_name in supported_os,
                                    f'{name} is required but cannot install on {os_name}/{cpu}')
                    self.assertTrue(not supported_cpu or cpu in supported_cpu,
                                    f'{name} is required but cannot install on {os_name}/{cpu}')


if __name__ == '__main__':
    unittest.main()
