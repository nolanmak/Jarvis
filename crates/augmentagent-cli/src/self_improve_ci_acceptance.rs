//! Opt-in real build-VM smoke. No provider calls, publication or merge authority.
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_EXEC_DENIAL_LIBRARY:&[u8]=include_bytes!(concat!(env!("OUT_DIR"),"/libci_cargo_exec.so"));
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_EXEC_DENIAL_CASES:&[u8]=include_bytes!("ci_cargo_exec_cases.py");
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_RELAY_LIBRARY:&[u8]=include_bytes!(concat!(env!("OUT_DIR"),"/libci_cargo_relay.so"));

/// Credential and V3 transport feasibility only. Generated input closure,
/// protected unit outputs and passing-CI publication are still unavailable.
#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_planner_protocol_v3()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private probe evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-planner-protocol-v3",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("planner-protocol-source-").tempdir_in(&target)?;
    let root=fixture.path();
    git(root,&["-c","init.templateDir=","init","-q"]).await?;
    std::fs::write(root.join("source.txt"),"literal immutable protocol fixture\n")?;
    std::fs::create_dir(root.join("src"))?;
    std::fs::write(root.join("Cargo.toml"),"[package]\nname=\"private-cargo-probe\"\nversion=\"0.0.0\"\nedition=\"2021\"\n")?;
    std::fs::write(root.join("Cargo.lock"),"version=4\n[[package]]\nname=\"private-cargo-probe\"\nversion=\"0.0.0\"\n")?;
    std::fs::write(root.join("src/main.rs"),r####"fn main(){
 assert_eq!(private_cargo_probe::value(),7);
 let out=std::path::PathBuf::from(env!("OUT_DIR"));
 assert_eq!(std::fs::read(out.join("generated.rs")).unwrap(),b"pub const GENERATED:u8=7;\n");
 assert!(out.join("empty").is_dir());
 assert_eq!(std::fs::read_dir(out.join("empty")).unwrap().count(),0);
 assert!(std::fs::write(out.join("forbidden"),b"x").is_err());
 assert!(std::fs::rename(&out,out.with_extension("renamed")).is_err());
 let root=std::path::Path::new("target/debug/.fingerprint");
 std::fs::create_dir_all(root).unwrap();
 std::fs::write(root.join("unit-forged"),b"private unit only").unwrap();
 println!("ISOLATED");
}
"####)?;
    std::fs::write(root.join("src/lib.rs"),r####"include!(concat!(env!("OUT_DIR"),"/generated.rs"));
pub fn value()->u8{GENERATED}
"####)?;
    std::fs::write(root.join("build.rs"),r####"fn main(){
 let out=std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
 std::fs::create_dir_all(out.join("empty")).unwrap();
 std::fs::write(out.join("generated.rs"),b"pub const GENERATED:u8=7;\n").unwrap();
 let metadata=std::path::Path::new("target/debug/.fingerprint");
 std::fs::create_dir_all(metadata).unwrap();
 std::fs::write(metadata.join("build-script-forged"),b"private unit only").unwrap();
 println!("cargo:rerun-if-changed=build.rs");
}
"####)?;
    git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
        "-c","core.hooksPath=/dev/null","commit","-qm","protocol fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
    let session=vm::Session::prepare(&owner,&target)?;session.enable_planner_protocol_probe()?;
    let observed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("true"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    ensure!(observed.status=="passed","V3 probe command failed: {}",observed.stderr);
    let probe=observed.planner_protocol_probe.context("original V3 probe receipt missing")?;
    ensure!(probe["version"]==3 && probe["cases"][0]["uid"]==2000 && probe["cases"][1]["uid"]==2001
        && probe["cases"][2]["cancelled_before_exec"]==true
        && probe["cases"][3]["uid"]==2003 && probe["cases"][3]["signal_forwarded"]==10
        && probe["cases"][3]["proxy_signal"]==10 && probe["units"][3]["returncode"]==-10
        && probe["units"][3]["started"]==true && probe["cases"][4]["uid"]==2004
        && probe["cases"][4]["filesystem"]["input_stable"]==true
        && probe["filesystem"]["input_inode_metadata_preserved"]==true
        && probe["filesystem"]["allocation_removed"]==true && probe["shipping_authority"]==false
        && probe["cargo"]["compiler_calls"]==serde_json::json!([3,0])
        && probe["cargo"]["build_script_calls"]==serde_json::json!([1,0])
        && probe["cargo"]["generated_tree_preserved"]==true && probe["cargo"]["library_input_mounted"]==true
        && probe["cargo"]["application_runs"]==2 && probe["cargo"]["actual_metadata_preserved"]==true
        && probe["cargo"]["sealed_artifact_detached"]==true && probe["cargo"]["allocation_removed"]==true
        && probe["kernel_observation"]["witness_observed"]==true
        && probe["kernel_observation"]["positive_query_observed"]==true
        && probe["kernel_observation"]["negative_query_observed"]==true
        && probe["kernel_observation"]["max_path_query_observed"]==true
        && probe["kernel_observation"]["binary_path_query_observed"]==true
        && probe["kernel_observation"]["missed_probe_census_complete"]==true
        && probe["kernel_observation"]["probe_census"]["before"]["links"].as_array().is_some_and(|values|values.len()==16)
        && probe["kernel_observation"]["probe_census"]["after"]["programs"].as_array().is_some_and(|values|values.len()==16)
        && probe["kernel_observation"]["lookup_base_query_observed"]==true
        && probe["kernel_observation"]["lookup_changed_cwd_observed"]==true
        && probe["kernel_observation"]["lookup_invalid_base_observed"]==true
        && probe["kernel_observation"]["lookup_base_attempts"].as_u64().is_some_and(|value|value>=4)
        && probe["kernel_observation"]["path_metadata_query_observed"]==true
        && probe["kernel_observation"]["statx_copy_verified"]==true
        && probe["kernel_observation"]["path_metadata_failed_copy_observed"]==true
        && probe["kernel_observation"]["path_metadata_lookup_failure_observed"]==true
        && probe["kernel_observation"]["fstat_query_observed"]==true
        && probe["kernel_observation"]["fstat_invalid_fd_observed"]==true
        && probe["kernel_observation"]["fstat_failed_copy_observed"]==true
        && probe["kernel_observation"]["metadata_queries"].as_u64().is_some_and(|value|value>=4)
        && probe["kernel_observation"]["directory_witness_observed"]==true
        && probe["kernel_observation"]["binary_directory_name_observed"]==true
        && probe["kernel_observation"]["directory_short_read_observed"]==true
        && probe["kernel_observation"]["directory_eof_observed"]==true
        && probe["kernel_observation"]["directory_cursor_query_observed"]==true
        && probe["kernel_observation"]["cargo_directory_queries_observed"]==true
        && probe["kernel_observation"]["directory_reads"].as_u64().is_some_and(|value|value>=14)
        && probe["kernel_observation"]["directory_entries"].as_u64().is_some_and(|value|value>=13)
        && probe["kernel_observation"]["after_kernel_copy"]==true
        && probe["kernel_observation"]["compiler_queries_observed"]==true
        && probe["kernel_observation"]["library_generated_query_observed"]==true
        && probe["kernel_observation"]["original_observer_cleanup"]==true
        && probe["kernel_observation"]["allocation_removed"]==true
        && probe["kernel_observation"]["input_closure_complete"]==false,
        "V3 probe did not establish original credential/cancellation observations");
    audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_planner_protocol_v3","head":head,
        "adapter_authority":session.authority_sha256,"probe":probe,"full_ci_pass":false,
        "cargo_metadata_isolation_established":false,"trusted_input_closure":false,
        "shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    Ok(())
}

#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[test]
fn cargo_relay_spawn_interface_fixture() {
    if std::env::var("JARVIS_CARGO_RELAY_FIXTURE").as_deref()!=Ok("1") {return;}
    use std::os::unix::ffi::OsStringExt;
    let args=[c"true".as_ptr(),std::ptr::null()];
    let env:[*const libc::c_char;1]=[std::ptr::null()];
    assert_eq!(unsafe {libc::syscall(libc::SYS_execve,c"/bin/true".as_ptr(),args.as_ptr(),env.as_ptr())},-1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(libc::EPERM));
    println!("RAW_EXEC_DENIED");
    let mode=std::env::var("JARVIS_RELAY_TEST_MODE").unwrap_or_default();
    if mode=="signal-state" {
        let mut mask=unsafe {std::mem::zeroed::<libc::sigset_t>()};
        assert_eq!(unsafe {libc::sigemptyset(&mut mask)},0);
        assert_eq!(unsafe {libc::sigaddset(&mut mask,libc::SIGUSR1)},0);
        assert_eq!(unsafe {libc::pthread_sigmask(libc::SIG_BLOCK,&mask,std::ptr::null_mut())},0);
        assert_ne!(unsafe {libc::signal(libc::SIGUSR2,libc::SIG_IGN)},libc::SIG_ERR);
        let output=std::process::Command::new("/usr/bin/python3").args(["-I","-c",
            "import signal;assert signal.SIGUSR1 in signal.pthread_sigmask(signal.SIG_BLOCK,[]);assert signal.getsignal(signal.SIGUSR2)==signal.SIG_IGN"]).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
        println!("RELAY_SIGNAL_STATE_COMPLETE");return;
    }
    if mode=="signal-exit" {
        use std::os::unix::process::ExitStatusExt;
        let ordinary=std::process::Command::new("/bin/sh").args(["-c","exit 143"]).output().unwrap();
        assert_eq!(ordinary.status.code(),Some(143));
        assert_eq!(ordinary.status.signal(),None);
        let output=std::process::Command::new("/bin/sh").args(["-c","kill -TERM $$"]).output().unwrap();
        assert_eq!(output.status.signal(),Some(libc::SIGTERM));
        assert_eq!(output.status.code(),None);
        println!("RELAY_SIGNAL_EXIT_COMPLETE");return;
    }
    if mode=="signal-forward" {
        use std::io::BufRead;
        let mut child=std::process::Command::new("/usr/bin/python3").args(["-I","-c",
            "import signal,time;signal.signal(signal.SIGUSR2,lambda *_:(print('HANDLED',flush=True),exit(0)));print('READY',flush=True);time.sleep(30)"])
            .stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut output=std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line=String::new();output.read_line(&mut line).unwrap();assert_eq!(line,"READY\n");
        assert_eq!(unsafe {libc::kill(child.id() as libc::pid_t,libc::SIGUSR2)},0);
        line.clear();output.read_line(&mut line).unwrap();assert_eq!(line,"HANDLED\n");
        assert!(child.wait().unwrap().success());
        println!("RELAY_SIGNAL_FORWARD_COMPLETE");return;
    }
    let job=std::env::var("JARVIS_RELAY_TEST_JOB_FD").unwrap();
    let output=std::process::Command::new("/usr/bin/python3").args(["-I","-c",&format!(
        "import os;assert os.environb[b'RAW']==b'\\xffdata';assert os.read({job},1)==b'+';os.write(1,b'x'*(1024*1024));raise SystemExit(17)")])
        .env("RAW",std::ffi::OsString::from_vec(b"\xffdata".to_vec()))
        .current_dir(std::env::current_dir().unwrap()).output().unwrap();
    assert_eq!(output.status.code(),Some(17));
    assert_eq!(output.stdout,vec![b'x';1024*1024]);
    let missing=std::process::Command::new("/missing-jarvis-relay-tool").output().unwrap_err();
    assert_eq!(missing.raw_os_error(),Some(libc::ENOENT));
    println!("RELAY_STD_COMPLETE");
}

#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[test]
fn same_uid_cargo_relay_exercises_real_rust_spawns_and_refuses_unsupported_requests() {
    let directory=tempfile::tempdir().unwrap();let library=directory.path().join("relay.so");
    std::fs::write(&library,CARGO_RELAY_LIBRARY).unwrap();
    for (name,count) in [("ci_cargo_relay_test.py",5),("ci_cargo_relay_native_test.py",8)] {
        let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests").join(name);
        let output=std::process::Command::new("python3").arg("-I").arg(tests)
            .env("AUGMENTAGENT_CARGO_RELAY_LIBRARY",&library)
            .env("AUGMENTAGENT_RELAY_FIXTURE_BINARY",std::env::current_exe().unwrap()).output().unwrap();
        let diagnostics=String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(),"relay cases failed: {diagnostics}");
        assert!(diagnostics.contains(&format!("Ran {count} tests")) && !diagnostics.contains("skipped"),
            "relay cases did not execute: {diagnostics}");
    }
}

