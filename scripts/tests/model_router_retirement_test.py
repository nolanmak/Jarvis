"""Retirement contract: active tooling cannot resurrect the removed gateway."""
from pathlib import Path
import json
import re
import subprocess
import unittest
import importlib.util

_fixture_spec = importlib.util.spec_from_file_location('omniroute_installer_fixture', Path(__file__).with_name('omniroute_install_test.py'))
installer_fixture = importlib.util.module_from_spec(_fixture_spec)
_fixture_spec.loader.exec_module(installer_fixture)

ROOT = Path(__file__).resolve().parents[2]


def legacy_allowed(relative, line):
    lower, title = '9' + 'router', '9' + 'Router'
    allowed = {
        'crates/augmentagent-channel-core/src/model_router.rs': {
            f'#[serde(rename = "{lower}")]', f'Self::NineRouter => "{title}",',
            f'Self::NineRouter => "X-{title}-Token-Saver",',
            f'("{lower}","{title}","X-{title}-Token-Saver"),',
            f'for gateway in ["omniroute","{lower}"] {{',
        },
        'scripts/tests/omniroute_install_test.py': {
            'self.inst.router.write_text(' + repr(json.dumps({'gateway': lower, 'mode': 'auto'}, separators=(',', ':'))) + ')',
        },
    }
    return (line.strip() in allowed.get(relative, set())
            or relative == 'docs/model-router.md' and line.startswith('**' + title + ' (removed).**'))


class RetirementTests(unittest.TestCase):
    def test_retired_installer_image_and_sidecar_are_absent(self):
        for relative in ['scripts/install-model-router.py', 'scripts/build-model-router-image.sh',
                         'sidecars/' + '9' + 'router', 'scripts/start-model-router.py',
                         'scripts/uninstall-model-router.sh', 'scripts/tests/run_model_router_image_fixture.py']:
            self.assertFalse((ROOT / relative).exists(), relative)

    def test_only_explicit_legacy_note_and_optional_kind_remain(self):
        paths = subprocess.check_output(['git', 'ls-files', '-z', '--', 'crates', 'scripts', 'docs', 'README.md'], cwd=ROOT).decode().split('\0')
        hits = []
        for relative in filter(None, paths):
            path = ROOT / relative
            if not path.is_file():
                continue
            try:
                for number, line in enumerate(path.read_text().splitlines(), 1):
                    if re.search('9' + 'router', line, re.I) and not legacy_allowed(relative, line):
                        hits.append(f'{relative}:{number}')
            except UnicodeError:
                pass
        self.assertEqual(hits, [])

    def test_compatibility_exception_does_not_hide_new_provisioning_reference(self):
        line = '// install ' + '9' + 'router' + ' automatically'
        for relative in ['crates/augmentagent-channel-core/src/model_router.rs', 'scripts/tests/omniroute_install_test.py']:
            self.assertFalse(legacy_allowed(relative, line))

    def test_required_gateway_job_identity_is_preserved_for_replacement_checks(self):
        contract = json.loads((ROOT / 'scripts/ci-contract.json').read_text())
        job = contract['workflows']['.github/workflows/model-router.yml']['jobs']['pinned-gateway']
        bodies = [contract['commands'][row['command_id']]['body'] for row in job['steps'] if 'command_id' in row]
        self.assertTrue(any('model_router_retirement_test.py' in body for body in bodies))
        self.assertTrue(any('omniroute_install_test.py' in body for body in bodies))

    def test_default_installation_and_routing_remain_opt_in(self):
        fixture = installer_fixture.InstallTests('test_node_floor_versions')
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        fixture.inst.install()
        self.assertEqual(json.loads(fixture.inst.router.read_text())['mode'], 'direct')
        for path in [ROOT / 'scripts/install-autostart.sh', ROOT / 'scripts/install-autoupdate.sh']:
            self.assertNotIn('install-omniroute', path.read_text())


if __name__ == '__main__':
    unittest.main()
