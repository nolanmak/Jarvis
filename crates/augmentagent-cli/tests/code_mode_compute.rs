//! Provider-free CLI contract tests. Missing Deno is a failure, not a skip.
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Command, Output},
};

fn invoke(root: &Path, program: &str, extra: &[(&str, &str)]) -> Output {
    std::fs::write(root.join("program.ts"), program).unwrap();
    // If compute accidentally loads the current-directory .env, this invalid
    // value will make the request fail configuration instead of policy.
    std::fs::write(
        root.join(".env"),
        "AUGMENTAGENT_COMPUTE_TIMEOUT_SECS=invalid\n",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    command
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("AUGMENTAGENT_COMPUTE_ENABLED", "false")
        .args([
            "code-mode",
            "compute-run",
            "--program",
            "program.ts",
            "--inputs",
            "{}",
            "--output-dir",
            "out",
            "--report",
            "report.json",
        ]);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().unwrap()
}
fn report(root: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(root.join("report.json")).unwrap()).unwrap()
}
#[test]
fn disabled_compute_reports_failure_without_opening_database_or_dotenv() {
    let root = tempfile::tempdir().unwrap();
    let result = invoke(root.path(), "async function main(){ return await tools.compute.run({runtime:'python',dependencies:[],code:'print(1)'}); } main();", &[]);
    assert_eq!(
        result.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = report(root.path());
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(report["ok"], false);
    assert_eq!(report["records"][0]["error"]["code"], "compute_disabled");
    assert_eq!(report["cleanup"]["cleanupVerified"], true);
    let audit_dir = report["auditDirectory"]
        .as_str()
        .expect("CLI keeps private audit evidence after task cleanup");
    let audit: Value = serde_json::from_slice(
        &std::fs::read(root.path().join(audit_dir).join("audit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(audit["records"], report["records"]);
    assert_eq!(audit["cleanupVerified"], true);
    assert!(!root.path().join("data.db").exists());
}
#[test]
fn invalid_configuration_exits_two_before_execution() {
    let root = tempfile::tempdir().unwrap();
    let result = invoke(
        root.path(),
        "throw new Error('must not run');",
        &[("AUGMENTAGENT_COMPUTE_TIMEOUT_SECS", "0")],
    );
    assert_eq!(result.status.code(), Some(2));
    assert!(!root.path().join("data.db").exists());
}
#[test]
fn output_directory_must_be_empty_and_report_must_not_clobber() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("out")).unwrap();
    std::fs::write(root.path().join("out/sentinel"), "keep").unwrap();
    let result = invoke(root.path(), "1", &[]);
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(
        std::fs::read_to_string(root.path().join("out/sentinel")).unwrap(),
        "keep"
    );
    std::fs::remove_file(root.path().join("out/sentinel")).unwrap();
    std::fs::write(root.path().join("report.json"), "keep").unwrap();
    let result = invoke(root.path(), "1", &[]);
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(
        std::fs::read_to_string(root.path().join("report.json")).unwrap(),
        "keep"
    );
}
#[test]
fn caught_bad_request_still_fails_cli_and_report_redacts_source() {
    let root = tempfile::tempdir().unwrap();
    let result = invoke(root.path(), "async function main(){try{await tools.compute.run({code:'SOURCE_CANARY',hostPath:'/tmp'});}catch(_){} return 1;} main();", &[]);
    assert_eq!(result.status.code(), Some(1));
    let report = report(root.path());
    assert_eq!(report["ok"], false);
    assert!(!report.to_string().contains("SOURCE_CANARY"));
    assert_eq!(report["final"], json!(1));
}

#[test]
#[ignore = "requires provisioned KVM, pinned guest pip and public PyPI; explicitly run in acceptance QA"]
fn real_cli_downloads_reuses_and_exports_spreadsheet_summary() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("program.ts");
    let sheet = root.path().join("numbers.xlsx");
    std::fs::write(
        &program,
        include_str!("../../../scripts/tests/fixtures/code-mode-compute/sum-xlsx.ts"),
    )
    .unwrap();
    std::fs::write(
        &sheet,
        include_bytes!("../../../scripts/tests/fixtures/code-mode-compute/numbers.xlsx"),
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.path())
        .env("AUGMENTAGENT_COMPUTE_ENABLED", "true")
        .env(
            "AUGMENTAGENT_BUILD_VM_CONFIG",
            std::env::var_os("JARVIS_TEST_VM_CONFIG").expect("VM config required"),
        )
        .env(
            "AUGMENTAGENT_BUILD_SCRATCH_DIR",
            std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH").expect("isolated scratch required"),
        )
        .env(
            "AUGMENTAGENT_COMPUTE_PIP_RUNTIME",
            std::env::var_os("JARVIS_TEST_COMPUTE_PIP").expect("pinned guest pip required"),
        )
        .args(["code-mode", "compute-run", "--program"])
        .arg(&program)
        .arg("--inputs")
        .arg(json!({"sheet":sheet}).to_string())
        .args(["--output-dir", "out", "--report", "report.json"])
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&result.stderr),
        std::fs::read_to_string(root.path().join("report.json")).unwrap_or_default()
    );
    let report = report(root.path());
    assert_eq!(report["ok"], true);
    assert_eq!(report["cleanup"]["cleanupVerified"], true);
    assert_eq!(report["records"].as_array().unwrap().len(), 2);
    assert_eq!(report["records"][0]["runner"], "vm");
    assert!(report["records"][0]["downloads"]["bytes"].as_u64().unwrap() > 0);
    assert_eq!(report["records"][1]["downloads"]["bytes"], 0);
    assert_eq!(report["records"][1]["environmentReused"], true);
    let audit_root = root.path().join(report["auditDirectory"].as_str().unwrap());
    assert!(audit_root.join("audit.json").is_file());
    for record in report["records"].as_array().unwrap() {
        for stream in ["stdout", "stderr"] {
            let metadata = &record["logs"][stream];
            let bytes = std::fs::read(audit_root.join(metadata["file"].as_str().unwrap())).unwrap();
            assert_eq!(bytes.len() as u64, metadata["bytes"].as_u64().unwrap());
        }
    }
    assert!(
        std::fs::read_dir(&audit_root).unwrap().all(|entry| entry
            .unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .starts_with("audit")),
        "raw inputs must not survive in the audit directory"
    );

    let summary: Value =
        serde_json::from_slice(&std::fs::read(root.path().join("out/summary.json")).unwrap())
            .unwrap();
    assert_eq!(summary, json!({"count":3,"total":60}));
    assert_eq!(report["final"], summary);
    assert!(!root.path().join("data.db").exists());
}