/// Interface feasibility only: every process still has the ordinary worker UID.
#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_same_uid_cargo_relay()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"same-uid-cargo-relay",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("cargo-relay-fixture-").tempdir_in(&target)?;
    let root=fixture.path();
    for (path,bytes) in [
        ("relay.so",CARGO_RELAY_LIBRARY),
        ("ci-cargo-relay.py",include_bytes!("../../../scripts/ci-cargo-relay.py").as_slice()),
        ("provider-supervisor.py",include_bytes!("../../../scripts/provider-supervisor.py").as_slice()),
        ("app/signal-probe.c",include_bytes!("ci_cargo_signal_probe.c").as_slice()),
        ("Cargo.toml",b"[workspace]\nmembers=['app','macros']\nresolver='2'\n".as_slice()),
        ("Cargo.lock",b"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"relay-app\"\nversion = \"0.1.0\"\ndependencies = [\n \"relay-macros\",\n]\n\n[[package]]\nname = \"relay-macros\"\nversion = \"0.1.0\"\n".as_slice()),
        ("app/Cargo.toml",b"[package]\nname='relay-app'\nversion='0.1.0'\nedition='2021'\nbuild='build.rs'\n[dependencies]\nrelay-macros={path='../macros'}\n".as_slice()),
        ("app/build.rs",b"fn main(){let p=std::path::PathBuf::from(std::env::var_os(\"OUT_DIR\").unwrap());std::fs::write(p.join(\"generated.rs\"),\"const GENERATED:usize=17;\").unwrap();assert!(std::process::Command::new(\"cc\").args([\"-std=c11\",\"-Wall\",\"-Wextra\",\"-Werror\",\"signal-probe.c\",\"-o\"]).arg(p.join(\"relay-signal-probe\")).status().unwrap().success());println!(\"cargo:rerun-if-changed=build.rs\");println!(\"cargo:rerun-if-changed=signal-probe.c\");}\n".as_slice()),
        ("app/src/lib.rs",b"include!(concat!(env!(\"OUT_DIR\"),\"/generated.rs\"));relay_macros::pass!{pub fn value()->usize{GENERATED}}\n#[test]fn generated_input(){assert_eq!(value(),17);}\n".as_slice()),
        ("macros/Cargo.toml",b"[package]\nname='relay-macros'\nversion='0.1.0'\nedition='2021'\n[lib]\nproc-macro=true\n".as_slice()),
        ("macros/src/lib.rs",b"extern crate proc_macro;#[proc_macro]pub fn pass(input:proc_macro::TokenStream)->proc_macro::TokenStream{input}\n".as_slice()),
    ] {
        let path=root.join(path);std::fs::create_dir_all(path.parent().unwrap())?;std::fs::write(path,bytes)?;
    }
    git(root,&["init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","same UID Cargo relay fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let binding=binding(root).await?;let session=vm::Session::prepare(&owner,&target)?;
    let source=snapshot(root,&binding,&target).await?;
    let command=r#"python3 -I - <<'PY'
import importlib.util,json,os,shutil,subprocess,tempfile,threading
from pathlib import Path
control=Path(tempfile.mkdtemp(prefix='jcr-',dir='/tmp'));control.chmod(0o700)
library=Path('/workspace/target/relay-experiment.so');shutil.copyfile('/workspace/relay.so',library)
for name in ('ci-cargo-relay.py','provider-supervisor.py'):shutil.copyfile('/workspace/'+name,control/name)
spec=importlib.util.spec_from_file_location('relay',control/'ci-cargo-relay.py');relay=importlib.util.module_from_spec(spec);spec.loader.exec_module(relay)
results=[];signal_result=None;requests=[];fingerprints=[];phase='initializing';observations_lock=threading.Lock()
identity_environment={b'CARGO_CRATE_NAME',b'CARGO_MANIFEST_DIR',b'CARGO_MANIFEST_PATH',b'CARGO_PKG_NAME',
 b'CARGO_PKG_VERSION',b'CARGO_PRIMARY_PACKAGE',b'CARGO_HOME',b'OUT_DIR',b'HOST',b'TARGET',b'PROFILE',
 b'OPT_LEVEL',b'DEBUG',b'RUSTFLAGS',b'CARGO_ENCODED_RUSTFLAGS',b'CARGO_MAKEFLAGS',b'MAKEFLAGS'}
server=relay.Relay(control,library=library,timeout=30)
receive=server._receive
def observe(connection,descriptors):
 numbers=receive(connection,descriptors)
 file,argv,environment,cwd,search,ignored,blocked=relay._payload(descriptors[0])
 # Synthetic fixture only: preserve exact argv and selected build variables,
 # never the complete inherited environment. This trace grants no authority.
 value={'phase':phase,'file':os.fsdecode(file),'argv':[os.fsdecode(arg) for arg in argv],
  'cwd':os.fsdecode(cwd),'environment':{os.fsdecode(k):os.fsdecode(v) for k,v in environment.items() if k in identity_environment},
  'inherited_pipe_numbers':numbers[3:]}
 with observations_lock:requests.append(value)
 return numbers
server._receive=observe
with server:
 for attempt in range(2):
  phase='cargo-'+str(attempt)
  fd=server.config_fd()
  try:
   environment=os.environ.copy();environment.update(LD_PRELOAD=str(library),JARVIS_CARGO_RELAY_FD=str(fd))
   result=subprocess.run(['/toolchain/bin/cargo','test','--workspace','--offline','--locked'],cwd='/workspace',env=environment,pass_fds=(fd,),capture_output=True,text=True,timeout=120)
  finally:os.close(fd)
  results.append({'exit_code':result.returncode,'stdout':result.stdout[-6000:],'stderr':result.stderr[-6000:]})
  snapshot={}
  for path in Path('/workspace/target/debug/.fingerprint').glob('*/*.json'):
   assert len(snapshot)<128 and path.stat().st_size<=65536,'bounded Cargo metadata observation required'
   snapshot[str(path.relative_to('/workspace/target'))]=json.loads(path.read_bytes())
  fingerprints.append({'phase':phase,'records':snapshot})
  if result.returncode:break
 if len(results)==2 and all(result['exit_code']==0 for result in results):
  phase='signal-probe'
  probes=list(Path('/workspace/target/debug/build').glob('relay-app-*/out/relay-signal-probe'))
  assert len(probes)==1,probes
  fd=server.config_fd()
  try:
   environment=os.environ.copy();environment.update(LD_PRELOAD=str(library),JARVIS_CARGO_RELAY_FD=str(fd))
   checked=subprocess.run([str(probes[0])],env=environment,pass_fds=(fd,),capture_output=True,text=True,timeout=30)
  finally:os.close(fd)
  signal_result={'exit_code':checked.returncode,'stdout':checked.stdout[-2000:],'stderr':checked.stderr[-2000:]}
 completed=list(server.completed);errors=list(server.errors)
compilations=[value for value in requests if Path(value['file']).name=='rustc' and '--crate-name' in value['argv']
 and value['argv'][value['argv'].index('--crate-name')+1] in ('relay_app','relay_macros','build_script_build')]
print(json.dumps({'cargo_runs':results,'signal_probe':signal_result,'supervised_tools':len(completed),
 'cargo_requests':requests,'cargo_fingerprints':fingerprints,'compiler_units_first_run':sum(v['phase']=='cargo-0' for v in compilations),
 'compiler_units_second_run':sum(v['phase']=='cargo-1' for v in compilations),
 'cleanup_verified':all(r['cleanup']=='all-descendants-reaped' for r in completed),'relay_errors':errors,
 'cargo_metadata_isolation_established':False,'shipping_authority':False}))
assert len(results)==2 and all(r['exit_code']==0 for r in results),results
assert signal_result and signal_result['exit_code']==0 and 'RELAY_GUEST_SIGNALS_COMPLETE' in signal_result['stdout'],signal_result
assert len(completed)>=4 and not errors
assert compilations and not any(value['phase']=='cargo-1' for value in compilations),'unchanged Cargo units recompiled'
assert any('--crate-type' in value['argv'] and 'proc-macro' in value['argv'] for value in compilations),'proc macro unit missing'
assert any('--test' in value['argv'] for value in compilations),'test unit missing'
assert any(Path(value['file']).name=='build-script-build' for value in requests),'build script execution missing'
assert len(fingerprints)==2 and all(value['records'] for value in fingerprints),'protected graph candidate needs actual Cargo metadata'
PY"#;
    let result=session.run("repository",&source,root,&binding,None,&[],&[],&[],Some(command),
        &storage,&target,&audit,std::time::Duration::from_secs(300)).await?;
    let observations:serde_json::Value=serde_json::from_str(&result.stdout).unwrap_or(serde_json::Value::Null);
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_same_uid_cargo_relay","head":head,"generation":source.generation(),
        "status":result.status,"observations":observations,"stderr":result.stderr,
        "actual_vm_commands":1,"cargo_metadata_isolation_established":false,"full_ci_pass":false,
        "shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    ensure!(result.status=="passed","same UID Cargo relay guest regression failed: {}",result.stdout);
    drop(source);drop(session);drop(storage);Ok(())
}

#[cfg(target_os="linux")]
#[test]
fn supervised_descriptor_handoff_requires_original_descendant_cleanup() {
    let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests/provider_supervisor_test.py");
    let output=std::process::Command::new("python3").arg("-I").arg(tests).output().unwrap();
    let diagnostics=String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(),"supervised descriptor cases failed: {diagnostics}");
    assert!(diagnostics.contains("Ran 5 tests") && !diagnostics.contains("skipped"),
        "supervised descriptor cases did not execute: {diagnostics}");
}

