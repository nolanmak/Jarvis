"""Render units in an isolated home; never touches the host service manager."""
import os
import plistlib
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
            for name in ('ensure-autopr-worker.sh','secure-release-executables.py', 'systemd/augmentagent-autopr.service'):
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
            # Render the Linux units on any host OS; macOS has its own test.
            (binary/'uname').write_text('#!/bin/sh\necho Linux\n');(binary/'uname').chmod(0o700)
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
            # Upgrade exits (75) overlapping the daemon restart must not trip
            # systemd's default 5-starts-in-10s limit and leave the worker dead.
            unit_section=worker.split('[Service]')[0]
            self.assertIn('StartLimitIntervalSec=60',unit_section)
            self.assertIn('StartLimitBurst=10',unit_section)
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
            # #2275: the daemon never hosts auto-ship, so the drop-in no
            # longer selects a mode with the deprecated flag.
            self.assertNotIn('AUGMENTAGENT_AUTOPR_EXTERNAL',dropin.read_text())
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
            # Validate both artifacts before changing either mode or units.
            before_units={p:p.read_bytes() for p in [units/'augmentagent-autopr.service',dropin,selection]}
            memory_bytes=artifacts[1].read_bytes()
            foreign=base/'foreign-executable';foreign.write_bytes(memory_bytes);foreign.chmod(0o775)
            for kind in ['symlink','fifo','non-executable']:
                artifacts[0].chmod(0o775);artifacts[1].unlink()
                if kind=='symlink':artifacts[1].symlink_to(foreign)
                elif kind=='fifo':os.mkfifo(artifacts[1])
                else:artifacts[1].write_bytes(memory_bytes);artifacts[1].chmod(0o600)
                rejected=subprocess.run(command,env=env,capture_output=True,text=True,timeout=5)
                self.assertNotEqual(rejected.returncode,0,kind)
                self.assertEqual(artifacts[0].stat().st_mode & 0o777,0o775,'partial executable permission change')
                self.assertEqual(foreign.stat().st_mode & 0o777,0o775,'followed a foreign symlink')
                self.assertEqual(before_units,{p:p.read_bytes() for p in before_units})
            artifacts[1].unlink();artifacts[1].write_bytes(memory_bytes);artifacts[1].chmod(0o755)
            artifacts[0].chmod(0o700)
            subprocess.run(command,env=env,check=True)
            self.assertEqual(artifacts[0].stat().st_mode & 0o777,0o700,'installer widened existing read access')
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

WORKER_LABEL='com.nolanmak.augmentagent.autopr'
DAEMON_LABEL='com.nolanmak.augmentagent'

