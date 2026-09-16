"""Real Linux confinement probes using only synthetic files and processes."""
import importlib.util
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

# Hosts that cannot enforce confinement (for example hosted CI kernels older
# than Linux 6.12) skip with the reason; supported hosts still run every probe.
CAPABILITIES_SPEC = importlib.util.spec_from_file_location(
    'host_capabilities', Path(__file__).with_name('host_capabilities.py'))
capabilities = importlib.util.module_from_spec(CAPABILITIES_SPEC)
CAPABILITIES_SPEC.loader.exec_module(capabilities)
SANDBOX_UNAVAILABLE = capabilities.sandbox_unavailable_reason()


@unittest.skipIf(SANDBOX_UNAVAILABLE, SANDBOX_UNAVAILABLE or 'sandbox available')
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

    def test_hard_link_inside_read_root_is_not_granted(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);workspace=root/'workspace';workspace.mkdir()
            private=root/'private.txt';private.write_text('SYNTHETIC_PRIVATE')
            os.link(private,workspace/'linked.txt')
            (workspace/'plain.txt').write_text('SYNTHETIC_PUBLIC')
            config=root/'policy.json'
            config.write_text(json.dumps({'cwd':str(workspace),'read_roots':[str(workspace)],'write_roots':[]}))
            config.chmod(0o600)
            probe=("import json\nresult={}\nfor name in ('plain.txt','linked.txt'):\n"
                   " try: result[name]=open(name).read()\n except PermissionError: result[name]='DENIED'\n"
                   "print(json.dumps(result))")
            run=subprocess.run([sys.executable,'-I',str(SCRIPT),str(config),PROBE,'-c',probe],
                capture_output=True,text=True,timeout=10)
            self.assertEqual(run.returncode,0,run.stderr)
            self.assertEqual(json.loads(run.stdout),{'plain.txt':'SYNTHETIC_PUBLIC','linked.txt':'DENIED'})

    def test_file_verification_helper_accepts_only_single_link_regular_files(self):
        import importlib.util
        spec=importlib.util.spec_from_file_location('sandbox',SCRIPT)
        sandbox=importlib.util.module_from_spec(spec);spec.loader.exec_module(sandbox)
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            (root/'plain.txt').write_text('SYNTHETIC')
            (root/'target.txt').write_text('SYNTHETIC')
            os.link(root/'target.txt',root/'linked.txt')
            os.mkfifo(root/'fifo')
            (root/'directory').mkdir()
            expected={'plain.txt':True,'linked.txt':False,'target.txt':False,'fifo':False,'directory':False}
            for name,accepted in expected.items():
                descriptor=os.open(root/name,os.O_PATH|os.O_NOFOLLOW)
                try:
                    with self.subTest(name=name):
                        info=sandbox.verify_regular_private_file(descriptor)
                        self.assertEqual(info is not None,accepted)
                        self.assertEqual(sandbox.regular_private_file(os.fstat(descriptor)),accepted)
                finally:
                    os.close(descriptor)

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
