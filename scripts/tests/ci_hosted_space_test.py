"""Hosted-only cleanup must never remove software on local/self-hosted replay."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]

class HostedSpaceTests(unittest.TestCase):
    def run_cleanup(self,settings):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);calls=root/'calls'
            sudo=root/'sudo';sudo.write_text('#!/usr/bin/python3\nimport json,os,sys\nwith open(os.environ["CALLS"],"a") as f:f.write(json.dumps(sys.argv[1:])+"\\n")\n');sudo.chmod(0o700)
            env={'PATH':str(root)+':/usr/bin:/bin','CALLS':str(calls),**settings}
            result=subprocess.run(['bash',str(ROOT/'scripts/ci-hosted-space.sh')],env=env,capture_output=True,text=True,timeout=10)
            self.assertEqual(result.returncode,0,result.stderr)
            return [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
    def test_local_self_hosted_and_non_linux_replay_never_remove_software(self):
        for settings in [{},{'GITHUB_ACTIONS':'true','RUNNER_ENVIRONMENT':'self-hosted','RUNNER_OS':'Linux'},
                         {'GITHUB_ACTIONS':'true','RUNNER_ENVIRONMENT':'github-hosted','RUNNER_OS':'macOS'}]:
            with self.subTest(settings=settings):self.assertEqual(self.run_cleanup(settings),[])
    def test_hosted_cleanup_reclaims_unused_compiler_and_analyzer_sdks(self):
        calls=self.run_cleanup({'GITHUB_ACTIONS':'true','RUNNER_ENVIRONMENT':'github-hosted','RUNNER_OS':'Linux'})
        self.assertEqual(len(calls),1)
        self.assertEqual(calls[0][:3],['rm','-rf','--'])
        self.assertIn('/opt/ghc',calls[0])
        self.assertIn('/opt/hostedtoolcache/CodeQL',calls[0])
        self.assertTrue(all(path.startswith(('/usr/','/opt/')) for path in calls[0][3:]))

if __name__=='__main__':unittest.main()
