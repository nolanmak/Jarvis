//! Real Deno boundary tests: host budgets, capabilities and protocol validation.
use augmentagent_channel_core::code_mode::runner::{run_program_with_options, RunOptions};
use augmentagent_channel_core::code_mode::{StubDispatcher, ToolManifest};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

#[tokio::test]
async fn compute_host_deadline_prevents_late_success() {
    let opts = RunOptions {
        timeout: Duration::from_millis(25),
        ..Default::default()
    };
    let result = run_program_with_options(
        "async function main() { await new Promise(r => setTimeout(r, 250)); return 'late'; } main();",
        &ToolManifest::default(), &StubDispatcher::always_null(&[]), &opts).await;
    assert!(result.is_err(), "host budget ignored: {result:?}");
}

#[tokio::test]
async fn compute_host_input_bindings_reach_real_program() {
    let opts = RunOptions {
        compute_inputs: BTreeMap::from([("sheet".into(), "opaque-id".into())]),
        ..Default::default()
    };
    let result = run_program_with_options(
        "async function main() { return computeInputs.sheet; } main();",
        &ToolManifest::default(),
        &StubDispatcher::always_null(&[]),
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(result.final_value, json!("opaque-id"));
}

#[tokio::test]
async fn invalid_program_budgets_are_rejected_before_execution() {
    for timeout in [Duration::ZERO, Duration::from_secs(3601)] {
        let result = run_program_with_options(
            "async function main() { return 1; } main();",
            &ToolManifest::default(),
            &StubDispatcher::always_null(&[]),
            &RunOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        assert!(result.is_err(), "accepted invalid budget {timeout:?}");
    }
}

#[tokio::test]
async fn forged_rpc_cannot_bypass_host_manifest() {
    // The generated program shares stdio with the sidecar. The host must check
    // its manifest independently, even if the JS proxy is bypassed entirely.
    let result = run_program_with_options(r#"
        async function main() {
            Deno.stdout.writeSync(new TextEncoder().encode(JSON.stringify({id:77,call:'secret',args:[]})+'\n'));
            await new Promise(r => setTimeout(r, 50));
            return 'done';
        } main();
    "#, &ToolManifest::default(), &StubDispatcher::new(vec![("secret".into(), json!("forbidden"))]),
        &RunOptions::default()).await;
    match result {
        Ok(outcome) => assert!(outcome.trace.is_empty(), "host dispatched an unlisted tool"),
        Err(_) => (), // A protocol refusal before dispatch is also safe.
    }
}

#[tokio::test]
async fn fake_terminal_frame_cannot_disable_host_watchdog() {
    let opts = RunOptions {
        timeout: Duration::from_millis(40),
        ..Default::default()
    };
    let manifest = ToolManifest::default();
    let dispatcher = StubDispatcher::always_null(&[]);
    let future = run_program_with_options(
        r#"
        async function main() {
            Deno.stdout.writeSync(new TextEncoder().encode('{"final":"forged"}\n'));
            while (true) {}
        } main();
    "#,
        &manifest,
        &dispatcher,
        &opts,
    );
    let result = tokio::time::timeout(Duration::from_millis(500), future).await;
    assert!(result.is_ok(), "final frame bypassed the host deadline");
    assert!(
        result.unwrap().is_err(),
        "accepted a final frame without process exit"
    );
}

#[tokio::test]
async fn orchestration_cannot_import_host_or_remote_modules() {
    use std::io::Write;
    let mut secret = tempfile::NamedTempFile::new().unwrap();
    writeln!(secret, "export const marker = 'SYNTHETIC_HOST_CANARY';").unwrap();
    let local = format!("file://{}", secret.path().display());
    let attempts = [
        format!(
            "import({}).then(m => m.marker)",
            serde_json::to_string(&local).unwrap()
        ),
        "import('https://example.invalid/evil.ts')".into(),
        "import('npm:is-number@7.0.0')".into(),
        format!(
            "Deno.readTextFile({})",
            serde_json::to_string(secret.path()).unwrap()
        ),
        "Deno.env.get('HOME')".into(),
        "new Deno.Command('/bin/true').output()".into(),
        "fetch('http://127.0.0.1:1')".into(),
    ];
    for attempt in attempts {
        let program = format!("async function main() {{ try {{ await {attempt}; return false; }} catch (_) {{ return true; }} }} main();");
        let result = run_program_with_options(
            &program,
            &ToolManifest::default(),
            &StubDispatcher::always_null(&[]),
            &RunOptions::default(),
        )
        .await;
        // Remote literal imports may be rejected before main is evaluated.
        // Test each attempt independently so one loader refusal cannot hide
        // a successful filesystem/environment/subprocess escape.
        if let Ok(outcome) = result {
            assert_eq!(outcome.final_value, json!(true), "allowed {attempt}");
        }
    }
}

fn service_config(
    root: &std::path::Path,
    enabled: bool,
) -> augmentagent_channel_core::code_mode::compute::ServiceConfig {
    use augmentagent_channel_core::code_mode::compute::{ComputePolicy, ServiceConfig};
    let artifacts = root.join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    ServiceConfig {
        policy: ComputePolicy {
            enabled,
            call_timeout: Duration::from_secs(30),
            task_timeout: Duration::from_secs(60),
        },
        runtime: root.join("missing-runtime.json"),
        scratch_root: root.join("scratch"),
        pip_runtime: None,
        scratch_limits: None,
        artifact_root: artifacts,
        input_files: BTreeMap::new(),
    }
}

#[tokio::test]
async fn disabled_service_returns_structured_denial_and_verified_close() {
    use augmentagent_channel_core::code_mode::compute::ComputeService;
    let root = tempfile::tempdir().unwrap();
    let service = ComputeService::start(service_config(root.path(), false))
        .await
        .expect("private helper starts even without a VM when disabled");
    let result = service
        .execute(json!({"runtime":"python","dependencies":[],"code":"print(1)"}))
        .await
        .unwrap();
    assert_eq!(result["error"]["code"], "compute_disabled");
    assert_eq!(result["runner"], "none");
    let finished = service.finish().await.unwrap();
    assert_eq!(finished["cleanupVerified"], true);
}

#[tokio::test]
async fn service_imports_selected_files_as_opaque_ids() {
    use augmentagent_channel_core::code_mode::compute::ComputeService;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("selected.csv");
    std::fs::write(&source, "a,b\n1,2\n").unwrap();
    let mut config = service_config(root.path(), false);
    config
        .input_files
        .insert("sheet.csv".into(), source.clone());
    let service = ComputeService::start(config).await.unwrap();
    assert!(
        service.artifact_root().join("audit.json").is_file(),
        "startup must register the helper before exposing selected inputs"
    );
    let ids = service.inputs();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids["sheet.csv"].len(), 32);
    assert_ne!(ids["sheet.csv"], source.to_string_lossy());
    service.finish().await.unwrap();
}

#[tokio::test]
async fn real_dispatcher_preserves_denial_and_redacts_compute_trace() {
    use augmentagent_channel_core::code_mode::{
        compute::{ComputeDispatcher, ComputeService},
        manifest::manifest_compute,
        Dispatcher,
    };
    let root = tempfile::tempdir().unwrap();
    let service = ComputeService::start(service_config(root.path(), false))
        .await
        .unwrap();
    let dispatcher = ComputeDispatcher::new(service.clone());
    let outcome = run_program_with_options(
        "async function main() { return await tools.compute.run({runtime:'python',dependencies:[],code:'SOURCE_CANARY'}); } main();",
        &manifest_compute(), &dispatcher, &service.run_options().unwrap()).await.unwrap();
    assert_eq!(outcome.final_value["error"]["code"], "compute_disabled");
    assert!(!serde_json::to_string(&outcome.trace)
        .unwrap()
        .contains("SOURCE_CANARY"));
    assert_eq!(outcome.trace.len(), 1);
    assert!(dispatcher.call("draft", json!([])).await.is_err());
    assert!(dispatcher
        .call("compute.run", json!([{}, {}]))
        .await
        .is_err());
    service.finish().await.unwrap();
}

#[tokio::test]
#[ignore = "requires JARVIS_TEST_VM_CONFIG and an isolated JARVIS_TEST_COMPUTE_SCRATCH; run explicitly in VM acceptance QA"]
async fn real_deno_rust_vm_artifact_round_trip() {
    use augmentagent_channel_core::code_mode::{
        compute::{ComputeDispatcher, ComputeService},
        manifest::manifest_compute,
    };
    let root = tempfile::tempdir().unwrap();
    let mut config = service_config(root.path(), true);
    config.runtime = std::env::var_os("JARVIS_TEST_VM_CONFIG")
        .expect("VM config required")
        .into();
    config.scratch_root = std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH")
        .expect("isolated scratch required")
        .into();
    let source = root.path().join("selected.txt");
    std::fs::write(&source, "10\n20\n30\n").unwrap();
    config.input_files.insert("numbers".into(), source);
    let service = ComputeService::start(config).await.unwrap();
    let dispatcher = ComputeDispatcher::new(service.clone());
    let program = r#"async function main() {
        const first = await tools.compute.run({runtime:'python',dependencies:[],
            inputs:[{artifactId:computeInputs.numbers,name:'numbers.txt'}], outputs:['total.json'],
            code:"import json\nnumbers=[int(x) for x in open('/inputs/numbers.txt')]\nopen('/outputs/total.json','w').write(json.dumps({'count':len(numbers),'total':sum(numbers)}))"});
        if (!first.ok) throw new Error(JSON.stringify(first.error));
        const second = await tools.compute.run({runtime:'python',dependencies:[],
            inputs:[{artifactId:first.artifacts[0].id,name:'total.json'}],
            code:"print(open('/inputs/total.json').read())"});
        if (!second.ok) throw new Error(JSON.stringify(second.error));
        return {runner:second.runner, value:JSON.parse(second.stdout)};
    } main();"#;
    let outcome = run_program_with_options(
        program,
        &manifest_compute(),
        &dispatcher,
        &service.run_options().unwrap(),
    )
    .await;
    let finished = service.finish().await.expect("verified VM cleanup");
    assert_eq!(finished["cleanupVerified"], true);
    let outcome = outcome.unwrap();
    assert_eq!(
        outcome.final_value,
        json!({"runner":"vm","value":{"count":3,"total":60}})
    );
    assert_eq!(outcome.trace.len(), 2);
}

