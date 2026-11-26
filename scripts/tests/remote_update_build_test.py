"""Run the updater hook with fake SSH/SCP and real hashes/files; no network/builds."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
NAMES = ('augmentagent', 'augmentagent-mcp-memory')

class RemoteUpdateTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root/'bin'; self.bin.mkdir()
        self.fixture = self.root/'builder'; self.fixture.mkdir()
        self.target = self.root/'target'; (self.target/'release').mkdir(parents=True)
        self.sha = subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip()
        for name in NAMES:
            p=self.target/'release'/name
            p.write_bytes(b'#!/bin/sh\n# old-'+name.encode()+b'\nexit 0\n');p.chmod(0o755)
        self.old = {n:(self.target/'release'/n).read_bytes() for n in NAMES}
        self.cli = self.root/'cli'
        self.cli.write_text('#!/bin/sh\necho local >> "$CALLS"\nexit 0\n'); self.cli.chmod(0o700)
        tool = '''#!/usr/bin/env python3
import json,os,pathlib,shutil,sys
with open(os.environ['CALLS'],'a') as f:f.write(json.dumps([pathlib.Path(sys.argv[0]).name,sys.argv[1:]])+'\\n')
if pathlib.Path(sys.argv[0]).name=='ssh':
 sys.exit(int(os.environ.get('BUILD_STATUS','0')))
if os.environ.get('DOWNLOAD_STATUS'):sys.exit(int(os.environ['DOWNLOAD_STATUS']))
source=sys.argv[-2].split(':',1)[1]
assert source.startswith(os.environ['SHA']+'/'),source
shutil.copyfile(pathlib.Path(os.environ['FIXTURE'])/source.split('/')[-1],sys.argv[-1])
'''
        for name in ('ssh','scp'):
            p=self.bin/name;p.write_text(tool);p.chmod(0o700)
        self.env=dict(os.environ,PATH=str(self.bin)+os.pathsep+os.environ['PATH'],
            CALLS=str(self.root/'calls'),SHA=self.sha,FIXTURE=str(self.fixture),
            AUGMENTAGENT_REMOTE_BUILD_HOST='builder@fixture.invalid',BUILD_STATUS='0')
        self.publish()

    def publish(self, cli=b'#!/bin/sh\necho "augmentagent 0.1.0"\n', memory=b'#!/bin/sh\nexit 0\n'):
        for name,data in zip(NAMES,(cli,memory)):(self.fixture/name).write_bytes(data)
        manifest={'commit':self.sha,'target':'x86_64-unknown-linux-gnu','version':'0.1.0',
            'sha256':{n:hashlib.sha256((self.fixture/n).read_bytes()).hexdigest() for n in NAMES}}
        (self.fixture/'manifest.json').write_text(json.dumps(manifest))

    def run_hook(self, sha=None):
        code='''set -euo pipefail
REPO_ROOT="$1"; LOG="$2/log"
log() { echo "$*" >> "$LOG"; }
. "$REPO_ROOT/scripts/lib/update-build.sh"
run_update_build "$2/cli" "$2/target" "$3"
'''
        return subprocess.run(['bash','-c',code,'fixture',str(ROOT),str(self.root),sha or self.sha],
            env=self.env,capture_output=True,text=True)

    def unchanged(self):
        self.assertEqual({n:(self.target/'release'/n).read_bytes() for n in NAMES},self.old)

    def test_success_pulls_exact_commit_and_atomically_replaces_both(self):
        held=(self.target/'release'/NAMES[0]).open('rb')
        self.addCleanup(held.close)
        result=self.run_hook();self.assertEqual(result.returncode,0,result.stderr)
        for name in NAMES:
            self.assertEqual((self.target/'release'/name).read_bytes(),(self.fixture/name).read_bytes())
            self.assertEqual((self.target/'release'/name).stat().st_mode & 0o777,0o755)
        self.assertEqual(held.read(),self.old[NAMES[0]])
        calls=(self.root/'calls').read_text();self.assertIn('build '+self.sha,calls);self.assertNotIn('local',calls)
        self.assertFalse(list((self.target/'release').glob('.remote-update-*')))

    def test_remote_failures_always_defer_without_local_fallback(self):
        for status in (1,73,75,76,101,255):
            with self.subTest(status=status):
                self.env['BUILD_STATUS']=str(status)
                self.assertEqual(self.run_hook().returncode,75)
                self.unchanged()
        self.assertNotIn('local',(self.root/'calls').read_text())

    def test_download_failure_preserves_previous_pair(self):
        self.env['DOWNLOAD_STATUS']='255'
        self.assertEqual(self.run_hook().returncode,75);self.unchanged()

    def test_corrupt_either_binary_installs_neither(self):
        for name in NAMES:
            with self.subTest(name=name):
                self.publish();(self.fixture/name).write_bytes(b'corrupted')
                self.assertEqual(self.run_hook().returncode,75);self.unchanged()

    def test_wrong_manifest_commit_or_target_defers(self):
        for key,value in [('commit','0'*40),('target','aarch64-unknown-linux-gnu')]:
            self.publish();p=self.fixture/'manifest.json';m=json.loads(p.read_text());m[key]=value;p.write_text(json.dumps(m))
            self.assertEqual(self.run_hook().returncode,75);self.unchanged()

    def test_load_and_version_failures_install_neither(self):
        for cli,mem in [(b'#!/bin/sh\necho wrong-version\n',b'#!/bin/sh\nexit 0\n'),
                        (b'#!/bin/sh\nexit 127\n',b'#!/bin/sh\nexit 0\n'),
                        (b'#!/bin/sh\necho "augmentagent 0.1.0"\n',b'#!/bin/sh\nexit 127\n')]:
            self.publish(cli,mem);self.assertEqual(self.run_hook().returncode,75);self.unchanged()

    def test_invalid_sha_does_not_contact_builder(self):
        self.assertEqual(self.run_hook('main; touch /tmp/invalid').returncode,75)
        self.unchanged();self.assertFalse((self.root/'calls').exists())

    def test_low_memory_remote_path_is_not_blocked_by_local_build_gate(self):
        source=(ROOT/'scripts/check-for-updates.sh').read_text()
        start=source.index('apply_update() {');end=source.index('    log "rust build ok',start)
        # Execute the real build section, returning at its first success marker.
        function=source[start:end]+'    return 0\n  fi\n}\n'
        harness='''set -euo pipefail
log() { :; }; update_source_clean() { return 0; }; memory_pressure_ok() { return 1; }
run_update_build() { printf '%s\\n' "$*" > "$CALLS"; return 75; }
NEEDS_REBUILD=1; DAEMON_BINARY=/fixture/cli
'''+function+'\napply_update "$SHA"\n'
        fake_repo=self.root/'repo';(fake_repo/'scripts').mkdir(parents=True)
        (fake_repo/'scripts/pdf-runtime.py').write_text('')
        env=dict(self.env,REPO_ROOT=str(fake_repo),LOG=str(self.root/'log'))
        r=subprocess.run(['bash','-c',harness],env=env,capture_output=True,text=True)
        self.assertEqual(r.returncode,0,r.stderr)
        self.assertIn(self.sha,(self.root/'calls').read_text())
        (self.root/'calls').unlink()
        env.pop('AUGMENTAGENT_REMOTE_BUILD_HOST')
        r=subprocess.run(['bash','-c',harness],env=env,capture_output=True,text=True)
        self.assertEqual(r.returncode,1,r.stderr)
        self.assertFalse((self.root/'calls').exists())

    def test_concurrent_updater_defers_before_contacting_builder(self):
        import fcntl
        with (self.target/'.remote-update.lock').open('a+b') as held:
            fcntl.flock(held,fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(self.run_hook().returncode,75)
        self.unchanged();self.assertFalse((self.root/'calls').exists())

    def test_install_failure_restores_previous_pair(self):
        spec=importlib.util.spec_from_file_location('remote_client',ROOT/'scripts/remote-builder/client.py')
        module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        stage=self.target/'release'/'.remote-update-test';stage.mkdir()
        for name in NAMES:(stage/name).write_bytes(b'new')
        real=os.replace
        def replace(src,dst):
            if Path(src)==stage/NAMES[1]:raise OSError('injected second rename failure')
            return real(src,dst)
        with mock.patch.object(module.os,'replace',side_effect=replace):
            with self.assertRaises(OSError):module.install(stage,self.target/'release')
        self.unchanged()

class BuilderProtocolTests(unittest.TestCase):
    def setUp(self):
        spec=importlib.util.spec_from_file_location('builder_dispatch',ROOT/'scripts/remote-builder/dispatch.py')
        self.module=importlib.util.module_from_spec(spec);spec.loader.exec_module(self.module)

    def test_forced_command_accepts_only_build_sha(self):
        sha='a'*40
        with mock.patch.object(self.module.os,'execv',side_effect=SystemExit) as execute:
            with self.assertRaises(SystemExit):self.module.dispatch('build '+sha)
            self.assertEqual(execute.call_args.args[1][-1],sha)
        for command in ['bash','build main','build '+sha+'; id','scp -t '+sha+'/augmentagent',
                        'scp -f '+sha+'/../../etc/passwd','internal-sftp','build '+sha+' extra']:
            with self.subTest(command=command):
                with self.assertRaises(ValueError):self.module.dispatch(command)

    def test_scp_can_only_read_an_immutable_named_artifact(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);sha='a'*40;artifact=root/'artifacts'/sha/'augmentagent'
            artifact.parent.mkdir(parents=True);artifact.write_bytes(b'fixture')
            with mock.patch.object(self.module,'ROOT',root), mock.patch.object(self.module.os,'execv',side_effect=SystemExit) as execute:
                with self.assertRaises(SystemExit):self.module.dispatch('scp -f '+sha+'/augmentagent')
                self.assertEqual(execute.call_args.args[1],['scp','-f','--',str(artifact)])
                artifact.unlink();artifact.symlink_to(root/'outside')
                with self.assertRaises(ValueError):self.module.dispatch('scp -f '+sha+'/augmentagent')

    @unittest.skipUnless(__import__('shutil').which('flock'), 'real host build lock requires Linux flock')
    def test_builds_requested_ancestor_not_tip_and_emits_real_checksums(self):
        import shutil
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);source=root/'origin';source.mkdir()
            def git(*args):
                return subprocess.check_output(['git','-C',str(source),*args],stderr=subprocess.DEVNULL,text=True,env=dict(os.environ,GIT_AUTHOR_DATE='2000-01-01T00:00:00Z',GIT_COMMITTER_DATE='2000-01-01T00:00:00Z')).strip()
            git('init','-q','-b','main');git('config','user.name','fixture');git('config','user.email','fixture@example.invalid')
            (source/'Cargo.toml').write_text('[workspace.package]\nversion="0.1.0"\n')
            (source/'rust-toolchain.toml').write_text('[toolchain]\nchannel="1.94.1"\n')
            (source/'marker').write_text('requested');git('add','.');git('commit','-qm','requested');sha=git('rev-parse','HEAD')
            (source/'marker').write_text('new-tip');git('commit','-qam','tip')
            builder=root/'builder';builder.mkdir();(builder/'artifacts').mkdir();(builder/'cache').mkdir()
            subprocess.run(['git','clone','--bare','-q',str(source),str(builder/'repo.git')],check=True)
            binaries=root/'bin';binaries.mkdir()
            df=binaries/'df';df.write_text('#!/bin/sh\nprintf "Filesystem 1024-blocks Used Available Capacity Mounted\nfixture 200000000 0 100000000 0 /\n"\n');df.chmod(0o700)
            docker=binaries/'docker';docker.write_text('''#!/usr/bin/env python3
import os,pathlib,sys
args=sys.argv[1:]
with open(pathlib.Path(os.environ['BUILDER'])/'docker.calls','a') as f:f.write('run\\n')
assert '--platform' in args and 'linux/amd64' in args
assert 'jarvis-release-builder:1.94.1' in args
assert args[-2:]==['/usr/local/bin/jarvis-container-build','1.94.1']
def mount(destination):
 return pathlib.Path(next(x for x in args if 'dst='+destination in x).split('src=')[1].split(',')[0])
source=mount('/work');assert (source/'marker').read_text()=='requested'
import subprocess
subprocess.run(['bash','-c','. "$1"; container_build "$2" "$3" "$4" "$5"','fixture',
 os.environ['CONTAINER_HELPER'],'1.94.1',str(source),str(mount('/cache')),str(mount('/output'))],check=True)
''');docker.chmod(0o700)
            cargo=binaries/'cargo';cargo.write_text('''#!/usr/bin/env python3
import os,pathlib,sys
assert sys.argv[1:]==['+1.94.1','build','--release','--locked','--target','x86_64-unknown-linux-gnu','-p','augmentagent-cli','-p','augmentagent-mcp-memory']
assert pathlib.Path('marker').read_text()=='requested'
assert pathlib.Path('marker').stat().st_mtime>946684801
release=pathlib.Path(os.environ['CARGO_TARGET_DIR'])/'x86_64-unknown-linux-gnu/release';release.mkdir(parents=True)
for name in ('augmentagent','augmentagent-mcp-memory'):(release/name).write_bytes(b'requested-binary')
''');cargo.chmod(0o700)
            env=dict(os.environ,PATH=str(binaries)+os.pathsep+os.environ['PATH'],BUILDER=str(builder),
                     CONTAINER_HELPER=str(ROOT/'scripts/remote-builder/container-build.sh'))
            result=subprocess.run(['bash','-c','. "$1"; remote_builder_build "$2" "$3"','fixture',
                str(ROOT/'scripts/remote-builder/build.sh'),sha,str(builder)],env=env,capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
            manifest=json.loads((builder/'artifacts'/sha/'manifest.json').read_text())
            self.assertEqual(manifest['commit'],sha)
            for name in NAMES:self.assertEqual(manifest['sha256'][name],hashlib.sha256(b'requested-binary').hexdigest())
            git('checkout','-qb','unapproved',sha)
            (source/'marker').write_text('unapproved');git('commit','-qam','unapproved')
            rogue=git('rev-parse','HEAD')
            rejected=subprocess.run(['bash','-c','. "$1"; remote_builder_build "$2" "$3"','fixture',
                str(ROOT/'scripts/remote-builder/build.sh'),rogue,str(builder)],env=env,capture_output=True,text=True)
            self.assertNotEqual(rejected.returncode,0)
            self.assertFalse((builder/'artifacts'/rogue).exists())
            self.assertEqual((builder/'docker.calls').read_text().splitlines(),['run'])


if __name__=='__main__':unittest.main()
