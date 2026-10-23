//! #1291 — scheduled Slack contact sends through the daemon's real pieces:
//! the `ReplyApprover` (schedule, reschedule, send now, cancel, back to
//! queue), the shared `ScheduledSendEngine` driven with an explicit clock
//! (`tick_once(now)`), the Slack approval surface over a recording Web API
//! (its card is the scheduled notice), a recording Discord broker beside it,
//! and the Composio Slack API as a local mock that counts sends. Data is
//! synthetic; nothing sleeps until a wall-clock time.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_approval_discord::{
    deciding, ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, ApprovalError,
    CardSurfaces, MultiSurfaceBroker, SyncingActionHandler,
};
use augmentagent_channel_email::{ScheduledPlatformSender, ScheduledSendEngine, TickSummary};
use augmentagent_channel_slack::approvals::{SlackApprovalConfig, SlackApprovals};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::web::{RecordedCall, RecordingSlackWebApi, SlackWebApi};
use augmentagent_channel_slack::{SlackAuth, SlackClient};
use augmentagent_store::approval_cards::ApprovalCardState;
use augmentagent_store::{ActionStatus, Email, Store, SurfacePlatform};

use crate::ComposioClient;

const TEAM: &str = "T00000009";
const APP_TEAM: &str = "T00000001";
const DM: &str = "D00000001";
const MIN: i64 = 60_000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Discord as the daemon's broker sees it: notices posted and deleted.
#[derive(Default)]
struct DiscordNotices {
    posted: Mutex<Vec<(String, i64)>>,
    deleted: Mutex<Vec<u64>>,
    next: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl ApprovalBroker for DiscordNotices {
    async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
    async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
    async fn post_scheduled_notice(
        &self,
        action_id: &str,
        _: &Email,
        _: &str,
        at_ms: i64,
        _: &str,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        self.posted.lock().unwrap().push((action_id.into(), at_ms));
        let m = 100 + self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some((9, m)))
    }
    async fn delete_message(&self, _: u64, m: u64) -> Result<(), ApprovalError> {
        self.deleted.lock().unwrap().push(m);
        Ok(())
    }
}

struct Daemon {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Arc<Store>,
    approver: Arc<crate::ReplyApprover>,
    web: Arc<RecordingSlackWebApi>,
    slack: Arc<SlackApprovals>,
    discord: Arc<DiscordNotices>,
    surfaces: CardSurfaces,
    _broker: Arc<dyn ApprovalBroker>,
}

fn slack_client(composio: &str) -> Arc<SlackClient> {
    Arc::new(SlackClient::with_base_url(
        SlackAuth {
            entity_id: "entity-test".into(),
            connection_id: "conn-test".into(),
            team_id: TEAM.into(),
            team_name: "Example".into(),
            user_id: "U00000009".into(),
            composio_api_key: "test-key".into(),
        },
        composio,
    ))
}

impl Daemon {
    fn start(composio: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let store = Arc::new(Store::open(&path).unwrap());
        store
            .upsert_slack_workspace(TEAM, "Example", "entity-test", "conn-test", "U00000009")
            .unwrap();
        Self::on(dir, path, store, composio)
    }

    /// The daemon again over the same database: a restart.
    fn restart(self, composio: &str) -> Self {
        let Daemon { _dir, path, .. } = self;
        let store = Arc::new(Store::open(&path).unwrap());
        Self::on(_dir, path, store, composio)
    }

