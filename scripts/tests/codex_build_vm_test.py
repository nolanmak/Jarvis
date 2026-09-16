"""Real guest isolation contracts. Runtime paths come only from private test configuration."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import time
import unittest

SPEC = importlib.util.spec_from_file_location('build_vm', Path(__file__).parents[1] / 'codex-build-vm.py')
vm = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(vm)


class SnapshotValidationTests(unittest.TestCase):
    def test_host_hard_links_are_rejected_before_a_vm_starts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); workspace = root / 'workspace'; workspace.mkdir()
            secret = root / 'private.txt'; secret.write_text('SYNTHETIC_PRIVATE')
            os.link(secret, workspace / 'alias.txt')
            with self.assertRaisesRegex(vm.Unavailable, 'hard links'):
                vm.run(None, workspace, ['true'], {})
            self.assertEqual(secret.read_text(), 'SYNTHETIC_PRIVATE')


@unittest.skipUnless(os.environ.get('JARVIS_TEST_VM_CONFIG'), 'requires a provisioned private KVM runtime')
class BuildVmTests(unittest.TestCase):
    def test_cargo_compiles_and_runs_tests_with_loopback_and_process_sessions(self):
        runtime = vm.Runtime.load(Path(os.environ['JARVIS_TEST_VM_CONFIG']))
        self.assertIn('toolchain', runtime.config, 'provision Rust for the build contract')
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary) / 'workspace'; workspace.mkdir()
            (workspace / 'src').mkdir()
            (workspace / 'Cargo.toml').write_text('[package]\nname="synthetic-vm-fixture"\nversion="0.1.0"\nedition="2021"\n')
            source = '''#[test]
fn guest_runtime_supports_normal_tests() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let _server = listener.accept().unwrap();
    drop(client);
    assert!(std::process::Command::new("setsid").arg("true").status().unwrap().success());
}
'''
            (workspace / 'src/lib.rs').write_text(source)
            result = vm.run(runtime, workspace, ['cargo', 'test', '--offline'], {}, timeout=30)
            self.assertEqual(result['exit_code'], 0, result)
            self.assertIn('1 passed', result['stdout'])
            self.assertEqual((workspace / 'src/lib.rs').read_text(), source)

    def test_guest_supports_loopback_sessions_and_scoped_writes_without_host_access(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / 'workspace'; workspace.mkdir()
            secret = root / 'private.txt'; secret.write_text('SYNTHETIC_PRIVATE')
            readonly = root / 'readonly'; readonly.mkdir()
            (workspace / 'escape').symlink_to(secret)
            (workspace / 'probe.py').write_text('''import json, os, socket, subprocess
from pathlib import Path
server=socket.socket(); server.bind(('127.0.0.1',0)); server.listen(1)
client=socket.create_connection(server.getsockname()); accepted,_=server.accept()
client.sendall(b'synthetic'); assert accepted.recv(9)==b'synthetic'
subprocess.run(['/usr/bin/setsid','/usr/bin/true'],check=True)
Path('result.txt').write_text('SYNTHETIC_GUEST_WRITE')
result={'interfaces':sorted(p.name for p in Path('/sys/class/net').iterdir()),
        'uid':os.getuid(),'secret_env':os.environ.get('SYNTHETIC_HOST_SECRET'),
        'escaped':Path('escape').exists(),'control_readable':os.access('/root/control/outcome.json',os.R_OK)}
result['no_new_privileges']='NoNewPrivs:\t1' in Path('/proc/self/status').read_text()
try:
 Path('/cargo/registry/denied.txt').write_text('bad')
 result['cache_write']=True
except OSError:
 result['cache_write']=False
try:
 Path('/root/control/outcome.json').write_text('forged')
 result['forged_receipt']=True
except PermissionError:
 result['forged_receipt']=False
print(json.dumps(result))
''')
            runtime = vm.Runtime.load(Path(os.environ['JARVIS_TEST_VM_CONFIG']))
            runtime.config['registry'] = str(readonly)
            result = vm.run(runtime, workspace, ['/usr/bin/python3', 'probe.py'], {}, timeout=20)
            self.assertEqual(result['exit_code'], 0, result)
            observed = json.loads(result['stdout'])
            self.assertEqual(observed['interfaces'], ['lo'])
            self.assertNotEqual(observed['uid'], 0)
            self.assertIsNone(observed['secret_env'])
            self.assertFalse(observed['escaped'])
            self.assertFalse(observed['control_readable'])
            self.assertFalse(observed['forged_receipt'])
            self.assertTrue(observed['no_new_privileges'])
            self.assertFalse(observed['cache_write'])
            self.assertFalse((readonly / 'denied.txt').exists())
            self.assertEqual((workspace / 'result.txt').read_text(), 'SYNTHETIC_GUEST_WRITE')
            self.assertEqual(secret.read_text(), 'SYNTHETIC_PRIVATE')

    def test_exit_status_is_from_guest_command_and_deadline_stops_vm(self):
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary) / 'workspace'; workspace.mkdir()
            runtime = vm.Runtime.load(Path(os.environ['JARVIS_TEST_VM_CONFIG']))
            result = vm.run(runtime, workspace, ['/usr/bin/python3', '-c', 'raise SystemExit(17)'], {}, timeout=20)
            self.assertEqual(result['exit_code'], 17)
            with self.assertRaisesRegex(vm.Unavailable, 'timed out'):
                vm.run(runtime, workspace, ['/usr/bin/sleep', '60'], {}, timeout=2)

    def test_timeout_prevents_later_background_writes(self):
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary) / 'workspace'; workspace.mkdir()
            runtime = vm.Runtime.load(Path(os.environ['JARVIS_TEST_VM_CONFIG']))
            program = "import os,time; os.setsid(); open('ready.txt','w').write('ready'); time.sleep(5); open('late.txt','w').write('late')"
            with self.assertRaisesRegex(vm.Unavailable, 'timed out'):
                vm.run(runtime, workspace, ['/usr/bin/python3', '-c',
                    "import subprocess,time; subprocess.Popen(['/usr/bin/python3','-c'," + repr(program) + "]); time.sleep(60)"],
                    {}, timeout=3)
            self.assertTrue((workspace / 'ready.txt').exists(), 'probe must reach its detached child before timeout')
            time.sleep(4)
            self.assertFalse((workspace / 'late.txt').exists())


if __name__ == '__main__':
    unittest.main()
