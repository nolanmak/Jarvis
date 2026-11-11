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

PYTHON_SOURCE_URL='https://www.python.org/ftp/python/3.9.25/Python-3.9.25.tar.xz'
# Matches the SHA2_256 messageDigest in the PSF's release .sigstore bundle.
PYTHON_SOURCE_SHA256='00e07d7c0f2f0cc002432d1ee84d2a40dae404a99303e3f97701c10966c91834'

def build_python39(target, stage):
 """Independent PSF source fallback; no package manager, pip or sudo."""
 archive=stage/'python-source.tar.xz'
 download([PYTHON_SOURCE_URL],PYTHON_SOURCE_SHA256,archive)
 source_root=stage/'source';source_root.mkdir()
 with tarfile.open(archive) as source:source.extractall(source_root,filter='data')
 source=source_root/'Python-3.9.25'
 env={**os.environ,'CFLAGS':'-O1 -g0'}
 log=stage/'python-source-build.log'
 commands=[([str(source/'configure'),'--prefix='+str(target),'--with-ensurepip=no'],180),
           (['make','-j2'],600),(['make','altinstall'],180)]
 try:
  with log.open('w') as output:
   for command,timeout in commands:
    subprocess.run(command,cwd=source,env=env,check=True,stdout=output,stderr=subprocess.STDOUT,timeout=timeout)
 except (subprocess.SubprocessError,OSError):
  print(log.read_text(errors='replace')[-8000:],flush=True)
  raise
 for link,destination in [('python','bin/python3.9'),('bin/python','python3.9'),('bin/python3','python3.9')]:
  (target/link).symlink_to(destination)
 # Fail if optional extension build failures left a deficient interpreter.
 subprocess.run([str(target/'bin/python3.9'),'-c','import ssl,sqlite3,zlib,bz2,lzma,ctypes; import sys; assert sys.version_info[:3]==(3,9,25)'],check=True,timeout=30)
 print('Built CPython 3.9.25 from SHA-256 verified PSF source',flush=True)

GITLEAKS_MODULE_URL='https://proxy.golang.org/github.com/zricethezav/gitleaks/v8/@v/v8.30.1.zip'
GITLEAKS_MODULE_SHA256='4fd70631c0f6159e38fdb99ad126e3ba60d4212420062029d3dc5f510f6f2961'

def build_gitleaks(stage):
 """Use the immutable public Go module when release-asset delivery is down."""
 archive=stage/'gitleaks-source.zip'
 download([GITLEAKS_MODULE_URL],GITLEAKS_MODULE_SHA256,archive)
 prefix='github.com/zricethezav/gitleaks/v8@v8.30.1/'
 source_root=stage/'source';source_root.mkdir()
 with zipfile.ZipFile(archive) as source:
  members=source.infolist()
  if sum(row.file_size for row in members)>128*1024*1024 or any(not row.filename.startswith(prefix) or '..' in Path(row.filename).parts or (row.external_attr>>16)&0o170000==0o120000 for row in members):
   raise ValueError('unexpected scanner source archive')
  source.extractall(source_root)
 source=source_root/prefix
 env={**os.environ,'GOTOOLCHAIN':'local','GOENV':'off','GOWORK':'off','GO111MODULE':'on',
      'GOPROXY':'https://proxy.golang.org','GOSUMDB':'sum.golang.org',
      'GOPRIVATE':'','GONOSUMDB':'','GONOPROXY':'','GOINSECURE':'','GOFLAGS':'-p=2',
      'GOMAXPROCS':'2','GOOS':'linux','GOARCH':'amd64','CGO_ENABLED':'0',
      'GOCACHE':str(stage/'go-cache'),'GOPATH':str(stage/'go-path'),'GOMODCACHE':str(stage/'go-modules')}
 version=subprocess.run(['go','version'],env=env,check=True,capture_output=True,text=True,timeout=30).stdout.strip()
 if version!='go version go1.27.1 linux/amd64':raise RuntimeError('scanner source build requires pinned Go 1.27.1 linux/amd64')
 binary=stage/'gitleaks'
 # Same regexp backend and version linker variable as the release's Goreleaser
 # configuration. Dependencies retain module go.sum and public checksum DB checks.
 command=['go','build','-mod=readonly','-tags=gore2regex','-trimpath',
          '-ldflags=-s -w -X=github.com/zricethezav/gitleaks/v8/version.Version=8.30.1','-o',str(binary),'.']
 log=stage/'gitleaks-source-build.log'
 try:
  with log.open('w') as output:subprocess.run(command,cwd=source,env=env,check=True,stdout=output,stderr=subprocess.STDOUT,timeout=600)
 except (subprocess.SubprocessError,OSError):
  print(log.read_text(errors='replace')[-8000:],flush=True)
  raise
 version=subprocess.run([str(binary),'version'],check=True,capture_output=True,text=True,timeout=30).stdout.strip()
 if version!='8.30.1':raise RuntimeError('source-built scanner version mismatch')
 print('Built gitleaks 8.30.1 from SHA-256 verified Go module',flush=True)
 return binary

