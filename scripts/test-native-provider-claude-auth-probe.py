#!/usr/bin/env python3
"""Pure Claude transport evidence fixtures: no CLI, credentials or provider."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
spec=importlib.util.spec_from_file_location('claude_probe',Path(__file__).with_name('native-provider-claude-auth-probe.py'))
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

class Tests(unittest.TestCase):
    def test_exact_native_model_session_and_completed_turn_required(self):
        with tempfile.TemporaryDirectory() as root:
            p=Path(root)/'events'
            events=[{'type':'system','subtype':'ui_invalidate','event':'fixture-ui-notification','session_id':'actual-session'},
                {'type':'system','session_id':'actual-session','model':m.probe.MODEL},
                {'type':'assistant','message':{'model':m.probe.MODEL,'content':[{'type':'text','text':m.probe.TOKEN}]}},
                {'type':'result','session_id':'actual-session','subtype':'success','is_error':False,'result':m.probe.TOKEN}]
            def save(rows):p.write_text('\n'.join(json.dumps(v) for v in rows));return p
            self.assertEqual(m.observations(save(events),'actual-session')['observed_model'],m.probe.MODEL)
            with self.assertRaises(ValueError):m.observations(p,'invented-session')
            for extra in ({'type':'error'}, {'type':'stream_event','event':'malformed'}, {'type':'assistant','message':{'model':'lower-quality','content':[]}},
                    {'type':'assistant','message':{'content':[{'type':'tool_use'}]}},
                    {'type':'stream_event','event':{'type':'content_block_start','content_block':{'type':'tool_use'}}}):
                with self.assertRaises(ValueError):m.observations(save(events+[extra]))
            with self.assertRaises(ValueError):m.observations(save(events[:-1]))
    def test_oauth_compatible_tool_free_native_resume(self):
        first=m.native_argv();resume=m.native_argv('actual-session')
        self.assertIn('--safe-mode',first);self.assertNotIn('--bare',first)
        self.assertEqual(first[first.index('--tools')+1],'')
        self.assertEqual(resume[-2:],['--resume','actual-session'])
        self.assertIn('claude-opus-4-8',first)
    def test_exact_policy_and_host_messages_inference_required(self):
        profile={'auth_relative':'.claude/.credentials.json','gateway':{'authorities':[{'routes':[{'exact':True}]}]}}
        m.validate_profile(profile)
        profile['gateway']['authorities'][0]['routes'][0]['exact']=False
        with self.assertRaises(ValueError):m.validate_profile(profile)
        row={'route_sha256':'a'*64,'transport':'http','inference':'messages','method':'POST','status':200,
             'authenticated_request':True,'remote_tls':True,'successful':True,'request_bytes':42,'response_bytes':99,'completed':True}
        def proof(row):return {'launch_sha256':'launch','gateway_transport':{'version':1,'overflow':False,'exchanges':[row]}}
        self.assertEqual(m.probe.inference_receipt(proof(row),'launch')['inference_exchanges'],1)
        for changes in ({'inference':'responses'},{'inference':None,'method':'GET'},{'authenticated_request':False},
                {'transport':'websocket','method':'GET','status':101,'completed':False}):
            with self.assertRaises(ValueError):m.probe.inference_receipt(proof(dict(row,**changes)),'launch')
        with self.assertRaises(ValueError):m.probe.inference_receipt(dict(proof(row),diagnostic_only=True),'launch')

if __name__=='__main__':unittest.main()
