#!/usr/bin/env python3
"""Execute the real wrapper with a recorder at the CLI boundary; no build runs."""
import json, os, pathlib, subprocess, tempfile, unittest
WRAPPER=pathlib.Path(__file__).resolve().parents[1]/'cargo-managed.sh'
class VolumeWrapper(unittest.TestCase):
 def test_volume_and_parent_override(self):
  for specific in (False,True):
   with self.subTest(specific=specific), tempfile.TemporaryDirectory(prefix='volume-wrapper-') as temporary:
    root=pathlib.Path(temporary);repo=root/'repo';repo.mkdir()
    subprocess.run(['git','init','-q',str(repo)],check=True)
    recorder=root/'cli';calls=root/'calls'
    recorder.write_text("#!/usr/bin/python3\nimport json,os,sys\nwith open(os.environ['VOLUME_CALLS'],'a') as f:f.write(json.dumps(sys.argv[1:])+'\\n')\n")
    recorder.chmod(0o700)
    env={k:v for k,v in os.environ.items() if k not in ('CARGO_TARGET_DIR','AUGMENTAGENT_BUILD_PARENT')}
    env.update(AUGMENTAGENT_BUILD_VOLUME=str(root/'volume'),AUGMENTAGENT_STORAGE_BIN=str(recorder),VOLUME_CALLS=str(calls))
    parent=root/('specific' if specific else 'volume/augmentagent-session-targets')
    if specific:env['AUGMENTAGENT_BUILD_PARENT']=str(parent)
    done=subprocess.run(['bash',str(WRAPPER),'check'],cwd=repo,env=env,capture_output=True,text=True)
    self.assertEqual(done.returncode,0,done.stderr)
    observed=[json.loads(line) for line in calls.read_text().splitlines()]
    self.assertEqual(observed[0],['disk','manage-parent','--root',str(parent)])
    self.assertEqual(observed[1][:3],['disk','build','--target-dir'])
    self.assertEqual(pathlib.Path(observed[1][3]).parent,parent)
    self.assertEqual(observed[1][-2:],['cargo','check'])
if __name__=='__main__':unittest.main()