#[tokio::test]
#[ignore = "requires real isolated VM configuration; run explicitly in VM lifecycle QA"]
async fn dropped_compute_future_cancels_vm_and_verifies_cleanup() {
    use augmentagent_channel_core::code_mode::compute::ComputeService;
    let root = tempfile::tempdir().unwrap();
    let mut config = service_config(root.path(), true);
    config.runtime = std::env::var_os("JARVIS_TEST_VM_CONFIG")
        .expect("VM config required")
        .into();
    config.scratch_root = std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH")
        .expect("isolated scratch required")
        .into();
    let service = ComputeService::start(config).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        service.execute(json!({
            "runtime":"python", "dependencies":[], "code":"import time\ntime.sleep(25)"
        })),
    )
    .await;
    assert!(
        result.is_err(),
        "workload should still be executing when canceled"
    );
    let receipt = tokio::time::timeout(Duration::from_secs(5), service.finish())
        .await
        .expect("cleanup must finish within five seconds")
        .expect("verified cleanup");
    assert_eq!(receipt["cancelled"], true);
    assert_eq!(receipt["cleanupVerified"], true);
    assert_eq!(
        receipt["records"].as_array().unwrap().len(),
        1,
        "cancelled call must retain its audit record"
    );
    assert_eq!(receipt["records"][0]["error"]["code"], "cancelled");
    assert_eq!(receipt["records"][0]["runner"], "vm");
    let audit: serde_json::Value =
        serde_json::from_slice(&std::fs::read(service.artifact_root().join("audit.json")).unwrap())
            .unwrap();
    assert_eq!(audit["records"], receipt["records"]);
    assert_eq!(audit["cleanupVerified"], true);
    assert!(service.execute(json!({})).await.is_err());
}