#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[test]
fn native_exec_denial_is_process_local_and_covers_real_syscalls() {
    let directory=tempfile::tempdir().unwrap();let library=directory.path().join("native.so");
    std::fs::write(&library,CARGO_EXEC_DENIAL_LIBRARY).unwrap();
    let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests/ci_cargo_native_test.py");
    let output=std::process::Command::new("python3").arg("-I").arg(tests)
        .env("AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY",library).output().unwrap();
    let diagnostics=String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(),"native syscall cases failed: {diagnostics}");
    assert!(diagnostics.contains("Ran 4 tests") && !diagnostics.contains("skipped"),"native cases did not execute: {diagnostics}");
    assert!(std::process::Command::new("/usr/bin/true").status().unwrap().success(),"child filter affected the Rust parent");
}

/// Exercise the controller-built primitive as an unprivileged guest process.
/// This does not run Cargo under a new identity or establish metadata isolation.
#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_native_exec_denial()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-native-exec-denial",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("native-exec-denial-fixture-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::write(root.join("native.so"),CARGO_EXEC_DENIAL_LIBRARY)?;
    std::fs::write(root.join("cases.py"),CARGO_EXEC_DENIAL_CASES)?;
    git(root,&["init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","native exec denial fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let binding=binding(root).await?;let session=vm::Session::prepare(&owner,&target)?;
    let source=snapshot(root,&binding,&target).await?;
    let command=r#"python3 -I - <<'PY'
import json,shutil,signal,subprocess,sys
shutil.copyfile('/workspace/native.so','/workspace/target/native.so')
results={}
for mode in ('execve','execveat','fork','x32'):
 result=subprocess.run([sys.executable,'-I','/workspace/cases.py','/workspace/target/native.so',mode],capture_output=True,text=True,timeout=15)
 expected=-signal.SIGSYS if mode=='x32' else 0
 results[mode]={'exit_code':result.returncode,'stdout':result.stdout,'stderr':result.stderr}
 assert result.returncode==expected,results[mode]
 if mode!='x32':assert 'DENIED\n' in result.stdout,results[mode]
 if mode=='fork':assert 'REAPED\n' in result.stdout,results[mode]
print(json.dumps(results))
PY"#;
    let result=session.run("repository",&source,root,&binding,None,&[],&[],&[],Some(command),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let cases:serde_json::Value=serde_json::from_str(&result.stdout).unwrap_or(serde_json::Value::Null);
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_native_exec_denial","head":head,"generation":source.generation(),
        "status":result.status,"cases":cases,"actual_vm_commands":1,"cargo_metadata_isolation_established":false,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    ensure!(result.status=="passed" && cases.as_object().is_some_and(|cases|cases.len()==4),
        "native exec denial unavailable: {} {}",result.stdout,result.stderr);
    Ok(())
}

/// Query the actual guest ABI without granting privileged execution. A
/// successful capability observation is deliberately not an enforcement proof.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_cargo_execution_capabilities()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-cargo-capabilities",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("cargo-capability-fixture-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::write(root.join("README.md"),"Execution capability fixture\n")?;
    git(root,&["init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","execution capability fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let binding=binding(root).await?;let session=vm::Session::prepare(&owner,&target)?;
    let source=snapshot(root,&binding,&target).await?;
    let result=session.run("repository",&source,root,&binding,None,&[],&[],&[],
        Some("/usr/bin/python3 -I /ci-tools/ci-cargo-boundary.py --cargo /toolchain/bin/cargo"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let observation:serde_json::Value=serde_json::from_str(&result.stdout)?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_cargo_execution_capabilities","head":head,
        "generation":source.generation(),"status":result.status,"observation":observation,
        "actual_vm_commands":1,"full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    ensure!(result.status=="passed" && observation["kind"]=="ci-cargo-execution-capabilities"
        && observation["seccomp_user_notification"]==true && observation["process_vm_self_read"]==true
        && observation["pidfd"]==true && observation["enforcement_established"]==false
        && observation["shipping_authority"]==false,"guest execution prerequisites unavailable: {}",result.stderr);
    Ok(())
}

/// The original Git source remains fixed while a declared generated input
/// changes between commands. Equal old timestamps must not resurrect old code.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_generated_input_mutation()->Result<()> {
    generated_input_mutation(false,false,false).await
}

/// Build-script rerun inputs need the same content-correctness boundary as
/// include_str! inputs; Cargo's compiler checksum mode alone does not cover it.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_generated_build_script_mutation()->Result<()> {
    generated_input_mutation(true,false,false).await
}

/// A newly introduced module candidate must invalidate reuse even though every
/// previously read source file retains its exact bytes and old timestamp.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_generated_module_resolution_mutation()->Result<()> {
    generated_input_mutation(false,true,false).await
}

/// Observation feasibility only. The original adapter regression above remains
/// independent and must go green through actual adapter invalidation.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_generated_module_lookup_trace()->Result<()> {
    generated_input_mutation(false,true,true).await
}

