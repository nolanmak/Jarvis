"""Retirement contract: active tooling cannot resurrect the removed gateway."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]


class RetirementTests(unittest.TestCase):
    def test_retired_installer_image_and_sidecar_are_absent(self):
        for relative in ['scripts/install-model-router.py', 'scripts/build-model-router-image.sh',
                         'sidecars/' + '9' + 'router', 'scripts/start-model-router.py',
                         'scripts/uninstall-model-router.sh', 'scripts/tests/run_model_router_image_fixture.py']:
            self.assertFalse((ROOT / relative).exists(), relative)

    def test_only_explicit_legacy_note_and_optional_kind_remain(self):
        allowed = {'crates/augmentagent-channel-core/src/model_router.rs',
                   'scripts/tests/omniroute_install_test.py'}
        hits = []
        paths = [ROOT / 'README.md']
        for directory in ['crates', 'scripts', 'docs']:
            paths.extend(p for p in (ROOT / directory).rglob('*') if p.is_file()
                         and '.git' not in p.parts and '__pycache__' not in p.parts)
        for path in paths:
            relative = path.relative_to(ROOT).as_posix()
            if relative in allowed or path == Path(__file__).resolve():
                continue
            try:
                for number, line in enumerate(path.read_text().splitlines(), 1):
                    if re.search('9' + 'router', line, re.I):
                        if relative == 'docs/model-router.md' and line.startswith('**9' + 'Router (removed).**'):
                            continue
                        hits.append(f'{relative}:{number}')
            except UnicodeError:
                pass
        self.assertEqual(hits, [])

    def test_default_installation_and_routing_remain_opt_in(self):
        config = (ROOT / 'scripts/lib/omniroute_install.py').read_text()
        self.assertIn("'mode': 'direct'", config)
        for path in [ROOT / 'scripts/install-autostart.sh', ROOT / 'scripts/install-autoupdate.sh']:
            self.assertNotIn('install-omniroute', path.read_text())


if __name__ == '__main__':
    unittest.main()