#[tokio::test]
async fn locally_rejected_call_budget_remains_observable_after_catch() {
    use augmentagent_channel_core::code_mode::manifest::manifest_compute;
    let outcome = run_program_with_options(
        "async function main(){for(let i=0;i<26;i++){try{await tools.compute.run({});}catch(_){}}return 'done';}main();",
        &manifest_compute(), &StubDispatcher::always_null(&["compute.run"]), &RunOptions::default()).await.unwrap();
    assert_eq!(outcome.final_value, json!("done"));
    assert_eq!(outcome.trace.len(), 25);
    assert!(
        outcome.dispatch_failures > 0,
        "local budget refusal was lost when the program caught it"
    );
}

#[tokio::test]
async fn malformed_protocol_does_not_echo_program_data() {
    let result = run_program_with_options(
        "async function main(){Deno.stdout.writeSync(new TextEncoder().encode('PROTOCOL_CANARY\\n'));return 1;}main();",
        &ToolManifest::default(), &StubDispatcher::always_null(&[]), &RunOptions::default()).await.unwrap_err();
    assert!(!result.to_string().contains("PROTOCOL_CANARY"));
}

#[tokio::test]
async fn oversized_protocol_frame_is_refused_before_final_success() {
    let result = run_program_with_options(
        "async function main(){const bytes=new TextEncoder().encode(JSON.stringify({final:'x'.repeat(3*1024*1024)})+'\\n');let n=0;while(n<bytes.length)n+=Deno.stdout.writeSync(bytes.subarray(n));return 1;}main();",
        &ToolManifest::default(), &StubDispatcher::always_null(&[]), &RunOptions {timeout:Duration::from_secs(3),..Default::default()}).await;
    assert!(result.is_err(), "oversized frame accepted");
    assert!(result.unwrap_err().to_string().contains("resource_limit"));
}