/// A publication inside one Cargo invocation must revoke a prior fresh
/// decision even when its consumer was skipped and never reached the relay.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_in_command_generated_publication()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private fixture evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-in-command-publication",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("cargo-publication-fixture-").tempdir_in(&target)?;
    let root=fixture.path();
    let driver=root.join("publication_fixture.py");
    std::fs::write(&driver,include_bytes!("ci_cargo_publication_fixture.py"))?;
    let prepared=tokio::process::Command::new("python3").args(["-I"]).arg(&driver).arg("prepare")
        .current_dir(root).output().await?;
    ensure!(prepared.status.success(),"fixed publication fixture preparation failed");
    git(root,&["-c","init.templateDir=","init","-q"]).await?;
    git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null",
        "-c","commit.gpgsign=false","commit","-qm","in-command publication fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let session=vm::Session::prepare(root,&target)?;
    let source=snapshot(root,&bound,&target).await?;let timeout=std::time::Duration::from_secs(180);
    let seeded=session.run("repository",&source,root,&bound,None,&[],&[],&[],
        Some("python3 -I publication_fixture.py seed"),&storage,&target,&audit,timeout).await?;
    ensure!(seeded.status=="passed","fixture initial generator failed");
    let warm=session.run("repository",&source,root,&bound,None,&[],&[],&[],
        Some("python3 -I publication_fixture.py warm"),&storage,&target,&audit,timeout).await?;
    std::fs::write(evidence.join("warm.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "status":&warm.status,"stdout":&warm.stdout,"stderr":&warm.stderr,"shipping_authority":false}))?)?;
    let first:serde_json::Value=serde_json::from_str(warm.stdout.trim()).context("warm fixture result missing; inspect private warm.json")?;
    ensure!(warm.status=="passed" && first["cargo_exit"]==0 && first["consumer_actual_rustc_calls"]==2
        && first["binary_output"]=="OLD","initial consumer library/binary did not compile and execute");
    let unchanged=session.run("repository",&source,root,&bound,None,&[],&[],&[],
        Some("python3 -I publication_fixture.py warm"),&storage,&target,&audit,timeout).await?;
    std::fs::write(evidence.join("unchanged.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "status":&unchanged.status,"stdout":&unchanged.stdout,"stderr":&unchanged.stderr,"shipping_authority":false}))?)?;
    let compatible:serde_json::Value=serde_json::from_str(unchanged.stdout.trim()).context("unchanged fixture result missing; inspect private unchanged.json")?;
    ensure!(unchanged.status=="passed" && compatible["cargo_exit"]==0 && compatible["consumer_actual_rustc_calls"]==0
        && compatible["binary_output"]=="OLD","unchanged consumer failed minimum reuse");
    let observed=session.run("repository",&source,root,&bound,None,&[],&[],&[],
        Some("python3 -I publication_fixture.py publish"),&storage,&target,&audit,timeout).await?;
    std::fs::write(evidence.join("publish.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "status":&observed.status,"stdout":&observed.stdout,"stderr":&observed.stderr,"shipping_authority":false}))?)?;
    let actual:serde_json::Value=serde_json::from_str(observed.stdout.trim()).context("publication fixture result missing; inspect private publish.json")?;
    ensure!(actual["publication_after_fresh_decision"]==true && actual["original_read_file_mtimes_unchanged"]==true
        && actual["original_read_file_bytes_unchanged"]==true && actual["conflict_mtime_ns"]==946684800000000000_u64,
        "fixture did not establish its in-command old-mtime publication order");
    let direct=&actual["direct_compiler_control"];
    ensure!(direct["e0761"]==true && direct["exit"].as_i64().is_some_and(|value|value!=0),
        "independent compiler did not reject the generated conflict");
    let fixed=observed.status=="failed" && actual["cargo_exit"].as_i64().is_some_and(|value|value!=0)
        && actual["diagnostics"].as_array().is_some_and(|values|values.iter().any(|value|value=="E0761"))
        && actual["consumer_actual_rustc_calls"].as_u64().is_some_and(|value|value>0);
    let report=serde_json::json!({"version":1,"kind":"real_shared_ci_in_command_generated_publication",
        "head":head,"generation":source.generation(),"adapter_authority":session.authority_sha256,
        "warm":first,"unchanged":compatible,"observed":actual,"status":observed.status,"passed":fixed,
        "input_behavior_fixed":fixed,"actual_vm_commands":4,"fixture_observations_are_not_input_proof":true,
        "trusted_input_closure":false,"cargo_metadata_isolation_established":false,"full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0});
    audit.record_controller(report.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&report)?)?;
    ensure!(fixed,"in-command generated publication left a skipped consumer unverified; see private receipt");
    Ok(())
}

async fn generated_input_mutation(build_script:bool,resolution:bool,read_trace:bool)->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-generated-input-mutation",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("generated-input-fixture-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir(root.join("src"))?;
    if read_trace {
        for (name,bytes) in [
            ("ci-input-lookups.py",include_bytes!("../../../scripts/ci-input-lookups.py").as_slice()),
            ("ci-lookup-inputs.py",include_bytes!("../../../scripts/ci-lookup-inputs.py").as_slice()),
            ("ci-input-manifest.py",include_bytes!("../../../scripts/ci-input-manifest.py").as_slice()),
            ("ci-cache-generation.py",include_bytes!("../../../scripts/ci-cache-generation.py").as_slice()),
        ] {std::fs::write(root.join(name),bytes)?;}
        std::fs::write(root.join("ci-fixture-inputs.py"),r#"import importlib.util,json,sys
from pathlib import Path
def load(name):
 spec=importlib.util.spec_from_file_location(name,Path('/workspace')/(name+'.py'))
 module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module
def snapshots():
 manifest=load('ci-input-manifest')
 return {name:[str(path),manifest.snapshot({name:path})] for name,path in
  [('source',Path('/workspace/src')),('generated',Path('/workspace/node_modules/generated'))]}
def content_observations(trace):
 inputs=load('ci-lookup-inputs');lookups=load('ci-input-lookups')
 queries=[q for q in lookups.path_queries(trace) if q['syscall'] in ('statx','openat')
  and q.get('path',b'').endswith((b'/chosen.rs',b'/chosen/mod.rs'))]
 assert queries,'actual module queries required'
 before=inputs.LookupCensus(json.loads(Path('target/ci-fixture-inputs-before.json').read_text())).observe(queries)
 unrelated=inputs.LookupCensus(json.loads(Path('target/ci-fixture-inputs-unrelated.json').read_text())).observe(queries)
 after=inputs.LookupCensus(snapshots()).observe(queries,require_original_results=False)
 return {'lookup_content_inputs_changed':before['sha256']!=after['sha256'],
  'unrelated_generated_input_keeps_identity':before==unrelated,
  'original_lookup_content_sha256':before['sha256'],'changed_lookup_content_sha256':after['sha256'],
  'content_input_closure_complete':after['input_closure_complete']}
if __name__=='__main__':
 assert len(sys.argv)==3 and sys.argv[1]=='snapshot' and sys.argv[2] in ('before','unrelated')
 Path('target/ci-fixture-inputs-'+sys.argv[2]+'.json').write_text(json.dumps(snapshots()))
"#)?;
    }
    std::fs::write(root.join("Cargo.toml"),"[package]\nname=\"generated-input-consumer\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[workspace]\n")?;
    std::fs::write(root.join("Cargo.lock"),"version=4\n[[package]]\nname=\"generated-input-consumer\"\nversion=\"0.0.0\"\n")?;
    std::fs::write(root.join("package.json"),"{\"name\":\"generated-input-fixture\",\"private\":true}\n")?;
    if resolution {
        std::fs::write(root.join("src/main.rs"),"#[path=\"../node_modules/generated/mod.rs\"] mod generated;\nfn main(){println!(\"{}\",generated::value());}\n")?;
    } else if build_script {
        std::fs::write(root.join("build.rs"),r#"fn main(){
    println!("cargo:rerun-if-changed=node_modules/generated.txt");
    let out=std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::copy("node_modules/generated.txt",out.join("generated.txt")).unwrap();
}
"#)?;
        std::fs::write(root.join("src/main.rs"),"fn main(){println!(\"{}\",include_str!(concat!(env!(\"OUT_DIR\"),\"/generated.txt\")));}\n")?;
    } else {
        std::fs::write(root.join("src/main.rs"),"fn main(){println!(\"{}\",include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"),\"/node_modules/generated.txt\")));}\n")?;
    }
    let counter=if read_trace {r#"#!/usr/bin/python3
import os,subprocess,sys
from pathlib import Path
tracked='--crate-name' in sys.argv and sys.argv[sys.argv.index('--crate-name')+1]=='generated_input_consumer'
if tracked:
    with Path('/tmp/ci-fixture-rustc-count').open('a') as stream:stream.write('compile\n')
    # Observe lookup arguments and results only. Do not trace read buffers,
    # exec environments, or treat this worker-owned log as trusted evidence.
    result=subprocess.run(['/usr/bin/strace','-f','-qq','-s','4096','-v','-yy','-xx',
        '-e','trace=openat,openat2,newfstatat,stat,statx,lstat,access,faccessat,faccessat2,readlink,readlinkat,getdents64',
        '-o','/workspace/target/ci-fixture-read-trace.txt',*sys.argv[1:]])
    raise SystemExit(result.returncode if result.returncode>=0 else 128-result.returncode)
os.execvp(sys.argv[1],sys.argv[1:])
"#} else {r#"#!/usr/bin/python3
import os,sys
from pathlib import Path
if '--crate-name' in sys.argv and sys.argv[sys.argv.index('--crate-name')+1]=='generated_input_consumer':
    with Path('/tmp/ci-fixture-rustc-count').open('a') as stream:stream.write('compile\n')
os.execvp(sys.argv[1],sys.argv[1:])
"#};
    std::fs::write(root.join("rustc-counter.py"),counter)?;
    std::fs::set_permissions(root.join("rustc-counter.py"),std::fs::Permissions::from_mode(0o755))?;
    git(root,&["init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","fixed generated-input consumer"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let session=vm::Session::prepare(root,&target)?;
    let source=snapshot(root,&bound,&target).await?;
    let timeout=std::time::Duration::from_secs(180);
    let mut generate_old=if resolution {r#"python3 -I - <<'PY'
import os
from pathlib import Path
root=Path('node_modules/generated');(root/'chosen').mkdir(parents=True)
files={root/'mod.rs':"mod chosen;pub fn value()->&'static str{chosen::VALUE}\n",
 root/'chosen/mod.rs':'pub const VALUE:&str="OLD";\n'}
for path,source in files.items():
 path.write_text(source);os.utime(path,ns=(946684800000000000,946684800000000000))
PY"#} else {"python3 -I -c 'import os;from pathlib import Path;p=Path(\"node_modules/generated.txt\");p.write_text(\"OLD\");os.utime(p,ns=(946684800000000000,946684800000000000))'"}.to_owned();
    if read_trace {generate_old.push_str("\npython3 -I /workspace/ci-fixture-inputs.py snapshot before");}
    let mut generate_new=if resolution {r#"python3 -I - <<'PY'
import os
from pathlib import Path
path=Path('node_modules/generated/chosen.rs');path.write_text('pub const VALUE:&str="NEW";\n')
os.utime(path,ns=(946684800000000000,946684800000000000))
PY"#.to_owned()} else {generate_old.replace("OLD","NEW")};
    if read_trace {generate_new=format!("python3 -I -c 'from pathlib import Path;Path(\"node_modules/generated/unrelated.rs\").write_text(\"unrelated\")'\npython3 -I /workspace/ci-fixture-inputs.py snapshot unrelated\n{generate_new}");}
    let compile=if resolution {r#"python3 -I - <<'PY'
import json,os,subprocess,sys
from pathlib import Path
environment=os.environ.copy();environment['RUSTC_WRAPPER']='/workspace/rustc-counter.py'
result=subprocess.run(['cargo','build','--offline','--locked','--message-format=json'],env=environment,capture_output=True,text=True)
counter=Path('/tmp/ci-fixture-rustc-count');print('CALLS='+str(len(counter.read_text().splitlines()) if counter.exists() else 0),flush=True)
for line in result.stdout.splitlines():
 try:message=json.loads(line)
 except ValueError:continue
 if message.get('reason')=='compiler-message' and isinstance(message.get('message',{}).get('code'),dict):
  print('DIAG='+message['message']['code']['code'],flush=True)
if result.returncode:raise SystemExit(result.returncode)
raise SystemExit(subprocess.run(['target/debug/generated-input-consumer']).returncode)
PY"#} else {"RUSTC_WRAPPER=/workspace/rustc-counter.py cargo build --offline --locked --message-format=json > target/messages.json && python3 -I -c 'from pathlib import Path;p=Path(\"/tmp/ci-fixture-rustc-count\");print(\"CALLS=\"+str(len(p.read_text().splitlines()) if p.exists() else 0))' && target/debug/generated-input-consumer"};
    let old=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some(&generate_old),&storage,&target,&audit,timeout).await?;
    ensure!(old.status=="passed","initial generator failed: {}",old.stderr);
    let first=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some(compile),&storage,&target,&audit,timeout).await?;
    ensure!(first.status=="passed" && first.stdout.trim()=="CALLS=1\nOLD","initial generated input did not execute: {} {}",first.stdout,first.stderr);
    let projected_source=first.command_receipt.as_ref().context("source projection controller receipt missing")?
        .value()["source_projection"].clone();
    ensure!(projected_source["owner_uid"]==0 && projected_source["access"][0]["uid"]==1001
        && projected_source["access"][1]["uid"]==2000 && projected_source["input_closure_complete"]==false
        && projected_source["shipping_authority"]==false,"source projection role probes missing");
    let unchanged=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some(compile),&storage,&target,&audit,timeout).await?;
    ensure!(unchanged.status=="passed" && unchanged.stdout.trim()=="CALLS=0\nOLD","unchanged compatible unit recompiled or did not remain valid: {}",unchanged.stdout);
    let changed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some(&generate_new),&storage,&target,&audit,timeout).await?;
    ensure!(changed.status=="passed" && changed.command_receipt.as_ref().is_some_and(|r|r.value()["input_write_observation"]["changed"]==true),
        "generated mutation was not observed");
    let observed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some(compile),&storage,&target,&audit,timeout).await?;
    let control_command=if read_trace {r#"python3 -I - <<'PY'
import importlib.util,json,subprocess,sys
from pathlib import Path
trace=Path('target/ci-fixture-read-trace.txt');assert trace.is_file() and trace.stat().st_size<=4*1024**2,'bounded trace required'
dep_infos={}
for path in Path('target/debug/deps').glob('generated_input_consumer-*.d'):
 assert path.stat().st_size<=512*1024 and len(dep_infos)<8,'bounded dep-info required'
 dep_infos[str(path)]=path.read_text()
spec=importlib.util.spec_from_file_location('lookups','/workspace/ci-input-lookups.py')
lookups=importlib.util.module_from_spec(spec);spec.loader.exec_module(lookups)
queries=lookups.failed_queries(trace.read_bytes())
missing=[query for query in queries if query['path'].endswith(b'/chosen.rs') and query['errno']=='ENOENT']
all_queries=lookups.path_queries(trace.read_bytes())
opened_module=[query for query in all_queries if query.get('resolved_path')==b'/workspace/node_modules/generated/chosen/mod.rs' and query['intent']=='read']
directory_reads=[query for query in all_queries if query['syscall']=='getdents64']
device_reads=[query for query in all_queries if query.get('descriptor_kind') in ('character-device','block-device')]
spec=importlib.util.spec_from_file_location('fixture_inputs','/workspace/ci-fixture-inputs.py')
fixture_inputs=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture_inputs)
content=fixture_inputs.content_observations(trace.read_bytes())
print(json.dumps({'trace':trace.read_text(),'dep_infos':dep_infos,'trusted_input_closure':False,
 'missing_module_queries':len(missing),'lookup_observation':lookups.observation(queries),
 'decoded_path_queries':len(all_queries),'opened_module_queries':len(opened_module),
 'directory_read_queries':len(directory_reads),'device_read_queries':len(device_reads),**content}),flush=True)
result=subprocess.run(['rustc','--edition=2021','--crate-name','resolution_control','src/main.rs','-o','target/resolution-control'],capture_output=True,text=True)
sys.stderr.write(result.stderr);sys.stdout.write(result.stdout);raise SystemExit(result.returncode)
PY"#} else {"rustc --edition=2021 --crate-name resolution_control src/main.rs -o target/resolution-control"};
    let control=if resolution {Some(session.run("repository",&source,root,&bound,None,&[],&[],&[],
        Some(control_command),
        &storage,&target,&audit,timeout).await?)} else {None};
    let passed=if resolution {observed.status=="failed" && observed.stdout.contains("DIAG=E0761")
        && control.as_ref().is_some_and(|value|value.status=="failed" && value.stderr.contains("E0761"))}
        else {observed.status=="passed" && observed.stdout.trim()=="CALLS=1\nNEW"};
    let lookup_observation=if read_trace {control.as_ref().and_then(|value|serde_json::from_str::<serde_json::Value>(value.stdout.trim()).ok())} else {None};
    let lookup_observed=lookup_observation.as_ref().is_some_and(|value|
        value["missing_module_queries"].as_u64().is_some_and(|count|count>0)
        && value["opened_module_queries"].as_u64().is_some_and(|count|count>0)
        && value["directory_read_queries"].as_u64().is_some_and(|count|count>0)
        && value["lookup_content_inputs_changed"]==true
        && value["unrelated_generated_input_keeps_identity"]==true
        && value["content_input_closure_complete"]==false
        && value["lookup_observation"]["input_closure_complete"]==false
        && value["lookup_observation"]["shipping_authority"]==false
        && value["dep_infos"].as_object().is_some_and(|files|!files.is_empty()
            && files.values().all(|value|value.as_str().is_some_and(|text|!text.contains("chosen.rs")))))
        && control.as_ref().is_some_and(|value|value.status=="failed" && value.stderr.contains("E0761"));
    let report=serde_json::json!({"version":1,"kind":if read_trace {"real_shared_ci_negative_lookup_trace"} else {"real_shared_ci_generated_input_mutation"},
        "head":head,"generation":source.generation(),"input_consumer":if resolution {"module_resolution"} else if build_script {"build_script"} else {"compiler_include"},
        "old_and_new_mtime_ns":946684800000000000_u64,
        "expected":if resolution {"compile failure E0761 after introducing a conflicting module"} else {"CALLS=1\nNEW"},
        "actual":observed.stdout.trim(),"status":observed.status,
        "direct_compiler_control":control.as_ref().map(|value|serde_json::json!({"status":value.status,"stdout":value.stdout,"stderr":value.stderr})),
        "unchanged_compatible_unit_actual_rustc_calls":0,
        "passed":if read_trace {lookup_observed} else {passed},
        "input_behavior_fixed":passed,"negative_lookup_observed_but_absent_from_dep_info":lookup_observed,
        "decoded_path_queries":lookup_observation.as_ref().map(|value|&value["decoded_path_queries"]),
        "opened_module_queries":lookup_observation.as_ref().map(|value|&value["opened_module_queries"]),
        "directory_read_queries":lookup_observation.as_ref().map(|value|&value["directory_read_queries"]),
        "device_read_queries":lookup_observation.as_ref().map(|value|&value["device_read_queries"]),
        "lookup_content_inputs_changed":lookup_observation.as_ref().map(|value|&value["lookup_content_inputs_changed"]),
        "unrelated_generated_input_keeps_identity":lookup_observation.as_ref().map(|value|&value["unrelated_generated_input_keeps_identity"]),
        "source_projection_observation":projected_source,
        "trusted_input_closure":false,"cargo_metadata_isolation_established":false,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0});
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&report)?)?;
    ensure!(report["passed"]==true,"generated-input observation or invalidation missing: executed {}",observed.stdout.trim());
    Ok(())
}

/// Complete a real compiled fixture plan, then prove a newer failing run
/// cannot return the previous run's protected local-completion capability.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_complete_coverage() -> Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-complete-coverage",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("ci-complete-fixture-").tempdir_in("/tmp")?;
    let root=fixture.path();
    git(root,&["-c","init.templateDir=","init","-q"]).await?;
    fixture::install(root)?;
    std::fs::write(root.join("arithmetic.py"),"def add(a, b):\n    return a + b\n")?;
    git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
        "-c","core.hooksPath=/dev/null","commit","-qm","working arithmetic"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    fixture::scope(root,async {
        let completed=verify_admitted(root,&audit,&storage,&target).await?;
        let record=completed.receipt().value();
        ensure!(record["kind"]=="shared_ci_local_complete" && record["reusable_passing_proof"]==false
            && record["shipping_authority"]==false && record["binding"]["verification_head"]==head,
            "local completion was misbound or claimed final freshness authority");
        let commands=record["command_receipts"].as_object().context("local coverage missing")?;
        ensure!(commands.len()==1,"fixture plan coverage was partial or overcounted");
        for (id,hash) in commands {
            let command=audit.controller_receipt(hash.as_str().context("command receipt hash missing")?)?;
            ensure!(command.value()["check"]==*id && command.value()["status"]=="passed",
                "local coverage used another command's receipt");
            ensure!(command.value()["guest_worker_quiescence"]["worker_free"]==true
                && command.value()["guest_worker_quiescence"]["scopes"].as_array().is_some_and(|scopes|!scopes.is_empty())
                && command.value()["input_write_observation"]["kind"]=="ci-input-write-observation"
                && command.value()["guest_worker_quiescence"]["scopes"].as_array().unwrap()
                    .contains(&command.value()["input_write_observation"]["guest_worker_quiescence"]),
                "input observation did not follow its own positive guest worker cleanup");
        }
        for (field,kind) in [("started_receipt_sha256","shared_ci_started"),("plan_receipt_sha256","shared_ci_plan")] {
            let prior=audit.controller_receipt(record[field].as_str().context("coverage lineage missing")?)?;
            ensure!(prior.value()["kind"]==kind && prior.value()["run_id"]==record["run_id"],
                "local completion borrowed another execution's plan or start");
        }
        let current_context=crate::autoship_workflow::Binding {head:head.clone(),
            contract:"isolated-ci-fixture-v1".into(),evidence_hash:digest(b"isolated CI fixture acceptance context")};
        let expected=proof::CiExpectation::current(root,"fixture/shared-ci#1",&current_context).await?;
        let refused=proof::publish_passing_ci(&audit,&completed,&expected).err()
            .context("preparation-only actual CI execution minted a final passing proof")?;
        ensure!(refused.downcast_ref::<crate::autoship_freshness::Hold>().is_some_and(|hold|
            hold.reason==crate::autoship_freshness::Reason::InputCensusUnknown),
            "preparation-only refusal was not the expected typed input prerequisite: {refused:#}");
        let old=completed.receipt().payload_sha256().to_owned();
        std::fs::write(root.join("arithmetic.py"),"def add(a, b):\n    return 0\n")?;
        git(root,&["add","arithmetic.py"]).await?;
        git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
            "-c","core.hooksPath=/dev/null","commit","-qm","intentional assertion failure"]).await?;
        let error=verify_admitted(root,&audit,&storage,&target).await.err()
            .context("failing newer CI run returned a completed capability")?;
        ensure!(format!("{error:#}").contains("shared CI check"),"new run failed before source assertion: {error:#}");
        ensure!(audit.controller_receipt(&old)?.value()["binding"]["verification_head"]==head,
            "failed retry corrupted historical completion evidence");
        std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
            "version":1,"kind":"real_shared_ci_complete_coverage","local_checks_covered":commands.len(),
            "protected_start_plan_and_command_lineage":true,"newer_failed_run_cannot_return_old_completion":true,
            "current_git_runtime_and_owned_generation_expectations":true,
            "guest_worker_quiescence_precedes_bound_input_capture":true,
            "preparation_only_actual_producer_refused_final_ci_authority":true,
            "historical_completion_preserved":true,"full_ci_pass":false,"shipping_authority":false,
            "autonomous_merges_claimed":0}))?)?;
        Ok::<_,anyhow::Error>(())
    }).await??;
    drop(storage);
    Ok(())
}

