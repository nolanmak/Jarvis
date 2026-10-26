#!/usr/bin/env python3
"""Pure artifact/protocol tests: never launch Codex, a VM, or a provider call."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec=importlib.util.spec_from_file_location('codex_probe',Path(__file__).with_name('native-provider-codex-auth-probe.py'))
probe=importlib.util.module_from_spec(spec);spec.loader.exec_module(probe)


class ProbeTests(unittest.TestCase):
    def records(self, directory, session='actual-fixture-session', text=probe.TOKEN, extra=None):
        path=Path(directory)/'stdout.jsonl'
        events=[{'type':'thread.started','thread_id':session},
                {'type':'item.completed','item':{'type':'agent_message','text':text}},
                {'type':'turn.completed','usage':{'input_tokens':1,'output_tokens':1}}]
        if extra:events.append(extra)
        path.write_text('\n'.join(json.dumps(v) for v in events)+'\n')
        return path

    def test_actual_completed_session_and_exact_reply_required(self):
        with tempfile.TemporaryDirectory() as temporary:
            path=self.records(temporary)
            result=probe.observations(path)
            self.assertTrue(result['exact_reply'])
            self.assertEqual(result['native_session'],'actual-fixture-session')
            self.assertEqual(probe.observations(path,'actual-fixture-session')['completed_turns'],1)
            with self.assertRaises(ValueError):probe.observations(path,'invented-session')
            self.records(temporary,text='not the acceptance token')
            with self.assertRaises(ValueError):probe.observations(path)

    def test_tool_use_and_partial_failure_never_pass_transport_proof(self):
        with tempfile.TemporaryDirectory() as temporary:
            for event in [{'type':'turn.failed'}, {'type':'error'},
                          {'type':'item.completed','item':{'type':'mcp_tool_call'}}]:
                with self.assertRaises(ValueError):probe.observations(self.records(temporary,extra=event))
            path=self.records(temporary)
            path.write_text(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':probe.TOKEN}}))
            with self.assertRaises(ValueError):probe.observations(path)

    def test_current_quality_and_isolated_resume_arguments(self):
        first=probe.native_argv();second=probe.native_argv('real-session')
        self.assertEqual(first[:2],['exec','--json'])
        self.assertEqual(second[:2],['exec','resume'])
        self.assertIn('real-session',second)
        self.assertNotIn('--ephemeral',first)
        self.assertIn('gpt-5.6-terra',first)
        self.assertNotIn('-C',second)
        self.assertIn('features.shell_tool=false',first)
        self.assertIn('mcp_servers={}',first)
        self.assertEqual(first[-1],'-')

    def test_evidence_is_private_and_immutable(self):
        with tempfile.TemporaryDirectory() as temporary:
            path=Path(temporary)/'receipt.json'
            probe.private_write(path,{'safe':True})
            self.assertEqual(path.stat().st_mode&0o777,0o600)
            self.assertEqual(probe.private_json(path),{'safe':True})
            with self.assertRaises(FileExistsError):probe.private_write(path,{'changed':True})
            path.chmod(0o644)
            with self.assertRaises(ValueError):probe.private_json(path)


if __name__=='__main__':unittest.main()