#[tokio::test]
async fn stderr_flood_is_bounded_and_aborts_program_promptly() {
    let started = std::time::Instant::now();
    let result = run_program_with_options(
        "async function main(){const bytes=new Uint8Array(9*1024*1024);let n=0;while(n<bytes.length)n+=Deno.stderr.writeSync(bytes.subarray(n));return 1;}main();",
        &ToolManifest::default(), &StubDispatcher::always_null(&[]), &RunOptions {timeout:Duration::from_secs(5),..Default::default()}).await;
    assert!(result.is_err(), "stderr byte limit ignored");
    assert!(result.unwrap_err().to_string().contains("resource_limit"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn invalid_compute_requests_keep_typed_errors_and_allow_repair() {
    use augmentagent_channel_core::code_mode::{
        compute::{ComputeDispatcher, ComputeService},
        Dispatcher,
    };
    let root = tempfile::tempdir().unwrap();
    let service = ComputeService::start(service_config(root.path(), false))
        .await
        .unwrap();
    let dispatcher = ComputeDispatcher::new(service.clone());
    for request in [
        json!({"runtime":"python","dependencies":[],"code":"SOURCE_CANARY","unexpected":1}),
        json!({"runtime":"python","dependencies":[],"code":"","timeoutSecs":0}),
    ] {
        let error = dispatcher
            .call("compute.run", json!([request]))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("bad_args:"), "{error}");
        assert!(!error.contains("SOURCE_CANARY"));
    }
    let error=dispatcher.call("compute.run",json!([{"runtime":"python","dependencies":["example @ https://example.invalid/package.whl"],"code":""}])).await.unwrap_err();
    assert!(error.to_string().starts_with("dependency_policy_denied:"));
    let result = dispatcher
        .call(
            "compute.run",
            json!([{"runtime":"python","dependencies":[],"code":"print(1)"}]),
        )
        .await
        .unwrap();
    assert_eq!(result["error"]["code"], "compute_disabled");
    assert_eq!(service.finish().await.unwrap()["cleanupVerified"], true);
}

#[tokio::test]
#[ignore = "requires isolated real VM configuration; cancellation during an active RPC"]
async fn orchestration_log_overflow_cancels_active_vm_rpc() {
    use augmentagent_channel_core::code_mode::{
        compute::{ComputeDispatcher, ComputeService},
        manifest::manifest_compute,
    };
    let root = tempfile::tempdir().unwrap();
    let mut config = service_config(root.path(), true);
    config.runtime = std::env::var_os("JARVIS_TEST_VM_CONFIG")
        .expect("VM required")
        .into();
    config.scratch_root = std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH")
        .expect("scratch required")
        .into();
    let service = ComputeService::start(config).await.unwrap();
    let dispatcher = ComputeDispatcher::new(service.clone());
    let result=run_program_with_options(r#"async function main(){
        const running=tools.compute.run({runtime:'python',dependencies:[],code:'import time\ntime.sleep(25)'});
        await new Promise(r=>setTimeout(r,2000));
        const bytes=new Uint8Array(9*1024*1024);let n=0;
        while(n<bytes.length)n+=Deno.stderr.writeSync(bytes.subarray(n));
        return await running;
    }main();"#, &manifest_compute(), &dispatcher,&service.run_options().unwrap()).await;
    assert!(result.unwrap_err().to_string().contains("resource_limit"));
    let receipt = tokio::time::timeout(Duration::from_secs(5), service.finish())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt["cleanupVerified"], true);
    assert_eq!(receipt["cancelled"], true);
}

#[tokio::test]
#[ignore = "requires provisioned KVM and private build-volume scratch; kills only its fixture owner"]
async fn killed_owner_recovery_removes_vm_and_task_lease() {
    use augmentagent_channel_core::{
        build_scratch::{BuildScratchLimits, ProcFs, ProcessTable},
        code_mode::compute::{retention, ComputeService},
    };
    use std::{
        os::unix::fs::PermissionsExt,
        process::{Child, Command, Stdio},
    };
    if let Some(root) = std::env::var_os("JARVIS_COMPUTE_CRASH_ROOT") {
        let root = std::path::PathBuf::from(root);
        let mut config = service_config(&root, true);
        config.runtime = std::env::var_os("JARVIS_TEST_VM_CONFIG")
            .expect("VM required")
            .into();
        config.scratch_root = root.clone();
        let lease = retention::Lease::reserve(
            &root.join("compute-artifacts"),
            "crash-fixture",
            retention::now().unwrap(),
            BuildScratchLimits::from_env(),
        )
        .unwrap();
        config.artifact_root = lease.path.clone();
        let service = ComputeService::start(config).await.unwrap();
        let _result = service
            .execute(
                json!({"runtime":"python","dependencies":[],"code":"import time\ntime.sleep(60)"}),
            )
            .await;
        drop(lease);
        panic!("fixture owner must be killed while its VM is running");
    }
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = tempfile::Builder::new()
        .prefix("compute-crash-qa-")
        .tempdir_in(
            std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH").expect("build-volume scratch required"),
        )
        .unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut owner = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "killed_owner_recovery_removes_vm_and_task_lease",
                "--exact",
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path())
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("JARVIS_COMPUTE_CRASH_ROOT", root.path())
            .env(
                "JARVIS_TEST_VM_CONFIG",
                std::env::var_os("JARVIS_TEST_VM_CONFIG").expect("VM required"),
            )
            .stdout(Stdio::from(log.reopen().unwrap()))
            .stderr(Stdio::from(log.reopen().unwrap()))
            .spawn()
            .unwrap(),
    );
    let mut sentinel = OwnedChild(Command::new("sleep").arg("60").spawn().unwrap());
    let processes = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "fixture exited before VM start: {}",
                std::fs::read_to_string(log.path()).unwrap()
            );
            let processes = ProcFs.vm_processes_using(root.path());
            if processes.iter().any(|(pid, _)| {
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("real VM must start");
    use std::os::fd::{AsRawFd, FromRawFd};
    let process_descriptors: Vec<_> = processes
        .iter()
        .map(|(pid, identity)| {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, *pid, 0) };
            assert!(fd >= 0, "cannot pin crash fixture process {pid}");
            let file = unsafe { std::fs::File::from_raw_fd(fd as i32) };
            assert_eq!(ProcFs.start_time(*pid).as_deref(), Some(identity.as_str()));
            (*pid, file)
        })
        .collect();
    let storage = root.path().join("compute-artifacts");
    let before = retention::sweep_at(&storage, retention::now().unwrap()).unwrap();
    assert_eq!(before.live, 1, "active owner must survive startup recovery");
    let helper_files: Vec<_> = std::fs::read_dir(&storage)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .flat_map(|path| std::fs::read_dir(path).unwrap())
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("compute-helper-")
        })
        .collect();
    assert_eq!(
        helper_files.len(),
        1,
        "crashed helper must be owned by its recovery lease"
    );
    owner.0.kill().unwrap();
    owner.0.wait().unwrap();
    let started = std::time::Instant::now();
    let recovered = retention::sweep_at(&storage, retention::now().unwrap()).unwrap();
    assert_eq!(recovered.removed, 1);
    assert!(
        helper_files.iter().all(|path| !path.exists()),
        "orphaned helper files survived lease recovery"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "unrelated process was killed"
    );
    for (pid, descriptor) in process_descriptors {
        // An orphan helper may be an exited zombie until init reaps it. PIDFD
        // readiness proves termination without confusing that state with work.
        let remaining = Duration::from_secs(5).saturating_sub(started.elapsed());
        let mut poll = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, remaining.as_millis() as i32) },
            1,
            "owned process {pid} survived recovery"
        );
        assert_ne!(poll.revents & libc::POLLIN, 0);
    }
    assert!(
        std::fs::read_dir(root.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("jarvis-vm-session-")),
        "VM scratch survived recovery"
    );
    assert!(
        std::fs::read_dir(&storage).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("task-")),
        "task artifacts survived recovery"
    );
}