def finish_cache(tool,target,complete):
 executable=target/('deno' if tool=='deno' else 'bin/python3.9')
 env=dict(os.environ)
 if tool=='python39':env['LD_LIBRARY_PATH']=str(target/'lib')
 subprocess.run([str(executable),'--version'],check=True,env=env,timeout=30)
 complete.touch()

def provision(tool):
 if os.environ.get('GITHUB_ACTIONS')!='true' or platform.system()!='Linux' or platform.machine()!='x86_64':
  raise RuntimeError('Provisioning requires an isolated GitHub Linux x64 job')
 version,digest,urls=PINS[tool]
 runner_temp=Path(os.environ['RUNNER_TEMP'])
 if tool=='python39' and 'VERSION_ID="24.04"' not in Path('/etc/os-release').read_text():
  raise RuntimeError('Pinned Python artifact requires Ubuntu 24.04')
 if tool!='gitleaks':
  # setup-python prefers AGENT_TOOLSDIRECTORY; setup-deno uses RUNNER_TOOL_CACHE.
  cache=os.environ.get('AGENT_TOOLSDIRECTORY','').strip() if tool=='python39' else ''
  root=Path(cache or os.environ['RUNNER_TOOL_CACHE'])/('deno' if tool=='deno' else 'Python')/version
  target=root/'x64';complete=root/'x64.complete'
  if target.is_dir() and complete.is_file():return
  # Never replace an incomplete installation belonging to a different step.
  if target.exists() or complete.exists():raise RuntimeError('Incomplete tool-cache entry; refusing replacement')
  root.mkdir(parents=True,exist_ok=True)
 else:
  target=runner_temp/'gitleaks'
  if target.exists():raise RuntimeError('Refusing to replace an existing scanner')
 with tempfile.TemporaryDirectory(prefix='pinned-ci-',dir=runner_temp) as temporary:
  stage=Path(temporary);archive=stage/'artifact'
  try:download(urls,digest,archive)
  except RuntimeError:
   if tool=='gitleaks':
    print('Binary transports unavailable; trying pinned Go module build',flush=True)
    build_gitleaks(stage).rename(target)
    return
   if tool!='python39':raise
   print('Binary transports unavailable; trying pinned PSF source build',flush=True)
   build_python39(target,stage)
   finish_cache(tool,target,complete)
   return
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
  finish_cache(tool,target,complete)
 print(f'Provisioned {tool} {version} from SHA-256 verified release bytes',flush=True)

def verify_python39_source():
 """Exercise the real source path in a private hosted prefix, even on cache hits."""
 if os.environ.get('GITHUB_ACTIONS')!='true' or platform.system()!='Linux' or platform.machine()!='x86_64':
  raise RuntimeError('Source verification requires an isolated GitHub Linux x64 job')
 if 'VERSION_ID="24.04"' not in Path('/etc/os-release').read_text():
  raise RuntimeError('Source verification requires Ubuntu 24.04')
 with tempfile.TemporaryDirectory(prefix='python-source-canary-',dir=Path(os.environ['RUNNER_TEMP'])) as temporary:
  stage=Path(temporary);target=stage/'python'
  build_python39(target,stage)
  finish_cache('python39',target,stage/'python.complete')
 print('Pinned Python source fallback verified in a private hosted prefix',flush=True)

if __name__=='__main__':
 parser=argparse.ArgumentParser(description=__doc__)
 parser.add_argument('tool',choices=PINS)
 parser.add_argument('--verify-source',action='store_true',help='Verify Python source fallback in a private hosted prefix')
 args=parser.parse_args()
 if args.verify_source:
  if args.tool!='python39':parser.error('--verify-source requires python39')
  verify_python39_source()
 else:provision(args.tool)
