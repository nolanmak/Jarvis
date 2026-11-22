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

#[tokio::test]
async fn phone_and_text_turns_share_one_queue_and_native_session() {
    for kind in [ProviderKind::Claude, ProviderKind::Codex] {
        let (_dir, store) = store();
        let provider = RecordingNativeProvider::new(kind);
        let chat = slack("D-phone-owner", None);
        let text = SurfaceTurnRef::new(chat.clone(), "slack:text-1").unwrap();
        let phone = SurfaceTurnRef::new(chat, "phone:call-1:1").unwrap();
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let answer = |turn: &SurfaceTurnRef, prompt: String| {
            let provider = &provider;
            let active = &active;
            let maximum = &maximum;
            let opts = opts(turn);
            async move {
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;
                let result = provider.call(&opts, &prompt).await;
                active.fetch_sub(1, Ordering::SeqCst);
                result
            }
        };
        let (first, second) = tokio::join!(
            run_surface_turn(
                &store,
                SurfaceTurnRequest {
                    turn: &text,
                    history: "earlier text",
                    current: "text question",
                    cwd: "/fixture"
                },
                || Ok(Some(kind)),
                None,
                |p| answer(&text, p)
            ),
            run_surface_turn(
                &store,
                SurfaceTurnRequest {
                    turn: &phone,
                    history: "earlier text",
                    current: "phone question",
                    cwd: "/fixture"
                },
                || Ok(Some(kind)),
                None,
                |p| answer(&phone, p)
            ),
        );
        assert!(first.is_ok(), "text: {first:?}");
        assert!(second.is_ok(), "phone: {second:?}");
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].native_session_id, calls[1].native_session_id);
    }
}

#[tokio::test]
async fn queued_phone_loads_history_only_if_no_native_session_exists_when_it_runs() {
    use augmentagent_channel_core::surface_turn::run_surface_turn_with_history;
    for kind in [ProviderKind::Claude, ProviderKind::Codex] {
        let (_dir, store) = store();
        let provider = RecordingNativeProvider::new(kind);
        let chat = slack("D-history", None);
        let text = SurfaceTurnRef::new(chat.clone(), "text:first").unwrap();
        let phone = SurfaceTurnRef::new(chat, "phone:first").unwrap();
        let entered = tokio::sync::Notify::new();
        let loads = AtomicUsize::new(0);
        let (text_result, phone_result) = tokio::join!(
            run_surface_turn(&store, SurfaceTurnRequest { turn: &text, history: "initial history", current: "text", cwd: "/fixture" },
                || Ok(Some(kind)), None, |prompt| {
                    let (entered, provider, text) = (&entered, &provider, &text);
                    async move {
                        entered.notify_one();
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        provider.call(&opts(text), &prompt).await
                    }
                }),
            async {
                entered.notified().await;
                run_surface_turn_with_history(&store, SurfaceTurnRequest { turn: &phone, history: "", current: "phone", cwd: "/fixture" },
                    || Ok(Some(kind)), None, |prompt| {
                        let (provider, phone) = (&provider, &phone);
                        async move { provider.call(&opts(phone), &prompt).await }
                    },
                    || async { loads.fetch_add(1, Ordering::SeqCst); anyhow::bail!("history is unavailable but native resume must work") }).await
            }
        );
        assert!(text_result.is_ok());
        assert!(phone_result.is_ok(), "{phone_result:?}");
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        let calls = provider.calls();
        assert_eq!(calls[0].native_session_id, calls[1].native_session_id);
    }
}

#[tokio::test]
async fn cancellation_during_initial_history_load_never_dispatches_or_claims_a_turn() {
    use augmentagent_channel_core::surface_turn::run_surface_turn_with_history;
    let (_dir, store) = store();
    let turn = SurfaceTurnRef::new(slack("D-history-cancel", None), "phone:cancel-history").unwrap();
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(1), run_surface_turn_with_history(&store,
        SurfaceTurnRequest { turn: &turn, history: "", current: "phone", cwd: "/fixture" },
        || Ok(Some(ProviderKind::Claude)), Some(&cancel),
        |_| async { panic!("cancelled history must not reach the model") },
        || async { cancel.cancel(); std::future::pending::<anyhow::Result<String>>().await })).await
        .expect("history cancellation must return promptly").unwrap();
    assert_eq!(result, SurfaceTurnOutcome::Cancelled);
    // A retry can claim the same ID: cancellation happened before submission.
    store.claim_surface_turn(&turn).unwrap();
}

