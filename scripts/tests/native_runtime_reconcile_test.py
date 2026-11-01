"""Exercise selected-policy reconciliation without providers or host services."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('ensure_native',ROOT/'scripts/ensure-native-runtime.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

def stage(destination,native,bases=()):
    destination.mkdir(mode=0o700)
    records={}
    for base in bases:m.provision.copy_base_runtime(base,destination,records)
    digest=hashlib.sha256(native.read_bytes()).hexdigest()
    relative='libexec/codex/'+digest+'/codex-code-mode-host'
    m.provision.copy_runtime_file(destination,relative,native.with_name('codex-code-mode-host'),records)
    manifest={'version':1,'files':[{'path':p,'sha256':v[0]} for p,v in sorted(records.items())],
              'native_companions':{digest:[{'name':'codex-code-mode-host','path':relative,'sha256':records[relative][0]}]}}
    (destination/'manifest.json').write_text(json.dumps(manifest));(destination/'manifest.json').chmod(0o400)

class RuntimeReconcileTests(unittest.TestCase):
    def test_selection_is_data_and_never_executes_shell_syntax(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);selection=root/'selection.env';marker=root/'executed'
            selection.write_text('AUGMENTAGENT_NATIVE_VM_POLICY="/tmp/$(touch '+str(marker)+')"\n');selection.chmod(0o600)
            with self.assertRaises(OSError):m.reconcile(selection,root/'codex')
            self.assertFalse(marker.exists())
            selection.write_text('AUGMENTAGENT_NATIVE_VM_POLICY="relative.json"\n')
            with self.assertRaises(ValueError):m.selected_policy(selection)

    def test_selected_runtime_refreshes_once_preserving_grants_and_old_generation(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(m.provision,'stage_runtime',stage):
            root=Path(directory);native=root/'codex';native.write_bytes(b'old executable');native.chmod(0o700)
            companion=root/'codex-code-mode-host';companion.write_bytes(b'old companion');companion.chmod(0o700)
            old=root/'runtime-old';stage(old,native)
            policy=root/'policy.json';value={'version':1,'guest_runtime':str(old),'state_root':str(root/'state'),
                'providers':{'codex':{'refresh_auth':False,'auth_file':'/never-read/credential.json','environment':[],'gateway':{'fixed':'grants'}}}}
            policy.write_text(json.dumps(value));policy.chmod(0o600)
            selection=root/'autoship-native.env';selection.write_text('AUGMENTAGENT_NATIVE_VM_POLICY="'+str(policy)+'"\n');selection.chmod(0o600)
            self.assertEqual(m.reconcile(root/'absent.env',native),'not-selected')
            native.write_bytes(b'new executable');companion.write_bytes(b'new companion')
            self.assertEqual(m.reconcile(selection,native),'refreshed')
            updated=json.loads(policy.read_text());self.assertNotEqual(updated['guest_runtime'],str(old))
            self.assertTrue(old.exists());self.assertEqual({k:v for k,v in updated.items() if k!='guest_runtime'},{k:v for k,v in value.items() if k!='guest_runtime'})
            previous=policy.read_bytes();generations=set(root.glob('runtime-*'))
            self.assertEqual(m.reconcile(selection,native),'current')
            self.assertEqual(policy.read_bytes(),previous);self.assertEqual(set(root.glob('runtime-*')),generations)
            def change_during_inspection(*args):
                policy.write_bytes(previous+b' ')
                return True
            with patch.object(m.provision,'runtime_matches_native',change_during_inspection), self.assertRaisesRegex(ValueError,'changed during runtime inspection'):
                m.reconcile(selection,native)
            self.assertEqual(policy.read_bytes(),previous+b' ')
            policy.write_bytes(previous)
            selection.chmod(0o644)
            with self.assertRaises(ValueError):m.reconcile(selection,native)
            self.assertEqual(policy.read_bytes(),previous)
            selection.chmod(0o600)
            manifest=json.loads((Path(updated['guest_runtime'])/'manifest.json').read_text())
            artifact=Path(updated['guest_runtime'])/manifest['files'][0]['path'];artifact.chmod(0o600);artifact.write_bytes(b'corrupt')
            with self.assertRaises(ValueError):m.reconcile(selection,native)
            self.assertEqual(policy.read_bytes(),previous)
            self.assertEqual(set(root.glob('runtime-*')),generations)

if __name__=='__main__':unittest.main()
