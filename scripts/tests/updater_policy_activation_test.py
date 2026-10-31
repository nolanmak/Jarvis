"""Exercise policy-only updater activation with a disposable service manager."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
ROOT=Path(__file__).resolve().parents[2]

@unittest.skipUnless(sys.platform=='linux' and Path('/usr/bin/python3').is_file(),'Linux updater')
class ActivationTests(unittest.TestCase):
    def test_activation_uses_restart_guards_and_never_restarts_worker(self):
        source=(ROOT/'scripts/check-for-updates.sh').read_text()
        start=source.index('reconcile_native_selection() {')
        function=source[start:source.index('\n}\n',start)+3]
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);(root/'scripts').mkdir();(root/'bin').mkdir()
            (root/'scripts/autoship-deployment-readiness.py').write_text('import os,sys\nsys.exit(int(os.environ["PROBE_STATUS"]))\n')
            manager=root/'bin/systemctl';manager.write_text('#!/bin/sh\necho "$*" >> "$CALLS"\nexit 0\n');manager.chmod(0o700)
            harness='''set -euo pipefail
log() { echo "$*" >> "$LOG"; }
deploy_pinned_elsewhere() { [ "$GUARD" = pinned ]; }
maybe_defer_restart() { [ "$GUARD" = busy ]; }
memory_pressure_ok() { [ "$GUARD" != memory ]; }
restart_budget_ok() { [ "$GUARD" != budget ]; }
restart_unit() { echo "restart $*" >> "$CALLS"; [ "$GUARD" != failure ]; }
record_restart() { echo recorded >> "$CALLS"; }
'''+function+'\nreconcile_native_selection\n'
            for status,guard,expected in [('0','none',False),('1','none',False),('10','pinned',False),('10','busy',False),('10','memory',False),('10','budget',False),('10','none',True),('10','failure',True)]:
                with self.subTest(status=status,guard=guard):
                    calls=root/'calls';calls.unlink(missing_ok=True)
                    env=dict(os.environ,REPO_ROOT=str(root),LOG_DIR=str(root),LOG=str(root/'log'),HOME=str(root),
                             SYSTEMD_UNIT='augmentagent.service',DAEMON_BINARY='/fixture/deployed-binary',
                             PATH=str(root/'bin')+os.pathsep+os.environ['PATH'],CALLS=str(calls),PROBE_STATUS=status,GUARD=guard)
                    result=subprocess.run(['bash','-c',harness],env=env,capture_output=True,text=True)
                    self.assertEqual(result.returncode,0,result.stderr)
                    text=calls.read_text() if calls.exists() else ''
                    self.assertEqual('restart augmentagent.service /fixture/deployed-binary' in text,expected)
                    self.assertNotIn('augmentagent-autopr.service',text)
                    self.assertEqual('recorded' in text,expected and guard!='failure')

if __name__=='__main__':unittest.main()