#[test]
fn forged_host_rpc_denial_cannot_be_hidden_by_successful_program_return() {
    let root = tempfile::tempdir().unwrap();
    let result = invoke(
        root.path(),
        r#"async function main(){
        Deno.stdout.writeSync(new TextEncoder().encode(JSON.stringify({id:77,call:'unlisted',args:[]})+'\n'));
        await new Promise(r=>setTimeout(r,50));
        return 1;
    } main();"#,
        &[],
    );
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(report(root.path())["ok"], false);
}

#[test]
fn mcp_facade_is_bounded_to_programs_and_refuses_missing_turn_grants() {
    use std::io::Write;
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(".env"),
        "AUGMENTAGENT_COMPUTE_TOOL_GRANT=fixture-from-dotenv\n",
    )
    .unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root.path())
        .env_clear()
        .arg("compute-tool")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = process.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
    )
    .unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run","arguments":{"program":"1"}}})).unwrap();
    drop(input);
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frames: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames.len(), 2);
    let tools = frames[0]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "run");
    assert_eq!(
        tools[0]["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(frames[1]["result"]["isError"], true);
    assert!(!root.path().join("data.db").exists());
}

#[test]
fn orchestration_limits_keep_typed_report_codes() {
    let root = tempfile::tempdir().unwrap();
    let result=invoke(root.path(), "async function main(){const b=new Uint8Array(9*1024*1024);let n=0;while(n<b.length)n+=Deno.stderr.writeSync(b.subarray(n));return 1;}main();", &[]);
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(report(root.path())["error"]["code"], "resource_limit");
    assert_eq!(report(root.path())["cleanup"]["cleanupVerified"], true);
}

#[test]
#[ignore = "requires provisioned KVM and private build-volume scratch; real CLI signal/recovery QA"]
fn real_cli_signals_clean_managed_inputs_and_vm_work() {
    cli_signal_cleanup(SignalStage::Running);
}

#[test]
#[ignore = "requires provisioned private scratch; signals the CLI before helper readiness"]
fn real_cli_startup_signals_are_reported_and_cleaned() {
    cli_signal_cleanup(SignalStage::Startup { stalled: false });
}

#[test]
#[ignore = "requires private scratch; holds compute helper stopped throughout cancellation"]
fn real_cli_stalled_startup_cancellation_reaps_helper() {
    cli_signal_cleanup(SignalStage::Startup { stalled: true });
}

#[test]
#[ignore = "requires KVM, pinned pip and public NumPy wheel; stops an active wheel fetch"]
fn real_cli_download_cancellation_reaps_stalled_fetch() {
    cli_signal_cleanup(SignalStage::Download);
}

#[test]
#[ignore = "requires KVM and private scratch; cancels after actual binary logs reach the host"]
fn real_cli_cancelled_logs_preserve_partial_binary_output() {
    cli_signal_cleanup(SignalStage::LoggedExecution);
}

#[test]
#[ignore = "requires KVM, pinned pip and public NumPy wheel; pauses preparation after pip announces installation"]
fn real_cli_install_cancellation_reaps_preparation_vm() {
    cli_signal_cleanup(SignalStage::Install);
}

enum SignalStage { Running, Startup { stalled: bool }, Download, Install, LoggedExecution }

fn cli_signal_cleanup(stage: SignalStage) {
    let startup = matches!(stage, SignalStage::Startup { .. });
    let stalled = matches!(stage, SignalStage::Startup { stalled: true } | SignalStage::Download);
    let download = matches!(stage, SignalStage::Download);
    let logged = matches!(stage, SignalStage::LoggedExecution);
    let install = matches!(stage, SignalStage::Install);
    use augmentagent_channel_core::{
        build_scratch::{ProcFs, ProcessTable},
        code_mode::compute::retention,
    };
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::{
        os::unix::fs::PermissionsExt,
        process::Stdio,
        time::{Duration, Instant},
    };
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for signal in if startup || download || install || logged { [libc::SIGTERM, libc::SIGINT] } else { [libc::SIGTERM, libc::SIGKILL] } {
        let mut sentinel = Owned(Command::new("/usr/bin/sleep").arg("120")
            .env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().unwrap());
        let root = tempfile::Builder::new()
            .prefix("compute-cli-signal-qa-")
            .tempdir_in(
                std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH").expect("build scratch required"),
            )
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
        let source = root.path().join("selected.txt");
        std::fs::write(&source, b"selected-input-canary").unwrap();
        std::fs::write(root.path().join("program.ts"), "async function main(){return await tools.compute.run({runtime:'python',dependencies:[],inputs:[{artifactId:computeInputs.selected,name:'selected.txt'}],code:'import time\\ntime.sleep(60)'});} main();").unwrap();
        if download || install {
            std::fs::write(root.path().join("program.ts"), "async function main(){return await tools.compute.run({runtime:'python',dependencies:['numpy==1.26.4'],code:'raise AssertionError(\"must not execute\")'});}main();").unwrap();
        }
        if logged {
            let request = json!({"runtime":"python", "dependencies":[], "code":r#"import os,time
os.write(1,b'partial-out\xff')
os.write(2,b'partial-err\xfe')
time.sleep(60)"#});
            std::fs::write(root.path().join("program.ts"), format!("async function main(){{return await tools.compute.run({request});}}main();")).unwrap();
        }
        let mut cli = Owned(
            Command::new(env!("CARGO_BIN_EXE_augmentagent"))
                .current_dir(root.path())
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", root.path())
                .env("AUGMENTAGENT_COMPUTE_ENABLED", "true")
                .env(
                    "AUGMENTAGENT_BUILD_VM_CONFIG",
                    std::env::var_os("JARVIS_TEST_VM_CONFIG").expect("VM required"),
                )
                .env("AUGMENTAGENT_BUILD_SCRATCH_DIR", &scratch)
                .envs(if download || install {
                    vec![("AUGMENTAGENT_COMPUTE_PIP_RUNTIME", std::env::var_os("JARVIS_TEST_COMPUTE_PIP").expect("pinned pip required"))]
                } else { vec![] })
                .args([
                    "code-mode",
                    "compute-run",
                    "--program",
                    "program.ts",
                    "--inputs",
                ])
                .arg(json!({"selected":source}).to_string())
                .args(["--output-dir", "out", "--report", "report.json"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let started = Instant::now();
        let mut stalled_pid = None;
        let mut installation_log = None;
        let processes = loop {
            assert!(
                cli.0.try_wait().unwrap().is_none(),
                "CLI exited before launching VM"
            );
            let processes = ProcFs.vm_processes_using(&scratch);
            if download {
                stalled_pid = processes.iter().find_map(|(pid, _)| {
                    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                    let args: Vec<_> = bytes.split(|byte| *byte == 0).filter(|part| !part.is_empty())
                        .map(|part| String::from_utf8_lossy(part).into_owned()).collect();
                    let fetch = args.iter().position(|arg| arg == "--fetch")?;
                    let value: Value = serde_json::from_slice(&std::fs::read(args.get(fetch + 1)?).ok()?).ok()?;
                    let route = value["route"].as_str()?;
                    (route.starts_with("/pypi-files/") && route.ends_with(".whl")).then_some(*pid)
                });
                if stalled_pid.is_some() { break processes; }
            } else if logged || install {
                let ready = std::fs::read_dir(&scratch).unwrap().filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("jarvis-vm-session-compute-"))
                    .filter_map(|entry| std::fs::read_dir(entry.path().join("tmp")).ok())
                    .flatten().filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("compute-execution-"))
                    .any(|entry| {
                        if install {
                            let path = entry.path().join("partial.stdout");
                            let output = std::fs::read_to_string(&path).unwrap_or_default();
                            if output.contains("Installing collected packages: numpy") && !output.contains("Successfully installed") {
                                installation_log = Some(path);
                                return true;
                            }
                            false
                        } else {
                            std::fs::read(entry.path().join("partial.stdout")).ok().as_deref() == Some(b"partial-out\xff")
                                && std::fs::read(entry.path().join("partial.stderr")).ok().as_deref() == Some(b"partial-err\xfe")
                        }
                    });
                if ready {
                    if install {
                        stalled_pid = processes.iter().find_map(|(pid, _)| {
                            std::fs::read_link(format!("/proc/{pid}/exe")).ok()
                                .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("qemu-system-"))
                                .map(|_| *pid)
                        });
                        assert!(stalled_pid.is_some(), "installation log without live preparation VM");
                    }
                    break processes;
                }
            } else if (startup && !processes.is_empty()) || processes.iter().any(|(pid, _)| {
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .ok()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().starts_with("qemu-system-"))
                    })
                    .unwrap_or(false)
            }) {
                break processes;
            }
            assert!(
                started.elapsed() < Duration::from_secs(if download || install { 45 } else { 15 }),
                "required VM or wheel-fetch stage did not start"
            );
            std::thread::sleep(Duration::from_millis(if startup || download || install { 1 } else { 20 }));
        };
        let process_descriptors: Vec<_> = processes
            .iter()
            .map(|(pid, identity)| {
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, *pid, 0) };
                assert!(fd >= 0, "cannot pin signal fixture process {pid}");
                let file = unsafe { std::fs::File::from_raw_fd(fd as i32) };
                assert_eq!(ProcFs.start_time(*pid).as_deref(), Some(identity.as_str()));
                (*pid, file)
            })
            .collect();
        struct ResumeOnDrop<'a>(&'a [(u32, std::fs::File)]);
        impl Drop for ResumeOnDrop<'_> {
            fn drop(&mut self) {
                for (_, fd) in self.0 {
                    unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(),
                        libc::SIGCONT, std::ptr::null::<libc::siginfo_t>(), 0); }
                }
            }
        }
        let _resume_on_failure = ResumeOnDrop(&process_descriptors);
        let storage = scratch.join("compute-artifacts");
        assert!(
            storage.is_dir(),
            "active CLI inputs need a managed recovery lease"
        );
        assert_eq!(
            retention::sweep_at(&storage, retention::now().unwrap())
                .unwrap()
                .live,
            1
        );
        if !startup {
            let managed = std::fs::read_dir(&storage).unwrap().filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("task-"))
                .flat_map(|e| std::fs::read_dir(e.path()).unwrap())
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("compute-orchestration-"))
                .count();
            assert_eq!(managed, 1, "live Deno files must belong to the crash-recovery lease");
        }
        if download || install {
            let (_, descriptor) = process_descriptors.iter().find(|(pid, _)| Some(*pid) == stalled_pid).unwrap();
            assert_eq!(unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
                descriptor.as_raw_fd(), libc::SIGSTOP, std::ptr::null::<libc::siginfo_t>(), 0) }, 0);
            let stop_requested = Instant::now();
            loop {
                let state = std::fs::read_to_string(format!("/proc/{}/stat", stalled_pid.unwrap())).unwrap();
                if state.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("T") { break; }
                assert!(stop_requested.elapsed() < Duration::from_secs(1), "selected preparation process did not stop");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if install {
            // Give the host time to drain already-sent frames from the now
            // stopped VM. Do not call a completed preparation an install cancel.
            std::thread::sleep(Duration::from_millis(150));
            let output = std::fs::read_to_string(installation_log.as_ref().unwrap()).unwrap();
            assert!(output.contains("Installing collected packages: numpy"));
            assert!(!output.contains("Successfully installed"), "installer completed before cancellation probe");
        }
        if startup {
            // Pin and stop the helper before its ready frame. This also keeps
            // the CLI from registering its old, late signal listener by racing
            // through initialization while the test prepares its assertions.
            for (_, descriptor) in &process_descriptors {
                assert_eq!(unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
                    descriptor.as_raw_fd(), libc::SIGSTOP, std::ptr::null::<libc::siginfo_t>(), 0) }, 0);
            }
        }
        let stopped = Instant::now();
        assert_eq!(unsafe { libc::kill(cli.0.id() as i32, signal) }, 0);
        if startup && !stalled {
            std::thread::sleep(Duration::from_millis(50));
            // Resume even the failing pre-fix CLI's orphan so parent-death
            // cleanup can finish; never strand a stopped fixture helper.
            for (_, descriptor) in &process_descriptors {
                unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
                    descriptor.as_raw_fd(), libc::SIGCONT, std::ptr::null::<libc::siginfo_t>(), 0); }
            }
        }
        let status = loop {
            if let Some(status) = cli.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                stopped.elapsed() < Duration::from_secs(5),
                "CLI signal cleanup exceeded five seconds"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        if signal != libc::SIGKILL {
            assert_eq!(status.code(), Some(1));
            let result = report(root.path());
            assert_eq!(result["error"]["code"], "cancelled");
            assert_eq!(result["cleanup"]["cleanupVerified"], true);
            if download || install {
                let rows = result["records"].as_array().unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0]["error"]["code"], "cancelled");
                assert_eq!(rows[0]["dependencyLock"], json!([]));
                assert_eq!(rows[0]["artifacts"], json!([]));
                let phases = rows[0]["phases"].as_array().unwrap();
                assert!(phases.iter().any(|phase| phase["phase"] == "prepare"));
                assert!(!phases.iter().any(|phase| phase["phase"] == "execute"));
                if install {
                    assert_eq!(rows[0]["preparation"]["error"], "cancelled");
                    assert_eq!(rows[0]["preparation"]["cleanupVerified"], true);
                    let audit = root.path().join(result["auditDirectory"].as_str().unwrap());
                    let output = std::fs::read_to_string(audit.join(rows[0]["preparation"]["logs"]["stdout"]["file"].as_str().unwrap())).unwrap();
                    assert!(output.contains("Installing collected packages: numpy"));
                    assert!(!output.contains("Successfully installed"));
                }
            }
            if logged {
                let rows = result["records"].as_array().unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0]["error"]["code"], "cancelled");
                assert_eq!(rows[0]["runner"], "vm");
                assert_eq!(rows[0]["artifacts"], json!([]));
                let audit = root.path().join(result["auditDirectory"].as_str().unwrap());
                for (name, expected) in [("stdout", b"partial-out\xff"), ("stderr", b"partial-err\xfe")] {
                    let path = audit.join(rows[0]["logs"][name]["file"].as_str().unwrap());
                    assert_eq!(std::fs::read(&path).unwrap(), expected);
                    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
                }
            }
            if startup {
                assert_eq!(result["records"], json!([]), "queued startup cancellation launched computation");
                if stalled {
                    let audit_path = root.path().join(result["auditDirectory"].as_str().unwrap()).join("audit.json");
                    let audit: Value = serde_json::from_slice(&std::fs::read(&audit_path).unwrap()).unwrap();
                    assert_eq!(audit["phase"], "initialization");
                    assert_eq!(audit["cancelled"], true);
                    assert_eq!(audit["cleanupVerified"], true);
                    assert_eq!(std::fs::metadata(audit_path).unwrap().permissions().mode() & 0o777, 0o600);
                }
            }
        } else {
            assert!(!root.path().join("report.json").exists());
            assert_eq!(
                retention::sweep_at(&storage, retention::now().unwrap())
                    .unwrap()
                    .removed,
                1
            );
        }
        assert!(stopped.elapsed() < Duration::from_secs(5));
        assert!(sentinel.0.try_wait().unwrap().is_none(), "cleanup killed an unrelated process");
        for (pid, descriptor) in &process_descriptors {
            let remaining = Duration::from_secs(5).saturating_sub(stopped.elapsed());
            let mut poll = libc::pollfd {
                fd: descriptor.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, remaining.as_millis() as i32) },
                1,
                "owned process {pid} survived CLI cleanup"
            );
            assert_ne!(poll.revents & libc::POLLIN, 0);
        }
        assert!(std::fs::read_dir(&scratch).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("jarvis-vm-session-")));
        assert!(std::fs::read_dir(&storage).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("task-")));
        assert_eq!(std::fs::read(&source).unwrap(), b"selected-input-canary");
        assert!(std::fs::read_dir(root.path().join("out"))
            .unwrap()
            .next()
            .is_none());
    }
}