/// Publication requires a second, separately supervised VM after the original
/// producer has exited. This checks actual Cargo reuse and kernel permissions.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_dependency_seed_replay()->Result<()> {
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-dependency-seed",Some(&root))?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&root);
    let storage=crate::build_storage::admit_ci(&root,&target).await?;
    let fixture=tempfile::Builder::new().prefix("dependency-seed-fixture-").tempdir_in(&target)?;
    let repo=fixture.path();std::fs::create_dir(repo.join("src"))?;
    let registry="registry+https://github.com/rust-lang/crates.io-index";
    let package_id="registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.18";
    let lock=format!("version = 4\n[[package]]\nname = \"seed-consumer\"\nversion = \"0.0.0\"\ndependencies = [\"itoa\"]\n\n[[package]]\nname = \"itoa\"\nversion = \"1.0.18\"\nsource = \"{registry}\"\nchecksum = \"8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682\"\n");
    std::fs::write(repo.join("Cargo.lock"),&lock)?;
    std::fs::write(repo.join("Cargo.toml"),"[package]\nname = \"seed-consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n[dependencies]\nitoa = \"=1.0.18\"\n")?;
    std::fs::write(repo.join("src/main.rs"),"fn main() { println!(\"{}\", itoa::Buffer::new().format(42)); }\n")?;
    std::fs::write(repo.join("package.json"),"{\"name\":\"ci-generated-input-fixture\",\"private\":true}\n")?;
    git(repo,&["-c","init.templateDir=","init","-q"]).await?;
    git(repo,&["add","."]).await?;
    git(repo,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","commit","-qm","seed consumer"]).await?;
    let head=git(repo,&["rev-parse","HEAD"]).await?;
    git(repo,&["update-ref","refs/remotes/origin/main",head.trim()]).await?;
    let binding=binding(repo).await?;
    let metadata=serde_json::json!({"version":1,"workspace_root":"/workspace","packages":[
        {"id":"root","name":"seed-consumer","version":"0.0.0","source":null,"manifest_path":"/workspace/Cargo.toml",
         "targets":[{"kind":["bin"],"src_path":"/workspace/src/main.rs"}]},
        {"id":package_id,"name":"itoa","version":"1.0.18","source":registry,
         "manifest_path":"/ci-cargo-home/registry/src/pinned/itoa-1.0.18/Cargo.toml",
         "targets":[{"kind":["lib"],"src_path":"/ci-cargo-home/registry/src/pinned/itoa-1.0.18/src/lib.rs"}]}],
         "resolve":{"nodes":[{"id":"root","dependencies":[package_id],"features":[]},
                                    {"id":package_id,"dependencies":[],"features":[]}]}});
    let session=vm::Session::prepare(&root,&target)?;
    let producer_source=session.dependency_source(&metadata,&lock,&binding,&target).await?;
    let producer=uuid::Uuid::new_v4().to_string();
    let mut action=serde_json::json!({"kind":"produce","producer":producer,
        "source_sha256":producer_source.source_sha256,"dependency_inputs_sha256":producer_source.dependency_inputs_sha256});
    ensure!(session.producer_ack(&producer_source.source,&producer).is_err(),"unexecuted producer received an acknowledgement");
    let timeout=std::time::Duration::from_secs(180);
    let outcome=session.run_with_dependency("dependency",&producer_source.source,repo,&binding,None,&[],&[],&[],None,
        &storage,&target,&audit,timeout,Some(&action)).await?;
    ensure!(outcome.status=="seed_produced","producer disposition invalid");
    ensure!(outcome.dependency_result.as_ref().context("producer receipt missing")?["cargo_exit_code"]==0,"isolated Cargo compilation failed");
    action["kind"]="publish".into();action["ack"]=session.producer_ack(&producer_source.source,&producer)?;
    let outcome=session.run_with_dependency("dependency",&producer_source.source,repo,&binding,None,&[],&[],&[],None,
        &storage,&target,&audit,timeout,Some(&action)).await?;
    let published=outcome.dependency_result.context("published seed receipt missing")?;
    ensure!(published["units"]==1,"isolated producer did not publish exactly the external unit");
    let install=session.dependency_seed(repo,&binding,&target,&storage,&audit).await?
        .context("ordinary verification did not select its dependency seed")?;
    ensure!(install["seed"]==published["seed"],"automatic lookup did not reuse the protected seed");
    let reopened=vm::Session::prepare(&root,&target)?;
    ensure!(reopened.authority_sha256==session.authority_sha256,"equivalent controller restart changed trusted authority");
    let restored=reopened.dependency_seed(repo,&binding,&target,&storage,&audit).await?
        .context("controller restart lost the trusted dependency seed")?;
    ensure!(restored==install,"restart selected a different seed");
    drop(reopened);
    let build=r#"set -e
python3 -I -c 'from pathlib import Path
modules=Path("node_modules")
if not (modules/"tool").exists():
 (modules/"tool").mkdir();(modules/"tool/cli.js").write_text("console.log(42)\n")
 (modules/".bin").mkdir();(modules/".bin/tool").symlink_to("../tool/cli.js")
 (modules/"tool-alias").symlink_to("tool",target_is_directory=True)'
cargo build --offline --locked --message-format=json > target/messages.json
python3 -c 'import json; from pathlib import Path
units=[json.loads(line) for line in Path("target/messages.json").read_text().splitlines()]
compiled=[m["target"]["name"] for m in units if m["reason"]=="compiler-artifact" and not m["fresh"]]
print(json.dumps(sorted(compiled)))'
target/debug/seed-consumer
"#;
    let mut generations=Vec::new();
    for _ in 0..2 {
        let source=snapshot(repo,&binding,&target).await?;generations.push(source.generation().to_owned());
        let first=session.run_with_dependency("repository",&source,repo,&binding,None,&[],&[],&[],Some(build),
            &storage,&target,&audit,timeout,Some(&install)).await?;
        ensure!(first.status=="passed" && first.stdout.trim()=="[\"seed-consumer\"]\n42",
            "external unit recompiled or consumer failed: {} {}",first.stdout,first.stderr);
        let committed=first.command_receipt.as_ref().context("successful VM command lacks protected host receipt")?;
        let record=committed.value();
        ensure!(record["kind"]=="shared_ci_command_complete" && record["status"]=="passed"
            && record["shipping_authority"]==false
            && record["cache_preparation"]["binding"]["generation"]==source.generation()
            && record["cache_preparation"]["binding"]["head"]==binding.verification_head
            && record["cache_preparation"]["binding"]["tree"]==binding.index_tree
            && record["cache_preparation"]["dependency_seed"]==published["seed"],
            "protected command receipt lost its original source/cache/seed binding");
        ensure!(record["input_write_observation"]["changed"]==true
            && record["input_write_observation"]["changed_roots"].as_array().is_some_and(|roots|!roots.is_empty()),
            "declared generated input creation and npm-style aliases were not observed");
        let quiescent=audit.controller_receipt(record["quiescence_receipt_sha256"].as_str()
            .context("command receipt lacks protected quiescence reference")?)?;
        ensure!(quiescent.value()["kind"]=="shared_ci_vm_quiescent"
            && quiescent.value()["command"]==record["cache_preparation"]["command"]
            && quiescent.value()["generation"]==source.generation(),
            "command receipt refers to another invocation's teardown");
        ensure!(audit.controller_receipt(committed.payload_sha256())?.value()==record,
            "command receipt did not survive committed audit reload");
        let again=session.run("repository",&source,repo,&binding,None,&[],&[],&[],Some(build),
            &storage,&target,&audit,timeout).await?;
        ensure!(again.status=="passed" && again.stdout.trim()=="[]\n42","compatible same-job units recompiled: {} {}",again.stdout,again.stderr);
        ensure!(again.command_receipt.as_ref().is_some_and(|receipt|
            receipt.payload_sha256()!=committed.payload_sha256()
                && receipt.value()["cache_preparation"]["binding"]["generation"]==source.generation()
                && receipt.value()["input_write_observation"]["changed"]==false),
            "repeated command reused an older invocation's protected completion");
        let tamper=r#"python3 -c 'import errno,os; from pathlib import Path
p=next(Path("target/debug/deps").glob("libitoa-*.rlib"))
for operation in (lambda:p.write_bytes(b"corrupt"),lambda:p.unlink(),lambda:p.parent.rename(p.parent.with_name("moved"))):
 try: operation()
 except OSError as error: assert error.errno in (errno.EROFS,errno.EACCES,errno.EPERM),error
 else: raise SystemExit("trusted artifact or ancestor was mutable")
assert os.environ["CARGO_HOME"]=="/ci-cargo-home"
try:list(Path("/build-cache").iterdir())
except PermissionError:pass
else:raise SystemExit("shared image visible")
print("SEALED")'"#;
        let tamper=session.run("repository",&source,repo,&binding,None,&[],&[],&[],Some(tamper),
            &storage,&target,&audit,timeout).await?;
        ensure!(tamper.status=="passed" && tamper.stdout.trim()=="SEALED","trusted seed confinement failed: {} {}",tamper.stdout,tamper.stderr);
    }
    ensure!(generations[0]!=generations[1],"source reconstruction reused generation");
    drop(producer_source);
    // Leave one real external-only producer unpublished. A live source must
    // pin its payload; after that exact owner drops, ordinary admitted work
    // retires it without deleting the independent seed or installed links.
    let abandoned=session.dependency_source(&metadata,&lock,&binding,&target).await?;
    let abandoned_id=uuid::Uuid::new_v4().to_string();
    let abandoned_action=serde_json::json!({"kind":"produce","producer":abandoned_id,
        "source_sha256":abandoned.source_sha256,"dependency_inputs_sha256":abandoned.dependency_inputs_sha256});
    let produced=session.run_with_dependency("dependency",&abandoned.source,repo,&binding,None,&[],&[],&[],None,
        &storage,&target,&audit,timeout,Some(&abandoned_action)).await?;
    ensure!(produced.status=="seed_produced","abandoned producer fixture did not execute");
    ensure!(produced.producer_reclaimed.contains(&producer),"released published producer was not retired");
    let probe=snapshot(repo,&binding,&target).await?;
    let live=session.run("repository",&probe,repo,&binding,None,&[],&[],&[],Some("printf LIVE"),
        &storage,&target,&audit,timeout).await?;
    ensure!(live.status=="passed" && !live.producer_reclaimed.contains(&abandoned_id),"live producer was reclaimed");
    drop(abandoned);
    let mut retired=false;
    for _ in 0..4 {
        let outcome=session.run("repository",&probe,repo,&binding,None,&[],&[],&[],Some("printf RETIRED"),
            &storage,&target,&audit,timeout).await?;
        ensure!(outcome.status=="passed","ordinary admitted producer retirement failed");
        if outcome.producer_reclaimed.contains(&abandoned_id) {retired=true;break;}
    }
    ensure!(retired,"inactive producer payload was not retired through ordinary admitted work");
    drop(probe);
    let after_retirement=session.dependency_seed(repo,&binding,&target,&storage,&audit).await?
        .context("producer retirement removed the independent published seed")?;
    ensure!(after_retirement==install,"producer retirement changed trusted seed lookup");
    // Supersede one real published seed with another independently supervised
    // producer. Ordinary admitted work must reclaim the released orphan while
    // preserving the current index and any generation still using either seed.
    let replacement=session.dependency_source(&metadata,&lock,&binding,&target).await?;
    let replacement_id=uuid::Uuid::new_v4().to_string();
    let mut replacement_action=serde_json::json!({"kind":"produce","producer":replacement_id,
        "source_sha256":replacement.source_sha256,"dependency_inputs_sha256":replacement.dependency_inputs_sha256});
    let produced=session.run_with_dependency("dependency",&replacement.source,repo,&binding,None,&[],&[],&[],None,
        &storage,&target,&audit,timeout,Some(&replacement_action)).await?;
    ensure!(produced.status=="seed_produced","replacement producer did not execute");
    replacement_action["kind"]="publish".into();
    replacement_action["ack"]=session.producer_ack(&replacement.source,&replacement_id)?;
    let republished=session.run_with_dependency("dependency",&replacement.source,repo,&binding,None,&[],&[],&[],None,
        &storage,&target,&audit,timeout,Some(&replacement_action)).await?;
    let replacement_seed=republished.dependency_result.context("replacement publication missing")?["seed"]
        .as_str().context("replacement seed missing")?.to_owned();
    let original_seed=published["seed"].as_str().context("original seed missing")?;
    ensure!(replacement_seed!=original_seed,"independent producer reused the old publication identity");
    drop(replacement);
    let mut orphan_retired=false;
    for _ in 0..4 {
        let source=snapshot(repo,&binding,&target).await?;
        let outcome=session.run("repository",&source,repo,&binding,None,&[],&[],&[],Some("printf ORPHAN_RETIRED"),
            &storage,&target,&audit,timeout).await?;
        ensure!(outcome.status=="passed" && !outcome.seed_reclaimed.contains(&replacement_seed),
            "current indexed seed was reclaimed");
        if outcome.seed_reclaimed.iter().any(|value|value==original_seed) {orphan_retired=true;break;}
    }
    ensure!(orphan_retired,"released unindexed seed was not reclaimed by ordinary admitted work");
    let current=session.dependency_seed(repo,&binding,&target,&storage,&audit).await?
        .context("orphan reclamation lost current dependency seed")?;
    ensure!(current["seed"]==replacement_seed,"orphan reclamation changed the current index");
    let receipt=serde_json::json!({"version":1,"kind":"real_shared_ci_dependency_seed_replay","seed":published["seed"],
        "adapter_authority":session.authority_sha256,"generations":generations,
        "external_units_recompiled_across_generations":0,"same_job_compatible_units_recompiled":0,
        "seed_corruption_denied":true,"automatic_seed_lookup":true,"equivalent_controller_restart_reuses_seed":true,
        "bound_preparation_and_committed_quiescence_receipts":true,
        "declared_generated_namespace_aliases_observed":true,"unchanged_generated_inputs_preserve_compatible_reuse":true,
        "live_producer_preserved":true,"inactive_producer_retired_after_source_drop":true,
        "published_seed_survives_producer_retirement":true,
        "released_unindexed_seed_reclaimed":true,"current_index_survives_orphan_reclamation":true,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

/// Behavioral cache replay through the production adapter and actual managed
/// image. This grants no workflow, review, publication or merge authority.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_cache_freshness_replay() -> Result<()> {
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;
    std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()?==evidence,"evidence must be literal");
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-cache-freshness",Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm",Some(&audit))?;
    let target=crate::build_storage::effective_target(&root);
    // The host's idle probe is advisory. Competing managed builds can acquire
    // the lease before this test starts; retry only typed storage deferrals,
    // with no guest, model call or protected storage ownership held meanwhile.
    let admission_deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(300);
    let storage=loop {
        match crate::build_storage::admit_ci(&root,&target).await {
            Ok(storage)=>break storage,
            Err(error) if error.downcast_ref::<crate::storage_reservations::Busy>().is_some()
                && tokio::time::Instant::now()<admission_deadline => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(error)=>return Err(error),
        }
    };
    let fixture=tempfile::Builder::new().prefix("freshness-source-fixture-").tempdir_in(&target)?;
    let repo=fixture.path();std::fs::create_dir(repo.join("src"))?;
    std::fs::write(repo.join("Cargo.toml"),"[package]\nname = \"freshness-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n")?;
    std::fs::write(repo.join("Cargo.lock"),"version = 4\n[[package]]\nname = \"freshness-fixture\"\nversion = \"0.0.0\"\n")?;
    git(repo,&["-c","init.templateDir=","init","-q"]).await?;
    async fn commit(repo:&Path,source:&str)->Result<String> {
        std::fs::write(repo.join("src/main.rs"),source)?;
        git(repo,&["add","."]).await?;
        let result=Command::new("/usr/bin/git").args(["-c","core.hooksPath=/dev/null","-c","commit.gpgsign=false",
            "-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","synthetic source"])
            .current_dir(repo).env_clear().env("PATH","/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
            .env("GIT_AUTHOR_DATE","2000-01-01T00:00:00Z").env("GIT_COMMITTER_DATE","2000-01-01T00:00:00Z")
            .output().await?;
        ensure!(result.status.success(),"fixture commit failed");
        Ok(git(repo,&["rev-parse","HEAD"]).await?.trim().into())
    }
    let original="fn main() { println!(\"first\"); }\n#[test] fn regression() { assert_eq!(2+2,4); }\n";
    let first=commit(repo,original).await?;
    git(repo,&["update-ref","refs/remotes/origin/main",&first]).await?;
    let session=vm::Session::prepare(&root,&target)?;
    let a=binding(repo).await?;let source=snapshot(repo,&a,&target).await?;
    let first_generation=source.generation().to_owned();
    let stamp=std::fs::metadata(source.path().join("src/main.rs"))?.modified()?;
    let command=r#"set -e
cat > target/rustc-probe <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> /workspace/target/compiler-calls
exec "$@"
SH
chmod 700 target/rustc-probe
RUSTC_WRAPPER=/workspace/target/rustc-probe cargo run --offline --locked --quiet
python3 -c 'from pathlib import Path; print("UNITS="+str(sum("--crate-name freshness_fixture " in line for line in Path("target/compiler-calls").read_text().splitlines())))'
"#;
    let timeout=std::time::Duration::from_secs(180);
    let mut reclaimed=0;
    for _ in 0..2 {
        let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some(command),&storage,&target,&audit,timeout).await?;
        reclaimed+=outcome.cache_reclaimed.len();
        ensure!(outcome.status=="passed" && outcome.stdout.trim()=="first\nUNITS=1",
            "same-generation output/reuse mismatch: {} {}",outcome.stdout,outcome.stderr);
    }
    let tamper=r#"python3 -c 'import errno; from pathlib import Path
p=Path("src/main.rs")
try: p.write_text("fn main() {}")
except OSError as error:
 assert error.errno in (errno.EROFS,errno.EACCES,errno.EPERM); print("TRACKED_READ_ONLY")
else: raise SystemExit("tracked source was writable")'"#;
    let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some(tamper),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="TRACKED_READ_ONLY","tracked-source substitution was possible");
    let private_cache=r#"python3 -c 'import os; from pathlib import Path
