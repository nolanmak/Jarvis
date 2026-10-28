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
 --version) echo "systemd 255";;
 *CPUQuotaPerSecUSec*) echo 1s;;
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
            self.assertIn('CPUQuota=100%',text)
            self.assertIn('AUGMENTAGENT_RUN_CPU_PERCENT=100',text)
            self.assertNotIn('ExecStart',text)
            self.assertNotIn('MemoryMax=',text)
            self.assertNotIn('TasksMax=',text)
            self.assertIn('daemon-reload',log.read_text())
            self.assertNotIn('restart',log.read_text())
            # An ordinary update preserves operator-tuned per-run ceilings.
            dropin.write_text(text.replace('AUGMENTAGENT_RUN_PIDS=512','AUGMENTAGENT_RUN_PIDS=64'))
            subprocess.run(['bash',str(SCRIPT),'--install'],env=env,check=True,capture_output=True)
            self.assertIn('AUGMENTAGENT_RUN_PIDS=64',dropin.read_text())
            result=subprocess.run(['bash',str(SCRIPT),'--apply'],env=env,capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertIn('restart augmentagent.service',log.read_text())
            env['AUGMENTAGENT_RESOURCE_UNIT']='augmentagent-custom.service'
            subprocess.run(['bash',str(SCRIPT),'--install'],env=env,check=True,capture_output=True)
            self.assertTrue((root/'config/systemd/user/augmentagent-custom.service.d/60-run-resources.conf').exists())
            self.assertIn('show augmentagent-custom.service',log.read_text())

    def test_render_carries_the_text_tier_only_when_an_operator_sets_it(self):
        # #1627 — the text tier is a separate operator knob from the builder tier.
        base=dict(os.environ,AUGMENTAGENT_UNIT_CPU_QUOTA='400%')
        base.pop('AUGMENTAGENT_RUN_MEMORY_BYTES',None);base.pop('AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES',None)
        plain=subprocess.run(['bash',str(SCRIPT),'--render'],env=base,capture_output=True,text=True,check=True).stdout
        self.assertNotIn('AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES',plain)
        self.assertNotIn('AUGMENTAGENT_RUN_MEMORY_BYTES',plain)
        tuned=subprocess.run(['bash',str(SCRIPT),'--render'],env=dict(base,AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES='805306368'),
                             capture_output=True,text=True,check=True).stdout
        self.assertIn('Environment=AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES=805306368',tuned)
        self.assertNotIn('Environment=AUGMENTAGENT_RUN_MEMORY_BYTES=',tuned)
        self.assertNotIn('ExecStart',tuned)

    def test_unsupported_host_does_not_install_or_disable_interactive_service(self):
        for version, memory, enabled, expected in [('252','10737418240','',3),
                ('255','infinity','',3),('unknown','10737418240','',3),
                ('252','10737418240','AUGMENTAGENT_RUN_RESOURCES=1',1)]:
            with self.subTest(version=version,memory=memory,enabled=enabled), tempfile.TemporaryDirectory() as tmp:
                root=Path(tmp);binary=root/'bin';binary.mkdir()
                command=binary/'systemctl'
                command.write_text('#!/bin/sh\ncase "$*" in\n'
                    f' --version) echo "systemd {version}";;\n'
                    f' *MemoryMax*) echo "{memory}";;\n'
                    f' *Environment*) echo "{enabled}";;\n'
                    ' *) exit 91;;\nesac\n')
                command.chmod(0o755)
                env=dict(os.environ,HOME=str(root),XDG_CONFIG_HOME=str(root/'config'),PATH=str(binary)+':/usr/bin:/bin')
                result=subprocess.run(['bash',str(SCRIPT),'--install'],env=env,capture_output=True,text=True)
                self.assertEqual(result.returncode,expected,result.stderr)
                self.assertFalse((root/'config').exists(),'unsupported admission must not mutate units')


if __name__=='__main__':unittest.main(verbosity=2)
