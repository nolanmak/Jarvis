"""Pinned diagnostic fixture for a publication after Cargo's fresh decision.

Worker-owned traces/counts describe fixture behavior only. They are not trusted
input closure, producer provenance, a passing-CI capability, or merge authority.
"""
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import time

PACKAGE='generated-input-consumer'
PUBLISHER=r'''use std::{env,fs::{self,File,FileTimes},path::PathBuf,thread,time::{Duration,SystemTime}};
fn main(){
 println!("cargo:rerun-if-env-changed=CI_FIXTURE_PUBLISH");
 if env::var("CI_FIXTURE_PUBLISH").as_deref()!=Ok("1"){return;}
 let root=PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).parent().unwrap().to_path_buf();
 let generated=root.join("node_modules/generated");fs::create_dir_all(generated.join("chosen")).unwrap();
 let conflict=generated.join("chosen.rs");
 if conflict.exists(){assert_eq!(fs::read(&conflict).unwrap(),b"pub const VALUE:&str=\"NEW\";\n");return;}
 // No mutable cross-unit barrier: verify the observed fresh/publication order
 // from this fixture's event timestamps after completion. They are not cache
 // identities or trusted producer evidence.
 thread::sleep(Duration::from_secs(2));
 let published=SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
 let old=SystemTime::UNIX_EPOCH+Duration::from_secs(946684800);
 for (name,data) in [("mod.rs",b"mod chosen;pub fn value()->&'static str{chosen::VALUE}\n".as_slice()),
   ("chosen/mod.rs",b"pub const VALUE:&str=\"OLD\";\n".as_slice()),
   ("chosen.rs",b"pub const VALUE:&str=\"NEW\";\n".as_slice())]{
  let path=generated.join(name);fs::write(&path,data).unwrap();
  File::open(path).unwrap().set_times(FileTimes::new().set_accessed(old).set_modified(old)).unwrap();
 }
 fs::write(generated.join("publication.json"),format!("{{\"published_at_ns\":{published}}}\n")).unwrap();
 println!("cargo:warning=fixture published generated conflict");
}
'''
COUNTER=r'''#!/usr/bin/python3
import json,os,sys
from pathlib import Path
if '--crate-name' in sys.argv and not any(value.startswith('--print') for value in sys.argv):
 name=sys.argv[sys.argv.index('--crate-name')+1]
 with Path(os.environ['CI_FIXTURE_COUNTER']).open('a') as output:
  output.write(json.dumps({'crate':name})+'\n')
os.execvp(sys.argv[1],sys.argv[1:])
'''


def prepare(root):
    root=Path(root);(root/'src').mkdir();(root/'publisher/src').mkdir(parents=True)
    files={
      'Cargo.toml':'[package]\nname="'+PACKAGE+'"\nversion="0.0.0"\nedition="2021"\n[workspace]\nmembers=["publisher"]\nresolver="2"\n',
      'Cargo.lock':'version=4\n[[package]]\nname="'+PACKAGE+'"\nversion="0.0.0"\n[[package]]\nname="generated-input-publisher"\nversion="0.0.0"\n',
      'package.json':'{"name":"publication-fixture","private":true}\n',
      'src/main.rs':'#[path="../node_modules/generated/mod.rs"] mod generated;\nfn main(){println!("{}",generated::value());}\n',
      'publisher/Cargo.toml':'[package]\nname="generated-input-publisher"\nversion="0.0.0"\nedition="2021"\n',
      'publisher/src/lib.rs':'pub fn unrelated()->u8{0}\n',
      'publisher/build.rs':PUBLISHER,'rustc-counter.py':COUNTER,
    }
    for name,data in files.items():(root/name).write_text(data)
    (root/'rustc-counter.py').chmod(0o755)


def seed(root):
    root=Path(root);generated=root/'node_modules/generated';(generated/'chosen').mkdir(parents=True)
    for name,data in [('mod.rs','mod chosen;pub fn value()->&\'static str{chosen::VALUE}\n'),
            ('chosen/mod.rs','pub const VALUE:&str="OLD";\n')]:
        path=generated/name;path.write_text(data);os.utime(path,ns=(946684800000000000,946684800000000000))


