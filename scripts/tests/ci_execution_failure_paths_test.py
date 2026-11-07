"""Both original guest call paths with actual worker supervision/cleanup."""
import ast
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import ci_execution_abandonment_test as fixtures
import codex_build_vm_test as vm_fixture
planner=fixtures.planner


class GuestFailureTests(unittest.TestCase):
    setUp=fixtures.AbandonmentTests.setUp

    def run_case(self,caller,mode):
        payload=json.dumps({'exit_code':0,'stdout':'','stderr':'','repository_execution':{'cargo_commands':[]}}) if mode=='finish' else '{bad'
        argv=['/usr/bin/false'] if mode in ('exit','run-error','verify') else ['/usr/bin/printf',payload]
        scope=self.workers.WorkerScope(self.control,os.getuid(),os.getgid(),argv)
        # The entry point will bind this actual scope to the original lease.
        self.lease.scope=None
        value=planner.cache._read_record(self.lease.path);value['scope']=None
        planner.cache._write_record(self.lease.path,value)
        journal=SimpleNamespace(closed=False)
        def finish(stopped):
            if mode=='journal':raise planner.FreshnessHold('input_census_unknown')
            return {'mechanics_only':True}
        journal.finish=finish;journal.close=lambda:setattr(journal,'closed',True)
        def run(argv,original_scope):
            process=subprocess.Popen(argv,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            original_scope.attach(process);out,err=process.communicate(timeout=5)
            if mode=='verify':scope.receipt.write_bytes(b'not original cleanup\n')
            if mode=='run-error':raise OSError('after foreground cleanup')
            return {'exit_code':process.returncode,'stdout':out.decode(),'stderr':err.decode()}
        names={'_run_execution','run_cargo_commands','run_repository_command'}
        tree=ast.parse(vm_fixture.vm.GUEST_RUNNER)
        selected=ast.Module(body=[node for node in tree.body if isinstance(node,ast.FunctionDef) and node.name in names],type_ignores=[])
        context={'Path':Path,'json':json,'_run_worker':run,'execution_lease':lambda:self.lease,
            'private_cache':self.root,'generation':{'binding':self.binding,'command':self.command,'outputs':{}},
            'environment':{},'worker_control':self.control,'ci_workers':SimpleNamespace(controller_execution_scope=lambda *a:scope,
                controller_repository_scope=lambda *a:scope),'ci_inputs':SimpleNamespace(ChangeJournal=lambda *a,**k:journal),
            'ci_cache':{'FreshnessHold':planner.FreshnessHold},'guest_worker_receipts':[]}
        exec(compile(selected,'original-guest-execution-callers','exec'),context)
        with patch.object(tempfile,'mkdtemp',return_value=str(self.root/'run')):
            if mode=='finish':
                with patch.object(self.lease,'finish',side_effect=planner.FreshnessHold('input_census_unknown')):
                    with self.assertRaises(planner.FreshnessHold):context[caller]([],None)
            else:
                with self.assertRaises(Exception):context[caller]([],None)
        self.assertTrue(journal.closed)
        state=planner.ExecutionLease._read(self.lease.path,self.binding)
        self.assertEqual(state['phase'],'active' if mode=='verify' else 'abandoned')
        self.assertFalse((self.generation/'completed-ci-commands').exists())

    def test_repository_failure_after_original_cleanup_is_abandoned(self):self.run_case('run_repository_command','exit')
    def test_cargo_failure_after_original_cleanup_is_abandoned(self):self.run_case('run_cargo_commands','exit')
    def test_malformed_result_after_original_cleanup_is_abandoned(self):self.run_case('run_repository_command','parse')
    def test_change_journal_failure_after_original_cleanup_is_abandoned(self):self.run_case('run_cargo_commands','journal')
    def test_supervised_run_error_can_use_only_its_original_cleanup(self):self.run_case('run_cargo_commands','run-error')
    def test_missing_original_cleanup_never_becomes_abandonment(self):self.run_case('run_repository_command','verify')
    def test_checkpoint_failure_after_original_cleanup_is_abandoned(self):self.run_case('run_repository_command','finish')


if __name__=='__main__':unittest.main()