#[tokio::test]
#[ignore = "requires real VM configuration; verifies deadline result and subsequent task call"]
async fn real_call_timeout_returns_typed_result_and_preserves_remaining_task_budget() {
    use augmentagent_channel_core::code_mode::compute::ComputeService;
    let root = tempfile::tempdir().unwrap();
    let mut config = service_config(root.path(), true);
    config.runtime = std::env::var_os("JARVIS_TEST_VM_CONFIG")
        .expect("VM config required")
        .into();
    config.scratch_root = std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH")
        .expect("isolated scratch required")
        .into();
    let service = ComputeService::start(config).await.unwrap();
    let before = service.run_options().unwrap().timeout;
    let started = std::time::Instant::now();
    let failed = service.execute(json!({"runtime":"python","dependencies":[],"code":"import time;time.sleep(20)","timeoutSecs":2})).await;
    assert!(
        failed.is_ok(),
        "deadline must return a ComputeResult, not a transport error: {failed:?}"
    );
    let failed = failed.unwrap();
    assert_eq!(failed["error"]["code"], "timeout");
    assert_eq!(failed["runner"], "vm");
    assert_eq!(failed["artifacts"], json!([]));
    assert!(started.elapsed() < Duration::from_secs(7));
    assert!(service.run_options().unwrap().timeout < before - Duration::from_secs(1));
    let next = service
        .execute(json!({"runtime":"python","dependencies":[],"code":"print(60)"}))
        .await
        .unwrap();
    assert_eq!(next["ok"], true, "{next:?}");
    assert_eq!(next["stdout"], "60\n");
    // A queued call spends its own budget while waiting for the helper. It
    // must not get a new second when the preceding call finally releases it.
    let occupied = service.clone();
    let first = tokio::spawn(async move {
        occupied.execute(json!({"runtime":"python","dependencies":[],"code":"import time;time.sleep(20)","timeoutSecs":2})).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let queued = service.execute(json!({"runtime":"python","dependencies":[],"code":"print('must not run')","timeoutSecs":1})).await.unwrap();
    assert_eq!(queued["error"]["code"], "timeout");
    assert_eq!(
        queued["runner"], "none",
        "expired queued call started a guest"
    );
    assert_eq!(first.await.unwrap().unwrap()["error"]["code"], "timeout");
    let receipt = service.finish().await.unwrap();
    assert_eq!(receipt["cleanupVerified"], true);
    assert_eq!(receipt["records"][0]["error"]["code"], "timeout");
    assert_eq!(receipt["records"][0]["cleanupVerified"], true);
}

#[tokio::test]
async fn stalled_helper_cannot_get_a_second_cleanup_allowance() {
    use augmentagent_channel_core::code_mode::compute::ComputeService;
    let root = tempfile::tempdir().unwrap();
    let mut config = service_config(root.path(), false);
    config.policy.call_timeout = Duration::from_secs(1);
    let artifact_root = config.artifact_root.clone();
    let service = ComputeService::start(config).await.unwrap();
    let audit: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact_root.join("audit.json")).unwrap()).unwrap();
    let pid = audit["ownerPid"].as_i64().unwrap() as libc::pid_t;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    struct Resume(OwnedFd);
    impl Resume {
        fn signal(&self, signal: libc::c_int) -> libc::c_long {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            }
        }
    }
    impl Drop for Resume {
        fn drop(&mut self) {
            self.signal(libc::SIGCONT);
        }
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0);
    let resume = Resume(unsafe { OwnedFd::from_raw_fd(fd as i32) });
    assert_eq!(resume.signal(libc::SIGSTOP), 0);
    let started = std::time::Instant::now();
    let result = service
        .execute(json!({"runtime":"python","dependencies":[],"code":"pass"}))
        .await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_millis(6500));
    let cleanup_started = std::time::Instant::now();
    assert!(
        service.finish().await.is_err(),
        "a killed/stalled helper cannot prove cleanup"
    );
    assert!(
        cleanup_started.elapsed() < Duration::from_millis(500),
        "cleanup restarted an already spent allowance"
    );
    let mut poll = libc::pollfd {
        fd: resume.0.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut poll, 1, 500) },
        1,
        "stopped helper survived the hard deadline"
    );
    assert_ne!(poll.revents & libc::POLLIN, 0);
}

