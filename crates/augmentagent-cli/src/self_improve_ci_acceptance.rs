//! Opt-in real build-VM smoke. No provider calls, publication or merge authority.
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_EXEC_DENIAL_LIBRARY:&[u8]=include_bytes!(concat!(env!("OUT_DIR"),"/libci_cargo_exec.so"));
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_EXEC_DENIAL_CASES:&[u8]=include_bytes!("ci_cargo_exec_cases.py");
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_UNIT_POLICY_CASES:&[u8]=include_bytes!("ci_cargo_unit_policy_cases.py");
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_EXEC_MEDIATION_CASES:&[u8]=include_bytes!("ci_cargo_exec_mediation_cases.py");
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_EXEC_EARLY:&[u8]=include_bytes!(concat!(env!("OUT_DIR"),"/libci_cargo_exec_early.so"));
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_EXEC_STATIC:&[u8]=include_bytes!(concat!(env!("OUT_DIR"),"/ci_cargo_exec_static"));
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_VFORK_PRODUCER:&[u8]=include_bytes!(concat!(env!("OUT_DIR"),"/ci_cargo_vfork_producer"));
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_VFORK_PROXY:&[u8]=include_bytes!(concat!(env!("OUT_DIR"),"/ci_cargo_vfork_proxy"));
#[cfg(all(target_os="linux",target_arch="x86_64"))]
const CARGO_VFORK_CASES:&[u8]=include_bytes!("ci_cargo_vfork_exec_cases.py");
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
 let expected=format!("#[path={:?}] mod generated_mod;\npub const GENERATED:u8=generated_mod::VALUE;\n",out.join("mod.rs"));
 assert_eq!(std::fs::read(out.join("generated.rs")).unwrap(),expected.as_bytes());
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
 std::fs::create_dir_all(out.join("chosen")).unwrap();
 std::fs::write(out.join("chosen/mod.rs"),b"pub const VALUE:u8=7;\n").unwrap();
 std::fs::write(out.join("mod.rs"),b"mod chosen;pub const VALUE:u8=chosen::VALUE;\n").unwrap();
 let generated=format!("#[path={:?}] mod generated_mod;\npub const GENERATED:u8=generated_mod::VALUE;\n",out.join("mod.rs"));
 std::fs::write(out.join("generated.rs"),generated.as_bytes()).unwrap();
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
        && probe["cargo"]["invalidation"]["pending"]==serde_json::json!(["binary","library"])
        && probe["cargo"]["invalidation"]["durable"]==true
        && probe["cargo"]["invalidation"]["original_generated_preserved"]==true
        && probe["cargo"]["invalidation"]["original_sha256"]!=probe["cargo"]["invalidation"]["refreshed_sha256"]
        && probe["cargo"]["invalidation"]["input_closure_complete"]==false
        && probe["cargo"]["invalidation"]["shipping_authority"]==false
        && probe["cargo"]["retry"]["cargo_exit"]==101 && probe["cargo"]["retry"]["library_exit"]==1
        && probe["cargo"]["retry"]["compiler_calls"]==1 && probe["cargo"]["retry"]["diagnostic"]=="E0761"
        && probe["cargo"]["retry"]["rounds"]==1 && probe["cargo"]["retry"]["original_planner_cleanup"]==true
        && probe["cargo"]["retry"]["planner_passes"]==2 && probe["cargo"]["retry"]["planner_pass_limit"]==4
        && probe["cargo"]["retry"]["original_grant_unchanged"]==true
        && probe["cargo"]["retry"]["original_unit_cleanup"]==true
        && probe["cargo"]["retry"]["unaffected_fingerprints_preserved"]==true
        && probe["cargo"]["retry"]["original_generated_preserved"]==true
        && probe["cargo"]["retry"]["failed_outputs_unpublished"]==true
        && probe["cargo"]["retry"]["pending"]==serde_json::json!(["binary","library"])
        && probe["cargo"]["retry"]["shipping_authority"]==false
        && probe["cargo"]["cargo_graph"]["packages"]==1 && probe["cargo"]["cargo_graph"]["targets"]==3
        && probe["cargo"]["cargo_graph"]["original_metadata_cleanup"]==true
        && probe["cargo"]["cargo_graph"]["graph_dispatch"]==true
        && probe["cargo"]["cargo_graph"]["admissions"].as_array().is_some_and(|values|values.len()==4)
        && probe["cargo"]["cargo_graph"]["input_closure_complete"]==false
        && probe["cargo"]["cargo_graph"]["shipping_authority"]==false
        && probe["cargo"]["publication_store"]["history_records"]==5
        && probe["cargo"]["publication_store"]["unit_producers"]==4
        && probe["cargo"]["publication_store"]["diagnostic_producers"]==1
        && probe["cargo"]["publication_store"]["invalidation_before_activation"]==true
        && probe["cargo"]["publication_store"]["history_preserved"]==true
        && probe["cargo"]["publication_store"]["shipping_authority"]==false
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
        && probe["kernel_observation"]["content_selection"]["kernel_bases_bound"]==true
        && probe["kernel_observation"]["content_selection"]["unrelated_content_unchanged"]==true
        && probe["kernel_observation"]["content_selection"]["negative_lookup_changed_identity"]==true
        && probe["kernel_observation"]["content_selection"]["input_closure_complete"]==false
        && probe["kernel_observation"]["directory_selection"]["kernel_namespaces_bound"]==true
        && probe["kernel_observation"]["directory_selection"]["rewound_scan_unbound"]==true
        && probe["kernel_observation"]["directory_selection"]["membership_changed_identity"]==true
        && probe["kernel_observation"]["directory_selection"]["queries"].as_u64().is_some_and(|value|value>=5)
        && probe["kernel_observation"]["directory_selection"]["input_closure_complete"]==false
        && probe["kernel_observation"]["directory_selection"]["shipping_authority"]==false
        && probe["kernel_observation"]["descriptor_selection"]["kernel_namespaces_bound"]==true
        && probe["kernel_observation"]["descriptor_selection"]["failed_calls_unbound"]==true
        && probe["kernel_observation"]["descriptor_selection"]["replacement_metadata_changed"]==true
        && probe["kernel_observation"]["descriptor_selection"]["queries"].as_u64().is_some_and(|value|value>=2)
        && probe["kernel_observation"]["descriptor_selection"]["input_closure_complete"]==false
        && probe["kernel_observation"]["descriptor_selection"]["shipping_authority"]==false
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
    if mode=="spawn-closefrom" {
        use std::os::fd::AsRawFd;
        let file=std::fs::File::create("closefrom-private").unwrap();
        let private=unsafe {libc::fcntl(file.as_raw_fd(),libc::F_DUPFD,80)};
        assert!(private>=80);
        let mut pipes=[0;2];assert_eq!(unsafe {libc::pipe2(pipes.as_mut_ptr(),libc::O_CLOEXEC)},0);
        assert_eq!(unsafe {libc::dup2(pipes[0],20)},20);
        assert_eq!(unsafe {libc::write(pipes[1],b"+".as_ptr().cast(),1)},1);
        let spawn=|ordered:bool,missing:bool,invalid_source:bool| {
            let mut actions=unsafe {std::mem::zeroed::<libc::posix_spawn_file_actions_t>()};
            assert_eq!(unsafe {libc::posix_spawn_file_actions_init(&mut actions)},0);
            assert_eq!(unsafe {libc::posix_spawn_file_actions_addclosefrom_np(&mut actions,-1)},libc::EBADF);
            assert_eq!(unsafe {libc::posix_spawn_file_actions_addclosefrom_np(&mut actions,libc::c_int::MAX)},0);
            assert_eq!(unsafe {libc::posix_spawn_file_actions_addclosefrom_np(&mut actions,if ordered {50} else {3})},0);
            if ordered {
                assert_eq!(unsafe {libc::posix_spawn_file_actions_adddup2(&mut actions,20,60)},0);
                assert_eq!(unsafe {libc::posix_spawn_file_actions_addclose(&mut actions,20)},0);
                assert_eq!(unsafe {libc::posix_spawn_file_actions_addclosefrom_np(&mut actions,61)},0);
            }
            if invalid_source {
                assert_eq!(unsafe {libc::posix_spawn_file_actions_adddup2(&mut actions,private,1)},0);
            }
            let script=std::ffi::CString::new(format!(
                "import os,errno\ntry:os.fstat({private})\nexcept OSError as e:assert e.errno==errno.EBADF\nelse:raise AssertionError('private FD leaked')\n{}\nprint('CLOSEFROM_CHILD_COMPLETE',flush=True)",
                if ordered {"assert os.read(60,1)==b'+'"} else {""})).unwrap();
            let executable=if missing {c"/missing-closefrom-tool"} else {c"/usr/bin/python3"};
            let args=[executable.as_ptr(),c"-I".as_ptr(),c"-c".as_ptr(),script.as_ptr(),std::ptr::null()];
            let environment:[*const libc::c_char;1]=[std::ptr::null()];let mut child=0;
            let error=unsafe {libc::posix_spawn(&mut child,executable.as_ptr(),&actions,
                std::ptr::null(),args.as_ptr().cast(),environment.as_ptr().cast())};
            assert_eq!(unsafe {libc::posix_spawn_file_actions_destroy(&mut actions)},0);
            if missing || invalid_source {
                assert_eq!(error,if invalid_source {libc::EBADF} else {libc::ENOENT});
                assert_eq!(child,0,"failed spawn published a PID");
            } else {
                assert_eq!(error,0);let mut status=0;
                assert_eq!(unsafe {libc::waitpid(child,&mut status,0)},child);assert_eq!(status,0);
            }
            assert!(unsafe {libc::fcntl(private,libc::F_GETFD)}>=0,"child actions closed the parent's FD");
        };
        spawn(false,false,false);spawn(false,true,false);spawn(true,false,false);spawn(false,false,true);
        for fd in [private,20,pipes[0],pipes[1]] {unsafe {libc::close(fd)};}
        println!("RELAY_CLOSEFROM_COMPLETE");return;
    }
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
    let policy=directory.path().join("native.so");std::fs::write(&policy,CARGO_EXEC_DENIAL_LIBRARY).unwrap();
    for (name,count) in [("ci_cargo_relay_test.py",5),("ci_cargo_relay_native_test.py",10)] {
        let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests").join(name);
        let output=std::process::Command::new("python3").arg("-I").arg(tests)
            .env("AUGMENTAGENT_CARGO_RELAY_LIBRARY",&library)
            .env("AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY",&policy)
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

#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[test]
fn private_inode_alias_observations_cover_real_fds_mappings_and_threads() {
    let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests/ci_cargo_inode_alias_test.py");
    let output=std::process::Command::new("python3").arg("-I").arg(tests).output().unwrap();
    let diagnostics=String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(),"task inode alias observations failed: {diagnostics}");
    assert!(diagnostics.contains("Ran 12 tests") && !diagnostics.contains("skipped"),
        "task inode alias observations did not execute: {diagnostics}");
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

#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[test]
fn native_unit_policy_covers_real_namespace_async_io_and_thread_syscalls() {
    let directory=tempfile::tempdir().unwrap();let library=directory.path().join("native.so");
    std::fs::write(&library,CARGO_EXEC_DENIAL_LIBRARY).unwrap();
    let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests/ci_cargo_unit_policy_test.py");
    let output=std::process::Command::new("python3").arg("-I").arg(tests)
        .env("AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY",library).output().unwrap();
    let diagnostics=String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(),"native unit policy failed: {diagnostics}");
    assert!(diagnostics.contains("Ran 10 tests") && !diagnostics.contains("skipped"),"unit policy cases did not execute: {diagnostics}");
    assert!(std::process::Command::new("/usr/bin/true").status().unwrap().success(),"child policy affected the Rust parent");
}

