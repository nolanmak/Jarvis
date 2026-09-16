"""Real Linux confinement probes using only synthetic files and processes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).parents[1] / 'codex-command-sandbox.py'
# The probe itself must reside under the sandbox's trusted executable roots.
# The interpreter running this test may instead live in a private virtualenv.
PROBE = next((str(path) for candidate in ('/usr/bin/python3', '/bin/python3', sys.executable)
              if (path := Path(candidate).resolve()).is_file() and os.access(path, os.X_OK)
              and any(path.is_relative_to(root) for root in ('/usr', '/bin', '/lib', '/lib64'))), None)


@unittest.skipIf(PROBE is None, "no Python interpreter under sandbox executable roots")
class SandboxTests(unittest.TestCase):
    def test_allowed_io_succeeds_but_escape_network_and_parent_signal_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);workspace=root/'workspace';workspace.mkdir()
            private=root/'private.txt';private.write_text('SYNTHETIC_PRIVATE')
            (workspace/'input.txt').write_text('ALLOWED')
            (workspace/'escape.txt').symlink_to(private)
            script=workspace/'probe.py'
            script.write_text('''import json,os,socket
from pathlib import Path
result={'read':Path('input.txt').read_text()}
Path('output.txt').write_text('ALLOWED_WRITE')
for key,action in [
 ('outside_read',lambda:Path('../private.txt').read_text()),
 ('symlink_read',lambda:Path('escape.txt').read_text()),
 ('outside_write',lambda:Path('../bad.txt').write_text('bad')),
 ('network',lambda:socket.socket(socket.AF_INET,socket.SOCK_STREAM)),
 ('udp',lambda:socket.socket(socket.AF_INET,socket.SOCK_DGRAM)),
 ('parent_signal',lambda:os.kill(os.getppid(),0))]:
 try: action();result[key]='ALLOWED'
 except PermissionError: result[key]='DENIED'
print(json.dumps(result))
''')
            config=root/'policy.json'
            config.write_text(json.dumps({'cwd':str(workspace),
                'read_roots':[str(workspace)],'write_roots':[str(workspace)]}))
            config.chmod(0o600)
            run=subprocess.run([sys.executable,'-I',str(SCRIPT),str(config),
                                PROBE,'probe.py'],capture_output=True,text=True,timeout=10)
            self.assertEqual(run.returncode,0,run.stderr)
            result=json.loads(run.stdout)
            self.assertEqual(result.pop('read'),'ALLOWED')
            self.assertTrue(all(v=='DENIED' for v in result.values()),result)
            self.assertEqual((workspace/'output.txt').read_text(),'ALLOWED_WRITE')
            self.assertFalse((root/'bad.txt').exists())

    def test_readonly_profile_cannot_write_even_inside_workspace(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);config=root/'policy.json'
            config.write_text(json.dumps({'cwd':tmp,'read_roots':[tmp],'write_roots':[]}));config.chmod(0o600)
            run=subprocess.run([sys.executable,'-I',str(SCRIPT),str(config),
                PROBE,'-c',"open('bad.txt','w').write('bad')"],
                capture_output=True,text=True,timeout=10)
            self.assertNotEqual(run.returncode,0)
            self.assertIn('PermissionError',run.stderr)
            self.assertFalse((root/'bad.txt').exists())


if __name__=='__main__': unittest.main()
