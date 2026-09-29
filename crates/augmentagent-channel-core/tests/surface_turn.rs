//! #1288 — the transport-neutral conversation turn Discord and Slack share:
//! native session continuity, the turn claim persisted before the agent
//! runs, cancellation and non-native fallback. The provider is a recording
//! stand-in that joins the native session exactly like the CLI adapters do.
//! Stores are temporary; identifiers are synthetic.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_core::surface_conformance::RecordingNativeProvider;
use augmentagent_channel_core::surface_turn::{
    run_surface_turn, turn_env, SurfaceTurnOutcome, SurfaceTurnRequest, TURN_ENV,
};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_store::{
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform, SurfaceTurnRef,
    SurfaceTurnResolution, SurfaceTurnStatus,
};
use tokio_util::sync::CancellationToken;

fn slack(conversation: &str, thread: Option<&str>) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team:T00000001").unwrap(),
        conversation,
        thread.map(str::to_string),
    )
    .unwrap()
}

fn store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state dir \u{fc}").join("data.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    (dir, Arc::new(Store::open(path).unwrap()))
}

fn opts(turn: &SurfaceTurnRef) -> ReasonerOpts {
    let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
    opts.session_id = Some(turn.turn_id().to_string());
    opts
}

async fn run(
    store: &Store,
    provider: &RecordingNativeProvider,
    turn: &SurfaceTurnRef,
    history: &str,
    current: &str,
    selected: Option<ProviderKind>,
    cancel: Option<&CancellationToken>,
) -> anyhow::Result<SurfaceTurnOutcome> {
    run_surface_turn(
        store,
        SurfaceTurnRequest {
            turn,
            history,
            current,
            cwd: "/workspace with space/wiki",
        },
        || Ok(selected),
        cancel,
        |prompt| async move { provider.call(&opts(turn), &prompt).await },
    )
    .await
}

fn answered(outcome: SurfaceTurnOutcome) -> String {
    match outcome {
        SurfaceTurnOutcome::Answered(text) => text,
        other => panic!("expected an answer, got {other:?}"),
    }
}

#[tokio::test]
async fn a_follow_up_resumes_the_conversations_native_session_and_history_is_sent_once() {
    let (_dir, store) = store();
    let provider = RecordingNativeProvider::new(ProviderKind::Claude);
    let thread = slack("C00000001", Some("1700000000.000100"));
    let first = SurfaceTurnRef::new(thread.clone(), "slack:t1").unwrap();
    let second = SurfaceTurnRef::new(thread.clone(), "slack:t2").unwrap();
    answered(
        run(
            &store,
            &provider,
            &first,
            "user: earlier",
            "first",
            None,
            None,
        )
        .await
        .unwrap(),
    );
    answered(
        run(
            &store,
            &provider,
            &second,
            "user: earlier",
            "second",
            None,
            None,
        )
        .await
        .unwrap(),
    );
    let calls = provider.calls();
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].resumed && calls[1].resumed);
    assert_eq!(calls[0].native_session_id, calls[1].native_session_id);
    assert!(calls[0].prompt.contains("user: earlier"));
    assert!(
        !calls[1].prompt.contains("user: earlier"),
        "history only bootstraps a new session"
    );
    // The audit/handoff identity is the turn, never the conversation.
    assert_eq!(calls[0].audit_session_id.as_deref(), Some("slack:t1"));
    assert_eq!(calls[1].audit_session_id.as_deref(), Some("slack:t2"));
    let binding = store.surface_conversation(&thread).unwrap().unwrap();
    assert_eq!(binding.native_session_id, calls[0].native_session_id);
    assert_eq!(binding.provider, "claude");
    assert_eq!(binding.cwd, "/workspace with space/wiki");
    assert!(!binding.uncertain);
    assert_eq!(
        store.surface_turn_state(&second).unwrap().unwrap().status,
        SurfaceTurnStatus::Complete
    );
}

