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