#[test]
fn storage_admission_refusal_is_a_reported_policy_failure() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let scratch = root.path().join("scratch");
    let retained = scratch.join("compute-artifacts");
    std::fs::create_dir_all(&retained).unwrap();
    for path in [&scratch, &retained] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    for digit in ['1', '2', '3'] {
        let task = retained.join(format!("task-{}", digit.to_string().repeat(32)));
        std::fs::create_dir(&task).unwrap();
        std::fs::set_permissions(&task, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory =
            augmentagent_channel_core::code_mode::compute::artifacts::Directory::open(&task, false)
                .unwrap();
        directory
            .write_report(
                "retention.json",
                &json!({"schemaVersion":1,"expiresAt":u64::MAX}),
            )
            .unwrap();
        std::fs::File::create(task.join("synthetic-sparse-artifact"))
            .unwrap()
            .set_len(512 * 1024 * 1024)
            .unwrap();
    }
    let result = invoke(
        root.path(),
        "throw new Error('must not execute');",
        &[
            ("AUGMENTAGENT_COMPUTE_ENABLED", "true"),
            ("AUGMENTAGENT_BUILD_SCRATCH_DIR", scratch.to_str().unwrap()),
        ],
    );
    assert_eq!(
        result.status.code(),
        Some(1),
        "storage exhaustion is a policy failure, not invalid CLI arguments"
    );
    let result = report(root.path());
    assert_eq!(result["error"]["code"], "resource_limit");
    assert_eq!(result["cleanup"]["cleanupVerified"], true);
    assert_eq!(result["records"], json!([]));
    assert!(std::fs::read_dir(&scratch).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("jarvis-vm-session-")));
}

