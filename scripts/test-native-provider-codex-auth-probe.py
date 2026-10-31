#!/usr/bin/env python3
"""Pure artifact/protocol tests: never launch Codex, a VM, or a provider call."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

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
            for event in ([{'type':'turn.failed'}, {'type':'error'}] +
                          [{'type':phase,'item':{'type':'mcp_tool_call'}} for phase in ('item.started','item.updated','item.completed')]):
                with self.assertRaises(ValueError):probe.observations(self.records(temporary,extra=event))
            path=self.records(temporary)
            path.write_text(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':probe.TOKEN}}))
            with self.assertRaises(ValueError):probe.observations(path)

    def test_cli_json_alone_and_metadata_never_prove_provider_exchange(self):
        with tempfile.TemporaryDirectory() as temporary:
            self.assertTrue(probe.observations(self.records(temporary))['exact_reply'])
            with self.assertRaises(ValueError):probe.inference_receipt({}, 'launch')
        row={'route_sha256':'a'*64,'transport':'http','inference':'responses','method':'POST','status':200,
             'authenticated_request':True,'remote_tls':True,'successful':True,'request_bytes':42,'response_bytes':99,'completed':True}
        def completion(value):return {'launch_sha256':'launch','gateway_transport':{'version':1,'overflow':False,'exchanges':[value]}}
        self.assertEqual(probe.inference_receipt(completion(row),'launch')['inference_exchanges'],1)
        with self.assertRaises(ValueError):
            probe.inference_receipt(dict(completion(row), diagnostic_only=True), 'launch')
        for change in ({'inference':None,'method':'GET'}, {'status':401}, {'request_bytes':0},
                       {'response_bytes':0}, {'authenticated_request':False}, {'remote_tls':False}, {'completed':False}):
            with self.assertRaises(ValueError):probe.inference_receipt(completion(dict(row,**change)),'launch')
        with self.assertRaises(ValueError):probe.inference_receipt(completion(row),'different-launch')
        ws=dict(row,transport='websocket',method='GET',status=101,completed=False)
        self.assertEqual(probe.inference_receipt(completion(ws),'launch')['inference_exchanges'],1)

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

    def recovery_fixture(self, directory):
        root=Path(directory);turn=root/'turn-1';turn.mkdir(mode=0o700)
        session=root/'native-session';session.mkdir(mode=0o700)
        generation='generation-'+'a'*32;(session/generation).mkdir(mode=0o700)
        executable=root/'native';executable.write_bytes(b'trusted fixture executable')
        auth=root/'auth.json';probe.private_write(auth,{'fixture':True})
        profile={'auth_file':str(auth),'auth_relative':'.codex/auth.json','gateway':{},'refresh_auth':False,'environment':[]}
        policy={'runtime_config':'fixture-runtime','guest_runtime':'fixture-guest','memory_mb':512,'providers':{'codex':profile}}
        policy_path=root/'policy.json';probe.private_write(policy_path,policy)
        probe.private_write(root/'probe.json',{'kind':'authenticated_codex_vm_transport_only','policy_sha256':probe.digest_file(policy_path),'executable':str(executable),'model':probe.MODEL,'auth_source_read_only':True})
        probe.private_write(root/'summary.json',{'status':'failed','auth_source_unchanged':True,'category':'AttributeError'})
        launch={'provider':'codex','runtime_config':policy['runtime_config'],'guest_runtime':policy['guest_runtime'],'memory_mb':512,'session_directory':str(session),'resume':False,'executable':str(executable),'argv':probe.native_argv(),'environment':{'LANG':'C.UTF-8'},'auth_file':str(auth),'auth_relative':profile['auth_relative'],'refresh_auth':False,'gateway':{},'mcp_argv':None,'assets':[]}
        probe.private_write(turn/'launch.json',launch)
        probe.private_write(turn/'command.json',{'native_argv':launch['argv'],'model':probe.MODEL,'executable_sha256':probe.digest_file(executable),'prompt':probe.PROMPT,'prompt_sha256':probe.hashlib.sha256(probe.PROMPT.encode()).hexdigest()})
        self.records(turn);(turn/'stderr.log').write_text('')
        probe.private_write(turn/'failure.json',{'stdout_sha256':probe.digest_file(turn/'stdout.jsonl'),'stderr_sha256':probe.digest_file(turn/'stderr.log')})
        completion={'exit_code':0,'generation':generation,'auth_generation':probe.digest_file(auth),'worker_quiescence':{'remaining_workers':0,'quiet_scans':2},'launch_sha256':probe.digest_file(turn/'launch.json'),'gateway_transport':{'version':1,'overflow':False,'exchanges':[{'route_sha256':'a'*64,'transport':'http','inference':'responses','method':'POST','status':200,'authenticated_request':True,'remote_tls':True,'successful':True,'request_bytes':1,'response_bytes':1,'completed':True}]}}
        probe.private_write(session/'complete.json',completion)
        return root,policy_path,policy,profile,executable

    def test_existing_proof_validates_original_auth_and_never_rebills_first_turn(self):
        with tempfile.TemporaryDirectory() as directory:
            args=self.recovery_fixture(directory);root=args[0]
            old_summary=(root/'summary.json').read_bytes()
            self.assertEqual(probe.validate_existing(*args)['native_session'],'actual-fixture-session')
            calls=[]
            def dispatch(root,policy,profile,executable,iteration,session,budget):
                calls.append((iteration,session));(root/'turn-2').mkdir(mode=0o700)
                probe.private_write(root/'turn-2/receipt.json',{'fixture':True})
                return {'native_session':session}
            with patch.object(probe,'launch_turn',side_effect=dispatch):
                self.assertEqual(probe.resume_existing(*args,60),0)
                with self.assertRaises(ValueError):probe.resume_existing(*args,60)
            self.assertEqual(calls,[(2,'actual-fixture-session')])
            self.assertEqual((root/'summary.json').read_bytes(),old_summary)
        for changed in ('missing_digest','changed_auth','changed_output','wrong_launch'):
            with tempfile.TemporaryDirectory() as directory:
                args=self.recovery_fixture(directory);root=args[0]
                path=root/'native-session/complete.json';completion=probe.private_json(path)
                if changed=='missing_digest':completion.pop('auth_generation')
                if changed=='changed_auth':Path(args[3]['auth_file']).write_text('{}')
                if changed=='changed_output':(root/'turn-1/stdout.jsonl').write_text('{}')
                if changed=='wrong_launch':completion['launch_sha256']='wrong'
                path.write_text(json.dumps(completion))
                with self.assertRaises(ValueError):probe.validate_existing(*args)

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