#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[test]
fn native_exec_mediation_covers_early_static_and_original_exec_ack() {
    let directory=tempfile::tempdir().unwrap();
    for (name,bytes) in [("native.so",CARGO_EXEC_DENIAL_LIBRARY),("early.so",CARGO_EXEC_EARLY),("static",CARGO_EXEC_STATIC)] {
        let path=directory.path().join(name);std::fs::write(&path,bytes).unwrap();
        std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    for (file,count) in [("ci_cargo_exec_mediation_test.py",6),("ci_cargo_exec_controller_test.py",9)] {
        let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests").join(file);
        let output=std::process::Command::new("python3").arg("-I").arg(tests)
            .env("AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY",directory.path().join("native.so"))
            .env("AUGMENTAGENT_CARGO_EXEC_EARLY_LIBRARY",directory.path().join("early.so"))
            .env("AUGMENTAGENT_CARGO_EXEC_STATIC_BINARY",directory.path().join("static"))
            .output().unwrap();
        let diagnostics=String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(),"first-exec {file} failed: {diagnostics}");
        assert!(diagnostics.contains(&format!("Ran {count} tests")) && !diagnostics.contains("skipped"),"first-exec cases did not execute: {diagnostics}");
    }
}

#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[test]
fn native_vfork_exec_boundary_preserves_real_parent_release_and_refuses_wrong_images() {
    let directory=tempfile::tempdir().unwrap();
    for (name,bytes) in [("native.so",CARGO_EXEC_DENIAL_LIBRARY),("ci_cargo_vfork_producer",CARGO_VFORK_PRODUCER),
        ("ci_cargo_vfork_proxy",CARGO_VFORK_PROXY),("ci_cargo_exec_static",CARGO_EXEC_STATIC)] {
        let path=directory.path().join(name);std::fs::write(&path,bytes).unwrap();
        std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let tests=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests/ci_cargo_vfork_exec_test.py");
    let output=std::process::Command::new("python3").arg("-I").arg(tests)
        .env("AUGMENTAGENT_CARGO_BOUNDARY_LIBRARY",directory.path().join("native.so")).output().unwrap();
    let diagnostics=String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(),"native vfork boundary failed: {diagnostics}");
    assert!(diagnostics.contains("Ran 6 tests") && !diagnostics.contains("skipped"),"vfork cases did not execute: {diagnostics}");
}