// #1626 — a provider refusal before the prompt reaches the native CLI (resource
// capacity, hook preparation, spawn failure) has no native side effect. It must
// not latch the conversation "uncertain": the owner's next message resumes the
// same native session.
fn discord_channel() -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "1000000000000000001")
            .unwrap(),
        "2000000000000000002",
        None,
    )
    .unwrap()
}

async fn refuse_before_submission(
    store: &Store,
    turn: &SurfaceTurnRef,
) -> anyhow::Result<SurfaceTurnOutcome> {
    use augmentagent_channel_core::native_session::CURRENT;
    run_surface_turn(
        store,
        SurfaceTurnRequest {
            turn,
            history: "",
            current: "schedule a meeting",
            cwd: "/workspace with space/wiki",
        },
        || Ok(None),
        None,
        |_prompt| async move {
            let session = CURRENT.with(Arc::clone);
            let _lease = session.begin(ProviderKind::Claude)?;
            anyhow::bail!(
                "claude resource capacity: declared run resource capacity unavailable or exhausted; reconcile capacity before retry"
            )
        },
    )
    .await
}

async fn a_pre_submission_refusal_does_not_latch(chat: SurfaceConversationRef) {
    let (_dir, store) = store();
    let provider = RecordingNativeProvider::new(ProviderKind::Claude);
    let first = SurfaceTurnRef::new(chat.clone(), "u1").unwrap();
    answered(run(&store, &provider, &first, "", "one", None, None).await.unwrap());
    let bound = store.surface_conversation(&chat).unwrap().unwrap();

    let refused = SurfaceTurnRef::new(chat.clone(), "u2").unwrap();
    let error = refuse_before_submission(&store, &refused).await.unwrap_err();
    assert!(error.to_string().contains("resource capacity"), "{error}");
    let binding = store.surface_conversation(&chat).unwrap().unwrap();
    assert!(!binding.uncertain, "nothing reached the CLI; the conversation must not be latched");
    assert_eq!(binding.native_session_id, bound.native_session_id);

    let third = SurfaceTurnRef::new(chat.clone(), "u3").unwrap();
    let answer = answered(run(&store, &provider, &third, "", "three", None, None).await.unwrap());
    assert!(answer.contains("turn=2"), "{answer}");
    let calls = provider.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].resumed);
    assert_eq!(calls[1].native_session_id, bound.native_session_id);
    if chat.account().platform().as_str() != "discord" {
        // Discord top-level turns keep their own table; for them the third
        // turn being claimed at all is the proof that nothing blocks.
        assert_eq!(
            store.surface_turn_state(&third).unwrap().unwrap().status,
            SurfaceTurnStatus::Complete
        );
    }
}

#[tokio::test]
async fn a_pre_submission_refusal_does_not_latch_a_slack_thread() {
    a_pre_submission_refusal_does_not_latch(slack("C00000009", Some("1700000000.000900"))).await;
}

#[tokio::test]
async fn a_pre_submission_refusal_does_not_latch_a_discord_channel() {
    a_pre_submission_refusal_does_not_latch(discord_channel()).await;
}

#[tokio::test]
async fn a_refusal_after_submission_still_latches_the_conversation() {
    use augmentagent_channel_core::native_session::CURRENT;
    let (_dir, store) = store();
    let provider = RecordingNativeProvider::new(ProviderKind::Claude);
    let chat = discord_channel();
    let first = SurfaceTurnRef::new(chat.clone(), "u1").unwrap();
    answered(run(&store, &provider, &first, "", "one", None, None).await.unwrap());
    let died = SurfaceTurnRef::new(chat.clone(), "u2").unwrap();
    let error = run_surface_turn(
        &store,
        SurfaceTurnRequest { turn: &died, history: "", current: "two", cwd: "/w" },
        || Ok(None),
        None,
        |_prompt| async move {
            let session = CURRENT.with(Arc::clone);
            let lease = session.begin(ProviderKind::Claude)?;
            lease.mark_submitting();
            anyhow::bail!("claude exited mid-turn")
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("mid-turn"), "{error}");
    assert!(store.surface_conversation(&chat).unwrap().unwrap().uncertain);
    let blocked = run(&store, &provider, &SurfaceTurnRef::new(chat, "u3").unwrap(), "", "three", None, None)
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("uncertain turn"), "{blocked}");
    assert_eq!(provider.calls().len(), 1);
}

