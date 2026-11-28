//! Jarvis Dashboard turn runner (spec web-dashboard-v2 §6 item 6a): the
//! shared session conformance scenario plus the web-specific turn rules,
//! all against the production `WikiQuerier` with only the provider replaced
//! by the offline recording stand-in. Each test re-runs itself in a child
//! process (see `surface_conformance_tests::isolated`).

use super::*;
use crate::surface_conformance_tests::{isolated, querier};
use crate::web_turn_runner::{in_web_turn, web_audit_ctx, WebTurnRunner};
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_core::surface_conformance::{
    native_session_conformance, ConformanceAdapter, RecordingNativeProvider,
};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_store::rusqlite::OptionalExtension;
use augmentagent_store::web_chat::{
    web_conversation, web_turn_ref, WebChatError, WebTurn, WebTurnErrorCode, WebTurnStatus,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

fn runner(
    root: &Path,
    provider: &Arc<RecordingNativeProvider>,
) -> (Arc<Store>, Arc<WebTurnRunner>) {
    let handler = querier(root, provider);
    let store = handler.conversation_store.clone().unwrap();
    let runner = WebTurnRunner::new(
        Arc::clone(&store),
        Arc::new(handler),
        root.display().to_string(),
    );
    (store, runner)
}

async fn wait_for(store: &Store, turn_id: &str, done: impl Fn(&WebTurn) -> bool) -> WebTurn {
    for _ in 0..1000 {
        if let Ok(turn) = store.web_turn(turn_id) {
            if done(&turn) {
                return turn;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "turn {turn_id} never reached the expected state: {:?}",
        store.web_turn(turn_id)
    );
}

async fn terminal(store: &Store, turn_id: &str) -> WebTurn {
    wait_for(store, turn_id, |t| {
        !matches!(t.status, WebTurnStatus::Queued | WebTurnStatus::Running)
    })
    .await
}

async fn status(store: &Store, turn_id: &str, status: WebTurnStatus) -> WebTurn {
    wait_for(store, turn_id, |t| t.status == status).await
}

fn reply(store: &Store, turn_id: &str) -> Option<String> {
    store
        .with_conn(|c| {
            c.query_row(
                "SELECT text FROM web_chat_messages WHERE turn_id = ?1 AND role = 'jarvis'",
                [turn_id],
                |r| r.get(0),
            )
            .optional()
        })
        .unwrap()
}

fn release_all(provider: &RecordingNativeProvider, n: usize) {
    for _ in 0..n {
        provider.release.notify_one();
    }
}

struct WebAdapter {
    root: PathBuf,
    provider: Arc<RecordingNativeProvider>,
    current: Mutex<(Arc<Store>, Arc<WebTurnRunner>)>,
    threads: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl ConformanceAdapter for WebAdapter {
    fn name(&self) -> &str {
        "web"
    }

    async fn turn(&self, conversation: &str, turn: &str, text: &str) -> anyhow::Result<String> {
        let (store, runner) = self.current.lock().unwrap().clone();
        let thread = {
            let mut threads = self.threads.lock().unwrap();
            match threads.get(conversation) {
                Some(id) => id.clone(),
                None => {
                    let id = store.web_create_thread(None)?.id;
                    threads.insert(conversation.into(), id.clone());
                    id
                }
            }
        };
        let turn_id = format!("{conversation}-{turn}");
        runner.submit(&thread, &turn_id, text)?;
        let done = terminal(&store, &turn_id).await;
        anyhow::ensure!(done.status == WebTurnStatus::Done, "{done:?}");
        reply(&store, &turn_id).context("done without a reply")
    }

    async fn restart(&self) {
        let (store, runner) = runner(&self.root, &self.provider);
        store.web_restart_sweep().unwrap();
        *self.current.lock().unwrap() = (store, runner);
    }
}

#[tokio::test]
async fn web_turns_pass_the_shared_session_conformance_scenario() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_turns_pass_the_shared_session_conformance_scenario",
        "WEB_TURN_CONFORMANCE_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let adapter = WebAdapter {
        current: Mutex::new(runner(&root, &provider)),
        root,
        provider: Arc::clone(&provider),
        threads: Mutex::default(),
    };
    native_session_conformance(&adapter, &provider).await;
    for call in provider.calls() {
        let id = call.audit_session_id.unwrap_or_default();
        assert!(
            id.starts_with("web:") && id.matches(':').count() == 2,
            "{id}"
        );
    }
}

struct OptsCapture(Arc<Mutex<Vec<ReasonerOpts>>>);

#[async_trait]
impl Reasoner for OptsCapture {
    async fn call(&self, opts: &ReasonerOpts, _prompt: &str) -> anyhow::Result<String> {
        self.0.lock().unwrap().push(opts.clone());
        Ok("ok".into())
    }
}

/// What a turn's tool surface is: allowed tools, MCP servers and env names.
fn surface(opts: &ReasonerOpts) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = opts
        .allowed_tools
        .iter()
        .map(|t| format!("tool {t}"))
        .collect();
    if let Some(settings) = opts.settings_json.as_deref() {
        let settings: serde_json::Value = serde_json::from_str(settings).unwrap();
        if let Some(servers) = settings["mcpServers"].as_object() {
            out.extend(servers.keys().map(|k| format!("mcp {k}")));
        }
    }
    out.extend(opts.env.iter().map(|(k, _)| format!("env {k}")));
    out
}

/// T1 (#2777): a web turn gets exactly the tools, MCP servers, env and brief
/// of a Discord owner turn. The channel-scoped tools bind to the web thread
/// instead of a Discord channel. Not compared: phone tools (no phone
/// transport scope on either turn), voice (needs a Discord voice channel) and
/// the Discord origin notifier sink.
#[tokio::test]
async fn web_turns_get_the_discord_owner_tool_surface() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_turns_get_the_discord_owner_tool_surface",
        "WEB_TURN_T1_ROOT",
    ) else {
        return;
    };
    // Make every channel-scoped tool attachable.
    std::fs::create_dir_all(root.join("sidecars/computer-use")).unwrap();
    std::fs::write(root.join("sidecars/computer-use/mcp.mjs"), "").unwrap();
    let token = root.join("tabagent.token");
    std::fs::write(&token, "token-for-tests\n").unwrap();
    std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    std::env::set_var("AUGMENTAGENT_TABAGENT_TOKEN_FILE", &token);
    std::env::set_var("NEWSLETTERBUDDY_URL", "http://127.0.0.1:9");

    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut handler = querier(
        &root,
        &Arc::new(RecordingNativeProvider::new(ProviderKind::Claude)),
    );
    handler.reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(
            ProviderKind::Claude,
            Arc::new(OptsCapture(Arc::clone(&captured))) as Arc<dyn Reasoner>,
        )],
        augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns.json")),
    ));
    let discord = augmentagent_approval_discord::AuditCtx {
        session_id: "2:10".into(),
        guild_id: Some(1),
        http: None,
        channel_id: Some(serenity::model::id::ChannelId::new(2)),
        owner_authorized: true,
    };
    augmentagent_approval_discord::interaction::owner_history(true, handler.answer(&discord, "hi"))
        .await
        .unwrap();
    let web = web_audit_ctx("thread", "turn");
    in_web_turn(
        "thread",
        augmentagent_approval_discord::interaction::owner_history(true, handler.answer(&web, "hi")),
    )
    .await
    .unwrap();
    // A bare web-shaped context outside a web turn gets none of them.
    handler.answer(&web, "hi").await.unwrap();

    let calls = captured.lock().unwrap();
    let (discord_opts, web_opts, bare) = (&calls[0], &calls[1], &calls[2]);
    let d = surface(discord_opts);
    for tool in [
        "mcp computer",
        "mcp tabagent",
        "mcp model",
        "env NEWSLETTERBUDDY_REQUEST_ID",
    ] {
        assert!(d.contains(tool), "fixture did not attach {tool}: {d:?}");
        assert!(
            !surface(bare).contains(tool),
            "{tool} without a web turn scope"
        );
    }
    let w = surface(web_opts);
    let missing: Vec<&String> = d.difference(&w).collect();
    let extra: Vec<&String> = w.difference(&d).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "discord-only {missing:?}, web-only {extra:?}"
    );
    assert!(
        web_opts.system_prompt == discord_opts.system_prompt,
        "same brief and owner rules"
    );
    assert_eq!(web_opts.session_id.as_deref(), Some("web:thread:turn"));
    assert!(
        web_opts
            .allowed_tools
            .iter()
            .any(|t| t.contains("approvals history")),
        "approval history is owner-private context on the web too"
    );
    // The tools are keyed on the web thread, not a Discord channel.
    let settings: serde_json::Value =
        serde_json::from_str(web_opts.settings_json.as_deref().unwrap()).unwrap();
    assert_eq!(settings["mcpServers"]["computer"]["args"][1], "web:thread");
    let model_args = settings["mcpServers"]["model"]["args"].to_string();
    assert!(
        model_args.contains("\"--channel\",\"web:thread\""),
        "{model_args}"
    );
    assert!(
        settings["mcpServers"]["tabagent"]["env"]
            .get(crate::tabagent_handoff::CHANNEL_ENV)
            .is_none(),
        "no Discord handoff channel for a web turn"
    );
}

