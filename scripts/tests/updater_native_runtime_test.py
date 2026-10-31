"""Run the real updater against a local Git remote; never touch host services."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]

class UpdaterRuntimeTests(unittest.TestCase):
    def test_reconciliation_runs_even_when_git_and_build_stamp_are_current(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);repo=root/'repo';repo.mkdir();origin=root/'origin.git'
            def git(*args,cwd=repo):
                return subprocess.check_output(['git',*args],cwd=cwd,text=True,stderr=subprocess.DEVNULL).strip()
            git('init','--bare',str(origin));git('init','-b','main')
            (repo/'scripts/lib').mkdir(parents=True)
            for source in ['check-for-updates.sh','lib/service-restart.sh','lib/update-build.sh']:
                shutil.copyfile(ROOT/'scripts'/source,repo/'scripts'/source)
            (repo/'scripts/ensure-native-runtime.py').write_text("import os,pathlib,sys\np=pathlib.Path(os.environ['CALLS']);p.write_text(p.read_text()+'called\\n' if p.exists() else 'called\\n')\nsys.exit(int(os.environ.get('RECONCILE_EXIT','0')))\n")
            git('add','.');git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-m','fixture')
            git('remote','add','origin',str(origin));git('push','origin','main');head=git('rev-parse','HEAD')
            state=root/'state/augmentagent';state.mkdir(parents=True);stamp=state/'built-commit';stamp.write_text(head+'\n')
            binary=root/'bin';binary.mkdir();(binary/'systemctl').write_text('#!/bin/sh\nexit 0\n');(binary/'systemctl').chmod(0o700)
            calls=root/'calls';env=dict(os.environ,PATH=str(binary)+os.pathsep+os.environ['PATH'],HOME=str(root),XDG_STATE_HOME=str(root/'state'),XDG_CONFIG_HOME=str(root/'config'),
                AUGMENTAGENT_GATE_TARGET_DIR=str(root/'absent-gate'),CALLS=str(calls))
            env.pop('DISCORD_WEBHOOK_URL',None)
            for code in [0,1]:
                result=subprocess.run(['bash',str(repo/'scripts/check-for-updates.sh')],env=dict(env,RECONCILE_EXIT=str(code)),capture_output=True,text=True)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertTrue(calls.exists(),'runtime reconciliation must run on the unchanged-revision path')
                self.assertEqual(stamp.read_text(),head+'\n')
            self.assertEqual(calls.read_text().splitlines(),['called','called'])
            self.assertIn('NATIVE RUNTIME RECONCILIATION FAILED',(state/'update.log').read_text())
            # A script/documentation-only pull runs the newly pulled helper too,
            # without depending on a Rust build or service restart.
            (repo/'doc.txt').write_text('new documentation')
            git('add','doc.txt');git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-m','docs')
            updated=git('rev-parse','HEAD');git('push','origin','main');git('reset','--hard',head)
            result=subprocess.run(['bash',str(repo/'scripts/check-for-updates.sh')],env=env,capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual(stamp.read_text(),updated+'\n')
            self.assertEqual(calls.read_text().splitlines(),['called']*4)


if __name__=='__main__':unittest.main()
