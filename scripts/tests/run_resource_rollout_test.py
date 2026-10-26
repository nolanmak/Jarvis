"""Render rollout against fake systemd and a disposable HOME, never production."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT=Path(__file__).parents[1]/'configure-run-resources.sh'


class RolloutTests(unittest.TestCase):
    def test_install_preserves_execstart_and_aggregate_caps_and_uses_normal_restart(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);binary=root/'bin';binary.mkdir();log=root/'calls'
            (binary/'systemd').write_text('#!/bin/sh\necho "systemd 255"\n')
            (binary/'systemctl').write_text('''#!/bin/sh
printf '%s\\n' "$*" >> "$TEST_CALLS"
case "$*" in
 *MemoryMax*) echo 10737418240;;
 *TasksMax*) echo 2048;;
 *DelegateSubgroup*) echo daemon;;
 *Delegate*) echo yes;;
esac
''')
            for file in binary.iterdir():file.chmod(0o755)
            env=dict(os.environ,HOME=str(root),XDG_CONFIG_HOME=str(root/'config'),PATH=str(binary)+':/usr/bin:/bin',TEST_CALLS=str(log))
            result=subprocess.run(['bash',str(SCRIPT),'--install'],env=env,capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
            dropin=root/'config/systemd/user/augmentagent.service.d/60-run-resources.conf'
            text=dropin.read_text()
            self.assertIn('DelegateSubgroup=daemon',text)
            self.assertNotIn('ExecStart',text)
            self.assertNotIn('MemoryMax=',text)
            self.assertNotIn('TasksMax=',text)
            self.assertIn('daemon-reload',log.read_text())
            self.assertNotIn('restart',log.read_text())
            # An ordinary update preserves operator-tuned per-run ceilings.
            dropin.write_text(text.replace('CPUQuota=400%','CPUQuota=300%'))
            subprocess.run(['bash',str(SCRIPT),'--install'],env=env,check=True,capture_output=True)
            self.assertIn('CPUQuota=300%',dropin.read_text())
            result=subprocess.run(['bash',str(SCRIPT),'--apply'],env=env,capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertIn('restart augmentagent.service',log.read_text())


if __name__=='__main__':unittest.main(verbosity=2)
