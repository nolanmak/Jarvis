//! Explicit paid constructor/route transport proof, not issue shipping proof.
use super::*;
use augmentagent_channel_core::{native_session::FreshBuilderSessions, ProviderKind::Codex};
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
    for args in [
        vec!["init", "-q", "-b", "disposable"],
        vec!["config", "user.name", "Disposable Transport"],
        vec!["config", "user.email", "probe@example.invalid"],
        vec!["commit", "--allow-empty", "-qm", "transport fixture"],
    ] {
        anyhow::ensure!(
            Command::new("/usr/bin/git")
                .args(args)
                .current_dir(fixture.path())
                .output()
                .await?
                .status
                .success(),
            "fixture git setup failed"
        );
    }
    let repository = format!("autoship-canary/routed-{}", uuid::Uuid::new_v4());
    let guard = workflow::WorkflowGuard::acquire(workflow::Start {
        repo_root: fixture.path(),
        worktree: fixture.path(),
        repository: &repository,
        source_issue: None,
        pr: None,
        branch: "disposable",
        contract: "routed-transport-v1",
        environment: "no-tools",
    })
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
        owner["owner"]["workflow_id"] == guard.id()
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