#[tokio::test]
async fn distinct_conversations_never_share_a_session_even_when_run_concurrently() {
    let (_dir, store) = store();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Codex).holding_on("slow"));
    let a = SurfaceTurnRef::new(slack("C00000001", Some("1700000000.000100")), "a1").unwrap();
    let b = SurfaceTurnRef::new(slack("C00000001", Some("1700000000.000200")), "b1").unwrap();
    let dm = SurfaceTurnRef::new(slack("D00000001", None), "d1").unwrap();
    let slow = {
        let (store, provider, a) = (Arc::clone(&store), Arc::clone(&provider), a.clone());
        tokio::spawn(async move {
            run(
                &store,
                &provider,
                &a,
                "",
                "slow question",
                Some(ProviderKind::Codex),
                None,
            )
            .await
        })
    };
    provider.started.notified().await;
    // While A is still running, B and the DM complete.
    answered(
        run(
            &store,
            &provider,
            &b,
            "",
            "quick b",
            Some(ProviderKind::Codex),
            None,
        )
        .await
        .unwrap(),
    );
    answered(
        run(
            &store,
            &provider,
            &dm,
            "",
            "quick dm",
            Some(ProviderKind::Codex),
            None,
        )
        .await
        .unwrap(),
    );
    provider.release.notify_one();
    answered(slow.await.unwrap().unwrap());
    let ids: Vec<String> = provider
        .calls()
        .into_iter()
        .map(|c| c.native_session_id)
        .collect();
    assert_eq!(ids.len(), 3);
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(ids[1], ids[2]);
}

#[tokio::test]
async fn the_claim_is_persisted_before_the_agent_runs_and_a_duplicate_never_runs_twice() {
    let (dir, store) = store();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("hold"));
    let turn = SurfaceTurnRef::new(slack("D00000001", None), "slack:dup").unwrap();
    let running = {
        let (store, provider, turn) = (Arc::clone(&store), Arc::clone(&provider), turn.clone());
        tokio::spawn(async move { run(&store, &provider, &turn, "", "hold on", None, None).await })
    };
    provider.started.notified().await;
    // Another process (a restarted daemon) sees the claim while the agent runs.
    let other = Store::open(dir.path().join("state dir \u{fc}").join("data.db")).unwrap();
    assert_eq!(
        other.surface_turn_state(&turn).unwrap().unwrap().status,
        SurfaceTurnStatus::Pending
    );
    provider.release.notify_one();
    answered(running.await.unwrap().unwrap());
    let again = run(&store, &provider, &turn, "", "hold on", None, None).await;
    assert!(again.is_err(), "a redelivered turn must not run again");
    assert_eq!(provider.calls().len(), 1);
}

#[tokio::test]
async fn cancel_tears_the_turn_down_resolves_the_claim_and_the_session_continues() {
    let (_dir, store) = store();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("long"));
    let thread = slack("C00000001", Some("1700000000.000100"));
    let first = SurfaceTurnRef::new(thread.clone(), "slack:c1").unwrap();
    let cancel = CancellationToken::new();
    let running = {
        let (store, provider, first, cancel) = (
            Arc::clone(&store),
            Arc::clone(&provider),
            first.clone(),
            cancel.clone(),
        );
        tokio::spawn(async move {
            run(
                &store,
                &provider,
                &first,
                "",
                "a long job",
                None,
                Some(&cancel),
            )
            .await
        })
    };
    provider.started.notified().await;
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("cancel returns promptly")
        .unwrap()
        .unwrap();
    assert!(
        matches!(outcome, SurfaceTurnOutcome::Cancelled),
        "{outcome:?}"
    );
    assert_eq!(
        provider.dropped_mid_turn(),
        1,
        "the provider future was torn down"
    );
    let state = store.surface_turn_state(&first).unwrap().unwrap();
    assert_eq!(state.resolution, Some(SurfaceTurnResolution::Cancelled));
    // The session the cancelled turn created is kept, so the next message
    // in the thread continues it instead of forking a new one.
    let started_id = provider.observed_ids()[0].clone();
    let binding = store.surface_conversation(&thread).unwrap().unwrap();
    assert_eq!(binding.native_session_id, started_id);
    assert!(!binding.uncertain);
    let next = SurfaceTurnRef::new(thread, "slack:c2").unwrap();
    answered(
        run(&store, &provider, &next, "", "next", None, None)
            .await
            .unwrap(),
    );
    let calls = provider.calls();
    assert_eq!(calls.last().unwrap().native_session_id, started_id);
    assert!(calls.last().unwrap().resumed);
}