assert os.environ["CARGO_HOME"]=="/ci-cargo-home"
try: list(Path("/build-cache").iterdir())
except PermissionError: pass
else: raise SystemExit("worker can inspect shared cache generations")
try: open("/dev/vda","rb")
except PermissionError: pass
else: raise SystemExit("worker can read the underlying cache block device")
print("CACHE_ROOT_PRIVATE")'"#;
    let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some(private_cache),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="CACHE_ROOT_PRIVATE","shared cache was visible outside the job mounts");
    let outcome=session.run("repository",&source,repo,&a,None,&[],&[],&[],Some("cargo test --offline --locked --quiet"),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.contains("1 passed"),"original assertion did not execute");
    drop(source);
    commit(repo,"fn main() { println!(\"other\"); }\n").await?;
    let b=binding(repo).await?;let source=snapshot(repo,&b,&target).await?;
    ensure!(std::fs::metadata(source.path().join("src/main.rs"))?.modified()?==stamp,"fixture timestamps differ");
    let outcome=session.run("repository",&source,repo,&b,None,&[],&[],&[],Some(command),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="other\nUNITS=1","changed source ran stale output: {} {}",outcome.stdout,outcome.stderr);
    drop(source);
    commit(repo,"fn main() { missing_function(); }\n").await?;
    let bad=binding(repo).await?;let source=snapshot(repo,&bad,&target).await?;
    let outcome=session.run("repository",&source,repo,&bad,None,&[],&[],&[],Some("cargo build --offline --locked --quiet"),&storage,&target,&audit,timeout).await?;
    ensure!(outcome.status=="failed" && outcome.stderr.contains("E0425"),"compile error was hidden by stale success");
    drop(source);
    commit(repo,"fn main() {}\n#[test] fn regression() { assert_eq!(2+2,5); }\n").await?;
    let bad=binding(repo).await?;let source=snapshot(repo,&bad,&target).await?;
    let outcome=session.run("repository",&source,repo,&bad,None,&[],&[],&[],Some("cargo test --offline --locked --quiet"),&storage,&target,&audit,timeout).await?;
    reclaimed+=outcome.cache_reclaimed.len();
    ensure!(outcome.status=="failed" && outcome.stdout.contains("1 failed"),"failing assertion was hidden by stale success");
    drop(source);
    git(repo,&["reset","--hard",&first]).await?;
    let restored=binding(repo).await?;let source=snapshot(repo,&restored,&target).await?;
    ensure!(source.generation()!=first_generation,"same-HEAD reconstruction inherited its generation");
    let outcome=session.run("repository",&source,repo,&restored,None,&[],&[],&[],Some(command),&storage,&target,&audit,timeout).await?;
    reclaimed+=outcome.cache_reclaimed.len();
    ensure!(outcome.status=="passed" && outcome.stdout.trim()=="first\nUNITS=1","same-HEAD reconstruction inherited changed artifacts");
    ensure!(reclaimed>0,"bounded reclamation did not retire any prior quiescent generation");
    audit.protected_health()?;
    let receipt=serde_json::json!({"version":1,"kind":"real_shared_ci_cache_freshness_replay",
        "adapter_authority":session.authority_sha256,"equal_old_timestamps":true,"changed_output":true,
        "compile_error_observed":"E0425","failing_assertion_observed":true,"same_head_reconstruction":true,
        "same_generation_compatible_units_recompiled":0,"tracked_source_read_only":true,
        "shared_cache_root_private":true,"stable_cargo_home":true,
        "reclaimed_generations_observed":reclaimed,"active_generation_preserved":true,
        "trusted_external_seed_tested":false,"full_ci_pass":false,"shipping_authority":false,
        "additional_model_calls":0,"autonomous_merges_claimed":0});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

