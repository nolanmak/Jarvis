#!/usr/bin/env python3
"""Provision immutable Linux CI artifacts without executing release install scripts.

Public downloads need no token. Each independent transport is tried at most twice, with
bounded time/size; only verified bytes reach extraction. Existing completed
GitHub runner tool-cache entries retain the setup actions' normal trust model.
"""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import zipfile

PINS = {
 'deno': ('2.7.14','3287efef53606966469cb6a02781327be22b908959397f976e2996dc1b64ae0f',[
  'https://dl.deno.land/release/v2.7.14/deno-x86_64-unknown-linux-gnu.zip',
  'https://github.com/denoland/deno/releases/download/v2.7.14/deno-x86_64-unknown-linux-gnu.zip']),
 'python39': ('3.9.25','3f3be0babc02c5c457df8546207c7bea3844c3eef21cb037485ef83c59118cbd',[
  'https://api.github.com/repos/actions/python-versions/releases/assets/311997504',
  'https://github.com/actions/python-versions/releases/download/3.9.25-19039189640/python-3.9.25-linux-24.04-x64.tar.gz']),
 'gitleaks': ('8.30.1','551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb',[
  'https://api.github.com/repos/gitleaks/gitleaks/releases/assets/378332058',
  'https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_linux_x64.tar.gz']),
}

def download(urls, digest, target, limit=128*1024*1024):
 target=Path(target)
 errors=[]
 deadline=time.monotonic()+120
 for url in list(urls)*2:
  partial=target.with_suffix('.partial')
  try:
   if time.monotonic()>=deadline:raise TimeoutError('artifact download deadline exceeded')
   request=urllib.request.Request(url,headers={'Accept':'application/octet-stream','User-Agent':'Jarvis-pinned-CI'})
   hashed=hashlib.sha256();size=0
   with urllib.request.urlopen(request,timeout=30) as response, partial.open('wb') as output:
    while True:
     data=response.read(1024*1024)
     if not data:break
     size+=len(data)
     if size>limit:raise ValueError('artifact size limit exceeded')
     if time.monotonic()>deadline:raise TimeoutError('artifact download deadline exceeded')
     hashed.update(data);output.write(data)
   if hashed.hexdigest()!=digest:raise ValueError('artifact checksum mismatch')
   partial.replace(target)
   return
  except (OSError, ValueError, urllib.error.URLError) as error:
   errors.append(f'{url}: {error}')
   print(f'Pinned artifact transport failed: {error}',flush=True)
  finally:partial.unlink(missing_ok=True)
 raise RuntimeError('All pinned artifact transports failed: '+'; '.join(errors))

def extract_deno(archive, target):
 # This release contains exactly one executable. Reject unexpected files before
 # writing anything, including traversal paths and symlinks.
 with zipfile.ZipFile(archive) as source:
  members=source.infolist()
  if len(members)!=1 or members[0].filename!='deno' or members[0].file_size>256*1024*1024:
   raise ValueError('unexpected Deno archive contents')
  target.mkdir(parents=True)
  with source.open(members[0]) as binary, (target/'deno').open('wb') as output:shutil.copyfileobj(binary,output)
 (target/'deno').chmod(0o755)

def provision(tool):
 if os.environ.get('GITHUB_ACTIONS')!='true' or platform.system()!='Linux' or platform.machine()!='x86_64':
  raise RuntimeError('Provisioning requires an isolated GitHub Linux x64 job')
 version,digest,urls=PINS[tool]
 runner_temp=Path(os.environ['RUNNER_TEMP'])
 if tool=='python39' and 'VERSION_ID="24.04"' not in Path('/etc/os-release').read_text():
  raise RuntimeError('Pinned Python artifact requires Ubuntu 24.04')
 if tool!='gitleaks':
  root=Path(os.environ['RUNNER_TOOL_CACHE'])/('deno' if tool=='deno' else 'Python')/version
  target=root/'x64';complete=root/'x64.complete'
  if target.is_dir() and complete.is_file():return
  # Never replace an incomplete installation belonging to a different step.
  if target.exists() or complete.exists():raise RuntimeError('Incomplete tool-cache entry; refusing replacement')
  root.mkdir(parents=True,exist_ok=True)
 else:
  target=runner_temp/'gitleaks'
  if target.exists():raise RuntimeError('Refusing to replace an existing scanner')
 with tempfile.TemporaryDirectory(prefix='pinned-ci-',dir=runner_temp) as temporary:
  stage=Path(temporary);archive=stage/'artifact';download(urls,digest,archive)
  extracted=stage/'extracted'
  if tool=='deno':extract_deno(archive,extracted)
  elif tool=='gitleaks':
   with tarfile.open(archive) as source:
    member=source.getmember('gitleaks')
    if not member.isfile() or member.size>64*1024*1024:raise ValueError('unexpected scanner archive')
    with source.extractfile(member) as binary, (stage/'gitleaks').open('wb') as output:shutil.copyfileobj(binary,output)
   (stage/'gitleaks').chmod(0o755)
   subprocess.run([str(stage/'gitleaks'),'version'],check=True,timeout=30)
   (stage/'gitleaks').rename(target)
   return
  else:
   extracted.mkdir()
   with tarfile.open(archive) as source:source.extractall(extracted,filter='data')
   # Do not execute bundled setup.sh (which upgrades pip over the network).
   (extracted/'setup.sh').unlink(missing_ok=True)
   (extracted/'python').symlink_to('bin/python3.9')
   (extracted/'bin/python').symlink_to('python3.9')
  # Move only verified artifacts, then smoke-test at the final prefix before
  # exposing the completion marker consumed by actions/tool-cache.
  shutil.move(str(extracted),target)
  executable=target/('deno' if tool=='deno' else 'bin/python3.9')
  env=dict(os.environ)
  if tool=='python39':env['LD_LIBRARY_PATH']=str(target/'lib')
  subprocess.run([str(executable),'--version'],check=True,env=env,timeout=30)
  complete.touch()
 print(f'Provisioned {tool} {version} from SHA-256 verified release bytes',flush=True)

if __name__=='__main__':
 parser=argparse.ArgumentParser(description=__doc__)
 parser.add_argument('tool',choices=PINS)
 provision(parser.parse_args().tool)
