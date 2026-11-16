"""Run the real updater in disposable Git repositories and script service/build boundaries."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args], stderr=subprocess.PIPE).decode().strip()


class DirtyDeployment(unittest.TestCase):
    def test_owner_changes_stop_the_updater_before_repository_helpers_or_builds(self):
        for fault in ('tracked', 'staged', 'untracked', 'dirty-helper'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                base = Path(temporary); work = base / 'work'; work.mkdir()
                git(work, 'init', '-q', '-b', 'main')
                scripts = work / 'scripts'; (scripts / 'lib').mkdir(parents=True)
                for name in ('check-for-updates.sh', 'secure-release-executables.py',
                             'lib/service-restart.sh', 'lib/update-build.sh'):
                    shutil.copyfile(ROOT / 'scripts' / name, scripts / name)
                (scripts / 'recover-autopr-worker.py').write_text('raise SystemExit(0)\n')
                (scripts / 'pdf-runtime.py').write_text('raise SystemExit(0)\n')
                helper = scripts / 'ensure-native-runtime.py'
                helper.write_text("import os\nfrom pathlib import Path\nPath(os.environ['HELPER_CALLED']).write_text('called')\n")
                (work / '.gitignore').write_text('target/\n.self-improve-worktrees/\n')
                (work / 'source.rs').write_text('trusted source\n')
                git(work, 'add', '-A')
                git(work, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'trusted main')
                git(work, 'remote', 'add', 'origin', str(work))
                git(work, 'fetch', '-q', 'origin', 'main')
                head = git(work, 'rev-parse', 'HEAD')
                release = work / 'target/release'; release.mkdir(parents=True)
                for name in ('augmentagent', 'augmentagent-mcp-memory'):
                    path = release / name
                    path.write_text('#!/bin/sh\nexit 0\n'); path.chmod(0o755)
                fake_bin = base / 'bin'; fake_bin.mkdir()
                for name, body in [('systemctl', '#!/bin/sh\nexit 1\n'), ('cargo', '#!/bin/sh\ntouch "$BUILD_CALLED"\nexit 0\n')]:
                    path = fake_bin / name; path.write_text(body); path.chmod(0o700)
                state = base / 'state/augmentagent'; state.mkdir(parents=True)
                stamp = state / 'built-commit'; stamp.write_text(head + '\n')
                if fault == 'tracked':
                    (work / 'source.rs').write_text('owner unfinished\n')
                elif fault == 'staged':
                    (work / 'source.rs').write_text('owner staged\n'); git(work, 'add', 'source.rs')
                    (work / 'source.rs').write_text('owner unfinished\n')
                elif fault == 'untracked':
                    (work / 'owner-note').write_text('private owner bytes\n')
                else:
                    helper.write_text(helper.read_text() + '# private owner edit\n')
                before = {str(p.relative_to(work)): p.read_bytes() for p in work.rglob('*')
                          if p.is_file() and '.git' not in p.relative_to(work).parts and 'target' not in p.relative_to(work).parts}
                index = (work / '.git/index').read_bytes()
                branch = (work / '.git/HEAD').read_bytes()
                env = {'PATH': str(fake_bin) + os.pathsep + os.defpath, 'HOME': str(base),
                       'XDG_STATE_HOME': str(base / 'state'), 'XDG_CONFIG_HOME': str(base / 'config'),
                       'HELPER_CALLED': str(base / 'helper-called'), 'BUILD_CALLED': str(base / 'build-called')}
                result = subprocess.run(['bash', str(scripts / 'check-for-updates.sh')], cwd=work,
                                        env=env, capture_output=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stderr.decode())
                self.assertFalse((base / 'helper-called').exists(), 'dirty deploy checkout executed a repository helper')
                self.assertFalse((base / 'build-called').exists(), 'dirty deploy checkout started a build')
                self.assertEqual(stamp.read_text(), head + '\n')
                self.assertEqual((work / '.git/index').read_bytes(), index)
                self.assertEqual((work / '.git/HEAD').read_bytes(), branch)
                self.assertEqual(git(work, 'rev-parse', 'HEAD'), head)
                self.assertEqual(before, {str(p.relative_to(work)): p.read_bytes() for p in work.rglob('*')
                                         if p.is_file() and '.git' not in p.relative_to(work).parts and 'target' not in p.relative_to(work).parts})
                self.assertIn('DEPLOY CHECKOUT DIRTY', (state / 'update.log').read_text())


if __name__ == '__main__':
    unittest.main()
