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
