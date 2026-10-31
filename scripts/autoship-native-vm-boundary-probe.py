#!/usr/bin/env python3
"""Explicit synthetic VM feasibility probe; no model inference or real auth.

Uses the production no-NIC runner and dependency gateway without changing its
configuration. Host service/cgroup settings are not touched. Caller should run
this in a bounded transient unit after the central build queue is idle.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import tempfile

SPEC = importlib.util.spec_from_file_location('native_vm_probe_runner', Path(__file__).with_name('codex-build-vm.py'))
vm = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(vm)

GUEST = r'''
import http.client,json,os,pathlib,socket,ssl,struct,subprocess
report={}
assert sorted(p.name for p in pathlib.Path('/sys/class/net').iterdir())==['lo']
report['interfaces']=['lo']
assert pathlib.Path('/workspace/declared-auth').read_text()=='SYNTHETIC_DECLARED_AUTH'
report['declared_auth_read']=True
for path in json.loads(pathlib.Path('/workspace/undeclared-paths.json').read_text()):
 try:
  pathlib.Path(path).read_bytes()
  raise AssertionError('undeclared host path was readable')
 except (FileNotFoundError,PermissionError,NotADirectoryError): pass
report['unmounted_host_reads_denied']=True
attempts=[('host_alias',socket.AF_INET,('10.0.2.2',443)),
 ('external_ipv4',socket.AF_INET,('1.1.1.1',443)),
 ('external_ipv6',socket.AF_INET6,('2606:4700:4700::1111',443)),
 ('host_ipv6_alias',socket.AF_INET6,('fec0::2',443))]
for name,family,address in attempts:
 try:
  with socket.socket(family,socket.SOCK_STREAM) as connection:
   connection.settimeout(1);connection.connect(address)
  raise AssertionError('undeclared direct route succeeded: '+name)
 except (OSError,TimeoutError): report[name]='denied'
for name,address in [('virtual_dns',('10.0.2.3',53)),('external_dns',('1.1.1.1',53))]:
 try:
  with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as connection:
   connection.settimeout(1)
   query=struct.pack('!HHHHHH',123,0x100,1,0,0,0)+b'\x07example\x03com\0'+struct.pack('!HH',1,1)
   connection.sendto(query,address);connection.recvfrom(512)
  raise AssertionError('undeclared DNS response received')
 except (OSError,TimeoutError): report[name]='denied'
# The one allowed route is interpreted by the existing guest-local HTTPS
# adapter and host broker. It is not guest IP forwarding or proxy env policy.
context=ssl.create_default_context(cafile='/etc/jarvis-registry-ca.pem')
connection=http.client.HTTPSConnection('index.crates.io',443,timeout=20,context=context)
connection.request('GET','/config.json');response=connection.getresponse();body=response.read()
assert response.status==200 and isinstance(json.loads(body),dict)
connection.close();report['declared_gateway_route']='allowed'
for host,path in [('unlisted.invalid','/config.json'),('index.crates.io','/../../private')]:
 connection=http.client.HTTPSConnection('127.0.0.1',443,timeout=20,context=ssl._create_unverified_context())
 connection.request('GET',path,headers={'Host':host});response=connection.getresponse();response.read()
 assert response.status in (400,403,404,502), (host,response.status)
 connection.close()
report['unlisted_host_and_route']='denied'
versions={}
for name in ['codex','claude']:
 binary=pathlib.Path('/workspace/native-'+name)
 if binary.exists():
  result=subprocess.run([str(binary),'--version'],capture_output=True,text=True,timeout=20)
  assert result.returncode==0,(name,result.returncode,result.stderr[:200])
  versions[name]=result.stdout.strip()[:200]
report['native_binary_versions']=versions
print(json.dumps(report,sort_keys=True))
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--runtime', type=Path, default=Path.home()/'.local/share/augmentagent/build-vm/runtime.json')
    parser.add_argument('--native-versions', action='store_true')
    args = parser.parse_args()
    if not args.execute:
        parser.error('explicit --execute is required; no model calls are made')
    runtime = vm.Runtime.load(args.runtime)
    runtime.config = dict(runtime.config, memory_mb=512)
    # No Cargo/npm cache, toolchain, or original home is needed for this probe.
    for key in ('toolchain','registry','cargo_git'):
        runtime.config.pop(key, None)
    with tempfile.TemporaryDirectory(prefix='autoship-native-vm-probe-',dir='/tmp') as temporary:
        root = Path(temporary)
        workspace = root/'workspace'
        workspace.mkdir()
        (workspace/'declared-auth').write_text('SYNTHETIC_DECLARED_AUTH')
        outside = root/'unrelated-auth'
        outside.write_text('SYNTHETIC_UNDECLARED_AUTH')
        (workspace/'undeclared-paths.json').write_text(json.dumps([str(outside),str(Path.home()/'.ssh/id_rsa'),str(Path.home()/'.codex/auth.json')]))
        if args.native_versions:
            for name in ('codex','claude'):
                executable = shutil.which(name)
                if not executable:
                    raise RuntimeError('requested native executable is unavailable: '+name)
                shutil.copyfile(Path(executable).resolve(strict=True),workspace/('native-'+name))
                (workspace/('native-'+name)).chmod(0o755)
        (root/'scratch').mkdir(mode=0o700)
        downloads={}
        result=vm.run(runtime,workspace,['/usr/bin/python3','-I','-c',GUEST],{},timeout=120,download_info=downloads,scratch_dir=root/'scratch')
        if result['exit_code']:
            raise RuntimeError('VM boundary probe failed: '+result['stderr'][:3000])
        report=json.loads(result['stdout'])
        report.update(paid_model_calls=0,real_credentials_supplied=False,production_changed=False,gateway_accounting=downloads)
        print(json.dumps(report,indent=2,sort_keys=True))


if __name__=='__main__':
    main()