// #1774 — a deploy restart SIGTERMs the provider mid-turn (after submission).
// That is our host, not the provider and not an uncertain session: the turn
// closes as interrupted (consumed, never re-run), the conversation is not
// latched, the error stays typed for the surface's wording, and the next turn
// resumes the same native session.
async fn interrupt_after_submission(store: &Store, turn: &SurfaceTurnRef) -> anyhow::Error {
    use augmentagent_channel_core::native_session::CURRENT;
    run_surface_turn(
        store,
        SurfaceTurnRequest { turn, history: "", current: "text my cousin", cwd: "/w" },
        || Ok(None),
        None,
        |_prompt| async move {
            let session = CURRENT.with(Arc::clone);
            let lease = session.begin(ProviderKind::Claude)?;
            lease.mark_submitting();
            Err(augmentagent_channel_core::reasoner::interrupted_error("claude", 15))
        },
    )
    .await
    .unwrap_err()
}

async fn a_host_signal_interrupts_without_wedging(chat: SurfaceConversationRef) {
    use augmentagent_channel_core::reasoner::ReasonerError;
    let (_dir, store) = store();
    let provider = RecordingNativeProvider::new(ProviderKind::Claude);
    let first = SurfaceTurnRef::new(chat.clone(), "c:1").unwrap();
    answered(run(&store, &provider, &first, "", "one", None, None).await.unwrap());
    let bound = store.surface_conversation(&chat).unwrap().unwrap();

    let stopped = SurfaceTurnRef::new(chat.clone(), "c:2").unwrap();
    let error = interrupt_after_submission(&store, &stopped).await;
    assert!(
        matches!(ReasonerError::find_in(&error), Some(ReasonerError::Interrupted { signal: 15, .. })),
        "the surface must still see the typed error: {error:#}"
    );
    let binding = store.surface_conversation(&chat).unwrap().unwrap();
    assert!(!binding.uncertain, "a host signal must not latch the conversation");
    assert_eq!(binding.native_session_id, bound.native_session_id);
    let state = store.surface_turn_state(&stopped).unwrap().unwrap();
    assert_eq!(state.status, SurfaceTurnStatus::Uncertain);
    assert_eq!(state.resolution, Some(SurfaceTurnResolution::Interrupted));
    if chat.account().platform().as_str() == "discord" {
        assert!(store.discord_turn_interrupted(chat.conversation_id(), "c:2").unwrap());
        assert!(!store.discord_turn_interrupted(chat.conversation_id(), "c:1").unwrap());
        assert!(store.interrupt_pending_discord_turns().unwrap().is_empty());
    }

    let next = SurfaceTurnRef::new(chat.clone(), "c:3").unwrap();
    let answer = answered(run(&store, &provider, &next, "", "three", None, None).await.unwrap());
    assert!(answer.contains("turn=2"), "{answer}");
    assert!(provider.calls()[1].resumed);
    let replay = run(&store, &provider, &stopped, "", "again", None, None).await.unwrap_err();
    assert!(replay.to_string().contains("already submitted"), "{replay}");
    assert_eq!(provider.calls().len(), 2, "the interrupted turn is never re-run");
}

#[tokio::test]
async fn a_host_signal_interrupts_a_discord_turn_without_wedging_the_channel() {
    a_host_signal_interrupts_without_wedging(discord_channel()).await;
}

#[tokio::test]
async fn a_host_signal_interrupts_a_slack_turn_without_wedging_the_thread() {
    a_host_signal_interrupts_without_wedging(slack("C00000010", Some("1700000000.001000"))).await;
}