/// Separate feasibility only: no candidate/runtime activation or passing-CI grant.
#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_vfork_exec_boundary()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-vfork-exec-boundary",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("vfork-boundary-fixture-").tempdir_in(&target)?;
    let root=fixture.path();
    for (name,bytes) in [("native.so",CARGO_EXEC_DENIAL_LIBRARY),("producer",CARGO_VFORK_PRODUCER),
        ("proxy",CARGO_VFORK_PROXY),("wrong",CARGO_EXEC_STATIC),("cases.py",CARGO_VFORK_CASES),
        ("provider-supervisor.py",include_bytes!("../../../scripts/provider-supervisor.py").as_slice()),
        ("ci-cargo-planner.py",include_bytes!("../../../scripts/ci-cargo-planner.py").as_slice()),
        ("ci-cargo-relay.py",include_bytes!("../../../scripts/ci-cargo-relay.py").as_slice()),
        ("ci-cargo-staging.py",include_bytes!("../../../scripts/ci-cargo-staging.py").as_slice()),
        ("ci-cache-generation.py",include_bytes!("../../../scripts/ci-cache-generation.py").as_slice()),
        ("ci-cargo-views.py",include_bytes!("../../../scripts/ci-cargo-views.py").as_slice()),
        ("ci-cargo-observer.py",include_bytes!("../../../scripts/ci-cargo-observer.py").as_slice()),
        ("ci-lookup-inputs.py",include_bytes!("../../../scripts/ci-lookup-inputs.py").as_slice())] {
        let path=root.join(name);std::fs::write(&path,bytes)?;
        std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700))?;
    }
    git(root,&["init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","vfork boundary fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let binding=binding(root).await?;let session=vm::Session::prepare(&owner,&target)?;
    let source=snapshot(root,&binding,&target).await?;
    let command=r#"python3 -I - <<'PY'
import json,pathlib,shutil,subprocess,sys
for name in ('native.so','producer','proxy','wrong'):
 shutil.copyfile('/workspace/'+name,'/workspace/target/'+name)
 pathlib.Path('/workspace/target/'+name).chmod(0o700)
results={}
for mode in ('finish','signal','wrong-image','cancel','listener-loss','production-deny'):
 receipt='/workspace/target/'+mode+'-cleanup'
 result=subprocess.run([sys.executable,'-I','/workspace/cases.py','/workspace/ci-cargo-planner.py',
  '/workspace/provider-supervisor.py','/workspace/target/native.so','/workspace/target/producer',
  '/workspace/target/proxy','/workspace/target/wrong',mode,receipt],capture_output=True,text=True,timeout=15)
 assert result.returncode==0,(mode,result.stdout,result.stderr)
 lines=result.stdout.splitlines();assert lines[:-1]==(['PROXY_OUTPUT'] if mode=='finish' else []),(mode,lines)
 assert result.stderr==('PROXY_ERROR\n' if mode=='finish' else ''),(mode,result.stderr)
 row=json.loads(lines[-1]);assert row['original_cleanup'] and row['feasibility_only']
 assert row['shipping_authority'] is False and row['full_ci_pass'] is False
 assert pathlib.Path(receipt).read_text()=='all-descendants-reaped\n'
 if mode in ('finish','signal','wrong-image'):
  assert row['parent_released_before_proxy_termination'] and row['shared_memory_observed'] and row['native_exec_stop']
  assert row['cloexec_closed_at_exec_stop'] and row['image_accepted']==(mode!='wrong-image')
 else:assert row['parent_released_before_proxy_termination'] is False
 results[mode]=row
print(json.dumps(results))
PY"#;
    let result=session.run("repository",&source,root,&binding,None,&[],&[],&[],Some(command),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let cases:serde_json::Value=serde_json::from_str(&result.stdout).unwrap_or(serde_json::Value::Null);
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_vfork_exec_boundary","head":head,"generation":source.generation(),
        "status":result.status,"cases":cases,"actual_vm_commands":1,"input_closure_complete":false,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    ensure!(result.status=="passed" && cases.as_object().is_some_and(|cases|cases.len()==6),
        "vfork boundary unavailable: {} {}",result.stdout,result.stderr);
    Ok(())
}

/// Actual initial exec, early constructor, static entry and closed-listener behavior.
#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_exec_mediation()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-exec-mediation",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("exec-mediation-fixture-").tempdir_in(&target)?;
    let root=fixture.path();
    for (name,bytes) in [("native.so",CARGO_EXEC_DENIAL_LIBRARY),("early.so",CARGO_EXEC_EARLY),
        ("static",CARGO_EXEC_STATIC),("cases.py",CARGO_EXEC_MEDIATION_CASES),
        ("supervisor.py",include_bytes!("../../../scripts/provider-supervisor.py").as_slice())] {
        let path=root.join(name);std::fs::write(&path,bytes)?;
        std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700))?;
    }
    git(root,&["init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","exec mediation fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let binding=binding(root).await?;let session=vm::Session::prepare(&owner,&target)?;
    let source=snapshot(root,&binding,&target).await?;
    let command=r#"python3 -I - <<'PY'
import json,pathlib,shutil,subprocess,sys
for name in ('native.so','early.so','static'):
 shutil.copyfile('/workspace/'+name,'/workspace/target/'+name)
 pathlib.Path('/workspace/target/'+name).chmod(0o700)
results={}
for mode in ('early','static','search','listener-loss','cancel','fork'):
 receipt='/workspace/target/'+mode+'-cleanup'
 result=subprocess.run([sys.executable,'-I','/workspace/cases.py','/workspace/supervisor.py','/workspace/target/native.so',
  '/workspace/target/early.so','/workspace/target/static',mode,receipt],capture_output=True,text=True,timeout=15)
 assert result.returncode==0,(mode,result.stdout,result.stderr)
 expected=['STATIC_DENIED'] if mode=='static' else [] if mode=='cancel' else ['EARLY_DENIED','BODY_DENIED'] if mode=='early' else ['BODY_DENIED']
 lines=result.stdout.splitlines();assert lines[:-1]==expected,(mode,lines)
 assert pathlib.Path(receipt).read_text()=='all-descendants-reaped\n'
 results[mode]=json.loads(lines[-1]);assert results[mode]['original_cleanup'] and results[mode]['shipping_authority'] is False
print(json.dumps(results))
PY"#;
    let result=session.run("repository",&source,root,&binding,None,&[],&[],&[],Some(command),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let cases:serde_json::Value=serde_json::from_str(&result.stdout).unwrap_or(serde_json::Value::Null);
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_exec_mediation","head":head,"generation":source.generation(),
        "status":result.status,"cases":cases,"actual_vm_commands":1,"input_closure_complete":false,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    ensure!(result.status=="passed" && cases.as_object().is_some_and(|cases|cases.len()==6),
        "exec mediation unavailable: {} {}",result.stdout,result.stderr);
    Ok(())
}

/// Kernel-policy feasibility in the pinned guest; this grants no CI authority.
#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_native_unit_policy()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-native-unit-policy",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("native-unit-policy-fixture-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::write(root.join("native.so"),CARGO_EXEC_DENIAL_LIBRARY)?;
    std::fs::write(root.join("cases.py"),CARGO_UNIT_POLICY_CASES)?;
    git(root,&["init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","native unit policy fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let binding=binding(root).await?;let session=vm::Session::prepare(&owner,&target)?;
    let source=snapshot(root,&binding,&target).await?;
    let command=r#"python3 -I - <<'PY'
import json,shutil,signal,subprocess,sys
shutil.copyfile('/workspace/native.so','/workspace/target/native.so')
results={}
for mode in ('namespace','async','clone','fork','exec','threads','x32'):
 result=subprocess.run([sys.executable,'-I','/workspace/cases.py','/workspace/target/native.so',mode],capture_output=True,text=True,timeout=15)
 expected=-signal.SIGSYS if mode=='x32' else 0
 results[mode]={'exit_code':result.returncode,'stdout':result.stdout,'stderr':result.stderr}
 assert result.returncode==expected,results[mode]
 if mode!='x32':assert result.stdout=='GUARDED\n',results[mode]
print(json.dumps(results))
PY"#;
    let result=session.run("repository",&source,root,&binding,None,&[],&[],&[],Some(command),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let cases:serde_json::Value=serde_json::from_str(&result.stdout).unwrap_or(serde_json::Value::Null);
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_native_unit_policy","head":head,"generation":source.generation(),
        "status":result.status,"cases":cases,"actual_vm_commands":1,"input_closure_complete":false,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    ensure!(result.status=="passed" && cases.as_object().is_some_and(|cases|cases.len()==7),
        "native unit policy unavailable: {} {}",result.stdout,result.stderr);
    Ok(())
}

/// Actual namespace syscall/replay cases on the pinned guest cache filesystem.
/// No original Cargo fixture, execution assertion or proof authority changes.
#[cfg(all(target_os="linux",target_arch="x86_64"))]
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_namespace_steps()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-namespace-steps",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("namespace-step-fixture-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir(root.join("tests"))?;
    for (name,bytes) in [
        ("ci-cargo-planner.py",include_bytes!("../../../scripts/ci-cargo-planner.py").as_slice()),
        ("ci-cargo-relay.py",include_bytes!("../../../scripts/ci-cargo-relay.py").as_slice()),
        ("ci-cargo-staging.py",include_bytes!("../../../scripts/ci-cargo-staging.py").as_slice()),
        ("ci-cargo-observer.py",include_bytes!("../../../scripts/ci-cargo-observer.py").as_slice()),
        ("ci-lookup-inputs.py",include_bytes!("../../../scripts/ci-lookup-inputs.py").as_slice()),
        ("tests/ci_cargo_parent_signals_test.py",include_bytes!("../../../scripts/tests/ci_cargo_parent_signals_test.py").as_slice()),
        ("tests/ci_cargo_jobserver_observation_test.py",include_bytes!("../../../scripts/tests/ci_cargo_jobserver_observation_test.py").as_slice()),
        ("tests/ci_cargo_credits_test.py",include_bytes!("../../../scripts/tests/ci_cargo_credits_test.py").as_slice()),
        ("tests/ci_cargo_active_signals_test.py",include_bytes!("../../../scripts/tests/ci_cargo_active_signals_test.py").as_slice()),
        ("ci-cargo-views.py",include_bytes!("../../../scripts/ci-cargo-views.py").as_slice()),
        ("ci-cache-generation.py",include_bytes!("../../../scripts/ci-cache-generation.py").as_slice()),
        ("tests/ci_cargo_capture_test.py",include_bytes!("../../../scripts/tests/ci_cargo_capture_test.py").as_slice()),
        ("tests/ci_cargo_namespace_apply_test.py",include_bytes!("../../../scripts/tests/ci_cargo_namespace_apply_test.py").as_slice()),
        ("tests/ci_cargo_namespace_sequence_test.py",include_bytes!("../../../scripts/tests/ci_cargo_namespace_sequence_test.py").as_slice()),
        ("tests/ci_cargo_namespace_test.py",include_bytes!("../../../scripts/tests/ci_cargo_namespace_test.py").as_slice()),
        ("tests/ci_cargo_transfer_test.py",include_bytes!("../../../scripts/tests/ci_cargo_transfer_test.py").as_slice()),
        ("tests/ci_cargo_transfer_objects_test.py",include_bytes!("../../../scripts/tests/ci_cargo_transfer_objects_test.py").as_slice())] {
        std::fs::write(root.join(name),bytes)?;
    }
    git(root,&["-c","init.templateDir=","init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null",
        "-c","commit.gpgsign=false","commit","-qm","namespace syscall fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let binding=binding(root).await?;let session=vm::Session::prepare(&owner,&target)?;
    let source=snapshot(root,&binding,&target).await?;
    let command=r#"TMPDIR=/workspace/target python3 -I - <<'PY'
import io,json,os,pathlib,sys,tempfile,unittest
assert os.getuid()==1000
assert pathlib.Path(tempfile.gettempdir())==pathlib.Path('/workspace/target')
sys.path.insert(0,'/workspace/tests')
suite=unittest.TestSuite([unittest.TestLoader().discover('/workspace/tests',pattern=pattern)
 for pattern in ('ci_cargo_namespace_apply_test.py','ci_cargo_namespace_sequence_test.py','ci_cargo_transfer_objects_test.py','ci_cargo_transfer_test.py','ci_cargo_parent_signals_test.py','ci_cargo_active_signals_test.py','ci_cargo_credits_test.py','ci_cargo_jobserver_observation_test.py')])
assert suite.countTestCases()==106
stream=io.StringIO();result=unittest.TextTestRunner(stream=stream,verbosity=2).run(suite)
print(json.dumps({'uid':os.getuid(),'cache_device':os.stat('/workspace/target').st_dev,
 'tests_run':result.testsRun,'failures':len(result.failures),'errors':len(result.errors),
 'successful':result.wasSuccessful(),'details':stream.getvalue(),
 'input_closure_complete':False,'full_ci':False,'shipping_authority':False}))
sys.exit(0 if result.wasSuccessful() else 1)
PY"#;
    let result=session.run("repository",&source,root,&binding,None,&[],&[],&[],Some(command),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let cases:serde_json::Value=serde_json::from_str(&result.stdout).unwrap_or(serde_json::Value::Null);
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_shared_ci_namespace_steps","head":head,"generation":source.generation(),
        "status":result.status,"cases":cases,"actual_vm_commands":1,"input_closure_complete":false,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0}))?)?;
    ensure!(result.status=="passed" && cases["uid"]==1000 && cases["tests_run"]==106
        && cases["failures"]==0 && cases["errors"]==0 && cases["successful"]==true,
        "namespace step guest cases unavailable: {} {}",result.stdout,result.stderr);
    Ok(())
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


/// Observe original worker admission through the ordinary repository adapter.
/// This does not exercise projection, parent resume or grant passing-CI authority.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_original_worker_admission() -> Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;
    std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-original-worker-entry",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("original-worker-entry-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir(root.join("src"))?;
    std::fs::write(root.join("Cargo.toml"),"[package]\nname=\"worker-entry-probe\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[workspace]\n")?;
    std::fs::write(root.join("Cargo.lock"),"version=4\n\n[[package]]\nname=\"worker-entry-probe\"\nversion=\"0.1.0\"\n")?;
    std::fs::write(root.join("src/main.rs"),"fn main() {}\n")?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;
    git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid",
        "-c","core.hooksPath=/dev/null","-c","commit.gpgsign=false","commit","-qm","worker admission fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
    let session=vm::Session::prepare(&owner,&target)?;
    session.enable_protected_repository()?;
    let result=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("printf ORIGINAL_WORKER_ENTRY"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    ensure!(result.status=="passed" && result.stdout=="ORIGINAL_WORKER_ENTRY","original worker body failed");
    let execution=result.repository_execution.context("worker entry receipt absent")?;
    ensure!(execution["version"]==2 && execution["original_worker_cleanup"]==true,"worker receipt version/cleanup absent");
    let entry=&execution["worker_entry"];
    ensure!(entry["kind"]=="ci-repository-worker-admission" && entry["entry"]["uids"]==serde_json::json!([1000,1000,1000,1000]),"worker identity absent");
    ensure!(execution["input_closure_complete"]==false && execution["full_ci"]==false && execution["shipping_authority"]==false,"diagnostic acquired CI authority");
    let receipt=serde_json::json!({"version":1,"kind":"real_original_worker_admission","execution":execution,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

/// Real ordinary workers and a fork descendant must share the original scope;
/// Cargo's initial snapshot pauses that full population, then runs after thaw.
/// This diagnostic grants no complete-input, projection or passing-CI authority.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_repository_worker_pause() -> Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence required");
    std::fs::create_dir(&evidence)?;
    std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-worker-pause",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("worker-pause-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir(root.join("src"))?;
    std::fs::write(root.join("Cargo.toml"),"[package]\nname=\"worker-pause-probe\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[workspace]\n[profile.dev]\ndebug=0\nincremental=false\n")?;
    std::fs::write(root.join("Cargo.lock"),"version=4\n\n[[package]]\nname=\"worker-pause-probe\"\nversion=\"0.1.0\"\n")?;
    std::fs::write(root.join("src/main.rs"),"fn main() {}\n")?;
    std::fs::write(root.join("driver.py"),r#"import os,subprocess
from pathlib import Path
scope=Path('/proc/self/cgroup').read_text()
assert scope.startswith('0::/ci-workers/'),scope
stop=Path('/tmp/worker-scope-stop')
child_program='''import os,time
from pathlib import Path
child=os.fork()
if child==0:
 while not Path('/tmp/worker-scope-stop').exists():time.sleep(.01)
 os._exit(0)
print(Path('/proc/self/cgroup').read_text().strip(),flush=True)
assert os.waitpid(child,0)[1]==0
'''
child=subprocess.Popen(['/usr/bin/python3','-I','-c',child_program],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,text=True)
try:
 assert child.stdout.readline().strip()==scope.strip()
 result=subprocess.run(['/toolchain/bin/cargo','build','--offline','--locked'])
 assert result.returncode==0,result.returncode
finally:
 stop.write_text('stop')
 output,_=child.communicate(timeout=10)
assert child.returncode==0,(child.returncode,output)
print('WORKER_SCOPE_PAUSE_OK',flush=True)
"#)?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid",
        "-c","core.hooksPath=/dev/null","-c","commit.gpgsign=false","commit","-qm","worker pause fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
    let session=vm::Session::prepare(&owner,&target)?;session.enable_protected_repository()?;
    let result=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    ensure!(result.status=="passed" && result.stdout.contains("WORKER_SCOPE_PAUSE_OK"),"worker pause/Cargo failed");
    let execution=result.repository_execution.context("worker scope receipt absent")?;
    let entry=&execution["worker_entry"];let group=&entry["worker_scope"];
    ensure!(entry["version"]==2 && group["scope_id"]==entry["scope_id"],"original worker scope absent");
    let pauses=entry["worker_pauses"].as_array().context("worker pauses absent")?;
    ensure!(pauses.len()==1 && pauses[0]["scope"]==*group && pauses[0]["frozen"]==true,"original worker pause absent");
    ensure!(pauses[0]["uid_cohort"]["tasks"].as_array().is_some_and(|tasks|tasks.len()>=4),"fork descendant census absent");
    let workers=execution["worker_units"].as_array().context("worker units absent")?;
    ensure!(workers.iter().filter(|unit|unit["uid"]==1000 && unit["worker_scope"]["scope_id"]==entry["scope_id"]).count()>=2,
        "additional worker enrollment absent");
    ensure!(execution["original_worker_cleanup"]==true && execution["input_closure_complete"]==false
        && execution["full_ci"]==false && execution["shipping_authority"]==false,"diagnostic acquired CI authority");
    let receipt=serde_json::json!({"version":1,"kind":"real_repository_worker_pause","execution":execution,
        "full_ci_pass":false,"shipping_authority":false,"autonomous_merges_claimed":0});
    audit.record_controller(receipt.clone())?;audit.protected_health()?;
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

/// Exercise the actual targeted adapter, including its original build/test argv
/// and a failing test's real Cargo exit. This is execution evidence, not a
/// complete-input or passing-CI capability. The original publication regression
/// remains a separate unchanged acceptance requirement.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_structured_cargo_execution()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private fixture evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-structured-cargo",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("structured-cargo-").tempdir_in(&target)?;
    let root=fixture.path();
    for name in ["arithmetic/src","consumer/src"] {std::fs::create_dir_all(root.join(name))?;}
    std::fs::write(root.join("Cargo.toml"),"[workspace]\nmembers = [\"arithmetic\", \"consumer\"]\nresolver = \"2\"\n[profile.dev]\ndebug = 0\nincremental = false\n[profile.test]\ndebug = 0\nincremental = false\n")?;
    std::fs::write(root.join("arithmetic/Cargo.toml"),"[package]\nname = \"arithmetic\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ndoctest = false\n")?;
    std::fs::write(root.join("arithmetic/src/lib.rs"),"pub fn answer() -> u32 { 42 }\n")?;
    std::fs::write(root.join("consumer/Cargo.toml"),"[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ndoctest = false\n[dependencies]\narithmetic = { path = \"../arithmetic\" }\n")?;
    let passing="pub fn value() -> u32 { arithmetic::answer() }\n#[test] fn original_case() { assert_eq!(value(), 42); }\n";
    std::fs::write(root.join("consumer/src/lib.rs"),passing)?;
    std::fs::write(root.join("Cargo.lock"),"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"arithmetic\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"consumer\"\nversion = \"0.1.0\"\ndependencies = [\n \"arithmetic\",\n]\n")?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;
    let commit=|message:&str|vec!["-c".to_owned(),"user.name=Fixture".to_owned(),"-c".to_owned(),"user.email=fixture@example.invalid".to_owned(),"-c".to_owned(),"core.hooksPath=/dev/null".to_owned(),"-c".to_owned(),"commit.gpgsign=false".to_owned(),"commit".to_owned(),"-qm".to_owned(),message.to_owned()];
    git(root,&["add","."]).await?;
    let args=commit("structured Cargo adapter fixture");git(root,&args.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let session=vm::Session::prepare(root,&target)?;session.enable_protected_cargo()?;
    let timeout=std::time::Duration::from_secs(180);let packages=vec!["consumer".to_owned()];
    let mut outcomes=Vec::new();
    for failing in [false,true] {
        if failing {
            std::fs::write(root.join("consumer/src/lib.rs"),passing.replace("assert_eq!(value(), 42)","assert_eq!(value(), 99, \"original-failing-test\")"))?;
            git(root,&["add","."]).await?;
            let args=commit("make original unit test fail");git(root,&args.iter().map(String::as_str).collect::<Vec<_>>()).await?;
        }
        let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
        let outcome=session.run("targeted",&source,root,&bound,None,&[],&packages,&[],None,
            &storage,&target,&audit,timeout).await?;
        let record=serde_json::json!({"status":&outcome.status,"stdout":&outcome.stdout,"stderr":&outcome.stderr,
            "execution":&outcome.cargo_execution,"shipping_authority":false});
        std::fs::write(evidence.join(if failing {"failing.json"} else {"passing.json"}),serde_json::to_vec_pretty(&record)?)?;
        ensure!(outcome.status==if failing {"failed"} else {"passed"},"structured Cargo status mismatch; inspect evidence");
        let execution=outcome.cargo_execution.as_ref().context("normal adapter execution receipt missing")?;
        let commands=execution["commands"].as_array().context("command evidence missing")?;
        ensure!(commands.len()==2 && commands[0]["exit_code"]==0,"original build/test driver order changed");
        ensure!(commands[0]["argv"]==serde_json::json!(["/toolchain/bin/cargo","build","-p","consumer"])
            && commands[1]["argv"]==serde_json::json!(["/toolchain/bin/cargo","test","-p","consumer","--no-fail-fast","--","--test-threads=1"]),"original structured argv changed");
        ensure!(commands[0]["compiler_calls"]==2 && commands[1]["compiler_calls"]==1,"unchanged dependency/library units were rebuilt");
        ensure!(commands[1]["exit_code"]==if failing {serde_json::json!(101)} else {serde_json::json!(0)},"real Cargo exit lost");
        ensure!(execution["shipping_authority"]==false && execution["full_ci"]==false
            && execution["input_closure_complete"]==false,"partial executor acquired proof authority");
        ensure!(outcome.stdout.contains("original_case"),"original test output lost");
        if failing {ensure!(outcome.stdout.contains("original-failing-test"),"original failing diagnostic lost");}
        outcomes.push(record);
    }
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"structured-cargo-adapter-replay","outcomes":outcomes,
        "original_commands_preserved":true,"failing_test_exit_preserved":true,
        "input_closure_complete":false,"full_ci":false,"shipping_authority":false}))?)?;
    Ok(())
}

/// Actual four-target Cargo scheduling through the normal protected adapter.
/// Component semaphores and a caller-created pipe cannot satisfy this gate.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_four_parent_jobserver_progress()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private fixture evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-four-parent-jobserver",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("four-parent-cargo-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir_all(root.join("src/bin"))?;
    std::fs::write(root.join("Cargo.toml"),"[package]\nname=\"four-parent-probe\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[profile.dev]\ndebug=0\nincremental=false\ncodegen-units=1\n")?;
    std::fs::write(root.join("Cargo.lock"),"version=4\n\n[[package]]\nname=\"four-parent-probe\"\nversion=\"0.1.0\"\n")?;
    // Enough real code generation for the four original compiler processes to
    // overlap. This changes neither the original V3 fixture nor its budgets.
    for index in 0..4 {
        let mut source=format!("#![allow(dead_code)]\n#[inline(never)] fn f0(x:u64)->u64{{x.wrapping_add({index})}}\n");
        for function in 1..2000 {source.push_str(&format!("#[inline(never)] fn f{function}(x:u64)->u64{{f{}(x).wrapping_add({function})}}\n",function-1));}
        source.push_str("fn main(){std::hint::black_box(f1999(17));}\n");
        std::fs::write(root.join(format!("src/bin/parent_{index}.rs")),source)?;
    }
    std::fs::write(root.join("driver.py"),r#"import json,os,subprocess
assert os.getresuid()==(1000,1000,1000) and os.getgroups()==[]
environment=dict(os.environ,CARGO_BUILD_JOBS='4',CARGO_INCREMENTAL='0')
command=['cargo','build','--offline','--locked','--jobs','4','--bins']
for iteration in range(2):
 result=subprocess.run(command,env=environment)
 assert result.returncode==0,(iteration,result.returncode)
print(json.dumps({'original_cargo_builds':2,'requested_jobs':4,'targets':4}),flush=True)
"#)?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null",
        "-c","commit.gpgsign=false","commit","-qm","four-parent Cargo fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
    let session=vm::Session::prepare(&owner,&target)?;session.enable_protected_repository()?;
    let outcome=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await;
    let result=match outcome {
        Ok(result)=>result,
        Err(error)=>{
            std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
                "version":1,"kind":"real_four_parent_jobserver","head":head,"generation":source.generation(),
                "status":"held","diagnostic":error.to_string(),"requested_jobs":4,"targets":4,
                "four_parent_exhaustion_verified":false,"cancellation_reacquisition_verified":false,
                "input_closure_complete":false,"full_ci":false,"shipping_authority":false}))?)?;
            return Err(error)
        }
    };
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"real_four_parent_jobserver","head":head,"generation":source.generation(),
        "status":result.status,"execution":result.repository_execution,"stdout":result.stdout,"stderr":result.stderr,
        "four_parent_exhaustion_verified":false,"cancellation_reacquisition_verified":false,
        "input_closure_complete":false,"full_ci":false,"shipping_authority":false}))?)?;
    ensure!(result.status=="passed","four-parent original Cargo execution did not complete");
    let execution=result.repository_execution.as_ref().context("original protected repository receipt absent")?;
    let commands=execution["cargo_commands"].as_array().context("original Cargo commands absent")?;
    ensure!(commands.len()==2 && commands.iter().all(|command|command["cargo_exit"]==0 && command["original_cargo_cleanup"]==true),
        "original four-parent Cargo cleanup/exit absent");
    ensure!(commands.iter().map(|command|command["compiler_calls"].as_u64().unwrap_or(u64::MAX)).collect::<Vec<_>>()==vec![4,0],
        "four original compilers or mandatory compatible within-job reuse absent");
    // This execution test is only the first stage. Its postmortem companion
    // must verify the root-owned pipe/slot/cohort observations. It does not
    // initiate cancellation, so it cannot claim that acceptance requirement.
    ensure!(execution["input_closure_complete"]==false && execution["full_ci"]==false && execution["shipping_authority"]==false,
        "jobserver experiment acquired CI authority");
    Ok(())
}