/// Observe the exact Rust build check that fails in the deployed main gate.
/// Persist bounded private output even on failure; no production verdict is set.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_rust_build_diagnostic() -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()?==evidence,"evidence path must be literal");
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-rust-build-diagnostic",Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm",Some(&audit))?;
    let before=binding(&root).await?;let target=crate::build_storage::effective_target(&root);
    // Run the already-built opt-in binary after the managed build exits.
    // This admission owns the actual storage lease throughout the guest step.
    let storage=crate::build_storage::admit_ci(&root,&target).await?;
    let session=vm::Session::prepare(&root,&target)?;
    let source=snapshot(&root,&before,&target).await?;
    let discovered=session.run("plan",&source,&root,&before,None,&["Cargo.toml".into()],&[],&[],None,
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    ensure!(discovered.status=="planned","guest planning unavailable");
    let plan:Plan=serde_json::from_value(discovered.plan.context("guest plan missing")?)?;plan.validate(&before)?;
    let check=plan.checks.iter().find(|check|check.id==r#"platform/platform/3 @{"os":"ubuntu-latest"}"#).context("normal Rust CLI build check missing")?;
    ensure!(check.status=="ready","normal Rust CLI build check is not ready: {}",check.reason);
    let mut onnx_cache_probe=None;
    let mut onnx_probe_error=None;
    if std::env::var("AUTOSHIP_CI_ACCEPTANCE_ORT_CACHE_PROBE").as_deref()==Ok("1") {
        let command=r#"python3 -c 'import json,os; from pathlib import Path
cache=Path(os.environ["ORT_CACHE_DIR"])
library=next(cache.glob("dfbin/*/*/libonnxruntime.a"))
writable=False
try:
 fd=os.open(library,os.O_WRONLY); os.close(fd); writable=True
except OSError: pass
print(json.dumps({"cache":str(cache),"library_bytes":library.stat().st_size,"writable":writable,"interfaces":sorted(p.name for p in Path("/sys/class/net").iterdir()),"home":os.environ["HOME"]}))'"#;
        let probe=session.run("repository",&source,&root,&before,None,&[],&[],&[],Some(command),
            &storage,&target,&audit,std::time::Duration::from_secs(180)).await;
        let mut diagnostic=match &probe {
            Ok(outcome)=>serde_json::json!({"status":outcome.status,"stdout":outcome.stdout,"stderr":outcome.stderr}),
            Err(error)=>serde_json::json!({"status":"unavailable","error":format!("{error:#}")}),
        };
        let evaluated:Result<serde_json::Value>=(|| {
            let probe=probe?;ensure!(probe.status=="passed","ONNX cache guest probe failed");
            let observed:serde_json::Value=serde_json::from_str(probe.stdout.trim())?;
            ensure!(observed["cache"]=="/ort-cache" && observed["writable"]==false
                && observed["interfaces"]==serde_json::json!(["lo"]) && observed["home"]=="/home/worker"
                && observed["library_bytes"].as_u64().is_some_and(|bytes|bytes>0),"ONNX cache exposure mismatch");
            Ok(observed)
        })();
        match evaluated {
            Ok(observed)=>onnx_cache_probe=Some(observed),
            Err(error)=>{
                diagnostic["error"]=format!("{error:#}").into();
                onnx_cache_probe=Some(diagnostic);onnx_probe_error=Some(error);
            },
        }
    }
    // An optional probe failure still reaches the private receipt writer. Do
    // not start a Rust build after failed confinement/dependency observation.
    let rust_build_attempted=onnx_probe_error.is_none();
    let observed=if let Some(error)=onnx_probe_error {Err(error)} else {async {
        drop(source);
        let seed=session.dependency_seed(&root,&before,&target,&storage,&audit).await?;
        let source=snapshot(&root,&before,&target).await?;
        session.run_with_dependency("step",&source,&root,&before,Some(check),&[],&[],&[],None,
            &storage,&target,&audit,check.remaining(std::time::Duration::ZERO)?,seed.as_ref()).await
    }.await};
    let (status,stdout,stderr,error)=match observed {
        Ok(outcome)=>(outcome.status,outcome.stdout,outcome.stderr,None),
        Err(error)=>("unavailable".into(),String::new(),String::new(),Some(format!("{error:#}"))),
    };
    let receipt=serde_json::json!({"version":1,"kind":"real-shared-ci-rust-build-diagnostic","binding":before,
        "adapter_authority":session.authority_sha256,"check":check.id,"status":status,
        "stdout":stdout,"stderr":stderr,"error":error,"additional_model_calls":0,"onnx_cache_probe":onnx_cache_probe,"rust_build_attempted":rust_build_attempted,
        "diagnostic_selection_only":true,"full_ci_pass":false,"shipping_authority":false});
    let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(evidence.join("receipt.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&receipt)?)?;file.sync_all()?;
    audit.record_controller(serde_json::json!({"version":1,"kind":"shared_ci_rust_diagnostic_observed","check":check.id,
        "status":status,"stdout_sha256":digest(stdout.as_bytes()),"stderr_sha256":digest(stderr.as_bytes()),"shipping_authority":false}))?;
    audit.protected_health()?;ensure!(binding(&root).await?==before,"source changed during diagnostic");
    ensure!(status=="passed","isolated Rust build failed; inspect the private diagnostic receipt");Ok(())
}

#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_internal_links() -> Result<()> {
    let root=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root),"evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()?==evidence,"evidence path must be literal");
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-link-smoke",Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm",Some(&audit))?;
    let before=binding(&root).await?;let target=crate::build_storage::effective_target(&root);
    // This opt-in test runs from the already-built binary after cargo-managed
    // exits; it obtains its own managed CI lease rather than nesting admission.
    let storage=crate::build_storage::admit_ci(&root,&target).await?;
    let session=vm::Session::prepare(&root,&target)?;
    let source=snapshot(&root,&before,&target).await?;
    let fixture=source.path().join("autoship-internal-link-fixture");std::fs::create_dir(&fixture)?;
    std::fs::write(fixture.join("addon.node"),"owned build artifact")?;
    std::fs::hard_link(fixture.join("addon.node"),fixture.join("alias.node"))?;
    let command=r#"python3 -c 'import json; from pathlib import Path; a=Path("autoship-internal-link-fixture/addon.node"); b=a.with_name("alias.node"); print(json.dumps({"same_inode":a.stat().st_ino==b.stat().st_ino,"link_count":a.stat().st_nlink,"value":b.read_text()}))'"#;
    let outcome=session.run("repository",&source,&root,&before,None,&[],&[],&[],Some(command),&storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let observed:serde_json::Value=serde_json::from_str(outcome.stdout.trim())?;
    ensure!(observed==serde_json::json!({"same_inode":true,"link_count":2,"value":"owned build artifact"}),"guest did not observe the unchanged internal aliases");
    // POSIX hard links require one filesystem. Keep this external fixture in
    // the admitted target, outside the exported snapshot, rather than /tmp.
    let external=tempfile::NamedTempFile::new_in(&target)?;std::fs::write(external.path(),"private host fixture")?;
    std::fs::hard_link(external.path(),fixture.join("external-alias"))?;
    let refused=session.run("repository",&source,&root,&before,None,&[],&[],&[],Some("touch autoship-internal-link-fixture/negative-dispatched"),&storage,&target,&audit,std::time::Duration::from_secs(180)).await;
    ensure!(refused.is_err(),"external host alias was admitted");
    ensure!(!fixture.join("negative-dispatched").exists() && std::fs::read_to_string(external.path())?=="private host fixture","negative probe reached execution or changed host data");
    ensure!(binding(&root).await?==before,"diagnostic changed source binding");audit.protected_health()?;
    let receipt=serde_json::json!({"version":1,"kind":"real_shared_ci_internal_link_smoke","binding":before,
        "adapter_authority":session.authority_sha256,"guest_internal_aliases":observed,"external_alias_refused":true,
        "negative_dispatch_absent":true,"diagnostic_selection_only":true,"full_ci_pass":false,"shipping_authority":false});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_vm_smoke() -> Result<()> {
    let root = PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")
        .context("explicit trusted acceptance repository required")?).canonicalize()?;
    let evidence = PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")
        .context("explicit new private evidence directory required")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&root), "evidence must be outside candidate");
    std::fs::create_dir(&evidence)?;
    std::fs::set_permissions(&evidence, std::fs::Permissions::from_mode(0o700))?;
    ensure!(evidence.canonicalize()? == evidence, "evidence path must be literal");
    let audit = workflow::workflow_audit(&evidence.join("workflow.db"), "shared-ci-vm-smoke", Some(&root))?;
    audit.require_run_resources()?;
    augmentagent_channel_core::run_resources::check_requirement("shared_ci_vm", Some(&audit))?;
    let before = binding(&root).await?;
    let target = crate::build_storage::effective_target(&root);
    // Invoke the already-built test binary directly, after cargo-managed exits:
    // nesting this admission inside its global lease would deadlock.
    let storage = crate::build_storage::admit_ci(&root, &target).await?;
    let inherited = storage.inherited_files()?;
    ensure!(inherited.len() >= 2, "global and target lease descriptors required");
    let identities: Vec<_> = inherited.iter().map(|file| {
        let meta = file.metadata()?;
        Ok::<_, std::io::Error>(serde_json::json!({"device":meta.dev(),"inode":meta.ino()}))
    }).collect::<std::result::Result<_, _>>()?;
    let session = vm::Session::prepare(&root, &target)?;
    let source = snapshot(&root, &before, &target).await?;
    // This is explicit diagnostic selection, not a claim about actual changed
    // paths or complete applicability on this branch.
    let diagnostic_paths = vec!["scripts/tests/model_router_install_test.py".to_owned()];
    let discovered = session.run("plan", &source, &root, &before, None,
        &diagnostic_paths, &[], &[], None, &storage, &target, &audit,
        std::time::Duration::from_secs(180)).await?;
    ensure!(discovered.status == "planned", "real guest planning did not succeed");
    let plan: Plan = serde_json::from_value(discovered.plan.context("guest plan missing")?)?;
    plan.validate(&before)?;
    let check = plan.checks.iter().find(|check|
        check.id == "model-router/installer/3 @{\"os\":\"ubuntu-latest\"}")
        .context("representative pinned Python check missing")?;
    ensure!(check.status == "ready", "representative check is not locally ready: {}", check.reason);
    // Production recreates the snapshot after prerequisite probes, too.
    drop(source);
    let source = snapshot(&root, &before, &target).await?;
    let outcome = session.run("step", &source, &root, &before, Some(check),
        &[], &[], &[], None, &storage, &target, &audit,
        check.remaining(std::time::Duration::ZERO)?).await?;
    ensure!(outcome.status == "passed", "real guest Python check failed: {}", outcome.stderr);
    ensure!(outcome.stderr.contains("Ran ") && outcome.stderr.contains("OK"),
        "Python fixture did not report executed tests");
    ensure!(binding(&root).await? == before, "source binding changed during diagnostic");
    audit.protected_health()?;
    let receipt = serde_json::json!({"version":1,"kind":"real_shared_ci_vm_smoke",
        "binding":before,"adapter_authority":session.authority_sha256,
        "check":check.id,"inherited_storage":identities,"guest_plan":"planned","guest_step":"passed",
        "stdout_sha256":digest(outcome.stdout.as_bytes()),"stderr_sha256":digest(outcome.stderr.as_bytes()),
        "diagnostic_selection_only":true,"full_ci_pass":false,"shipping_authority":false});
    audit.record_controller(receipt.clone())?;
    audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"), serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}
