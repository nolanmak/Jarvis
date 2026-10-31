"""Render units in an isolated home; never touches the host service manager."""
import os
import json
import hashlib
import shlex
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]

class InstallTests(unittest.TestCase):
    def test_preserves_configuration_and_orders_first_migration(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            repo = base / 'repo fixture'
            (repo / 'scripts/systemd').mkdir(parents=True)
            for name in ('ensure-autopr-worker.sh', 'systemd/augmentagent-autopr.service'):
                shutil.copy(ROOT / 'scripts' / name, repo / 'scripts' / name)
            runner=repo/'scripts/run-rs.sh'
            runner.write_text('#!/bin/sh\nexit 0\n');runner.chmod(0o700)
            release=repo/'target/release';release.mkdir(parents=True)
            artifacts=[release/name for name in ('augmentagent','augmentagent-mcp-memory')]
            for artifact in artifacts:
                artifact.write_text('#!/bin/sh\nexit 0\n');artifact.chmod(0o775)
            # Cargo's public artifact and deps entry share the executable inode.
            os.link(artifacts[0],release/'cargo-deps-link')
            binary = base / 'bin'; binary.mkdir()
            stub = binary / 'systemctl'
            stub.write_text('''#!/usr/bin/env python3
import os,sys
if os.environ.get('FAIL_MANAGER') in sys.argv:sys.exit(1)
with open(os.environ['CALLS'],'a') as f:f.write(' '.join(sys.argv[1:])+'\\n')
if 'show' in sys.argv:
 key=sys.argv[sys.argv.index('-p')+1]
 print({'Environment':'PATH=/usr/bin RUST_LOG=warn SYNTHETIC=fixture',
 'EnvironmentFiles':os.environ.get('FIXTURE_ENVIRONMENT_FILES','/fixture/config (ignore_errors=yes) /fixture/required config% (ignore_errors=no) /fixture/pattern*.env (ignore_errors=yes)'),
 'ExecStart':'{ argv[]=/fixture/bin serve --dry-run false ; }',
 'MemoryHigh':'8589934592','MemoryMax':'10737418240',
 'TasksMax':'2048','LimitNOFILE':'4096','CPUQuotaPerSecUSec':'4s',
 'Delegate':'yes','DelegateSubgroup':'daemon'}.get(key,''))
''')
            stub.chmod(0o700)
            env = dict(os.environ, PATH=str(binary)+os.pathsep+os.environ['PATH'],
                       XDG_CONFIG_HOME=str(base/'config fixture%[prod]*?\\folder'), XDG_STATE_HOME=str(base/'state'),
                       CALLS=str(base/'calls'))
            command = ['bash', str(repo/'scripts/ensure-autopr-worker.sh')]
            subprocess.run(command, env=env, check=True)
            self.assertEqual([p.stat().st_mode & 0o777 for p in artifacts],[0o755,0o755],
                'normal worker installation must make cached Cargo executables pass deployment trust checks')
            self.assertEqual((release/'cargo-deps-link').stat().st_mode & 0o777,0o755)
            units = base/'config fixture%[prod]*?\\folder/systemd/user'
            worker = (units/'augmentagent-autopr.service').read_text()
            self.assertIn('--dry-run false', worker)
            self.assertIn('Environment="SYNTHETIC=fixture"', worker)
            self.assertIn('EnvironmentFile=-/fixture/config', worker)
            self.assertIn('EnvironmentFile=/fixture/required config%%',worker)
            self.assertIn('EnvironmentFile=-/fixture/pattern*.env',worker)
            self.assertLess(worker.index('EnvironmentFile=-/fixture/config'),
                            worker.index('EnvironmentFile=/fixture/required config%%'))
            self.assertIn('After=network-online.target augmentagent.service', worker)
            self.assertNotIn('PartOf=', worker)
            self.assertNotIn('BindsTo=', worker)
            self.assertIn('MemoryMax=10737418240', (units/'augmentagent-runtime.slice').read_text())
            self.assertIn('Slice=augmentagent-runtime.slice',worker)
            self.assertIn('DelegateSubgroup=daemon',worker)
            self.assertIn('Delegate=cpu memory pids',worker)
            self.assertIn('MemoryMax=10737418240',worker)
            self.assertIn('CPUQuota=400%',worker)
            self.assertIn('CPUQuota=400%',(units/'augmentagent-runtime.slice').read_text())
            dropin = units/'augmentagent.service.d/autoship-worker.conf'
            self.assertIn('AUGMENTAGENT_AUTOPR_EXTERNAL=1',dropin.read_text())
            self.assertNotIn('ExecStart=',dropin.read_text())
            before = (units/'augmentagent-autopr.service').stat().st_mtime_ns
            subprocess.run(command, env=env, check=True)
            self.assertEqual(before,(units/'augmentagent-autopr.service').stat().st_mtime_ns)
            self.assertNotIn('start ',(base/'calls').read_text())
            self.assertNotIn('--now',(base/'calls').read_text())
            if shutil.which('systemd-analyze'):
                # Offline parser validation; this neither reloads nor starts
                # any host unit. Supply the daemon dependency as a fixture.
                (units/'augmentagent.service').write_text('[Service]\nExecStart=/usr/bin/true\n')
                verify=subprocess.run(['systemd-analyze','--user','verify',str(units/'augmentagent-autopr.service')],
                    env=dict(env,SYSTEMD_UNIT_PATH=str(units)+':'),capture_output=True,text=True)
                self.assertEqual(verify.returncode,0,verify.stderr)
                self.assertNotIn('EnvironmentFile= path is not absolute',verify.stderr,
                    'systemd must accept every generated environment file path')
            # Explicit install dry-run wins; configuration cannot silently
            # enable write actions when reinstalling a dry-run daemon.
            subprocess.run(command, env=dict(env,AUGMENTAGENT_AUTOSTART_DRY_RUN='true'),check=True)
            self.assertIn('--dry-run true',(units/'augmentagent-autopr.service').read_text())
            # Explicit policy selection persists independently of installer env,
            # and both service processes receive the same final environment file.
            policy=base/'private profile.json'
            policy.write_text(json.dumps({'version':1,'providers':{'claude':{'refresh_auth':False},'codex':{'refresh_auth':False}}}))
            policy.chmod(0o600)
            original=hashlib.sha256(policy.read_bytes()).hexdigest()
            subprocess.run(command,env=dict(env,AUGMENTAGENT_NATIVE_VM_POLICY=str(policy)),check=True)
            selection=base/'config fixture%[prod]*?\\folder/augmentagent/autoship-native.env'
            self.assertTrue(selection.exists(),'explicit native profile selection must persist')
            self.assertEqual(shlex.split(selection.read_text().splitlines()[-1]),['AUGMENTAGENT_NATIVE_VM_POLICY='+str(policy)])
            self.assertEqual(selection.stat().st_mode & 0o777,0o600)
            import ctypes
            pattern=next(line.removeprefix('EnvironmentFile=-') for line in (units/'augmentagent-autopr.service').read_text().splitlines() if line.startswith('EnvironmentFile=-') and 'autoship-native.env' in line).replace('%%','%')
            fnmatch=ctypes.CDLL(None).fnmatch
            fnmatch.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_int]
            self.assertEqual(fnmatch(os.fsencode(pattern),os.fsencode(selection),0),0,
                             'shared policy path must glob-match its literal filename')
            self.assertNotEqual(fnmatch(os.fsencode(pattern),
                os.fsencode(str(selection).replace('*?','ANYTHINGz')),0),0,
                'literal policy path must not glob-match a neighboring filename')
            shared='EnvironmentFile=-'+pattern.replace('%','%%')
            self.assertIn(shared,dropin.read_text())
            prior=selection.read_bytes()
            alternate=base/'alternate.json'
            alternate.write_bytes(policy.read_bytes());alternate.chmod(0o600)
            watched=[selection,units/'augmentagent-autopr.service',units/'augmentagent-runtime.slice',dropin]
            before_failed={p:p.read_bytes() for p in watched}
            for failure in ({'AUGMENTAGENT_AUTOSTART_DRY_RUN':'invalid'},
                            {'AUGMENTAGENT_UNIT_MEMORY_MAX':'invalid'},
                            {'FAIL_MANAGER':'daemon-reload'},{'FAIL_MANAGER':'enable'},
                            {'FIXTURE_ENVIRONMENT_FILES':'/fixture/trailing'+chr(92)+' (ignore_errors=yes)'}):
                rejected=subprocess.run(command,env=dict(env,AUGMENTAGENT_NATIVE_VM_POLICY=str(alternate),**failure),capture_output=True,text=True)
                self.assertNotEqual(rejected.returncode,0)
                self.assertEqual(before_failed,{p:p.read_bytes() for p in watched},'failed install must restore every managed file')
            subprocess.run(command,env=env,check=True)
            self.assertEqual(selection.read_bytes(),prior)
            self.assertEqual(hashlib.sha256(policy.read_bytes()).hexdigest(),original,'installer must not enlarge credential grants')
            policy.chmod(0o644)
            rejected=subprocess.run(command,env=dict(env,AUGMENTAGENT_NATIVE_VM_POLICY=str(policy)),capture_output=True,text=True)
            self.assertNotEqual(rejected.returncode,0,'unsafe explicit profile must fail before replacing selection')
            self.assertEqual(selection.read_bytes(),prior)
            policy.chmod(0o600)
            subprocess.run(['bash',str(ROOT/'scripts/uninstall-autostart.sh')],env=env,check=True,capture_output=True)
            self.assertFalse((units/'augmentagent-autopr.service').exists())
            self.assertFalse((units/'augmentagent-runtime.slice').exists())
            self.assertFalse(dropin.exists())
            self.assertIn('disable --now augmentagent-autopr.service',(base/'calls').read_text())
            subprocess.run(command,env=env,check=True)
            (units/'augmentagent-autopr.service').write_text('# user managed\n')
            result=subprocess.run(command,env=env,capture_output=True,text=True)
            self.assertNotEqual(result.returncode,0)
            self.assertIn('preserving unmanaged unit',result.stderr)

if __name__ == '__main__': unittest.main()