#[tokio::test]
async fn web_resend_after_restart_succeeds() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_resend_after_restart_succeeds",
        "WEB_TURN_RESEND_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let (store, _) = runner(&root, &provider);
    let thread = store.web_create_thread(None).unwrap().id;
    // A daemon died mid-turn: recorded, running, claimed, never finished.
    store
        .web_submit_turn(&thread, "lost", "lost question")
        .unwrap();
    store.web_start_turn("lost").unwrap();
    store
        .claim_surface_turn(&web_turn_ref(&thread, "lost"))
        .unwrap();
    drop(store);

    let (store, runner) = runner(&root, &provider);
    assert_eq!(store.web_restart_sweep().unwrap(), 1);
    assert_eq!(
        store.web_turn("lost").unwrap().error_code,
        Some(WebTurnErrorCode::Interrupted)
    );
    runner.submit(&thread, "again", "lost question").unwrap();
    assert_eq!(terminal(&store, "again").await.status, WebTurnStatus::Done);
    assert_eq!(provider.calls().len(), 1);
}

#[tokio::test]
async fn web_cancel_while_queued_never_runs_the_turn() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_cancel_while_queued_never_runs_the_turn",
        "WEB_TURN_CANCEL_QUEUED_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("HOLD"));
    let (store, runner) = runner(&root, &provider);
    let threads: Vec<String> = (0..4)
        .map(|_| store.web_create_thread(None).unwrap().id)
        .collect();
    // Behind its thread's queue: the token stops it.
    runner.submit(&threads[0], "a1", "HOLD a1").unwrap();
    status(&store, "a1", WebTurnStatus::Running).await;
    runner.submit(&threads[0], "a2", "a2").unwrap();
    assert_eq!(runner.cancel("a2").unwrap(), WebTurnStatus::Cancelled);
    // The CAS race: cancelled in the store while waiting for a permit, with
    // no token fired; the queued → running swap fails and nothing runs.
    runner.submit(&threads[1], "b1", "HOLD b1").unwrap();
    runner.submit(&threads[2], "c1", "HOLD c1").unwrap();
    status(&store, "b1", WebTurnStatus::Running).await;
    status(&store, "c1", WebTurnStatus::Running).await;
    runner.submit(&threads[3], "d1", "d1 raced").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.web_cancel_queued_turn("d1").unwrap(),
        WebTurnStatus::Cancelled
    );
    release_all(&provider, 3);
    for id in ["a1", "b1", "c1"] {
        assert_eq!(terminal(&store, id).await.status, WebTurnStatus::Done);
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    for id in ["a2", "d1"] {
        assert_eq!(
            store.web_turn(id).unwrap().status,
            WebTurnStatus::Cancelled,
            "{id}"
        );
        assert!(reply(&store, id).is_none());
    }
    assert_eq!(provider.calls().len(), 3);
    assert!(!provider
        .calls()
        .iter()
        .any(|c| c.prompt.contains("raced") || c.prompt.contains("a2")));
}

