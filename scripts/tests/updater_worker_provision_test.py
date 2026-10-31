"""Run the real updater for a script-only pull using disposable external tools."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[2]

@unittest.skipUnless(sys.platform=='linux','Linux worker provisioning')
class WorkerProvisionTests(unittest.TestCase):
    def test_script_only_update_repairs_cached_release_modes_without_restarting_daemon(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            for name in ['check-for-updates.sh','lib/service-restart.sh','lib/update-build.sh',
                         'ensure-autopr-worker.sh','systemd/augmentagent-autopr.service']:
                target=root/'scripts'/name;target.parent.mkdir(parents=True,exist_ok=True)
                shutil.copy(ROOT/'scripts'/name,target)
            tools=root/'bin';tools.mkdir();release=root/'target/release';release.mkdir(parents=True)
            cli=release/'augmentagent'
            cli.write_text('''#!/bin/sh
echo "cli $*" >> "$CALLS"
if [ "$1 $2" = 'auto-pr-worker --help' ]; then echo 'Usage: augmentagent auto-pr-worker'; fi
exit 0
''');cli.chmod(0o775)
            memory=release/'augmentagent-mcp-memory';memory.write_text('#!/bin/sh\nexit 0\n');memory.chmod(0o775)
            git=tools/'git';git.write_text('''#!/bin/sh
case "$1" in
rev-parse) if [ "$2" = HEAD ]; then echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; else echo bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb; fi ;;
merge-base) [ "$3" = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ] ;;
diff) echo scripts/ensure-autopr-worker.sh ;;
fetch|pull) exit 0 ;;
*) exit 97 ;;
esac
''');git.chmod(0o700)
            manager=tools/'systemctl';manager.write_text('''#!/usr/bin/env python3
import os,sys
with open(os.environ['CALLS'],'a') as f:f.write('systemctl '+' '.join(sys.argv[1:])+'\\n')
if 'show' in sys.argv:
 key=sys.argv[sys.argv.index('-p')+1]
 print({'ExecStart':'{ argv[]=/fixture/serve --dry-run false ; }'}.get(key,''))
''');manager.chmod(0o700)
            cargo=tools/'cargo';cargo.write_text('#!/bin/sh\necho forbidden-cargo >> "$CALLS"\nexit 99\n');cargo.chmod(0o700)
            calls=root/'calls'
            env={'HOME':str(root),'XDG_STATE_HOME':str(root/'state'),'XDG_CONFIG_HOME':str(root/'config'),
                 'PATH':str(tools)+':/usr/bin:/bin','CALLS':str(calls)}
            result=subprocess.run(['bash',str(root/'scripts/check-for-updates.sh')],env=env,capture_output=True,text=True,timeout=30)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertEqual((root/'state/augmentagent/built-commit').read_text().strip(),'b'*40)
            self.assertEqual([p.stat().st_mode & 0o777 for p in [cli,memory]],[0o755,0o755],
                             'script-only updates skipped worker provisioning and left executables untrusted')
            text=calls.read_text()
            self.assertIn('enable augmentagent-autopr.service',text)
            self.assertNotIn('restart augmentagent.service',text)
            self.assertNotIn('forbidden-cargo',text)

if __name__=='__main__':unittest.main()
