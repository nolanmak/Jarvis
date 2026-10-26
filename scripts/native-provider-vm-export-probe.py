#!/usr/bin/env python3
"""Explicit no-auth VM red/green export-drain proof. Run in a bounded unit.
The red shim removes only the receipt wait; neither mode contacts a provider.
"""
import argparse
import pathlib
import sys

if len(sys.argv)>1 and sys.argv[1]=='--shim':
    import importlib.util,sys,time
    p=str(pathlib.Path(__file__).with_name('native-provider-vm.py'))
    s=importlib.util.spec_from_file_location('vm',p);m=importlib.util.module_from_spec(s);s.loader.exec_module(m)
    fsync=m.os.fsync
    def slow(fd):time.sleep(.08);fsync(fd)
    m.os.fsync=slow
    if sys.argv[3]=='red':
     original=m.boot_entries
     def entries(config,job,seed,guest,gateway,certs):
      guest=guest.replace(b"if export_received is not None:",b"if False:")
      return original(config,job,seed,guest,gateway,certs)
     m.boot_entries=entries
     send=m.Channel.send
     def sending(self,frame):
      if frame['type']!='export_received':send(self,frame)
     m.Channel.send=sending
    try:sys.exit(m.main(sys.argv[2]))
    except Exception as e:print(type(e).__name__,file=sys.stderr);sys.exit(126)

import json,os,pathlib,subprocess,time
parser=argparse.ArgumentParser()
parser.add_argument('--execute',action='store_true',required=True)
parser.add_argument('--runtime-config',type=pathlib.Path,required=True)
parser.add_argument('--guest-runtime',type=pathlib.Path,required=True)
parser.add_argument('--output',type=pathlib.Path,required=True)
args=parser.parse_args()
root=args.output.absolute();root.mkdir(mode=0o700)
for mode in ('red','green'):
 r=root/mode;r.mkdir(mode=0o700)
 s={'version':1,'runtime_config':str(args.runtime_config.resolve()),'guest_runtime':str(args.guest_runtime.resolve()),'memory_mb':512,'environment':{'LANG':'C.UTF-8'},'auth_relative':'.codex/auth.json','mcp_argv':None,'assets':[],'gateway':{'version':1,'authorities':[{'authority':'api.example.com:443','tls':True,'routes':[{'path_prefix':'/unused','exact':True,'methods':['GET']}]}]}}
 worker="from pathlib import Path\np=Path.home()\nfor i in range(64):(p/f'export-{i:03}').write_bytes(bytes([i])*65536)\nprint('synthetic-export-ready',flush=True)"
 s.update(executable='/usr/bin/python3.12',argv=['-I','-S','-c',worker],auth_file=None,refresh_auth=False,probe_route_diagnostics=False,session_directory=str(r/'session'),resume=False)
 (r/'session').mkdir(mode=0o700);p=r/'launch.json';p.write_text(json.dumps(s));p.chmod(0o600)
 env={'PATH':'/usr/bin:/bin','JARVIS_NATIVE_VM_READY':str(r/'ready'),'JARVIS_NATIVE_VM_CONTINUE':str(r/'continue')}
 with open(r/'stdout','wb') as out,open(r/'stderr','wb') as err:
  os.chmod(r/'stdout',0o600);os.chmod(r/'stderr',0o600)
  proc=subprocess.Popen(['/usr/bin/python3','-I',str(pathlib.Path(__file__).resolve()),'--shim',str(p),mode],env=env,stdin=subprocess.DEVNULL,stdout=out,stderr=err)
  deadline=time.monotonic()+45
  while not (r/'ready').exists() and proc.poll() is None and time.monotonic()<deadline:time.sleep(.02)
  if (r/'ready').exists():
   (r/'continue').write_text('admitted\n');(r/'continue').chmod(0o600)
  try:code=proc.wait(timeout=60)
  finally:
   if proc.poll() is None:proc.terminate();proc.wait(timeout=10)
 complete=(r/'session'/'complete.json').exists()
 print(json.dumps({'mode':mode,'exit_code':code,'complete':complete,'auth_mounted':False}),flush=True)
 for line in (r/'stderr').read_text().splitlines():
  try:
   x=json.loads(line)
   if 'native_host_failure_state' in x:print(json.dumps(x),flush=True)
  except:pass
 if mode=='red':assert code!=0 and not complete
 else:
  assert code==0 and complete
  c=json.load(open(r/'session'/'complete.json'));g=r/'session'/c['generation']
  for i in range(64):assert (g/f'export-{i:03}').read_bytes()==bytes([i])*65536
  print('PASS exact 64 files / 4MiB retained after delayed consumer',flush=True)
