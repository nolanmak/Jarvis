#!/usr/bin/env python3
import importlib.util,json
from pathlib import Path
import tempfile,unittest
s=importlib.util.spec_from_file_location('routing',Path(__file__).with_name('native-probe-routing.py'));m=importlib.util.module_from_spec(s);s.loader.exec_module(m)
class Tests(unittest.TestCase):
 def test_explicit_config_preserves_auto_and_hides_secrets(self):
  with tempfile.TemporaryDirectory() as d:
   r=Path(d)/'router';p=Path(d)/'selection'
   data={'version':1,'mode':'auto','base_url':'http://127.0.0.1:20128/v1','api_key':'private-fixture-key','models':{'codex':{'quality':'cx/gpt-5.6-terra'}}}
   r.write_text(json.dumps(data));r.chmod(0o600);p.write_text(json.dumps({'version':1,'default':None,'conversations':{'private-conversation':'claude'}}));p.chmod(0o600)
   evidence,env=m.snapshot(r,p)
   self.assertEqual(evidence['operator_route']['mode'],'auto');self.assertIsNone(evidence['default_selection'])
   self.assertNotIn('private-fixture-key',json.dumps(evidence));self.assertNotIn('private-conversation',json.dumps(evidence))
   self.assertEqual(env['AUGMENTAGENT_MODEL_ROUTER_CONFIG'],str(r));self.assertTrue(m.unchanged(evidence))
   data['mode']='codex';r.write_text(json.dumps(data));self.assertFalse(m.unchanged(evidence))
 def test_absent_settings_are_explicit_and_creation_invalidates(self):
  with tempfile.TemporaryDirectory() as d:
   r=Path(d)/'router';p=Path(d)/'selection';e,env=m.snapshot(r,p)
   self.assertEqual(env['AUGMENTAGENT_MODEL_SELECTION_CONFIG'],str(p));self.assertTrue(m.unchanged(e))
   p.write_text('{"version":1,"default":"claude"}');p.chmod(0o600);self.assertFalse(m.unchanged(e))
if __name__=='__main__':unittest.main()
