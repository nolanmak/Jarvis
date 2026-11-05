"""Run the real updater for a script-only pull using disposable external tools."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]

@unittest.skipUnless(sys.platform=='linux','Linux worker provisioning')
class WorkerProvisionTests(unittest.TestCase):
    def test_script_only_update_repairs_cached_release_modes_without_restarting_daemon(self):
        self.run_update(False)

    def test_failed_cached_artifact_validation_withholds_stamp_and_services(self):
        self.run_update(True)

    def test_failed_provisioning_retries_on_unchanged_head_without_rebuild(self):
        self.run_update(False,manager_failure=True)

    def test_unchanged_tick_runs_worker_recovery_and_reports_failure_without_rebuild(self):
        self.run_update(False,verify_recovery=True)

    def run_update(self,invalid,manager_failure=False,verify_recovery=False):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            for name in ['check-for-updates.sh','lib/service-restart.sh','lib/update-build.sh',
                         'ensure-autopr-worker.sh','secure-release-executables.py','systemd/augmentagent-autopr.service',
                         'recover-autopr-worker.py']:
                target=root/'scripts'/name;target.parent.mkdir(parents=True,exist_ok=True)
                shutil.copy(ROOT/'scripts'/name,target)
            if verify_recovery:
                # The caller is the real updater; the separately tested recovery
                # boundary stands in for a failed systemd worker in this fixture.
                (root/'scripts/recover-autopr-worker.py').write_text('''import os,sys
from pathlib import Path
assert sys.argv[1:]==['--repository',str(Path(__file__).resolve().parents[1]),'--state',str(Path(os.environ['XDG_STATE_HOME'])/'augmentagent')]
with open(os.environ['CALLS'],'a') as f:f.write('worker-recovery\\n')
raise SystemExit(1 if os.environ.get('FAIL_RECOVERY') else 0)
''')
            tools=root/'bin';tools.mkdir();release=root/'target/release';release.mkdir(parents=True)
            cli=release/'augmentagent'
            cli.write_text('''#!/bin/sh
if [ "$(( 0$(stat -c %a "$0") & 022 ))" != 0 ]; then echo unsafe-cli >> "$CALLS"; fi
echo "cli $*" >> "$CALLS"
if [ "$1 $2" = 'auto-pr-worker --help' ]; then echo 'Usage: augmentagent auto-pr-worker'; fi
exit 0
''');cli.chmod(0o775)
            memory=release/'augmentagent-mcp-memory';memory.write_text('#!/bin/sh\nexit 0\n');memory.chmod(0o775)
            git=tools/'git';git.write_text('''#!/bin/sh
case "$1" in
rev-parse) if [ "$2" = HEAD ] && [ ! -f "$HOME/pulled" ]; then echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; else echo bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb; fi ;;
merge-base) [ "$3" = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ] ;;
diff) echo scripts/ensure-autopr-worker.sh ;;
pull) touch "$HOME/pulled" ;;
fetch) exit 0 ;;
*) exit 97 ;;
esac
''');git.chmod(0o700)
            manager=tools/'systemctl';manager.write_text('''#!/usr/bin/env python3
import os,sys
with open(os.environ['CALLS'],'a') as f:f.write('systemctl '+' '.join(sys.argv[1:])+'\\n')
if os.environ.get('FAIL_MANAGER') in sys.argv:sys.exit(1)
if 'show' in sys.argv:
 key=sys.argv[sys.argv.index('-p')+1]
 print({'ExecStart':'{ argv[]=/fixture/serve --dry-run false ; }'}.get(key,''))
''');manager.chmod(0o700)
            cargo=tools/'cargo';cargo.write_text('#!/bin/sh\necho forbidden-cargo >> "$CALLS"\nexit 99\n');cargo.chmod(0o700)
            calls=root/'calls';calls.touch()
            env={'HOME':str(root),'XDG_STATE_HOME':str(root/'state'),'XDG_CONFIG_HOME':str(root/'config'),
                 'PATH':str(tools)+':/usr/bin:/bin','CALLS':str(calls)}
            stamp=root/'state/augmentagent/built-commit';stamp.parent.mkdir(parents=True)
            stamp.write_text('a'*40+'\n')
            if invalid:memory.chmod(0o600)
            if manager_failure:env['FAIL_MANAGER']='enable' 
            result=subprocess.run(['bash',str(root/'scripts/check-for-updates.sh')],env=env,capture_output=True,text=True,timeout=30)
            if invalid:
                self.assertNotEqual(result.returncode,0)
                self.assertEqual(stamp.read_text().strip(),'a'*40)
                self.assertEqual(cli.stat().st_mode & 0o777,0o775)
                self.assertEqual(calls.read_text(),'','invalid artifacts must fail before any CLI or service call')
                self.assertNotIn('restart augmentagent.service',calls.read_text())
                return
            pending=stamp.parent/'autopr-provision-pending'
            if manager_failure:
                self.assertNotEqual(result.returncode,0)
                self.assertEqual(stamp.read_text().strip(),'b'*40,
                                 'provisioning must not invalidate a completed script-only deploy')
                self.assertTrue(pending.is_file())
                # A second tick sees HEAD==origin/main, with the same failure.
                retry=subprocess.run(['bash',str(root/'scripts/check-for-updates.sh')],env=env,capture_output=True,text=True,timeout=30)
                self.assertNotEqual(retry.returncode,0)
                self.assertTrue(pending.is_file())
                del env['FAIL_MANAGER']
                result=subprocess.run(['bash',str(root/'scripts/check-for-updates.sh')],env=env,capture_output=True,text=True,timeout=30)
                self.assertFalse(pending.exists(),'successful unchanged-head reconciliation must clear retry state')
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual((root/'state/augmentagent/built-commit').read_text().strip(),'b'*40)
            self.assertEqual([p.stat().st_mode & 0o777 for p in [cli,memory]],[0o755,0o755],
                             'script-only updates skipped worker provisioning and left executables untrusted')
            text=calls.read_text()
            self.assertIn('enable augmentagent-autopr.service',text)
            self.assertNotIn('restart augmentagent.service',text)
            self.assertNotIn('forbidden-cargo',text)
            self.assertNotIn('unsafe-cli',text,'a CLI preflight executed a writable artifact')
            if verify_recovery:
                retry=subprocess.run(['bash',str(root/'scripts/check-for-updates.sh')],env=env,capture_output=True,text=True,timeout=30)
                self.assertEqual(retry.returncode,0,retry.stderr)
                self.assertIn('worker-recovery',calls.read_text(),'unchanged build skipped independent worker recovery')
                env['FAIL_RECOVERY']='1'
                failed=subprocess.run(['bash',str(root/'scripts/check-for-updates.sh')],env=env,capture_output=True,text=True,timeout=30)
                self.assertNotEqual(failed.returncode,0,'failed recovery was reported as a healthy update')
                self.assertEqual(stamp.read_text().strip(),'b'*40)
                self.assertNotIn('restart augmentagent.service',calls.read_text())
                self.assertNotIn('forbidden-cargo',calls.read_text())

if __name__=='__main__':unittest.main()
