import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

spec=importlib.util.spec_from_file_location('capacity_policy',Path(__file__).with_name('native-capacity-policy.py'))
capacity=importlib.util.module_from_spec(spec);spec.loader.exec_module(capacity)


class CapacityPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.state=self.root/'state';self.state.mkdir(mode=0o700)
        for path in (self.root/'provision.lock',self.state/'state.lock'):
            path.write_bytes(b'');path.chmod(0o600)
        self.db=self.state/'runtime-retirement.sqlite'
        with sqlite3.connect(self.db) as db:
            db.execute('CREATE TABLE retired_owner(audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation))')
            db.execute("INSERT INTO retired_owner VALUES('/private/audit','fixture',1)")
        self.db.chmod(0o600)
        self.retained=self.state/'codex'/'session-unknown';self.retained.mkdir(parents=True)
        self.retained.joinpath('unknown-data').write_bytes(b'preserve retained state')
        self.path=self.root/'policy.json';self.value={'version':1,'runtime_config':'/private/config',
            'guest_runtime':'/private/runtime','state_root':str(self.state),'memory_mb':1024,
            'max_state_bytes':8*1024**3,'max_sessions':16,'providers':{'codex':{
                'gateway':{'fixture':'preserved'},'auth_relative':'.codex/auth.json','auth_file':None}}}
        self.path.write_text(json.dumps(self.value));self.path.chmod(0o600)

    def snapshot(self):
        return {str(p.relative_to(self.root)):p.read_bytes() for p in self.root.rglob('*') if p.is_file()}

    def test_preview_preserves_every_live_file_and_makes_no_grant(self):
        before=self.snapshot();p=capacity.update(self.path,16*1024**3)
        self.assertEqual(self.snapshot(),before)
        self.assertFalse(p['policy_applied']);self.assertFalse(p['execution_authorized'])
        self.assertEqual(p['memory_mb'],1024);self.assertEqual(p['max_sessions'],16)

    def test_explicit_hash_bound_apply_changes_only_capacity_and_replays_once(self):
        p=capacity.update(self.path,16*1024**3);before=self.snapshot()
        result=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(result['policy_applied'])
        expected=dict(self.value,max_state_bytes=16*1024**3)
        self.assertEqual(json.loads(self.path.read_bytes()),expected)
        after=self.snapshot();before.pop('policy.json');after.pop('policy.json');self.assertEqual(before,after)
        again=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(again['already_applied']);self.assertFalse(again['policy_applied'])

    def test_apply_requires_both_reviewed_hashes(self):
        before=self.snapshot()
        for source,candidate in ((None,None),('a'*64,None),(None,'a'*64),('a'*64,'b'*64)):
            with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=source,expected_candidate=candidate)
        self.assertEqual(self.snapshot(),before)

    def test_bound_or_unknown_state_never_uses_legacy_apply(self):
        p=capacity.update(self.path,16*1024**3)
        self.state.joinpath('authority-epoch.json').write_text('{}')
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_shrink_or_unchanged_limit_is_refused(self):
        before=self.snapshot()
        for size in (0,4*1024**3,8*1024**3):
            with self.assertRaises(capacity.Hold):capacity.update(self.path,size)
        self.assertEqual(self.snapshot(),before)

    def test_both_original_locks_are_required_without_creating_replacements(self):
        for path in (self.root/'provision.lock',self.state/'state.lock'):
            before=self.snapshot()
            with path.open('r+b') as lock:
                fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
                with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3)
            self.assertEqual(self.snapshot(),before)

    def test_new_ledger_without_epoch_marker_is_not_treated_as_legacy(self):
        with sqlite3.connect(self.db) as db:db.execute('CREATE TABLE native_transfer(id TEXT)')
        p=capacity.update(self.path,16*1024**3);self.assertFalse(p['apply_supported'])
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_reviewed_source_changes_are_refused_and_never_overwritten(self):
        p=capacity.update(self.path,16*1024**3)
        altered=dict(self.value,memory_mb=2048);self.path.write_text(json.dumps(altered))
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_publication_error_before_replace_keeps_original_and_cleans_only_own_temp(self):
        p=capacity.update(self.path,16*1024**3);before=self.snapshot()
        with mock.patch.object(capacity.os,'replace',side_effect=OSError(errno.EIO,'fixture')):
            with self.assertRaises(OSError):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_directory_sync_failure_retains_published_policy_and_uncertainty_on_retry(self):
        p=capacity.update(self.path,16*1024**3);fsync=os.fsync
        def fail_directory(fd):
            if os.path.isdir(f'/proc/self/fd/{fd}'):raise OSError(errno.EIO,'fixture')
            fsync(fd)
        with mock.patch.object(capacity.os,'fsync',side_effect=fail_directory):
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
            self.assertEqual(json.loads(self.path.read_bytes())['max_state_bytes'],16*1024**3)
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        result=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(result['already_applied']);self.assertFalse(result['policy_applied'])

    def test_lost_rename_response_never_claims_original_policy_survived(self):
        p=capacity.update(self.path,16*1024**3);replace=os.replace
        def lost_response(*args,**kwargs):replace(*args,**kwargs);raise OSError(errno.EIO,'lost acknowledgement')
        with mock.patch.object(capacity.os,'replace',side_effect=lost_response):
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(json.loads(self.path.read_bytes())['max_state_bytes'],16*1024**3)

    def test_policy_and_lock_symlinks_are_refused(self):
        for path in (self.path,self.root/'provision.lock',self.state/'state.lock'):
            saved=path.with_name(path.name+'.fixture-original');path.rename(saved);path.symlink_to(saved)
            before=self.snapshot()
            with self.assertRaises((capacity.Hold,OSError)):capacity.update(self.path,16*1024**3)
            self.assertEqual(self.snapshot(),before)
            path.unlink();saved.rename(path)

    def test_duplicate_policy_fields_are_refused_without_rewrite(self):
        self.path.write_text(self.path.read_text()[:-1]+',"max_state_bytes":8589934592}')
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3)
        self.assertEqual(self.snapshot(),before)

    def test_real_cli_defaults_to_preview_and_emits_no_provider_configuration(self):
        before=self.snapshot()
        result=subprocess.run([sys.executable,str(Path(__file__).with_name('native-capacity-policy.py')),
            '--policy',str(self.path),'--max-state-bytes',str(16*1024**3)],capture_output=True,text=True)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertFalse(json.loads(result.stdout)['policy_applied']);self.assertNotIn('preserved',result.stdout)
        self.assertEqual(self.snapshot(),before)

    def test_temporary_name_collision_never_deletes_existing_file(self):
        p=capacity.update(self.path,16*1024**3)
        other=self.root/'.capacity-policy-fixture';other.write_bytes(b'not this invocation');other.chmod(0o600)
        before=self.snapshot()
        with mock.patch.object(capacity.secrets,'token_hex',return_value='fixture'):
            with self.assertRaises(FileExistsError):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_replaced_temporary_file_is_not_published_or_deleted(self):
        p=capacity.update(self.path,16*1024**3);original=self.path.read_bytes();fsync=os.fsync
        temporary=self.root/'.capacity-policy-fixture'
        def replace_after_sync(fd):
            fsync(fd)
            if os.readlink(f'/proc/self/fd/{fd}')==str(temporary):
                temporary.rename(self.root/'fixture-original-retained')
                temporary.write_text(json.dumps(dict(self.value,memory_mb=2048,max_state_bytes=16*1024**3)))
                temporary.chmod(0o600)
        with mock.patch.object(capacity.secrets,'token_hex',return_value='fixture'),mock.patch.object(capacity.os,'fsync',side_effect=replace_after_sync):
            with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.path.read_bytes(),original);self.assertTrue(temporary.exists())


if __name__=='__main__':unittest.main()
