import errno
import fcntl
import importlib.util
import io
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
        """Create a private legacy-store fixture with retained data and original locks."""
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
        """Observe fixture file contents to detect unintended writes or deletion."""
        return {str(p.relative_to(self.root)):p.read_bytes() for p in self.root.rglob('*') if p.is_file()}

    def test_preview_preserves_every_live_file_and_makes_no_grant(self):
        """Verify preview leaves every fixture file and execution permission unchanged."""
        before=self.snapshot();p=capacity.update(self.path,16*1024**3)
        self.assertEqual(self.snapshot(),before)
        self.assertFalse(p['policy_applied']);self.assertFalse(p['execution_authorized'])
        self.assertEqual(p['memory_mb'],1024);self.assertEqual(p['max_sessions'],16)

    def test_explicit_hash_bound_apply_changes_only_capacity_and_replays_once(self):
        """Verify an approved update preserves other state and replay does not increase again."""
        p=capacity.update(self.path,16*1024**3);before=self.snapshot()
        result=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(result['policy_applied'])
        expected=dict(self.value,max_state_bytes=16*1024**3)
        self.assertEqual(json.loads(self.path.read_bytes()),expected)
        after=self.snapshot();before.pop('policy.json');after.pop('policy.json');self.assertEqual(before,after)
        again=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(again['already_applied']);self.assertFalse(again['policy_applied'])

    def test_apply_requires_both_reviewed_hashes(self):
        """Refuse absent or mismatched approval hashes without changing fixture files."""
        before=self.snapshot()
        for source,candidate in ((None,None),('a'*64,None),(None,'a'*64),('a'*64,'b'*64)):
            with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=source,expected_candidate=candidate)
        self.assertEqual(self.snapshot(),before)

    def test_bound_or_unknown_state_never_uses_legacy_apply(self):
        """Refuse legacy apply when the store declares binding-aware authority."""
        p=capacity.update(self.path,16*1024**3)
        self.state.joinpath('authority-epoch.json').write_text('{}')
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_shrink_or_unchanged_limit_is_refused(self):
        """Keep unchanged and reduced budgets outside the supported increase operation."""
        before=self.snapshot()
        for size in (0,4*1024**3,8*1024**3):
            with self.assertRaises(capacity.Hold):capacity.update(self.path,size)
        self.assertEqual(self.snapshot(),before)

    def test_both_original_locks_are_required_without_creating_replacements(self):
        """Refuse either busy original lock without creating new coordination files."""
        for path in (self.root/'provision.lock',self.state/'state.lock'):
            before=self.snapshot()
            with path.open('r+b') as lock:
                fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
                with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3)
            self.assertEqual(self.snapshot(),before)

    def test_new_ledger_without_epoch_marker_is_not_treated_as_legacy(self):
        """Refuse an unfamiliar schema even when its epoch marker is missing."""
        with sqlite3.connect(self.db) as db:db.execute('CREATE TABLE native_transfer(id TEXT)')
        p=capacity.update(self.path,16*1024**3);self.assertFalse(p['apply_supported'])
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_reviewed_source_changes_are_refused_and_never_overwritten(self):
        """Preserve an intervening policy edit after the owner reviewed an older source."""
        p=capacity.update(self.path,16*1024**3)
        altered=dict(self.value,memory_mb=2048);self.path.write_text(json.dumps(altered))
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_publication_error_before_replace_keeps_original_and_cleans_only_own_temp(self):
        """Verify a pre-commit I/O failure preserves the policy and removes only owned staging."""
        p=capacity.update(self.path,16*1024**3);before=self.snapshot()
        with mock.patch.object(capacity.os,'replace',side_effect=OSError(errno.EIO,'fixture')):
            with self.assertRaises(OSError):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_directory_sync_failure_retains_published_policy_and_uncertainty_on_retry(self):
        """Keep publication uncertainty until both the file and directory sync can succeed."""
        p=capacity.update(self.path,16*1024**3);fsync=os.fsync
        def fail_directory(fd):
            """Inject a directory durability failure after real file sync."""
            if os.path.isdir(f'/proc/self/fd/{fd}'):raise OSError(errno.EIO,'fixture')
            fsync(fd)
        with mock.patch.object(capacity.os,'fsync',side_effect=fail_directory):
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
            self.assertEqual(json.loads(self.path.read_bytes())['max_state_bytes'],16*1024**3)
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        result=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(result['already_applied']);self.assertFalse(result['policy_applied'])

    def test_lost_rename_response_never_claims_original_policy_survived(self):
        """Verify a committed rename with lost response is reported as uncertain."""
        p=capacity.update(self.path,16*1024**3);replace=os.replace
        def lost_response(*args,**kwargs):
            """Commit the actual rename before simulating a lost acknowledgement."""
            replace(*args,**kwargs);raise OSError(errno.EIO,'lost acknowledgement')
        with mock.patch.object(capacity.os,'replace',side_effect=lost_response):
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(json.loads(self.path.read_bytes())['max_state_bytes'],16*1024**3)

    def test_policy_and_lock_symlinks_are_refused(self):
        """Prevent policy or coordination links from redirecting the owner command."""
        for path in (self.path,self.root/'provision.lock',self.state/'state.lock'):
            saved=path.with_name(path.name+'.fixture-original');path.rename(saved);path.symlink_to(saved)
            before=self.snapshot()
            with self.assertRaises((capacity.Hold,OSError)):capacity.update(self.path,16*1024**3)
            self.assertEqual(self.snapshot(),before)
            path.unlink();saved.rename(path)

    def test_duplicate_policy_fields_are_refused_without_rewrite(self):
        """Reject duplicate JSON keys without rewriting ambiguous source evidence."""
        self.path.write_text(self.path.read_text()[:-1]+',"max_state_bytes":8589934592}')
        before=self.snapshot()
        with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3)
        self.assertEqual(self.snapshot(),before)

    def test_real_cli_defaults_to_preview_and_emits_no_provider_configuration(self):
        """Exercise the actual CLI preview and its restricted public projection."""
        before=self.snapshot()
        result=subprocess.run([sys.executable,str(Path(__file__).with_name('native-capacity-policy.py')),
            '--policy',str(self.path),'--max-state-bytes',str(16*1024**3)],capture_output=True,text=True)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertFalse(json.loads(result.stdout)['policy_applied']);self.assertNotIn('preserved',result.stdout)
        self.assertEqual(self.snapshot(),before)

    def test_temporary_name_collision_never_deletes_existing_file(self):
        """Preserve a pre-existing file when staging cannot create its own pathname."""
        p=capacity.update(self.path,16*1024**3)
        other=self.root/'.capacity-policy-fixture';other.write_bytes(b'not this invocation');other.chmod(0o600)
        before=self.snapshot()
        with mock.patch.object(capacity.secrets,'token_hex',return_value='fixture'):
            with self.assertRaises(FileExistsError):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_replaced_temporary_file_is_not_published_or_deleted(self):
        """Preserve a substituted staging file and refuse publishing it as the approved policy."""
        p=capacity.update(self.path,16*1024**3);original=self.path.read_bytes();fsync=os.fsync
        temporary=self.root/'.capacity-policy-fixture'
        def replace_after_sync(fd):
            """Substitute the staging pathname while its original inode remains held."""
            fsync(fd)
            if os.readlink(f'/proc/self/fd/{fd}')==str(temporary):
                temporary.rename(self.root/'fixture-original-retained')
                temporary.write_text(json.dumps(dict(self.value,memory_mb=2048,max_state_bytes=16*1024**3)))
                temporary.chmod(0o600)
        with mock.patch.object(capacity.secrets,'token_hex',return_value='fixture'),mock.patch.object(capacity.os,'fsync',side_effect=replace_after_sync):
            with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.path.read_bytes(),original);self.assertTrue(temporary.exists())

    def test_replay_does_not_confirm_a_policy_replaced_during_sync(self):
        """Retain uncertainty when a matching target disappears during confirmation."""
        p=capacity.update(self.path,16*1024**3)
        capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        fsync=os.fsync;changed=False
        def replace_during_confirmation(fd):
            """Replace the selected policy while the retry syncs its prior inode."""
            nonlocal changed
            fsync(fd)
            if not changed and os.readlink(f'/proc/self/fd/{fd}')==str(self.path):
                replacement=self.root/'fixture-intervening-policy'
                replacement.write_text(json.dumps(dict(self.value,max_state_bytes=32*1024**3)));replacement.chmod(0o600)
                os.replace(replacement,self.path);changed=True
        with mock.patch.object(capacity.os,'fsync',side_effect=replace_during_confirmation):
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(changed);self.assertEqual(json.loads(self.path.read_bytes())['max_state_bytes'],32*1024**3)

    def test_file_sync_capacity_failure_keeps_original_and_releases_staging(self):
        """Preserve the original policy when staged file sync reports no space."""
        p=capacity.update(self.path,16*1024**3);before=self.snapshot()
        with mock.patch.object(capacity.os,'fsync',side_effect=OSError(errno.ENOSPC,'fixture')):
            with self.assertRaises(OSError):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(self.snapshot(),before)

    def test_replaced_original_lock_prevents_publication(self):
        """Refuse publication after the held native lock loses its original pathname."""
        p=capacity.update(self.path,16*1024**3);original=self.path.read_bytes();fsync=os.fsync;changed=False
        def replace_lock_after_stage(fd):
            """Replace the native lock after staging to test loss of coordination."""
            nonlocal changed
            fsync(fd)
            if not changed and '/.capacity-policy-' in os.readlink(f'/proc/self/fd/{fd}'):
                lock=self.state/'state.lock';lock.rename(self.state/'fixture-original-lock')
                lock.write_bytes(b'');lock.chmod(0o600);changed=True
        with mock.patch.object(capacity.os,'fsync',side_effect=replace_lock_after_stage):
            with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(changed);self.assertEqual(self.path.read_bytes(),original)

    def test_wal_mode_without_sidecars_cannot_make_preview_create_them(self):
        """Verify unsupported WAL mode cannot make a read-only preview create sidecars."""
        db=sqlite3.connect(self.db)
        try:self.assertEqual(db.execute('PRAGMA journal_mode=WAL').fetchone(),('wal',))
        finally:db.close()
        before=self.snapshot()
        try:
            result=capacity.update(self.path,16*1024**3)
            self.assertFalse(result['apply_supported'])
        except (capacity.Hold,sqlite3.Error):pass
        self.assertEqual(self.snapshot(),before,'read-only preview created SQLite WAL/SHM files')

    def test_pending_wal_schema_is_refused_without_ignoring_its_authority(self):
        """Keep pending WAL schema authority intact instead of reading only the base database."""
        db=sqlite3.connect(self.db)
        try:
            db.execute('PRAGMA journal_mode=WAL');db.execute('CREATE TABLE native_transfer(id TEXT)');db.commit()
            before=self.snapshot()
            with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3)
            self.assertEqual(self.snapshot(),before)
        finally:db.close()

    def test_legacy_schema_rejects_added_objects_and_altered_table_definitions(self):
        """Refuse schema extensions even when the visible legacy columns still match."""
        additions=(
            'CREATE VIEW extension AS SELECT * FROM retired_owner',
            'CREATE TRIGGER extension AFTER INSERT ON retired_owner BEGIN SELECT 1; END',
            'CREATE INDEX extension ON retired_owner(generation)',
        )
        for sql in additions:
            with self.subTest(sql=sql):
                with sqlite3.connect(self.db) as db:db.execute(sql)
                try:
                    p=capacity.update(self.path,16*1024**3)
                    self.assertFalse(p['apply_supported'])
                    before=self.snapshot()
                    with self.assertRaises(capacity.Hold):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
                    self.assertEqual(self.snapshot(),before)
                finally:
                    with sqlite3.connect(self.db) as db:db.execute('DROP '+sql.split()[1]+' extension')
        for definition in (
            'audit_path TEXT NOT NULL DEFAULT "",workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation)',
            'audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL CHECK(generation>0),PRIMARY KEY(audit_path,workflow,generation)',
            'audit_path TEXT NOT NULL COLLATE NOCASE,workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation)',
            'audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL,extra TEXT GENERATED ALWAYS AS (workflow) VIRTUAL,PRIMARY KEY(audit_path,workflow,generation)',
        ):
            with self.subTest(definition=definition):
                with sqlite3.connect(self.db) as db:
                    db.execute('DROP TABLE retired_owner');db.execute('CREATE TABLE retired_owner('+definition+')')
                p=capacity.update(self.path,16*1024**3)
                self.assertFalse(p['apply_supported'])

    def test_identical_policy_replacement_cannot_confirm_an_unsynced_inode(self):
        """Retain uncertainty for identical bytes on a different unsynced policy inode."""
        for replay in (False,True):
            with self.subTest(replay=replay):
                self.path.write_text(json.dumps(self.value))
                p=capacity.update(self.path,16*1024**3)
                if replay:capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
                fsync=os.fsync;changed=False
                def replace_at_directory_sync(fd):
                    """Swap approved contents after the policy file was synced."""
                    nonlocal changed
                    if not changed and os.path.isdir(f'/proc/self/fd/{fd}'):
                        replacement=self.root/'fixture-identical-policy'
                        replacement.write_bytes(self.path.read_bytes());replacement.chmod(0o600)
                        os.replace(replacement,self.path);changed=True
                    fsync(fd)
                with mock.patch.object(capacity.os,'fsync',side_effect=replace_at_directory_sync):
                    with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
                self.assertTrue(changed)
                self.assertEqual(capacity.digest(self.path.read_bytes()),p['candidate_sha256'])

    def test_lock_replacement_at_rename_reports_uncertainty_without_rollback(self):
        """Report a late coordination replacement after publication without undoing it."""
        p=capacity.update(self.path,16*1024**3);replace=os.replace
        def late_replacement(*args,**kwargs):
            """Change the lock pathname after the last prepublication context check."""
            lock=self.state/'state.lock';lock.rename(self.state/'fixture-original-lock')
            lock.write_bytes(b'');lock.chmod(0o600)
            return replace(*args,**kwargs)
        with mock.patch.object(capacity.os,'replace',side_effect=late_replacement):
            with self.assertRaises(capacity.PublicationUnknown):capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertEqual(json.loads(self.path.read_bytes())['max_state_bytes'],16*1024**3)

    def test_cli_errors_keep_both_authorization_flags_false(self):
        """Project explicit false authorization flags for refusal and evidence failures."""
        for path,size in ((self.path,8*1024**3),(self.root/'missing'/'policy.json',16*1024**3)):
            with self.subTest(path=path):
                result=subprocess.run([sys.executable,str(Path(__file__).with_name('native-capacity-policy.py')),
                    '--policy',str(path),'--max-state-bytes',str(size)],capture_output=True,text=True)
                self.assertIn(result.returncode,(3,4),result.stderr)
                data=json.loads(result.stdout)
                self.assertIs(data['execution_authorized'],False)
                self.assertIs(data['binding_migration_authorized'],False)

    def test_real_cli_apply_and_replay_report_confirmed_outcomes(self):
        """Exercise argument forwarding and success projections through the actual CLI."""
        p=capacity.update(self.path,16*1024**3)
        args=[sys.executable,str(Path(__file__).with_name('native-capacity-policy.py')),
            '--policy',str(self.path),'--max-state-bytes',str(16*1024**3),'--apply',
            '--expected-source-sha256',p['source_sha256'],'--expected-candidate-sha256',p['candidate_sha256']]
        for reason,applied in (('capacity-policy-applied',True),('already-applied',False)):
            result=subprocess.run(args,capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
            data=json.loads(result.stdout)
            self.assertEqual(data['reason'],reason);self.assertIs(data['policy_applied'],applied)
            self.assertIs(data['execution_authorized'],False);self.assertIs(data['binding_migration_authorized'],False)

    def test_lost_rename_and_cleanup_errors_preserve_unknown_cli_result(self):
        """Keep a possibly committed publication unknown despite failed temporary cleanup."""
        p=capacity.update(self.path,16*1024**3);replace=os.replace;original_stat=os.stat
        def lose_ack(*args,**kwargs):
            """Commit the rename and lose its acknowledgement."""
            replace(*args,**kwargs);raise OSError(errno.EIO,'fixture')
        def fail_cleanup(path,*args,**kwargs):
            """Fail only the cleanup lookup of the now absent temporary pathname."""
            if str(path).startswith('.capacity-policy-'):
                try:return original_stat(path,*args,**kwargs)
                except FileNotFoundError:raise OSError(errno.EIO,'cleanup fixture') from None
            return original_stat(path,*args,**kwargs)
        args=['native-capacity-policy','--policy',str(self.path),'--max-state-bytes',str(16*1024**3),'--apply',
            '--expected-source-sha256',p['source_sha256'],'--expected-candidate-sha256',p['candidate_sha256']]
        with mock.patch.object(sys,'argv',args),mock.patch.object(sys,'stdout',new_callable=io.StringIO) as output,\
            mock.patch.object(capacity.os,'replace',side_effect=lose_ack),mock.patch.object(capacity.os,'stat',side_effect=fail_cleanup):
            self.assertEqual(capacity.main(),4)
            data=json.loads(output.getvalue())
        self.assertEqual(data['policy_applied'],'unknown')
        self.assertIs(data['execution_authorized'],False);self.assertIs(data['binding_migration_authorized'],False)
        self.assertEqual(json.loads(self.path.read_bytes())['max_state_bytes'],16*1024**3)

    def test_descriptor_close_errors_do_not_override_confirmed_publication(self):
        """Keep synced publication results when descriptor release reports an error."""
        p=capacity.update(self.path,16*1024**3);close=os.close
        def close_with_error(fd):
            """Release the real descriptor before injecting a cleanup error."""
            close(fd);raise OSError(errno.EIO,'close fixture')
        with mock.patch.object(capacity.os,'close',side_effect=close_with_error):
            first=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
            second=capacity.update(self.path,16*1024**3,apply=True,expected_source=p['source_sha256'],expected_candidate=p['candidate_sha256'])
        self.assertTrue(first['policy_applied']);self.assertTrue(second['already_applied'])


if __name__=='__main__':unittest.main()