/// The actual repository adapter must mediate nested worker spawns and hand
/// Cargo to the planner credential. The worker's own target metadata cannot
/// cause reuse, and its direct exec syscalls must remain denied.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_repository_cargo_execution()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private fixture evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-repository-cargo",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("repository-cargo-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir(root.join("src"))?;
    std::fs::write(root.join("Cargo.toml"),"[package]\nname = \"nested-consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ndoctest = false\n[profile.dev]\ndebug = 0\nincremental = false\n[profile.test]\ndebug = 0\nincremental = false\n")?;
    std::fs::write(root.join("Cargo.lock"),"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"nested-consumer\"\nversion = \"0.1.0\"\n")?;
    std::fs::write(root.join("src/lib.rs"),"pub fn value() -> u8 { 7 }\n#[test] fn nested_original_case() { assert_eq!(value(), 7); }\n")?;
    std::fs::write(root.join("repository_driver.py"),r#"import ctypes,errno,json,os,platform,subprocess
from pathlib import Path
assert os.getresuid()==(1000,1000,1000) and os.getresgid()==(1000,1000,1000) and os.getgroups()==[]
libc=ctypes.CDLL(None,use_errno=True)
number={'x86_64':59,'aarch64':221}[platform.machine()]
args=(ctypes.c_char_p*2)(b'/usr/bin/true',None);env=(ctypes.c_char_p*1)(None)
assert libc.syscall(number,ctypes.c_char_p(b'/usr/bin/true'),args,env)==-1
assert ctypes.get_errno()==errno.EPERM
# Worker-owned metadata is deliberately writable and must be ignored by the
# protected planner; the subsequent first build must actually compile.
target=Path(os.environ['CARGO_TARGET_DIR']);fake=target/'debug/.fingerprint/worker-forged'
fake.mkdir(parents=True);(fake/'lib-nested_consumer').write_text('fake-success')
for command in (['cargo','build','-p','nested-consumer'],['cargo','build','-p','nested-consumer'],
 ['cargo','test','-p','nested-consumer','--lib','--','--test-threads=1']):
 result=subprocess.run(command)
 assert result.returncode==0,result.returncode
protected=[path for path in target.iterdir() if path.stat().st_uid==0 and path.stat().st_gid==1001]
assert len(protected)==1
try:list(protected[0].iterdir())
except PermissionError:pass
else:raise RuntimeError('worker can traverse protected Cargo state')
print(json.dumps({'worker_uid':os.getuid(),'raw_exec_denied':True,'planner_metadata_denied':True}),flush=True)
"#)?;
    std::fs::write(root.join("followup_driver.py"),r#"import subprocess
for command in (['cargo','build','-p','nested-consumer'],['cargo','test','-p','nested-consumer','--lib','--','--test-threads=1']):
 result=subprocess.run(command)
 assert result.returncode==0,result.returncode
print('RECOVERED_ORIGINAL_EXECUTOR',flush=True)
"#)?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null",
        "-c","commit.gpgsign=false","commit","-qm","nested repository Cargo fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
    let session=vm::Session::prepare(root,&target)?;session.enable_protected_repository()?;
    let outcome=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I repository_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let record=serde_json::json!({"status":&outcome.status,"stdout":&outcome.stdout,"stderr":&outcome.stderr,
        "execution":&outcome.repository_execution,"shipping_authority":false});
    std::fs::write(evidence.join("observed.json"),serde_json::to_vec_pretty(&record)?)?;
    ensure!(outcome.status=="passed","repository adapter failed; inspect private observed.json");
    let execution=outcome.repository_execution.as_ref().context("repository execution receipt absent")?;
    let commands=execution["cargo_commands"].as_array().context("nested Cargo evidence absent")?;
    ensure!(commands.len()==3 && commands.iter().all(|command|command["cargo_exit"]==0
        && command["original_cargo_cleanup"]==true),"nested original Cargo cleanup or exit lost");
    ensure!(commands.iter().map(|command|command["compiler_calls"].as_u64().unwrap_or(u64::MAX)).collect::<Vec<_>>()==vec![1,0,1],
        "worker metadata was trusted, unchanged Cargo compiled, or original test failed to compile");
    let units=execution["worker_units"].as_array().context("original worker scopes absent")?;
    ensure!(units.iter().any(|unit|unit["uid"]==1000) && units.iter().filter(|unit|unit["uid"]==1001).count()==3,
        "worker/planner credential handoff absent");
    ensure!(outcome.stdout.contains("nested_original_case") && outcome.stdout.contains("\"raw_exec_denied\": true")
        && outcome.stdout.contains("\"planner_metadata_denied\": true"),"original execution guard/output absent");
    ensure!(execution["input_closure_complete"]==false && execution["full_ci"]==false && execution["shipping_authority"]==false,
        "partial repository routing acquired CI authority");
    let resumed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I followup_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let resumed_record=serde_json::json!({"status":&resumed.status,"stdout":&resumed.stdout,"stderr":&resumed.stderr,
        "execution":&resumed.repository_execution,"shipping_authority":false});
    std::fs::write(evidence.join("resumed.json"),serde_json::to_vec_pretty(&resumed_record)?)?;
    ensure!(resumed.status=="passed" && resumed.stdout.contains("RECOVERED_ORIGINAL_EXECUTOR")
        && resumed.stdout.contains("nested_original_case"),"original executor recovery failed; inspect resumed.json");
    let recovered=resumed.repository_execution.as_ref().context("resumed repository evidence absent")?;
    let recovered_commands=recovered["cargo_commands"].as_array().context("resumed commands absent")?;
    ensure!(recovered_commands.len()==2 && recovered_commands.iter().all(|command|command["compiler_calls"]==0
        && command["cargo_exit"]==0 && command["original_cargo_cleanup"]==true),"cross-VM unchanged units rebuilt or failed");
    ensure!(recovered["checkpoint"]["retained_executor"]==true
        && recovered["checkpoint"]["predecessor"]==execution["command"],"recovery did not bind original predecessor");
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"repository-nested-cargo-adapter-replay","observed":record,"resumed":resumed_record,
        "cross_vm_unchanged_compiler_calls_zero":true,
        "raw_worker_exec_denied":true,"worker_metadata_ignored":true,"planner_metadata_denied":true,
        "input_closure_complete":false,"full_ci":false,"shipping_authority":false}))?)?;
    Ok(())
}