#[tokio::test]
async fn helper_files_live_in_managed_storage_and_cleanup_pins_the_original_directory() {
    use augmentagent_channel_core::code_mode::compute::ComputeService;
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let config = service_config(root.path(), false);
    let artifacts = config.artifact_root.clone();
    let service = ComputeService::start(config).await.unwrap();
    let directories: Vec<_> = std::fs::read_dir(&artifacts)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(
        directories.len(),
        1,
        "helper files escaped managed task storage"
    );
    let helper = &directories[0];
    assert_eq!(
        std::fs::metadata(helper).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(helper.join("code-mode-compute.py").is_file());
    assert_eq!(
        std::fs::metadata(helper.join("policy.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let name = helper.file_name().unwrap();
    let moved = root.path().join("moved-artifacts");
    std::fs::rename(&artifacts, &moved).unwrap();
    std::fs::create_dir(&artifacts).unwrap();
    let replacement = artifacts.join(name);
    std::fs::create_dir(&replacement).unwrap();
    std::fs::write(replacement.join("sentinel"), "unrelated directory").unwrap();
    assert_eq!(service.finish().await.unwrap()["cleanupVerified"], true);
    assert!(
        !moved.join(name).exists(),
        "finished task retained executable helper files"
    );
    assert_eq!(
        std::fs::read_to_string(replacement.join("sentinel")).unwrap(),
        "unrelated directory"
    );
}