#[tokio::test]
async fn web_cancel_while_running_tears_the_turn_down() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_cancel_while_running_tears_the_turn_down",
        "WEB_TURN_CANCEL_RUNNING_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("HOLD"));
    let (store, runner) = runner(&root, &provider);
    let thread = store.web_create_thread(None).unwrap().id;
    runner.submit(&thread, "t1", "HOLD me").unwrap();
    provider.started.notified().await;
    assert_eq!(runner.cancel("t1").unwrap(), WebTurnStatus::Running);
    assert_eq!(
        terminal(&store, "t1").await.status,
        WebTurnStatus::Cancelled
    );
    assert_eq!(provider.dropped_mid_turn(), 1);
    assert!(reply(&store, "t1").is_none());
    let claim = store
        .surface_turn_state(&web_turn_ref(&thread, "t1"))
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.resolution,
        Some(augmentagent_store::SurfaceTurnResolution::Cancelled)
    );
    // The thread continues: the next turn runs.
    runner.submit(&thread, "t2", "next").unwrap();
    assert_eq!(terminal(&store, "t2").await.status, WebTurnStatus::Done);
}

#[tokio::test]
async fn web_busy_thread_refuses_a_fourth_turn_and_reset_but_not_other_threads() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_busy_thread_refuses_a_fourth_turn_and_reset_but_not_other_threads",
        "WEB_TURN_BUSY_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("HOLD"));
    let (store, runner) = runner(&root, &provider);
    let a = store.web_create_thread(None).unwrap().id;
    let b = store.web_create_thread(None).unwrap().id;
    runner.submit(&a, "a1", "HOLD a1").unwrap();
    status(&store, "a1", WebTurnStatus::Running).await;
    runner.submit(&a, "a2", "a2").unwrap();
    runner.submit(&a, "a3", "a3").unwrap();
    assert!(matches!(
        runner.submit(&a, "a4", "a4"),
        Err(WebChatError::ThreadBusy)
    ));
    assert!(matches!(
        runner.reset_session(&a),
        Err(WebChatError::ThreadBusy)
    ));
    // One busy thread does not block another.
    runner.submit(&b, "b1", "b1").unwrap();
    assert_eq!(terminal(&store, "b1").await.status, WebTurnStatus::Done);
    assert_eq!(store.web_turn("a1").unwrap().status, WebTurnStatus::Running);
    release_all(&provider, 1);
    for id in ["a1", "a2", "a3"] {
        assert_eq!(terminal(&store, id).await.status, WebTurnStatus::Done);
    }
    runner.reset_session(&a).unwrap();
    assert!(store
        .surface_conversation(&web_conversation(&a))
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn web_turns_are_capped_at_three_concurrent() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_turns_are_capped_at_three_concurrent",
        "WEB_TURN_CAP_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("HOLD"));
    let (store, runner) = runner(&root, &provider);
    let threads: Vec<String> = (0..4)
        .map(|_| store.web_create_thread(None).unwrap().id)
        .collect();
    for (i, thread) in threads.iter().take(3).enumerate() {
        runner
            .submit(thread, &format!("h{i}"), &format!("HOLD {i}"))
            .unwrap();
        status(&store, &format!("h{i}"), WebTurnStatus::Running).await;
    }
    runner.submit(&threads[3], "fourth", "fourth").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        store.web_turn("fourth").unwrap().status,
        WebTurnStatus::Queued
    );
    release_all(&provider, 1);
    assert_eq!(terminal(&store, "fourth").await.status, WebTurnStatus::Done);
    release_all(&provider, 2);
    for i in 0..3 {
        assert_eq!(
            terminal(&store, &format!("h{i}")).await.status,
            WebTurnStatus::Done
        );
    }
}