/// Completed generated inputs must reach the actual Cargo/compiler views.
/// Unchanged commands retain their original producer and compiled units; a new
/// same-mtime producer must hold before stale reuse until selector recovery lands.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_generated_forest_execution()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private fixture evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-generated-forest",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("generated-forest-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir(root.join("arbitrary"))?;
    std::fs::write(root.join("Cargo.toml"),"[package]\nname = \"nested-consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"arbitrary/source.rs\"\ndoctest = false\n[profile.dev]\ndebug = 0\nincremental = false\n[profile.test]\ndebug = 0\nincremental = false\n")?;
    std::fs::write(root.join("Cargo.lock"),"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"nested-consumer\"\nversion = \"0.1.0\"\n")?;
    std::fs::write(root.join("arbitrary/source.rs"),"include!(\"../node_modules/generated.rs\"); pub fn value() -> u8 { GENERATED_VALUE }\n#[test] fn nested_original_case() { assert_eq!(value(), 7); }\n")?;
    std::fs::write(root.join("package.json"),"{}\n")?;
    std::fs::write(root.join("seed_driver.py"),r#"import os
from pathlib import Path
p=Path('node_modules/generated.rs');p.write_text('pub const GENERATED_VALUE: u8 = 7;\n')
os.utime(p,ns=(946684800000000000,946684800000000000))
print('ORIGINAL_GENERATED_SEED',flush=True)
"#)?;
    std::fs::write(root.join("change_driver.py"),r#"import os
from pathlib import Path
p=Path('node_modules/generated.rs');before=p.stat()
p.write_text('pub const GENERATED_VALUE: u8 = 9;\n')
os.utime(p,ns=(before.st_atime_ns,before.st_mtime_ns))
assert p.stat().st_mtime_ns==before.st_mtime_ns
print('ORIGINAL_GENERATED_CHANGE_SAME_MTIME',flush=True)
"#)?;
    std::fs::write(root.join("repository_driver.py"),r#"import ctypes,errno,json,os,platform,subprocess
from pathlib import Path
assert os.getresuid()==(1000,1000,1000) and os.getresgid()==(1000,1000,1000) and os.getgroups()==[]
libc=ctypes.CDLL(None,use_errno=True)
number={'x86_64':59,'aarch64':221}[platform.machine()]
args=(ctypes.c_char_p*2)(b'/usr/bin/true',None);env=(ctypes.c_char_p*1)(None)
assert libc.syscall(number,ctypes.c_char_p(b'/usr/bin/true'),args,env)==-1
assert ctypes.get_errno()==errno.EPERM
# Worker-owned metadata is deliberately writable and must be ignored by the
# protected planner; the subsequent first build must actually compile.
target=Path(os.environ['CARGO_TARGET_DIR']);fake=target/'debug/.fingerprint/worker-forged'
fake.mkdir(parents=True);(fake/'lib-nested_consumer').write_text('fake-success')
for command in (['cargo','build','-p','nested-consumer'],['cargo','build','-p','nested-consumer'],
 ['cargo','test','-p','nested-consumer','--lib','--','--test-threads=1']):
 result=subprocess.run(command)
 assert result.returncode==0,result.returncode
protected=[path for path in target.iterdir() if path.stat().st_uid==0 and path.stat().st_gid==1001]
assert len(protected)==1
try:list(protected[0].iterdir())
except PermissionError:pass
else:raise RuntimeError('worker can traverse protected Cargo state')
print(json.dumps({'worker_uid':os.getuid(),'raw_exec_denied':True,'planner_metadata_denied':True}),flush=True)
"#)?;
    std::fs::write(root.join("followup_driver.py"),r#"import subprocess
for command in (['cargo','build','-p','nested-consumer'],['cargo','test','-p','nested-consumer','--lib','--','--test-threads=1']):
 result=subprocess.run(command)
 assert result.returncode==0,result.returncode
print('RECOVERED_ORIGINAL_EXECUTOR',flush=True)
"#)?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null",
        "-c","commit.gpgsign=false","commit","-qm","nested repository Cargo fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
    let session=vm::Session::prepare(root,&target)?;session.enable_protected_repository()?;
    let seeded=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I seed_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    std::fs::write(evidence.join("seeded.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "status":&seeded.status,"stdout":&seeded.stdout,"stderr":&seeded.stderr,"execution":&seeded.repository_execution}))?)?;
    ensure!(seeded.status=="passed" && seeded.stdout.contains("ORIGINAL_GENERATED_SEED"),"seed step failed; inspect seeded.json");
    let seed_execution=seeded.repository_execution.as_ref().context("seed execution absent")?;
    ensure!(seed_execution["cargo_commands"].as_array().is_some_and(|commands|commands.is_empty())
        && seed_execution["checkpoint"]["generated_forests"].as_array().is_some_and(|forests|forests.len()==1),
        "original completed step did not publish generated inputs");
    let outcome=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I repository_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let record=serde_json::json!({"status":&outcome.status,"stdout":&outcome.stdout,"stderr":&outcome.stderr,
        "execution":&outcome.repository_execution,"shipping_authority":false});
    std::fs::write(evidence.join("observed.json"),serde_json::to_vec_pretty(&record)?)?;
    ensure!(outcome.status=="passed","repository adapter failed; inspect private observed.json");
    let execution=outcome.repository_execution.as_ref().context("repository execution receipt absent")?;
    let commands=execution["cargo_commands"].as_array().context("nested Cargo evidence absent")?;
    ensure!(commands.len()==3 && commands.iter().all(|command|command["cargo_exit"]==0
        && command["original_cargo_cleanup"]==true),"nested original Cargo cleanup or exit lost");
    ensure!(commands.iter().map(|command|command["compiler_calls"].as_u64().unwrap_or(u64::MAX)).collect::<Vec<_>>()==vec![1,0,1],
        "worker metadata was trusted, unchanged Cargo compiled, or original test failed to compile");
    let units=execution["worker_units"].as_array().context("original worker scopes absent")?;
    ensure!(units.iter().any(|unit|unit["uid"]==1000) && units.iter().filter(|unit|unit["uid"]==1001).count()==3,
        "worker/planner credential handoff absent");
    ensure!(outcome.stdout.contains("nested_original_case") && outcome.stdout.contains("\"raw_exec_denied\": true")
        && outcome.stdout.contains("\"planner_metadata_denied\": true"),"original execution guard/output absent");
    ensure!(execution["input_closure_complete"]==false && execution["full_ci"]==false && execution["shipping_authority"]==false,
        "partial repository routing acquired CI authority");
    let resumed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I followup_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let resumed_record=serde_json::json!({"status":&resumed.status,"stdout":&resumed.stdout,"stderr":&resumed.stderr,
        "execution":&resumed.repository_execution,"shipping_authority":false});
    std::fs::write(evidence.join("resumed.json"),serde_json::to_vec_pretty(&resumed_record)?)?;
    ensure!(resumed.status=="passed" && resumed.stdout.contains("RECOVERED_ORIGINAL_EXECUTOR")
        && resumed.stdout.contains("nested_original_case"),"original executor recovery failed; inspect resumed.json");
    let captured=&execution["kernel_observation"];
    let observed_units=captured["units"].as_array().context("actual compiler kernel observations missing")?;
    ensure!(observed_units.iter().filter(|unit|unit["kind"]=="compile").count()==2
        && observed_units.iter().filter(|unit|unit["kind"]=="compile").all(|unit|unit["source_open_observed"]==true
            && unit["queries"].as_u64().is_some_and(|count|count>0)),
        "non-fixture compiler sources lack original kernel opens");
    ensure!(captured["input_closure_complete"]==false && captured["shipping_authority"]==false,
        "partial observation acquired proof authority");
    let recovered=resumed.repository_execution.as_ref().context("resumed repository evidence absent")?;
    let recovered_commands=recovered["cargo_commands"].as_array().context("resumed commands absent")?;
    ensure!(recovered_commands.len()==2 && recovered_commands.iter().all(|command|command["compiler_calls"]==0
        && command["cargo_exit"]==0 && command["original_cargo_cleanup"]==true),"cross-VM unchanged units rebuilt or failed");
    ensure!(recovered["kernel_observation"]["units"].as_array().is_some_and(|units|units.iter().all(|unit|unit["kind"]!="compile")),
        "unchanged recovery recorded compilers that did not execute");
    ensure!(recovered["checkpoint"]["retained_executor"]==true
        && recovered["checkpoint"]["predecessor"]==execution["command"],"recovery did not bind original predecessor");
    ensure!(recovered["checkpoint"]["generated_forests"]==seed_execution["checkpoint"]["generated_forests"],
        "unchanged generated forest lost original producer identity");
    let changed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I change_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    std::fs::write(evidence.join("changed.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "status":&changed.status,"stdout":&changed.stdout,"stderr":&changed.stderr,"execution":&changed.repository_execution}))?)?;
    ensure!(changed.status=="passed" && changed.stdout.contains("ORIGINAL_GENERATED_CHANGE_SAME_MTIME"),
        "new original producer step failed; inspect changed.json");
    let changed_execution=changed.repository_execution.as_ref().context("changed execution absent")?;
    ensure!(changed_execution["checkpoint"]["generated_forests"]!=seed_execution["checkpoint"]["generated_forests"],
        "same-mtime generated change reused the old producer");
    let refused=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I followup_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await;
    let error=match refused {Ok(_)=>anyhow::bail!("changed generated inputs authorized stale compiled output"),Err(error)=>error};
    let hold=error.downcast_ref::<crate::autoship_freshness::Hold>().context("generated-input refusal lost typed hold")?;
    let held_record=serde_json::to_value(hold)?;
    std::fs::write(evidence.join("held.json"),serde_json::to_vec_pretty(&held_record)?)?;
    ensure!(hold.reason==crate::autoship_freshness::Reason::InputCensusUnknown,
        "generated-input refusal did not preserve input-census reason; inspect held.json");
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"generated-forest-original-adapter-replay","observed":record,"resumed":resumed_record,
        "cross_vm_unchanged_compiler_calls_zero":true,"changed_generated_input_held":true,"held":held_record,
        "raw_worker_exec_denied":true,"worker_metadata_ignored":true,"planner_metadata_denied":true,
        "input_closure_complete":false,"full_ci":false,"shipping_authority":false}))?)?;
    Ok(())
}

/// Missing generated module queries must survive real execution and recovery.
/// A new conflicting module changes absence evidence even when every previously
/// read file retains its original bytes and old timestamp.
#[tokio::test]
#[ignore = "requires declared build VM, delegated resource policy and exclusive managed storage slot"]
async fn live_shared_ci_generated_negative_lookup_revalidation()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private fixture evidence required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"shared-ci-generated-negative",None)?;
    audit.require_run_resources()?;
    let target=crate::build_storage::effective_target(&owner);
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("generated-negative-").tempdir_in(&target)?;
    let root=fixture.path();std::fs::create_dir(root.join("arbitrary"))?;
    std::fs::write(root.join("Cargo.toml"),"[package]\nname = \"nested-consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"arbitrary/source.rs\"\ndoctest = false\n[profile.dev]\ndebug = 0\nincremental = false\n[profile.test]\ndebug = 0\nincremental = false\n")?;
    std::fs::write(root.join("Cargo.lock"),"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"nested-consumer\"\nversion = \"0.1.0\"\n")?;
    std::fs::write(root.join("arbitrary/source.rs"),"#[path=\"../node_modules/generated/mod.rs\"] mod generated; pub fn value() -> u8 { generated::value() }\n#[test] fn nested_original_case() { assert_eq!(value(), 7); }\n")?;
    std::fs::write(root.join("package.json"),"{}\n")?;
    std::fs::write(root.join("seed_driver.py"),r#"import os
from pathlib import Path
d=Path('node_modules/generated');(d/'chosen').mkdir(parents=True)
for name,text in [('mod.rs','mod chosen;pub fn value()->u8{chosen::VALUE}\n'),
 ('chosen/mod.rs','pub const VALUE:u8=7;\n')]:
 p=d/name;p.write_text(text);os.utime(p,ns=(946684800000000000,946684800000000000))
print('ORIGINAL_GENERATED_SEED',flush=True)
"#)?;
    std::fs::write(root.join("change_driver.py"),r#"import os
from pathlib import Path
d=Path('node_modules/generated');old=[d/'mod.rs',d/'chosen/mod.rs']
before=[(p.read_bytes(),p.stat().st_mtime_ns) for p in old]
p=d/'chosen.rs';p.write_text('pub const VALUE:u8=9;\n')
os.utime(p,ns=(946684800000000000,946684800000000000))
assert [(p.read_bytes(),p.stat().st_mtime_ns) for p in old]==before
print('ORIGINAL_GENERATED_CHANGE_SAME_MTIME',flush=True)
"#)?;
    std::fs::write(root.join("repository_driver.py"),r#"import ctypes,errno,json,os,platform,subprocess
from pathlib import Path
assert os.getresuid()==(1000,1000,1000) and os.getresgid()==(1000,1000,1000) and os.getgroups()==[]
libc=ctypes.CDLL(None,use_errno=True)
number={'x86_64':59,'aarch64':221}[platform.machine()]
args=(ctypes.c_char_p*2)(b'/usr/bin/true',None);env=(ctypes.c_char_p*1)(None)
assert libc.syscall(number,ctypes.c_char_p(b'/usr/bin/true'),args,env)==-1
assert ctypes.get_errno()==errno.EPERM
# Worker-owned metadata is deliberately writable and must be ignored by the
# protected planner; the subsequent first build must actually compile.
target=Path(os.environ['CARGO_TARGET_DIR']);fake=target/'debug/.fingerprint/worker-forged'
fake.mkdir(parents=True);(fake/'lib-nested_consumer').write_text('fake-success')
for command in (['cargo','build','-p','nested-consumer'],['cargo','build','-p','nested-consumer'],
 ['cargo','test','-p','nested-consumer','--lib','--','--test-threads=1']):
 result=subprocess.run(command)
 assert result.returncode==0,result.returncode
protected=[path for path in target.iterdir() if path.stat().st_uid==0 and path.stat().st_gid==1001]
assert len(protected)==1
try:list(protected[0].iterdir())
except PermissionError:pass
else:raise RuntimeError('worker can traverse protected Cargo state')
print(json.dumps({'worker_uid':os.getuid(),'raw_exec_denied':True,'planner_metadata_denied':True}),flush=True)
"#)?;
    std::fs::write(root.join("followup_driver.py"),r#"import subprocess
for command in (['cargo','build','-p','nested-consumer'],['cargo','test','-p','nested-consumer','--lib','--','--test-threads=1']):
 result=subprocess.run(command)
 assert result.returncode==0,result.returncode
print('RECOVERED_ORIGINAL_EXECUTOR',flush=True)
"#)?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null",
        "-c","commit.gpgsign=false","commit","-qm","nested repository Cargo fixture"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let bound=binding(root).await?;let source=snapshot(root,&bound,&target).await?;
    let session=vm::Session::prepare(root,&target)?;session.enable_protected_repository()?;
    let seeded=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I seed_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    std::fs::write(evidence.join("seeded.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "status":&seeded.status,"stdout":&seeded.stdout,"stderr":&seeded.stderr,"execution":&seeded.repository_execution}))?)?;
    ensure!(seeded.status=="passed" && seeded.stdout.contains("ORIGINAL_GENERATED_SEED"),"seed step failed; inspect seeded.json");
    let seed_execution=seeded.repository_execution.as_ref().context("seed execution absent")?;
    ensure!(seed_execution["cargo_commands"].as_array().is_some_and(|commands|commands.is_empty())
        && seed_execution["checkpoint"]["generated_forests"].as_array().is_some_and(|forests|forests.len()==1),
        "original completed step did not publish generated inputs");
    let outcome=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I repository_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let record=serde_json::json!({"status":&outcome.status,"stdout":&outcome.stdout,"stderr":&outcome.stderr,
        "execution":&outcome.repository_execution,"shipping_authority":false});
    std::fs::write(evidence.join("observed.json"),serde_json::to_vec_pretty(&record)?)?;
    ensure!(outcome.status=="passed","repository adapter failed; inspect private observed.json");
    let execution=outcome.repository_execution.as_ref().context("repository execution receipt absent")?;
    let commands=execution["cargo_commands"].as_array().context("nested Cargo evidence absent")?;
    ensure!(commands.len()==3 && commands.iter().all(|command|command["cargo_exit"]==0
        && command["original_cargo_cleanup"]==true),"nested original Cargo cleanup or exit lost");
    ensure!(commands.iter().map(|command|command["compiler_calls"].as_u64().unwrap_or(u64::MAX)).collect::<Vec<_>>()==vec![1,0,1],
        "worker metadata was trusted, unchanged Cargo compiled, or original test failed to compile");
    let units=execution["worker_units"].as_array().context("original worker scopes absent")?;
    ensure!(units.iter().any(|unit|unit["uid"]==1000) && units.iter().filter(|unit|unit["uid"]==1001).count()==3,
        "worker/planner credential handoff absent");
    ensure!(outcome.stdout.contains("nested_original_case") && outcome.stdout.contains("\"raw_exec_denied\": true")
        && outcome.stdout.contains("\"planner_metadata_denied\": true"),"original execution guard/output absent");
    ensure!(execution["input_closure_complete"]==false && execution["full_ci"]==false && execution["shipping_authority"]==false,
        "partial repository routing acquired CI authority");
    let resumed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I followup_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    let resumed_record=serde_json::json!({"status":&resumed.status,"stdout":&resumed.stdout,"stderr":&resumed.stderr,
        "execution":&resumed.repository_execution,"shipping_authority":false});
    std::fs::write(evidence.join("resumed.json"),serde_json::to_vec_pretty(&resumed_record)?)?;
    ensure!(resumed.status=="passed" && resumed.stdout.contains("RECOVERED_ORIGINAL_EXECUTOR")
        && resumed.stdout.contains("nested_original_case"),"original executor recovery failed; inspect resumed.json");
    let captured=&execution["kernel_observation"];
    let observed_units=captured["units"].as_array().context("actual compiler kernel observations missing")?;
    ensure!(observed_units.iter().filter(|unit|unit["kind"]=="compile").count()==2
        && observed_units.iter().filter(|unit|unit["kind"]=="compile").all(|unit|unit["source_open_observed"]==true
            && unit["queries"].as_u64().is_some_and(|count|count>0)),
        "non-fixture compiler sources lack original kernel opens");
    ensure!(captured["input_closure_complete"]==false && captured["shipping_authority"]==false,
        "partial observation acquired proof authority");
    let recovered=resumed.repository_execution.as_ref().context("resumed repository evidence absent")?;
    let recovered_commands=recovered["cargo_commands"].as_array().context("resumed commands absent")?;
    ensure!(recovered_commands.len()==2 && recovered_commands.iter().all(|command|command["compiler_calls"]==0
        && command["cargo_exit"]==0 && command["original_cargo_cleanup"]==true),"cross-VM unchanged units rebuilt or failed");
    ensure!(recovered["kernel_observation"]["units"].as_array().is_some_and(|units|units.iter().all(|unit|unit["kind"]!="compile")),
        "unchanged recovery recorded compilers that did not execute");
    ensure!(recovered["checkpoint"]["retained_executor"]==true
        && recovered["checkpoint"]["predecessor"]==execution["command"],"recovery did not bind original predecessor");
    ensure!(recovered["checkpoint"]["generated_forests"]==seed_execution["checkpoint"]["generated_forests"],
        "unchanged generated forest lost original producer identity");
    let changed=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I change_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await?;
    std::fs::write(evidence.join("changed.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "status":&changed.status,"stdout":&changed.stdout,"stderr":&changed.stderr,"execution":&changed.repository_execution}))?)?;
    ensure!(changed.status=="passed" && changed.stdout.contains("ORIGINAL_GENERATED_CHANGE_SAME_MTIME"),
        "new original producer step failed; inspect changed.json");
    let changed_execution=changed.repository_execution.as_ref().context("changed execution absent")?;
    ensure!(changed_execution["checkpoint"]["generated_forests"]!=seed_execution["checkpoint"]["generated_forests"],
        "same-mtime generated change reused the old producer");
    let refused=session.run("repository",&source,root,&bound,None,&[],&[],&[],Some("python3 -I followup_driver.py"),
        &storage,&target,&audit,std::time::Duration::from_secs(180)).await;
    let error=match refused {Ok(_)=>anyhow::bail!("changed generated inputs authorized stale compiled output"),Err(error)=>error};
    let hold=error.downcast_ref::<crate::autoship_freshness::Hold>().context("generated-input refusal lost typed hold")?;
    let held_record=serde_json::to_value(hold)?;
    std::fs::write(evidence.join("held.json"),serde_json::to_vec_pretty(&held_record)?)?;
    ensure!(hold.reason==crate::autoship_freshness::Reason::InputCensusUnknown,
        "generated-input refusal did not preserve input-census reason; inspect held.json");
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"generated-negative-original-adapter-replay","observed":record,"resumed":resumed_record,
        "cross_vm_unchanged_compiler_calls_zero":true,"changed_generated_input_held":true,"held":held_record,
        "raw_worker_exec_denied":true,"worker_metadata_ignored":true,"planner_metadata_denied":true,
        "input_closure_complete":false,"full_ci":false,"shipping_authority":false}))?)?;
    Ok(())
}

/// Rehearse actual legacy-image cleanup while the original raw image is held
/// read-only behind a temporary-write view. This cannot authorize CI or merge.
#[tokio::test]
#[ignore = "requires an existing registered legacy image, delegated VM policy and exclusive managed storage"]
async fn live_shared_ci_legacy_recovery_preview()->Result<()> {
    let owner=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_REPO")?).canonicalize()?;
    let target=PathBuf::from(std::env::var("AUTOSHIP_CI_PREVIEW_TARGET")?);
    ensure!(target.is_absolute() && target.canonicalize()?==target
        && target.join(".augmentagent-target.json").is_file() && target.join("ci-vm/cache.img").is_file(),
        "existing registered image required for read-only rehearsal");
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_CI_ACCEPTANCE_EVIDENCE")?);
    ensure!(evidence.is_absolute() && !evidence.starts_with(&owner),"private evidence directory required");
    std::fs::create_dir(&evidence)?;std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let audit=workflow::workflow_audit(&evidence.join("workflow.db"),"legacy-recovery-preview",None)?;
    audit.require_run_resources()?;
    let storage=crate::build_storage::admit_ci(&owner,&target).await?;
    let fixture=tempfile::Builder::new().prefix("recovery-preview-context-").tempdir_in(&target)?;
    let root=fixture.path();
    // This repository identifies the diagnostic input, not application CI.
    std::fs::write(root.join("context.json"),serde_json::to_vec(&serde_json::json!({
        "kind":"legacy-cache-recovery-rehearsal","image":target.join("ci-vm/cache.img"),"shipping_authority":false}))?)?;
    git(root,&["-c","init.templateDir=","init","-q"]).await?;
    git(root,&["add","."]).await?;
    git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null",
        "-c","commit.gpgsign=false","commit","-qm","identify actual legacy recovery input"]).await?;
    let head=git(root,&["rev-parse","HEAD"]).await?.trim().to_owned();
    git(root,&["update-ref","refs/remotes/origin/main",&head]).await?;
    let before=binding(root).await?;let source=snapshot(root,&before,&target).await?;
    let session=vm::Session::prepare(root,&target)?;
    // Two separately supervised commands retain one original storage admission.
    // Each gets the unchanged 180-second command deadline and a fresh COW view.
    let reference=session.preview_legacy_recovery(&source,root,&before,&target,&storage,&audit,false).await?;
    std::fs::write(evidence.join("strict-reference.json"),serde_json::to_vec_pretty(&reference)?)?;
    let receipt=session.preview_legacy_recovery(&source,root,&before,&target,&storage,&audit,true).await?;
    let comparison=serde_json::json!({"version":1,"kind":"ci-legacy-recovery-comparison",
        "strict":reference,"closed":receipt,"input_closure_complete":false,"full_ci":false,"shipping_authority":false});
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&comparison)?)?;
    let outcome=&receipt["outcome"];
    ensure!(outcome["retired"].as_array().is_some_and(|rows|!rows.is_empty()),"preview did not exercise real legacy retirement");
    ensure!(outcome["available_after"].as_u64()>outcome["available_before"].as_u64(),"preview did not recover worker capacity");
    ensure!(outcome["protected_before"]==outcome["protected_after"],"protected artifacts changed during recovery");
    ensure!(reference["base_before"]==receipt["base_before"] && reference["base_after"]==receipt["base_after"],
        "recovery policies did not use the same immutable base");
    ensure!(reference["outcome"]["protected_before"]==outcome["protected_before"],"policy protected inventories differ");
    let reference_outcome=&reference["outcome"];
    ensure!(reference_outcome["available_before"]==outcome["available_before"],"policy initial worker capacity differs");
    ensure!(outcome["available_after"].as_u64()>reference_outcome["available_after"].as_u64(),
        "closed-link recovery did not improve capacity over strict selection");
    ensure!(binding(root).await?==before,"diagnostic source changed");
    audit.protected_health()?;
    Ok(())
}