#[test]
#[cfg(target_os = "linux")]
fn killed_cli_cannot_leave_spinning_orchestration_process() {
    use std::{
        os::fd::{AsRawFd, FromRawFd},
        process::Stdio,
        time::{Duration, Instant},
    };
    struct Owner(std::process::Child);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct Pinned(std::fs::File);
    impl Drop for Pinned {
        fn drop(&mut self) {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("spin.ts"),
        "async function main(){while(true){}} main();",
    )
    .unwrap();
    let mut owner = Owner(
        Command::new(env!("CARGO_BIN_EXE_augmentagent"))
            .current_dir(root.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.path())
            .env("AUGMENTAGENT_COMPUTE_ENABLED", "false")
            .args([
                "code-mode",
                "compute-run",
                "--program",
                "spin.ts",
                "--output-dir",
                "out",
                "--report",
                "report.json",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    let (pid, pinned) = loop {
        assert!(
            owner.0.try_wait().unwrap().is_none(),
            "CLI exited before orchestration started"
        );
        let mut found = None;
        for thread in std::fs::read_dir(format!("/proc/{}/task", owner.0.id())).unwrap() {
            let children = std::fs::read_to_string(thread.unwrap().path().join("children"))
                .unwrap_or_default();
            for pid in children
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
            {
                let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
                if executable
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .is_some_and(|name| name == "deno")
                {
                    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                    assert!(fd >= 0);
                    found = Some((
                        pid,
                        Pinned(unsafe { std::fs::File::from_raw_fd(fd as i32) }),
                    ));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        if let Some(found) = found {
            break found;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "Deno did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        if fields[11].parse::<u64>().unwrap() >= 20 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "fixture did not enter its busy loop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    owner.0.kill().unwrap();
    owner.0.wait().unwrap();
    let mut exited = libc::pollfd {
        fd: pinned.0.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let status = unsafe { libc::poll(&mut exited, 1, 1000) };
    assert!(
        status > 0 && exited.revents & libc::POLLIN != 0,
        "orchestration survived owner SIGKILL and kept using host CPU"
    );
}

#[test]
#[ignore = "requires real VM; races final report with a no-clobber sentinel"]
fn real_cli_report_failure_rolls_back_exported_outputs() {
    cli_export_failure(false);
}

#[test]
#[ignore = "requires real VM; cancels while a verified output batch is copied"]
fn real_cli_export_cancellation_rolls_back_outputs() {
    cli_export_failure(true);
}

fn cli_export_failure(cancel: bool) {
    use augmentagent_channel_core::build_scratch::{ProcFs, ProcessTable};
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let root = tempfile::Builder::new().prefix("compute-export-qa-")
        .tempdir_in(std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH").unwrap()).unwrap();
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
    let code = if cancel {
        "from pathlib import Path; data=b'x'*(32*1024*1024); Path('/outputs/result.json').write_bytes(data); Path('/outputs/second.bin').write_bytes(data)"
    } else { "import time;time.sleep(1);open('/outputs/result.json','w').write('{}')" };
    let outputs = if cancel { vec!["result.json", "second.bin"] } else { vec!["result.json"] };
    std::fs::write(root.path().join("program.ts"), format!("async function main(){{return await tools.compute.run({});}}main();",
        json!({"runtime":"python","dependencies":[],"code":code,"outputs":outputs}))).unwrap();
    struct Owned(std::process::Child);
    impl Drop for Owned { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut cli = Owned(Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root.path()).env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default()).env("HOME",root.path())
        .env("AUGMENTAGENT_COMPUTE_ENABLED","true")
        .env("AUGMENTAGENT_BUILD_VM_CONFIG",std::env::var_os("JARVIS_TEST_VM_CONFIG").unwrap())
        .env("AUGMENTAGENT_BUILD_SCRATCH_DIR",&scratch)
        .args(["code-mode","compute-run","--program","program.ts","--output-dir","out","--report","report.json"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(if cancel { 60 } else { 15 });
    loop {
        assert!(cli.0.try_wait().unwrap().is_none());
        if ProcFs.vm_processes_using(&scratch).iter().any(|(pid,_)| {
            std::fs::read_link(format!("/proc/{pid}/exe")).ok().is_some_and(|p| p.file_name().unwrap().to_string_lossy().starts_with("qemu-system-"))
        }) { break; }
        assert!(Instant::now() < deadline,"VM did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    if cancel {
        while !root.path().join("out/result.json").exists() {
            assert!(cli.0.try_wait().unwrap().is_none(), "CLI exited before export began");
            assert!(Instant::now() < deadline, "export did not begin");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(unsafe { libc::kill(cli.0.id() as i32, libc::SIGTERM) }, 0);
    } else {
        std::fs::write(root.path().join("report.json"),b"no-clobber sentinel").unwrap();
    }
    let cancelled_at = Instant::now();
    let status = loop {
        if let Some(status) = cli.0.try_wait().unwrap() { break status; }
        assert!(Instant::now() < deadline,"CLI did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(),Some(1));
    if cancel {
        assert!(cancelled_at.elapsed() < Duration::from_secs(5), "export cancellation exceeded allowance");
        let result = report(root.path());
        assert_eq!(result["error"]["code"], "cancelled");
        assert_eq!(result["artifacts"], json!([]));
        assert_eq!(result["cleanup"]["cleanupVerified"], true);
        let audit_path = root.path().join(result["auditDirectory"].as_str().unwrap()).join("audit-cli.json");
        let audit: Value = serde_json::from_slice(&std::fs::read(audit_path).unwrap()).unwrap();
        assert_eq!(audit["cancelled"], true);
        assert_eq!(audit["phase"], "finalization");
    } else {
        assert_eq!(std::fs::read(root.path().join("report.json")).unwrap(),b"no-clobber sentinel");
    }
    assert_eq!(std::fs::read_dir(root.path().join("out")).unwrap().count(),0,"failed CLI published success artifacts");
}

#[test]
#[ignore = "requires real VM; expires enclosing task while a compute call is active"]
fn real_cli_task_deadline_cancels_active_compute() {
    use augmentagent_channel_core::build_scratch::{ProcFs, ProcessTable};
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let root = tempfile::Builder::new().prefix("compute-task-timeout-qa-")
        .tempdir_in(std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH").unwrap()).unwrap();
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = std::env::var("JARVIS_TEST_VM_CONFIG").unwrap();
    let started = Instant::now();
    let output = invoke(root.path(), "async function main(){return await tools.compute.run({runtime:'python',dependencies:[],code:'import time;time.sleep(20)'});}main();", &[
        ("AUGMENTAGENT_COMPUTE_ENABLED", "true"),
        ("AUGMENTAGENT_CODE_MODE_COMPUTE_TIMEOUT_SECS", "5"),
        ("AUGMENTAGENT_COMPUTE_TIMEOUT_SECS", "60"),
        ("AUGMENTAGENT_BUILD_VM_CONFIG", &runtime),
        ("AUGMENTAGENT_BUILD_SCRATCH_DIR", scratch.to_str().unwrap()),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(started.elapsed() < Duration::from_secs(10), "task exceeded deadline plus cleanup allowance");
    let result = report(root.path());
    assert_eq!(result["error"]["code"], "timeout", "{result}");
    assert_eq!(result["cleanup"]["cleanupVerified"], true, "{result}");
    assert_eq!(result["artifacts"], json!([]));
    assert_eq!(result["records"].as_array().unwrap().len(), 1);
    assert_eq!(result["records"][0]["runner"], "vm", "{result}");
    assert!(ProcFs.vm_processes_using(&scratch).is_empty());
    assert!(std::fs::read_dir(&scratch).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("jarvis-vm-session-")));
    assert!(std::fs::read_dir(root.path().join("out")).unwrap().next().is_none());
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "requires real VM; injects a synthetic inherited descriptor into CLI"]
fn real_cli_unrelated_fds_are_absent_from_children_and_guest() {
    use std::{os::{fd::AsRawFd, unix::{fs::{MetadataExt, PermissionsExt}, process::CommandExt}},
              process::Stdio, time::{Duration, Instant}};
    struct Owner(std::process::Child);
    impl Drop for Owner {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_some() { return; }
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM); }
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if self.0.try_wait().ok().flatten().is_some() { return; }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
    let canary = root.path().join("private-fd-canary");
    std::fs::write(&canary, b"SYNTHETIC_UNSELECTED_DESCRIPTOR_CANARY").unwrap();
    let file = std::fs::File::open(&canary).unwrap();
    let identity = file.metadata().unwrap();
    let code = "import os\nfor fd in range(3,1024):\n try:os.fstat(fd)\n except OSError:continue\n raise AssertionError('unexpected inherited guest descriptor')\nprint(60)";
    let request = json!({"runtime":"python","dependencies":[],"code":code});
    std::fs::write(root.path().join("program.ts"), format!(
        "async function main(){{await new Promise(r=>setTimeout(r,1000));return await tools.compute.run({request});}}main();")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    command.current_dir(root.path()).env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.path()).env("AUGMENTAGENT_COMPUTE_ENABLED", "true")
        .env("AUGMENTAGENT_BUILD_SCRATCH_DIR", &scratch)
        .env("AUGMENTAGENT_BUILD_VM_CONFIG", std::env::var_os("JARVIS_TEST_VM_CONFIG").expect("VM required"))
        .args(["code-mode", "compute-run", "--program", "program.ts", "--output-dir", "out", "--report", "report.json"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let descriptor = file.as_raw_fd();
    unsafe { command.pre_exec(move || {
        if libc::dup2(descriptor, 200) == -1 { return Err(std::io::Error::last_os_error()); }
        Ok(())
    }); }
    let mut owner = Owner(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = std::collections::BTreeSet::new();
    while seen.len() < 2 {
        assert!(owner.0.try_wait().unwrap().is_none(), "CLI exited before child inspection");
        for thread in std::fs::read_dir(format!("/proc/{}/task", owner.0.id())).unwrap() {
            let children = std::fs::read_to_string(thread.unwrap().path().join("children")).unwrap_or_default();
            for pid in children.split_whitespace().filter_map(|p| p.parse::<u32>().ok()) {
                let Ok(executable) = std::fs::read_link(format!("/proc/{pid}/exe")) else { continue; };
                let name = executable.file_name().unwrap().to_string_lossy();
                let kind = if name == "deno" { "deno" } else if name.starts_with("python3") { "helper" } else { continue; };
                let descriptors = std::fs::read_dir(format!("/proc/{pid}/fd")).unwrap();
                for entry in descriptors.flatten() {
                    if let Ok(metadata) = std::fs::metadata(entry.path()) {
                        assert!(!(metadata.dev() == identity.dev() && metadata.ino() == identity.ino()),
                                "{kind} inherited the unrelated canary descriptor");
                    }
                }
                seen.insert(kind);
            }
        }
        assert!(Instant::now() < deadline, "did not inspect both Deno and compute helper");
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = loop {
        if let Some(status) = owner.0.try_wait().unwrap() { break status; }
        assert!(Instant::now() < deadline, "CLI did not finish its descriptor probe");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "guest descriptor isolation failed");
    let result = report(root.path());
    assert_eq!(result["final"]["runner"], "vm");
    assert_eq!(result["final"]["stdout"].as_str().unwrap().trim(), "60");
    assert_eq!(result["cleanup"]["cleanupVerified"], true);
    assert!(!result.to_string().contains("SYNTHETIC_UNSELECTED_DESCRIPTOR_CANARY"));
    assert!(std::fs::read_dir(&scratch).unwrap().all(|entry| !entry.unwrap().file_name().to_string_lossy().starts_with("jarvis-vm-session-")));
}