class MacInstallTests(unittest.TestCase):
    """#2275: macOS runs the same external worker, under a launchd LaunchAgent."""
    def setUp(self):
        self.scratch=tempfile.TemporaryDirectory(prefix='autopr-launchd-')
        self.addCleanup(self.scratch.cleanup)
        base=self.base=Path(self.scratch.name)
        self.repo=repo=base/'repo & <fixture>'
        (repo/'scripts/lib').mkdir(parents=True)
        for name in ('ensure-autopr-worker.sh','secure-release-executables.py','uninstall-autostart.sh',
                     'lib/launchd-install.sh','systemd/augmentagent-autopr.service'):
            (repo/'scripts'/name).parent.mkdir(parents=True,exist_ok=True)
            shutil.copy(ROOT/'scripts'/name,repo/'scripts'/name)
        runner=repo/'scripts/run-rs.sh';runner.write_text('#!/bin/sh\nexit 0\n');runner.chmod(0o755)
        release=repo/'target/release';release.mkdir(parents=True)
        for name in ('augmentagent','augmentagent-mcp-memory'):
            (release/name).write_text('#!/bin/sh\nexit 0\n');(release/name).chmod(0o755)
        self.home=base/'home'
        self.agents=self.home/'Library/LaunchAgents';self.agents.mkdir(parents=True)
        bin_dir=base/'bin';bin_dir.mkdir()
        for name,body in {
            'uname':'echo Darwin\n',
            'systemctl':'echo "$*" >> "$SYSTEMCTL_LOG"\nexit 1\n',
            'launchctl':'printf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\n'
                        'if [ "$1" = print ]; then [ -f "$LAUNCHCTL_LOADED" ]; exit; fi\n'
                        'if [ "$1" = bootout ]; then rm -f "$LAUNCHCTL_LOADED"; exit; fi\n'
                        '[ "$1" = bootstrap ] && touch "$LAUNCHCTL_LOADED"\nexit 0\n',
        }.items():
            (bin_dir/name).write_text('#!/bin/sh\n'+body);(bin_dir/name).chmod(0o755)
        self.calls=base/'launchctl.log';self.systemctl=base/'systemctl.log'
        self.env=dict(os.environ,HOME=str(self.home),PATH=str(bin_dir)+':/usr/bin:/bin',
            XDG_STATE_HOME=str(base/'state'),LAUNCHCTL_LOG=str(self.calls),
            LAUNCHCTL_LOADED=str(base/'loaded'),SYSTEMCTL_LOG=str(self.systemctl))
        self.env.pop('AUGMENTAGENT_AUTOSTART_DRY_RUN',None)
        self.env.pop('AUGMENTAGENT_NATIVE_VM_POLICY',None)
        # The worker mirrors the installed daemon's environment and dry-run,
        # as the Linux unit mirrors augmentagent.service.
        (self.agents/f'{DAEMON_LABEL}.plist').write_bytes(plistlib.dumps({
            'Label':DAEMON_LABEL,'WorkingDirectory':str(repo),
            'ProgramArguments':[str(runner),'--wiki-dir','./wiki','serve','--dry-run','false'],
            'EnvironmentVariables':{'PATH':'/fixture/tools:/usr/bin:/bin','RUST_LOG':'warn',
                                    'HOME':str(self.home),'SYNTHETIC':'fixture'}}))
        self.plist=self.agents/f'{WORKER_LABEL}.plist'

    def ensure(self,**extra):
        return subprocess.run(['bash',str(self.repo/'scripts/ensure-autopr-worker.sh')],
            env=dict(self.env,**extra),capture_output=True,text=True)

    def test_installer_emits_a_launchd_worker_mirroring_the_linux_unit(self):
        result=self.ensure()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertTrue(self.plist.read_text().splitlines()[1].startswith('<!-- Managed by ensure-autopr-worker.sh'),
            'the worker plist must be marked as managed so uninstall and reinstall can own it')
        job=plistlib.loads(self.plist.read_bytes())
        self.assertEqual(job['Label'],WORKER_LABEL)
        self.assertEqual(job['ProgramArguments'],
            [str(self.repo/'scripts/run-rs.sh'),'auto-pr-worker','--dry-run','false'])
        self.assertEqual(job['WorkingDirectory'],str(self.repo))
        environment=job['EnvironmentVariables']
        self.assertEqual(environment['SYNTHETIC'],'fixture')
        self.assertEqual(environment['PATH'],'/fixture/tools:/usr/bin:/bin')
        self.assertEqual(environment['HOME'],str(self.home))
        self.assertNotIn('AUGMENTAGENT_AUTOPR_EXTERNAL',environment)
        logs=self.base/'state/augmentagent'
        self.assertEqual(job['StandardOutPath'],str(logs/'stdout.log'))
        self.assertEqual(job['StandardErrorPath'],str(logs/'stderr.log'))
        # Restart=on-failure + RestartForceExitStatus=75: any non-zero exit
        # (75 = load the updated executable) restarts; a disabled worker's
        # clean exit 0 stays stopped.
        self.assertTrue(job['RunAtLoad'])
        self.assertEqual(job['KeepAlive'],{'SuccessfulExit':False,'Crashed':True})
        self.assertEqual(job['ThrottleInterval'],3)
        self.assertIn(f'bootstrap gui/',self.calls.read_text())
        self.assertFalse(self.systemctl.exists(),'macOS must not call systemctl')
        # An unchanged reinstall (every updater tick) must not interrupt the
        # running worker: no bootout, no bootstrap.
        before=self.calls.read_text()
        result=self.ensure()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertNotIn('bootout',self.calls.read_text()[len(before):])
        self.assertNotIn('bootstrap',self.calls.read_text()[len(before):])
        # An explicit install dry-run wins over the daemon's setting.
        self.assertEqual(self.ensure(AUGMENTAGENT_AUTOSTART_DRY_RUN='true').returncode,0)
        self.assertEqual(plistlib.loads(self.plist.read_bytes())['ProgramArguments'][-1],'true')
        rejected=self.ensure(AUGMENTAGENT_AUTOSTART_DRY_RUN='maybe')
        self.assertNotEqual(rejected.returncode,0)
        self.assertEqual(plistlib.loads(self.plist.read_bytes())['ProgramArguments'][-1],'true')

    def test_without_a_daemon_plist_the_worker_defaults_to_dry_run(self):
        (self.agents/f'{DAEMON_LABEL}.plist').unlink()
        result=self.ensure()
        self.assertEqual(result.returncode,0,result.stderr)
        job=plistlib.loads(self.plist.read_bytes())
        self.assertEqual(job['ProgramArguments'][-2:],['--dry-run','true'])
        self.assertEqual(job['EnvironmentVariables']['RUST_LOG'],'info')

    def test_unmanaged_plist_is_preserved_and_uninstall_removes_the_managed_worker(self):
        self.plist.write_text('<?xml version="1.0"?><plist version="1.0"><dict/></plist>\n')
        result=self.ensure()
        self.assertNotEqual(result.returncode,0)
        self.assertIn('preserving unmanaged',result.stderr)
        self.plist.unlink()
        self.assertEqual(self.ensure().returncode,0)
        result=subprocess.run(['bash',str(self.repo/'scripts/uninstall-autostart.sh')],
            env=self.env,capture_output=True,text=True)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertFalse(self.plist.exists())
        self.assertIn(f'bootout gui/{os.getuid()}/{WORKER_LABEL}',self.calls.read_text())

if __name__ == '__main__': unittest.main()