    fn on(
        dir: tempfile::TempDir,
        path: std::path::PathBuf,
        store: Arc<Store>,
        composio: &str,
    ) -> Self {
        let mut approver = crate::test_support::approver_with_store(Arc::clone(&store));
        approver.slack.insert(TEAM.into(), slack_client(composio));
        let approver = Arc::new(approver);
        let web = Arc::new(RecordingSlackWebApi::default());
        let surfaces = CardSurfaces::new();
        let slack = Arc::new(
            SlackApprovals::new(
                Arc::clone(&store),
                Arc::clone(&web) as Arc<dyn SlackWebApi>,
                SlackApprovalConfig {
                    workspace: SlackWorkspace::new(APP_TEAM, None).unwrap(),
                    channel: DM.into(),
                },
                surfaces.clone(),
            )
            .with_timezone(chrono_tz::America::New_York),
        );
        slack.set_handler(Arc::clone(&approver) as Arc<dyn ApprovalActionHandler>);
        slack.register();
        let discord = Arc::new(DiscordNotices::default());
        let broker: Arc<dyn ApprovalBroker> = Arc::new(MultiSurfaceBroker::new(vec![
            ("discord", discord.clone() as Arc<dyn ApprovalBroker>),
            ("slack", slack.clone() as Arc<dyn ApprovalBroker>),
        ]));
        approver.broker.set(Arc::downgrade(&broker)).ok();
        Daemon {
            _dir: dir,
            path,
            store,
            approver,
            web,
            slack,
            discord,
            surfaces,
            _broker: broker,
        }
    }

    fn engine(&self) -> ScheduledSendEngine<ComposioClient> {
        ScheduledSendEngine::new(
            Arc::clone(&self.store),
            Arc::new(ComposioClient::new("test-key".into())),
            Arc::clone(&self._broker),
            false,
        )
        .with_platform_sender(Arc::clone(&self.approver) as Arc<dyn ScheduledPlatformSender>)
        .with_card_surfaces(self.surfaces.clone())
    }

    /// A drafted reply to a Slack contact, carded on Slack.
    async fn pending_reply(&self, n: u32) -> String {
        let email = Email {
            message_id: format!("slack:C00000009:1700000000.00010{n}"),
            thread_id: Some("C00000009".into()),
            from: "Contact Example <slack:U00000077>".into(),
            to: String::new(),
            cc: String::new(),
            attachments: Vec::new(),
            subject: format!("Lunch {n}?"),
            body: "Are you free for lunch?".into(),
            date: String::new(),
            account_entity_id: Some(format!("slack:team:{TEAM}")),
            platform: "slack".into(),
            kind: "dm".into(),
        };
        self.store.upsert_email(&email).unwrap();
        self.store
            .record_slack_send_target(&augmentagent_channel_slack::contact::ingested_reply_target(
                &email.message_id,
                TEAM,
                "C00000009",
                "#general",
                "1700000000.000100",
                Some("1700000000.000050"),
            ))
            .unwrap();
        let id = self
            .store
            .log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some("Sure — Tuesday works."),
                ActionStatus::Pending,
            )
            .unwrap();
        self.slack
            .post_approval(&id, &email, "Sure — Tuesday works.")
            .await
            .unwrap();
        id
    }

    fn status(&self, id: &str) -> String {
        self.store
            .get_action_with_email(id)
            .unwrap()
            .unwrap()
            .action
            .status
    }

    /// The latest drawing of the action's live Slack card.
    fn card_text(&self, id: &str) -> String {
        let pointer = self
            .store
            .approval_cards_for_action(&SurfacePlatform::new("slack").unwrap(), id)
            .unwrap()
            .into_iter()
            .rfind(|c| c.state != ApprovalCardState::Replaced)
            .expect("a Slack card");
        let ts = pointer.message.message_id().to_string();
        let messages = self.web.messages();
        let mut posted = 0usize;
        let mut latest = String::new();
        for call in self.web.calls() {
            match call {
                RecordedCall::PostMessage(p) => {
                    if messages.get(posted).is_some_and(|m| m.ts == ts) {
                        latest = p.blocks.map(|b| b.to_string()).unwrap_or(p.text);
                    }
                    posted += 1;
                }
                RecordedCall::UpdateMessage(u) if u.ts == ts => {
                    latest = u.blocks.map(|b| b.to_string()).unwrap_or(u.text)
                }
                _ => {}
            }
        }
        latest
    }

    fn flag_notices(&self) -> Vec<String> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostMessage(p) if p.text.starts_with("🚩") => Some(p.text),
                _ => None,
            })
            .collect()
    }
}

