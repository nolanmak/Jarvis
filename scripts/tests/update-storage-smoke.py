"""An auto-ship writer stays alive while an independently admitted update runs."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

cli=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='jarvis-update-smoke-') as temporary:
    base=Path(temporary);repo=base/'repo'
    subprocess.run(['git','init','-q',str(repo)],check=True)
    env=dict(os.environ,XDG_STATE_HOME=str(base/'state'),
             AUGMENTAGENT_GATE_TARGET_DIR=str(base/'gate'),
             AUGMENTAGENT_GATE_MIN_FREE_GB='0',AUGMENTAGENT_BUILD_GROWTH_GB='0',
             AUGMENTAGENT_PRUNE_TARGET_DIRS='')
    env.pop('AUGMENTAGENT_BUILD_PARENT_LEASE',None)
    disabled=subprocess.run([cli,'auto-pr-worker','--dry-run','false'],cwd=repo,
                            env=dict(env,AUGMENTAGENT_AUTOPR='0'),capture_output=True,text=True,timeout=5)
    assert disabled.returncode==0,disabled.stderr
    assert not (base/'state/augmentagent/autopr-supervisor.lock').exists()
    started=base/'started'; release=base/'release'
    hold='from pathlib import Path; import sys,time; Path(sys.argv[1]).touch(); exec("while not Path(sys.argv[2]).exists(): time.sleep(.02)")'
    writer=subprocess.Popen([cli,'disk','build','--target-dir',str(base/'gate'),'--',
                             sys.executable,'-c',hold,str(started),str(release)],
                            cwd=repo,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    try:
        deadline=time.monotonic()+15
        while not started.exists():
            assert writer.poll() is None,writer.communicate()
            assert time.monotonic()<deadline,'writer did not start'
            time.sleep(.02)
        # A checkout debug symlink onto the gate cache is common on this host.
        # The update must protect only its independent release outputs.
        (repo/'target').mkdir(exist_ok=True)
        (base/'gate/debug').mkdir(exist_ok=True)
        (repo/'target/debug').symlink_to(base/'gate/debug',target_is_directory=True)
        code='''import os,pathlib,subprocess,sys
assert pathlib.Path(os.environ['TMPDIR']).name=='.augmentagent-update-tmp'
cli=sys.argv[1]
args=[cli,'disk','build','--update','--target-dir',os.environ['CARGO_TARGET_DIR'],'--',sys.executable,'-c','pass']
subprocess.run(args,check=True,capture_output=True,timeout=8)
wrong=args.copy();wrong.remove('--update')
assert subprocess.run(wrong,capture_output=True,timeout=8).returncode!=0
fresh=dict(os.environ);fresh.pop('AUGMENTAGENT_BUILD_PARENT_LEASE',None)
blocked=subprocess.run(args,env=fresh,capture_output=True,text=True,timeout=8)
assert blocked.returncode==75,(blocked.returncode,blocked.stderr)
assert 'storage busy:' in blocked.stderr and 'low disk' not in blocked.stderr
'''
        update=subprocess.run([cli,'disk','build','--update','--target-dir',str(repo/'target'),
                               '--',sys.executable,'-c',code,cli],cwd=repo,env=env,
                              capture_output=True,text=True,timeout=20)
        assert update.returncode==0,(update.stdout,update.stderr)
        assert writer.poll() is None,'update interrupted auto-ship'
        print('ok: update overlaps active writer, ignores shared debug, rejects scope changes and competing updates')
    finally:
        release.touch()
        output=writer.communicate(timeout=15)
        assert writer.returncode==0,output