def run_cargo(root,*,publish,cargo=None,cargo_cwd=None,direct_control=True):
    root=Path(root);target=Path(os.environ['CARGO_TARGET_DIR'])
    counter=root/'node_modules/generated/rustc-count.jsonl';counter.unlink(missing_ok=True)
    conflict=root/'node_modules/generated/chosen.rs'
    if conflict.exists():raise RuntimeError('fixture must start without the conflicting module')
    before={name:(root/'node_modules/generated'/name).stat().st_mtime_ns for name in ('mod.rs','chosen/mod.rs')}
    original_bytes={name:(root/'node_modules/generated'/name).read_bytes() for name in before}
    environment=dict(os.environ,RUSTC_WRAPPER=str(root/'rustc-counter.py'),CARGO_INCREMENTAL='0',CI_FIXTURE_PUBLISH='1' if publish else '0',CI_FIXTURE_COUNTER=str(counter))
    command=[*(cargo or ['cargo']),'build','--manifest-path',str(root/'Cargo.toml'),'--offline','--locked',
        '--message-format=json',*(['--workspace'] if publish else ['-p',PACKAGE])]
    process=subprocess.Popen(command,cwd=cargo_cwd or root,env=environment,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    fresh=False;fresh_at_ns=None;fresh_absent=False;messages=[];stderr=bytearray();pending=bytearray();total=0;deadline=time.monotonic()+120
    try:
        with selectors.DefaultSelector() as streams:
            streams.register(process.stdout,selectors.EVENT_READ,'stdout');streams.register(process.stderr,selectors.EVENT_READ,'stderr')
            while streams.get_map():
                if time.monotonic()>=deadline:raise RuntimeError('fixture cargo deadline exceeded')
                for key,_ in streams.select(.05):
                    block=os.read(key.fileobj.fileno(),65536)
                    if not block:streams.unregister(key.fileobj);continue
                    total+=len(block)
                    if total>4*1024**2:raise RuntimeError('fixture output exceeded bound')
                    if key.data=='stderr':stderr.extend(block);continue
                    pending.extend(block)
                    while b'\n' in pending:
                        line,_,remaining=pending.partition(b'\n');pending=bytearray(remaining)
                        try:message=json.loads(line)
                        except ValueError:continue
                        messages.append(message)
                        if (message.get('reason')=='compiler-artifact' and message.get('target',{}).get('name')==PACKAGE
                                and message.get('fresh') is True):
                            fresh=True;fresh_at_ns=time.time_ns();fresh_absent=not conflict.exists()
        code=process.wait(timeout=5)
    finally:
        if process.poll() is None:
            os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
        process.stdout.close();process.stderr.close()
    counts=[json.loads(line)['crate'] for line in counter.read_text().splitlines()] if counter.exists() else []
    binary=None
    if code==0:
        binary_result=subprocess.run([str(target/'debug'/PACKAGE)],capture_output=True,timeout=5)
        if binary_result.returncode!=0:raise RuntimeError('fixture binary failed unexpectedly')
        binary=binary_result.stdout.decode().strip()
    control=None
    if publish and not conflict.is_file():raise RuntimeError('publisher did not produce the conflicting module')
    if publish and direct_control:
        direct=subprocess.run(['rustc','--edition=2021','--crate-name','publication_control',str(root/'src/main.rs'),
            '-o',str(target/'publication-control')],capture_output=True,timeout=30)
        control={'exit':direct.returncode,'e0761':b'E0761' in direct.stderr}
    publication=json.loads((root/'node_modules/generated/publication.json').read_text()) if publish else None
    ordered=bool(publish and fresh and fresh_absent and type(publication.get('published_at_ns')) is int
        and publication['published_at_ns']>fresh_at_ns)
    if publish and not ordered:raise RuntimeError('fixture did not observe publication after the fresh decision')
    after={name:(root/'node_modules/generated'/name).stat().st_mtime_ns for name in before}
    diagnostics=[message.get('message',{}).get('code',{}).get('code') for message in messages
        if message.get('reason')=='compiler-message' and isinstance(message.get('message',{}).get('code'),dict)]
    return {'cargo_exit':code,'consumer_actual_rustc_calls':counts.count('generated_input_consumer'),
        'all_actual_rustc_crates':counts,'consumer_reported_fresh':fresh,'binary_output':binary,
        'publication_after_fresh_decision':ordered,'event_ordering_observation_only':True,
        'original_read_file_mtimes_unchanged':before==after,
        'original_read_file_bytes_unchanged':all((root/'node_modules/generated'/name).read_bytes()==data for name,data in original_bytes.items()),'conflict_mtime_ns':conflict.stat().st_mtime_ns if conflict.exists() else None,
        'diagnostics':diagnostics,'direct_compiler_control':control,'full_ci_pass':False,'shipping_authority':False}


if __name__=='__main__':
    root=Path.cwd()
    if sys.argv[1:]==['prepare']:prepare(root)
    elif sys.argv[1:]==['seed']:seed(root)
    elif sys.argv[1:] in (['warm'],['publish']):
        result=run_cargo(root,publish=sys.argv[1]=='publish');print(json.dumps(result),flush=True);raise SystemExit(result['cargo_exit'])
    else:raise SystemExit('unsupported fixed diagnostic operation')