async fn composio_sends(server: &mut mockito::ServerGuard, hits: usize) -> mockito::Mock {
    server
        .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "user_id": "entity-test",
            "arguments": {"channel": "C00000009", "thread_ts": "1700000000.000050",
                          "as_user": true, "text": "Sure — Tuesday works."}
        })))
        .with_body(
            r#"{"successful": true, "data": {"ok": true, "channel": "C00000009",
                "ts": "1700000001.000100", "message": {"user": "U00000009"}}}"#,
        )
        .expect(hits)
        .create_async()
        .await
}

fn scheduled(out: &ApprovalActionOutcome) -> i64 {
    match out {
        ApprovalActionOutcome::Scheduled { at_ms, .. } => *at_ms,
        other => panic!("expected Scheduled, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_slack_reply_scheduled_on_slack_fires_once_at_its_time_as_the_owner_and_the_notice_says_sent(
) {
    let mut server = mockito::Server::new_async().await;
    let sends = composio_sends(&mut server, 1).await;
    let d = Daemon::start(&server.url());
    let id = d.pending_reply(1).await;
    let at = now_ms() + 10 * MIN;

    let out = deciding("slack", d.approver.schedule(&id, at)).await;
    assert_eq!(scheduled(&out), at);
    assert_eq!(d.status(&id), "scheduled");
    assert_eq!(
        d.store.action_status_source(&id).unwrap().as_deref(),
        Some("slack")
    );
    // The Slack card became the notice, in place, with the time and zone.
    let card = d.card_text(&id);
    assert!(card.contains("Scheduled send"), "{card}");
    assert!(card.contains("America/New_York"), "{card}");
    // Discord got its notice; its pointers are stored for retirement.
    assert_eq!(
        d.discord.posted.lock().unwrap().clone(),
        vec![(id.clone(), at)]
    );
    assert_eq!(
        d.store.action_notice(&id).unwrap(),
        Some(("9".into(), "100".into()))
    );

    let engine = d.engine();
    assert_eq!(
        engine.tick_once(at - 1).await.unwrap(),
        TickSummary::default()
    );
    assert_eq!(d.status(&id), "scheduled");
    let fired = engine.tick_once(at).await.unwrap();
    assert_eq!(fired.fired, 1, "{fired:?}");
    sends.assert_async().await;
    assert_eq!(d.status(&id), "sent");
    assert_eq!(
        d.store.action_status_source(&id).unwrap().as_deref(),
        Some("scheduled-send-engine")
    );
    let ledger = d.store.slack_contact_send(&id).unwrap().unwrap();
    assert_eq!(ledger.sender_user_id, "U00000009");
    assert_eq!(
        d.store
            .self_sent_message_platform("slack:C00000009:1700000001.000100")
            .unwrap()
            .as_deref(),
        Some("slack")
    );
    assert!(
        d.card_text(&id).contains("✅ Sent."),
        "{}",
        d.card_text(&id)
    );
    assert_eq!(d.discord.deleted.lock().unwrap().clone(), vec![100]);
    assert_eq!(
        engine.tick_once(at + MIN).await.unwrap(),
        TickSummary::default()
    );
    sends.assert_async().await;
}

#[tokio::test]
async fn a_restart_between_scheduling_and_the_fire_time_sends_exactly_once() {
    let mut server = mockito::Server::new_async().await;
    let sends = composio_sends(&mut server, 1).await;
    let d = Daemon::start(&server.url());
    let id = d.pending_reply(2).await;
    let at = now_ms() + 10 * MIN;
    scheduled(&deciding("slack", d.approver.schedule(&id, at)).await);
    assert_eq!(
        d.engine().tick_once(at - MIN).await.unwrap(),
        TickSummary::default()
    );

    let d = d.restart(&server.url());
    assert_eq!(d.status(&id), "scheduled");
    assert_eq!(d.engine().tick_once(at).await.unwrap().fired, 1);
    let d = d.restart(&server.url());
    assert_eq!(
        d.engine().tick_once(at + MIN).await.unwrap(),
        TickSummary::default()
    );
    sends.assert_async().await;
    assert_eq!(d.status(&id), "sent");
}

#[tokio::test]
async fn send_now_sends_once_and_the_scheduler_then_finds_nothing() {
    let mut server = mockito::Server::new_async().await;
    let sends = composio_sends(&mut server, 1).await;
    let d = Daemon::start(&server.url());
    let id = d.pending_reply(3).await;
    let at = now_ms() + 60 * MIN;
    scheduled(&deciding("slack", d.approver.schedule(&id, at)).await);
    let out = deciding("slack", d.approver.send_now(&id)).await;
    assert!(matches!(out, ApprovalActionOutcome::Approved), "{out:?}");
    assert_eq!(d.status(&id), "sent");
    assert_eq!(d.store.action_notice(&id).unwrap(), None, "notice retired");
    assert_eq!(d.discord.deleted.lock().unwrap().clone(), vec![100]);
    // A second Send now, and the timer, send nothing.
    assert!(matches!(
        deciding("slack", d.approver.send_now(&id)).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert_eq!(
        d.engine().tick_once(at).await.unwrap(),
        TickSummary::default()
    );
    sends.assert_async().await;
}

#[tokio::test]
async fn cancel_and_back_to_queue_never_send_and_reschedule_moves_both_notices() {
    let mut server = mockito::Server::new_async().await;
    let sends = composio_sends(&mut server, 0).await;
    let d = Daemon::start(&server.url());
    let at = now_ms() + 30 * MIN;

    let cancelled = d.pending_reply(4).await;
    scheduled(&deciding("slack", d.approver.schedule(&cancelled, at)).await);
    // The owner's `cancel <ref>` on Slack: the handler, then Slack redraws.
    let answer = d
        .slack
        .handle_command(&format!("cancel {}", &cancelled[..8]))
        .await
        .unwrap();
    assert!(answer.contains("Schedule cancelled"), "{answer}");
    assert_eq!(d.status(&cancelled), "rejected");
    assert!(d.card_text(&cancelled).contains("Schedule cancelled"));

    let requeued = d.pending_reply(5).await;
    scheduled(&deciding("slack", d.approver.schedule(&requeued, at)).await);
    assert!(matches!(
        deciding("slack", d.approver.back_to_queue(&requeued)).await,
        ApprovalActionOutcome::Unscheduled
    ));
    assert_eq!(d.status(&requeued), "pending");
    assert_eq!(d.store.action_scheduled_at(&requeued).unwrap(), None);

    let moved = d.pending_reply(6).await;
    scheduled(&deciding("slack", d.approver.schedule(&moved, at)).await);
    let later = at + 60 * MIN;
    let out = deciding("slack", d.approver.reschedule(&moved, later)).await;
    assert_eq!(scheduled(&out), later);
    assert_eq!(d.store.action_scheduled_at(&moved).unwrap(), Some(later));
    assert_eq!(d.status(&moved), "scheduled");
    // Discord: the old notice for this action deleted, a new one posted.
    let posted = d.discord.posted.lock().unwrap().clone();
    assert_eq!(posted.last(), Some(&(moved.clone(), later)));
    let (_, old_msg) = (9, 100 + posted.len() as u64 - 2);
    assert!(d.discord.deleted.lock().unwrap().contains(&old_msg));
    assert_eq!(
        d.store.action_notice(&moved).unwrap(),
        Some(("9".into(), (100 + posted.len() as u64 - 1).to_string()))
    );
    // Rescheduling something not armed is refused without effect.
    assert!(matches!(
        deciding("slack", d.approver.reschedule(&requeued, later)).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    // Nothing fires for any of them at the old time; the moved one waits.
    assert_eq!(
        d.engine().tick_once(at).await.unwrap(),
        TickSummary::default()
    );
    sends.assert_async().await;
}

#[tokio::test]
async fn the_handler_refuses_past_and_too_soon_times_and_unsupported_platforms() {
    let mut server = mockito::Server::new_async().await;
    let sends = composio_sends(&mut server, 0).await;
    let d = Daemon::start(&server.url());
    let id = d.pending_reply(7).await;
    for at in [now_ms() - MIN, now_ms() + 30_000] {
        match deciding("slack", d.approver.schedule(&id, at)).await {
            ApprovalActionOutcome::Failed { message } => {
                assert!(message.contains("too soon"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(d.status(&id), "pending");
    // A platform the scheduler cannot send still refuses before arming.
    let tg = Email {
        message_id: "tg-1".into(),
        thread_id: Some("chat-1".into()),
        from: "someone".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: String::new(),
        body: "hi".into(),
        date: String::new(),
        account_entity_id: None,
        platform: "telegram".into(),
        kind: "dm".into(),
    };
    d.store.upsert_email(&tg).unwrap();
    let tg_id = d
        .store
        .log_action(
            "tg-1",
            Some("chat-1"),
            "someone",
            "",
            None,
            Some("ok"),
            ActionStatus::Pending,
        )
        .unwrap();
    assert!(matches!(
        d.approver.schedule(&tg_id, now_ms() + 30 * MIN).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    assert_eq!(d.status(&tg_id), "pending");
    sends.assert_async().await;
}

/// The Mac slept through the fire time. Within the window the send goes
/// out once when it wakes; beyond it, nothing is sent: the reply is back
/// in the queue with an actionable card and the owner is told.
#[tokio::test]
async fn a_host_asleep_past_the_fire_time_sends_within_the_window_and_requeues_beyond_it() {
    let mut server = mockito::Server::new_async().await;
    let sends = composio_sends(&mut server, 1).await;
    let d = Daemon::start(&server.url());
    let soon = d.pending_reply(8).await;
    let missed = d.pending_reply(9).await;
    let at = now_ms() + 10 * MIN;
    scheduled(&deciding("slack", d.approver.schedule(&soon, at)).await);
    scheduled(&deciding("slack", d.approver.schedule(&missed, at - 5 * MIN)).await);
    let engine = d.engine();

    // Woke 25 minutes after `soon` was due: 30 after `missed`, past the
    // default 30-minute window.
    let woke = at + 25 * MIN + 1;
    let s = engine.tick_once(woke).await.unwrap();
    assert_eq!(s.fired, 1, "{s:?}");
    assert_eq!(s.returned_to_queue, 1, "{s:?}");
    assert_eq!(d.status(&soon), "sent");
    assert_eq!(d.status(&missed), "pending");
    assert_eq!(d.store.action_scheduled_at(&missed).unwrap(), None);
    let card = d.card_text(&missed);
    assert!(
        card.contains("aa_approve") && card.contains("aa_schedule"),
        "{card}"
    );
    let flags = d.flag_notices();
    assert!(
        flags
            .iter()
            .any(|f| f.contains("was not sent") && f.contains("back in the queue")),
        "{flags:?}"
    );
    assert_eq!(
        engine.tick_once(woke + 60 * MIN).await.unwrap(),
        TickSummary::default()
    );
    sends.assert_async().await;
}

#[tokio::test]
async fn a_discord_click_on_a_slack_scheduled_send_redraws_the_slack_notice() {
    let mut server = mockito::Server::new_async().await;
    let sends = composio_sends(&mut server, 1).await;
    let d = Daemon::start(&server.url());
    let id = d.pending_reply(10).await;
    scheduled(&deciding("slack", d.approver.schedule(&id, now_ms() + 30 * MIN)).await);
    // The Discord bot beside Slack gets the approver through the syncing
    // wrapper; its Send now click is this call.
    let discord = SyncingActionHandler::new(
        "discord",
        Arc::clone(&d.approver) as Arc<dyn ApprovalActionHandler>,
        d.surfaces.clone(),
    );
    assert!(matches!(
        discord.send_now(&id).await,
        ApprovalActionOutcome::Approved
    ));
    sends.assert_async().await;
    assert_eq!(
        d.store.action_status_source(&id).unwrap().as_deref(),
        Some("discord")
    );
    assert!(
        d.card_text(&id).contains("✅ Sent."),
        "{}",
        d.card_text(&id)
    );
}