struct Failing;

#[async_trait]
impl Reasoner for Failing {
    async fn call(&self, _opts: &ReasonerOpts, _prompt: &str) -> anyhow::Result<String> {
        anyhow::bail!("provider unavailable")
    }
}

#[tokio::test]
async fn web_failures_are_classified_by_store_probe() {
    let Some(root) = isolated(
        "web_turn_runner_tests::web_failures_are_classified_by_store_probe",
        "WEB_TURN_FAILURE_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let (store, runner) = runner(&root, &provider);
    let thread = store.web_create_thread(None).unwrap().id;
    store
        .bind_surface_conversation(&augmentagent_store::NativeConversation {
            conversation: web_conversation(&thread),
            provider: "claude".into(),
            native_session_id: "s-uncertain".into(),
            cwd: root.display().to_string(),
            uncertain: true,
        })
        .unwrap();
    runner
        .submit(&thread, "u1", "after an uncertain turn")
        .unwrap();
    let failed = terminal(&store, "u1").await;
    assert_eq!(failed.status, WebTurnStatus::Failed);
    assert_eq!(failed.error_code, Some(WebTurnErrorCode::UncertainSession));
    assert!(provider.calls().is_empty());

    let mut handler = querier(&root, &provider);
    handler.reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(ProviderKind::Claude, Arc::new(Failing) as Arc<dyn Reasoner>)],
        augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns-failing.json")),
    ));
    let runner = WebTurnRunner::new(
        Arc::clone(&store),
        Arc::new(handler),
        root.display().to_string(),
    );
    let other = store.web_create_thread(None).unwrap().id;
    runner.submit(&other, "p1", "hello").unwrap();
    let failed = terminal(&store, "p1").await;
    assert_eq!(
        failed.error_code,
        Some(WebTurnErrorCode::ProviderError),
        "{failed:?}"
    );
    assert!(reply(&store, "p1").is_none(), "failures are not messages");
}