#[tokio::test]
async fn a_non_native_selection_keeps_the_legacy_prompt_route_without_a_claim() {
    let (_dir, store) = store();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let turn = SurfaceTurnRef::new(slack("D00000001", None), "slack:q1").unwrap();
    let outcome = run_surface_turn(
        &store,
        SurfaceTurnRequest {
            turn: &turn,
            history: "user: before",
            current: "now",
            cwd: "/wiki",
        },
        || Ok(Some(ProviderKind::Qwen)),
        None,
        |prompt| {
            let seen = Arc::clone(&seen);
            async move {
                let native = augmentagent_channel_core::native_session::CURRENT
                    .try_with(|_| ())
                    .is_ok();
                seen.lock().unwrap().push((prompt, native));
                Ok("legacy".to_string())
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(answered(outcome), "legacy");
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0],
        (
            "user: before\n\nuser's current message:\nnow".to_string(),
            false
        )
    );
    assert_eq!(store.surface_turn_state(&turn).unwrap(), None);
}

#[tokio::test]
async fn an_uncertain_session_refuses_the_next_turn_and_the_selection_must_match_the_binding() {
    let (_dir, store) = store();
    let provider = RecordingNativeProvider::new(ProviderKind::Claude);
    let thread = slack("C00000001", Some("1700000000.000900"));
    let first = SurfaceTurnRef::new(thread.clone(), "u1").unwrap();
    answered(
        run(&store, &provider, &first, "", "one", None, None)
            .await
            .unwrap(),
    );
    let conflict = run(
        &store,
        &provider,
        &SurfaceTurnRef::new(thread.clone(), "u2").unwrap(),
        "",
        "two",
        Some(ProviderKind::Codex),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        conflict
            .to_string()
            .contains("conflicts with bound native session"),
        "{conflict}"
    );
    store.mark_surface_conversation_uncertain(&thread).unwrap();
    let blocked = run(
        &store,
        &provider,
        &SurfaceTurnRef::new(thread, "u3").unwrap(),
        "",
        "three",
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(blocked.to_string().contains("uncertain turn"), "{blocked}");
    assert_eq!(provider.calls().len(), 1);
}

#[tokio::test]
async fn turn_env_reaches_the_agent_inside_the_turn_only() {
    let (_dir, store) = store();
    let turn = SurfaceTurnRef::new(slack("D00000001", None), "slack:env").unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let env = vec![(
        "AUGMENTAGENT_SLACK_INBOUND_DIR".to_string(),
        "/state/slack-inbound/msg-a".to_string(),
    )];
    assert!(turn_env().is_empty());
    let outcome = TURN_ENV
        .scope(env.clone(), async {
            run_surface_turn(
                &store,
                SurfaceTurnRequest {
                    turn: &turn,
                    history: "",
                    current: "hi",
                    cwd: "/wiki",
                },
                || Ok(Some(ProviderKind::Qwen)),
                None,
                |_prompt| {
                    let seen = Arc::clone(&seen);
                    let env = env.clone();
                    async move {
                        assert_eq!(turn_env(), env);
                        seen.fetch_add(1, Ordering::SeqCst);
                        Ok("ok".to_string())
                    }
                },
            )
            .await
        })
        .await
        .unwrap();
    answered(outcome);
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(turn_env().is_empty());
}
