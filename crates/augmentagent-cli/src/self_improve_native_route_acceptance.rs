//! Explicit paid constructor/route transport proof, not issue shipping proof.
use super::*;
use augmentagent_channel_core::{native_session::FreshBuilderSessions, ProviderKind, ProviderKind::{Claude, Codex}};
use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

fn private_json(path: &Path) -> Result<Value> {
    use std::io::Read;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 2 * 1024 * 1024,
        "private bounded evidence required"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn route_digest(authority: &str, route: &Value) -> Result<String> {
    let sorted: BTreeMap<String, Value> = serde_json::from_value(route.clone())?;
    Ok(crate::autoship_review::digest(&serde_json::to_vec(&(
        authority, sorted,
    ))?))
}
fn validate_completion(value: &Value, route: &str) -> Result<()> {
    anyhow::ensure!(
        value["diagnostic_only"] != true
            && value["exit_code"] == 0
            && value["worker_quiescence"]["remaining_workers"] == 0
            && value["worker_quiescence"]["quiet_scans"]
                .as_u64()
                .unwrap_or(0)
                >= 2
            && value["launch_sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            && value["gateway_transport"]["version"] == 1
            && value["gateway_transport"]["overflow"] == false,
        "native completion/host observation is incomplete"
    );
    let exchanges = value["gateway_transport"]["exchanges"]
        .as_array()
        .context("host exchange inventory missing")?;
    anyhow::ensure!(
        exchanges.len() <= 256
            && exchanges.iter().any(|e| e["route_sha256"] == route
                && e["transport"] == "http"
                && e["method"] == "POST"
                && e["inference"] == "responses"
                && e["successful"] == true
                && e["authenticated_request"] == true
                && e["completed"] == true
                && e["status"]
                    .as_u64()
                    .is_some_and(|s| (200..300).contains(&s))
                && e["request_bytes"].as_u64().unwrap_or(0) > 0
                && e["response_bytes"].as_u64().unwrap_or(0) > 0),
        "configured routed Responses exchange was not observed"
    );
    Ok(())
}

async fn initialize_fixture(root: &Path) -> Result<()> {
    for args in [
        vec!["init", "-q", "-b", "disposable"],
        vec!["config", "user.name", "Disposable Transport"],
        vec!["config", "user.email", "probe@example.invalid"],
        vec!["commit", "--allow-empty", "-qm", "transport fixture"],
        vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
    ] {
        anyhow::ensure!(
            Command::new("/usr/bin/git")
                .args(args)
                .current_dir(root)
                .output()
                .await?
                .status
                .success(),
            "fixture git setup failed"
        );
    }
    Ok(())
}

fn fixture_start<'a>(root: &'a Path, repository: &'a str) -> workflow::Start<'a> {
    workflow::Start {
        repo_root: root,
        worktree: root,
        repository,
        source_issue: Some(840),
        pr: None,
        branch: "disposable",
        contract: "routed-transport-v1",
        environment: "no-tools",
    }
}

#[tokio::test]
async fn routed_fixture_supports_real_workflow_git_binding_before_dispatch() -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("autoship-route-fixture-")
        .tempdir_in("/tmp")?;
    let control = tempfile::Builder::new()
        .prefix("autoship-route-control-")
        .tempdir_in("/tmp")?;
    initialize_fixture(root.path()).await?;
    let database = control.path().join("workflow.db");
    let guard = workflow::WorkflowGuard::acquire_fixture(
        &database,
        fixture_start(root.path(), "autoship-canary/routed-fixture"),
    )
    .await?
    .context("fixture did not acquire real workflow lease")?;
    guard.audit_health().await?;
    let id = guard.id().to_owned();
    let store = crate::autoship_workflow::WorkflowStore::open(&database)?;
    let snapshot = store
        .snapshot(&id)?
        .context("acquired fixture missing durable state")?;
    anyhow::ensure!(
        snapshot.checkpoint.binding.head.len() == 40 && snapshot.model_calls == 0,
        "pre-dispatch fixture state invalid"
    );
    anyhow::ensure!(
        guard
            .audit_logger()
            .native_runtime_owner()?
            .context("fenced fixture owner missing")?
            .workflow_id
            == id,
        "fixture runtime owner missing"
    );
    anyhow::ensure!(
        guard.audit_logger().path().starts_with(control.path()),
        "fixture audit escaped isolated control directory"
    );
    guard.finish()?;
    let resumed = workflow::WorkflowGuard::acquire_fixture(
        &database,
        fixture_start(root.path(), "autoship-canary/routed-fixture"),
    )
    .await?
    .context("fixture lease was not cleanly released")?;
    anyhow::ensure!(resumed.id() == id, "fixture recovery changed identity");
    resumed.finish()?;
    Ok(())
}

#[tokio::test]
#[ignore = "paid production Rust operator-route VM transport; explicit central run only"]
async fn live_native_operator_routed_transport() -> Result<()> {
    anyhow::ensure!(
        std::env::var_os("AUGMENTAGENT_MODEL_ROUTER_CONFIG").is_some(),
        "operator routing must be explicit, including test feature builds"
    );
    let router =
        augmentagent_channel_core::model_router::current()?.context("operator router missing")?;
    anyhow::ensure!(
        matches!(router.mode.as_str(), "auto" | "codex"),
        "this probe does not override another operator route"
    );
    let selection = augmentagent_channel_core::model_selection::current()?;
    anyhow::ensure!(
        selection.is_none() || selection == Some(Codex),
        "this probe does not override operator selection"
    );
    let url = reqwest::Url::parse(&router.base_url)?;
    anyhow::ensure!(
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.username().is_empty()
            && url.password().is_none(),
        "this fixture proves only the explicitly configured loopback router"
    );
    let authority = format!(
        "127.0.0.1:{}",
        url.port_or_known_default().context("router port missing")?
    );
    let route_path = format!("{}/responses", url.path().trim_end_matches('/'));
    let policy = private_json(&PathBuf::from(
        std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").context("declared VM policy missing")?,
    ))?;
    let gateway = &policy["providers"]["codex"]["gateway"];
    let host = gateway["authorities"]
        .as_array()
        .context("gateway authorities missing")?
        .iter()
        .find(|a| a["authority"] == authority && a["allow_loopback"] == true && a["tls"] == false)
        .context("configured router has no explicit loopback grant")?;
    let route = host["routes"]
        .as_array()
        .context("gateway routes missing")?
        .iter()
        .find(|r| {
            r["path_prefix"] == route_path
                && r["exact"] == true
                && r["methods"] == serde_json::json!(["POST"])
                && r["websocket"] == false
        })
        .context("configured Responses path lacks exact POST-only grant")?;
    let expected_route = route_digest(&authority, route)?;
    let state =
        PathBuf::from(std::env::var_os("XDG_STATE_HOME").context("isolated state required")?)
            .canonicalize()?;
    anyhow::ensure!(
        state.starts_with("/tmp") && workflow::path().starts_with(&state),
        "probe workflow state must be isolated"
    );
    let fixture = tempfile::Builder::new()
        .prefix("autoship-routed-transport-")
        .tempdir_in("/tmp")?;
    initialize_fixture(fixture.path()).await?;
    let repository = format!("autoship-canary/routed-{}", uuid::Uuid::new_v4());
    let guard = workflow::WorkflowGuard::acquire(fixture_start(fixture.path(), &repository))
        .await?
        .context("workflow lease unavailable")?;
    guard.check_resource_policy()?;
    let reasoner = augmentagent_channel_core::build_workflow_pinned(Codex)
        .context("required routed constructor unavailable")?;
    guard.bind_reasoner(reasoner.as_ref())?;
    let mut opts = codex_review_opts(
        fixture.path().to_owned(),
        "Return only the requested fixed token. No tools, file access, or other actions.",
    );
    opts.allowed_tools.clear();
    opts.add_dirs.clear();
    opts.restrict_env = true;
    opts.audit_logger = Some(guard.audit_logger());
    opts.session_id = Some(format!("routed-probe:{}", uuid::Uuid::new_v4()));
    let sessions = FreshBuilderSessions::new();
    // Deliberately no native_reviewer_scope/without_router override: production
    // FallbackReasoner and Codex command construction must honor operator auto.
    let answer = sessions
        .scope(reasoner.call(
            &opts,
            "Reply with exactly AUTOSHIP_ROUTED_VM_OK. Do not call any tools.",
        ))
        .await?;
    anyhow::ensure!(
        answer.trim() == "AUTOSHIP_ROUTED_VM_OK",
        "routed fixed observation failed"
    );
    let observed = sessions.observed();
    anyhow::ensure!(
        observed.len() == 1 && observed[0].provider == Codex && !observed[0].uncertain,
        "actual native conversation not established"
    );
    let provider_root = PathBuf::from(
        policy["state_root"]
            .as_str()
            .context("VM state root missing")?,
    )
    .join("codex");
    let name = private_json(
        &provider_root
            .join("aliases")
            .join(crate::autoship_review::digest(observed[0].id.as_bytes())),
    )?;
    let name = name.as_str().context("native session alias invalid")?;
    anyhow::ensure!(
        name.starts_with("session-") && Path::new(name).components().count() == 1,
        "unsafe native alias"
    );
    let session = provider_root.join(name);
    let owner = private_json(&session.join("owned.json"))?;
    anyhow::ensure!(
        owner["owner"]
            == serde_json::to_value(
                guard
                    .audit_logger()
                    .native_runtime_owner()?
                    .context("fenced runtime owner missing")?
            )?
            && owner["audit_path"] == guard.audit_logger().path().to_string_lossy().as_ref(),
        "runtime owner does not match protected call"
    );
    let completion = private_json(&session.join("complete.json"))?;
    validate_completion(&completion, &expected_route)?;
    guard.audit_health().await?;
    for line in std::fs::read_to_string(guard.audit_logger().path())?.lines() {
        let row: Value = serde_json::from_str(line)?;
        anyhow::ensure!(
            row.get("tool").is_none(),
            "transport-only probe observed a tool invocation"
        );
    }
    let calls = crate::autoship_workflow::WorkflowStore::open(&workflow::path())?
        .snapshot(guard.id())?
        .context("workflow snapshot missing")?
        .model_calls;
    anyhow::ensure!(
        calls == 1,
        "actual routed call must be charged exactly once"
    );
    guard.audit_logger().record_controller(serde_json::json!({"kind":"operator_routed_transport_acceptance","native_session":observed[0].id,"configured_router_mode":router.mode,"configured_model":router.models.codex.quality,"host_completion":completion,"model_calls":calls,"tool_calls":0,"scope":"routed provider transport only; no issue build/review/merge authority"}))?;
    guard.finish()?;
    Ok(())
}

#[test]
fn routed_transport_needs_actual_matching_completed_exchange() {
    assert_eq!(route_digest("127.0.0.1:20128", &serde_json::json!({"path_prefix":"/v1/responses","exact":true,"methods":["POST"],"websocket":false})).unwrap(), "d5df35f815f6fd683d6fea8cf9fce8379af9356bf5d6fa9781b83aaa1b7ed2e2");
    let row = serde_json::json!({"route_sha256":"expected","transport":"http","method":"POST","inference":"responses","successful":true,"authenticated_request":true,"completed":true,"status":200,"request_bytes":1,"response_bytes":1});
    let base = serde_json::json!({"exit_code":0,"launch_sha256":"a".repeat(64),"worker_quiescence":{"remaining_workers":0,"quiet_scans":2},"gateway_transport":{"version":1,"overflow":false,"exchanges":[row]}});
    assert!(validate_completion(&base, "expected").is_ok());
    assert!(validate_completion(&base, "another-route").is_err());
    for (key, value) in [
        ("inference", serde_json::json!(null)),
        ("authenticated_request", serde_json::json!(false)),
        ("completed", serde_json::json!(false)),
        ("response_bytes", serde_json::json!(0)),
    ] {
        let mut broken = base.clone();
        broken["gateway_transport"]["exchanges"][0][key] = value;
        assert!(validate_completion(&broken, "expected").is_err());
    }
    let mut broken = base;
    broken["diagnostic_only"] = true.into();
    assert!(validate_completion(&broken, "expected").is_err());
}

fn file_edit_opts(root: PathBuf) -> augmentagent_channel_core::ReasonerOpts {
    let mut opts = fix_opts(root);
    opts.allowed_tools = ["Read", "Grep", "Glob", "Write", "Edit"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    opts.system_prompt = "Use only the supplied guarded file tools to perform the requested edit. No shell, build, installation, or external action is available. Report honestly whether the edit succeeded.".into();
    opts
}

fn file_edit_audit_tool(_provider: ProviderKind, tool: &str) -> Result<bool> {
    anyhow::ensure!(
        ["Read", "Grep", "Glob", "Write", "Edit"].contains(&tool),
        "file-only probe observed undeclared tool"
    );
    Ok(["Write", "Edit"].contains(&tool))
}

#[test]
fn file_edit_audit_checks_provider_namespace_and_keeps_shells_forbidden() {
    for provider in [Claude, Codex] {
        for name in ["Read", "Grep", "Glob", "Write", "Edit"] {
            let tool=if provider==Claude {format!("mcp__jarvis__{name}")} else {name.into()};
            assert_eq!(file_edit_audit_tool(provider,&tool).unwrap(),matches!(name,"Write"|"Edit"));
        }
        for name in ["Bash", "mcp__jarvis__Bash", "mcp__other__Write", "mcp__jarvis__WriteExtra", "mcp__jarvis__mcp__jarvis__Write"] {
            assert!(file_edit_audit_tool(provider,name).is_err(),"{provider:?}: {name}");
        }
    }
    assert!(file_edit_audit_tool(Claude,"Write").is_err());
    assert!(file_edit_audit_tool(Codex,"mcp__jarvis__Write").is_err());
}

#[test]
fn file_edit_diagnostic_cannot_reach_a_build_backend() {
    let opts = file_edit_opts(PathBuf::from("/tmp/synthetic-file-only"));
    assert_eq!(
        opts.allowed_tools,
        vec!["Read", "Grep", "Glob", "Write", "Edit"]
    );
    assert!(!opts.allowed_tools.iter().any(|tool| tool.contains("Bash")));
}

#[tokio::test]
#[ignore = "paid guarded native file edit only; no full issue or shipping authority"]
async fn live_native_guarded_file_edit() -> Result<()> {
    guarded_file_edit(Codex).await
}

#[tokio::test]
#[ignore = "paid guarded Claude file edit through configured provider route; no full issue or shipping authority"]
async fn live_native_guarded_claude_file_edit() -> Result<()> {
    guarded_file_edit(Claude).await
}

async fn guarded_file_edit(provider: ProviderKind) -> Result<()> {
    anyhow::ensure!(matches!(provider, Claude | Codex), "guarded edit requires a native provider");
    let state =
        PathBuf::from(std::env::var_os("XDG_STATE_HOME").context("isolated state required")?)
            .canonicalize()?;
    anyhow::ensure!(
        state.starts_with("/tmp") && workflow::path().starts_with(&state),
        "isolated probe state required"
    );
    let fixture = tempfile::Builder::new()
        .prefix("native-file-edit-")
        .tempdir_in(state.parent().context("evidence parent missing")?)?
        .keep();
    initialize_fixture(&fixture).await?;
    let target = fixture.join("arithmetic.py");
    std::fs::write(&target, "def add(a, b):\n    return a + b - 1\n")?;
    let before = crate::autoship_review::digest(&std::fs::read(&target)?);
    let repository = format!("autoship-canary/file-edit-{}", uuid::Uuid::new_v4());
    let guard = workflow::WorkflowGuard::acquire(fixture_start(&fixture, &repository))
        .await?
        .context("workflow lease unavailable")?;
    guard.check_resource_policy()?;
    augmentagent_channel_core::autoship_write_scope::register(
        &fixture,
        "file-edit-diagnostic-v1",
        vec![
            augmentagent_channel_core::autoship_write_scope::DeclaredPath {
                path: "arithmetic.py".into(),
                directory: false,
            },
        ],
        vec![],
    )?;
    let reasoner = augmentagent_channel_core::build_workflow_pinned(provider)
        .context("production native constructor unavailable")?;
    guard.bind_reasoner(reasoner.as_ref())?;
    let mut opts = file_edit_opts(fixture.clone());
    opts.audit_logger = Some(guard.audit_logger());
    let sessions = FreshBuilderSessions::new();
    let answer = sessions.scope(reasoner.call(&opts,
        "Read arithmetic.py with the guarded tools, then replace its contents with the following two lines, including the final newline:\n```python\ndef add(a, b):\n    return a + b\n```\nUse Write or Edit, not shell. Return a concise honest summary.")).await?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(fixture.join("private-builder-response.txt"))?;
        file.write_all(answer.as_bytes())?;
    }
    guard.audit_health().await?;
    anyhow::ensure!(
        std::fs::read(&target)? == b"def add(a, b):\n    return a + b\n",
        "native file edit did not produce requested bytes"
    );
    let observed = sessions.observed();
    anyhow::ensure!(
        observed.len() == 1 && observed[0].provider == provider && !observed[0].uncertain,
        "actual native session missing"
    );
    let policy = private_json(&PathBuf::from(
        std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").context("native VM policy required")?,
    ))?;
    let provider_state =
        PathBuf::from(policy["state_root"].as_str().context("VM state missing")?).join(provider.name());
    let alias = private_json(
        &provider_state
            .join("aliases")
            .join(crate::autoship_review::digest(observed[0].id.as_bytes())),
    )?;
    let alias = alias.as_str().context("session alias missing")?;
    anyhow::ensure!(
        alias.starts_with("session-") && Path::new(alias).components().count() == 1,
        "invalid native session alias"
    );
    let runtime = provider_state.join(alias);
    let owner = private_json(&runtime.join("owned.json"))?;
    anyhow::ensure!(
        owner["owner"]
            == serde_json::to_value(
                guard
                    .audit_logger()
                    .native_runtime_owner()?
                    .context("runtime owner missing")?
            )?
            && owner["audit_path"] == guard.audit_logger().path().to_string_lossy().as_ref(),
        "native owner mismatch"
    );
    let completion = private_json(&runtime.join("complete.json"))?;
    anyhow::ensure!(
        completion["exit_code"] == 0
            && completion["diagnostic_only"] != true
            && completion["worker_quiescence"]["remaining_workers"] == 0
            && completion["worker_quiescence"]["quiet_scans"]
                .as_u64()
                .unwrap_or(0)
                >= 2,
        "native completion incomplete"
    );
    let mut edits = 0;
    for line in std::fs::read_to_string(guard.audit_logger().path())?.lines() {
        let row: Value = serde_json::from_str(line)?;
        if let Some(tool) = row["tool"].as_str() {
            if file_edit_audit_tool(provider, tool)? && row["stderr_truncated"].is_null() {
                let path = row["args"]["file_path"]
                    .as_str()
                    .or_else(|| row["args"]["path"].as_str())
                    .context("edit receipt lacks target path")?;
                anyhow::ensure!(
                    Path::new(path) == target || path == "arithmetic.py",
                    "edit receipt targets another file"
                );
                edits += 1;
            }
        }
    }
    anyhow::ensure!(edits > 0, "changed source lacks guarded edit audit receipt");
    let calls = crate::autoship_workflow::WorkflowStore::open(&workflow::path())?
        .snapshot(guard.id())?
        .context("workflow missing")?
        .model_calls;
    anyhow::ensure!(
        calls == 1,
        "file edit must charge exactly one native dispatch"
    );
    guard.audit_logger().record_controller(serde_json::json!({"kind":"native_file_edit_diagnostic","provider":provider.name(),"native_session":observed[0].id,
        "before_sha256":before,"after_sha256":crate::autoship_review::digest(&std::fs::read(&target)?),"guarded_edit_receipts":edits,
        "host_completion":completion,"model_calls":calls,"shipping_authority":false,"full_issue_pass":false,"reachable_build_memory_mb":0}))?;
    guard.finish()?;
    Ok(())
}
