//! iMessage replies through the approval card (#1303).
//!
//! Approve never touches Messages. It claims the action (`pending →
//! sending`) and queues one outbox row; the Mac-side sender claims that row,
//! sends it, verifies the result in `chat.db` and reports back through
//! `augmentagent imessage outbox complete`, which is what finally flips the
//! action to `sent` or `error` (#1304).

use std::path::PathBuf;

use augmentagent_approval_discord::ApprovalActionOutcome;
use augmentagent_store::{
    ActionStatus, ActionWithEmail, ImessageOutboxItem, ImessageOutboxStatus, ImessageSendOutcome,
    NewImessageOutboxItem, Store, TriageResult,
};

use crate::ReplyApprover;

pub(crate) const PLATFORM: &str = "imessage";

/// Where approve reads conversations from and whether sends are armed.
/// Built from the environment in the daemon; tests set it directly.
#[derive(Debug, Clone, Default)]
pub(crate) struct ImessageSendConfig {
    pub bundle_dir: Option<PathBuf>,
    pub send_enabled: bool,
}

impl ImessageSendConfig {
    pub(crate) fn from_env() -> Self {
        Self {
            bundle_dir: augmentagent_channel_imessage::ImessageConfig::load().map(|c| c.repo_dir),
            send_enabled: augmentagent_channel_imessage::send_enabled(),
        }
    }
}

impl ReplyApprover {
    /// Every refusal happens before the claim, so a refused card stays
    /// `pending` and can be approved again once the cause is fixed.
    pub(crate) async fn approve_imessage(
        &self,
        action_id: &str,
        action: ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let failed = |message: String| ApprovalActionOutcome::Failed { message };
        if !self.imessage.send_enabled {
            return failed(format!(
                "iMessage sending is off; set {}=1 on the agent to enable it",
                augmentagent_channel_imessage::ENV_SEND_ENABLED
            ));
        }
        let Some(bundle_dir) = self.imessage.bundle_dir.as_deref() else {
            return failed(
                "no iMessage bundle configured; set AUGMENTAGENT_IMESSAGE_REPO_DIR".into(),
            );
        };
        let conversations =
            match augmentagent_channel_imessage::Bundle::open(bundle_dir).conversations() {
                Ok(c) => c,
                Err(e) => return failed(format!("reading the iMessage bundle: {e:#}")),
            };
        let thread_id = action.email.thread_id.as_deref().unwrap_or_default();
        let target = match augmentagent_channel_imessage::resolve_target(thread_id, &conversations)
        {
            Ok(t) => t,
            Err(e) => return failed(format!("cannot send: {e}")),
        };
        match self
            .store
            .is_imessage_outbound_allowed(&target.conversation)
        {
            Ok(true) => {}
            Ok(false) => {
                return failed(format!(
                    "this conversation is not allowed to receive sends; run \
                     `augmentagent imessage allow-outbound {}` to allow it",
                    target.conversation
                ))
            }
            Err(e) => return failed(format!("checking the outbound allowlist: {e}")),
        }
        let Some(draft) = action.action.draft_body.as_deref() else {
            return failed("no draft body on action; cannot send".into());
        };
        let body = augmentagent_approval_discord::strip_assumes_for_send(draft);
        if body.trim().is_empty() {
            return failed("draft is empty after removing card markers; cannot send".into());
        }

        match self.store.claim_action_for_send(
            action_id,
            ActionStatus::Pending,
            crate::decision_source(),
        ) {
            Ok(true) => {}
            Ok(false) => return Self::resolved_outcome(&self.store, action_id),
            Err(e) => return failed(format!("claim for send failed: {e}")),
        }
        let item = NewImessageOutboxItem {
            action_id,
            target: &target.target,
            target_kind: target.kind,
            service: &target.service,
            body: &body,
        };
        if let Err(e) = self.store.enqueue_imessage_outbox(&item) {
            let msg = format!("queueing the iMessage send failed: {e}");
            let _ =
                self.store
                    .update_action_status(action_id, ActionStatus::Error, None, Some(&msg));
            return failed(msg);
        }
        // Keep the text that will actually go out on the row.
        let _ = self.store.with_conn(|c| {
            c.execute(
                "UPDATE actions SET draftBody = ?2 WHERE id = ?1",
                augmentagent_store::rusqlite::params![action_id, body],
            )
        });
        tracing::info!(action_id, "imessage reply queued for the Mac sender");
        ApprovalActionOutcome::Approved
    }

    pub(crate) fn skip_imessage(
        &self,
        action_id: &str,
        action: ActionWithEmail,
    ) -> ApprovalActionOutcome {
        match self.store.try_resolve_action(
            action_id,
            ActionStatus::Rejected,
            crate::decision_source(),
            Some("skipped by approver"),
        ) {
            Ok(true) => {}
            Ok(false) => return Self::resolved_outcome(&self.store, action_id),
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("skip: resolve failed: {e}"),
                }
            }
        }
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        ApprovalActionOutcome::Skipped
    }
}

/// How long a claimed row may go unreported before its outcome is unknown.
/// Must exceed the sender's worst case: the first Apple event after
/// Messages starts took 60.6 s in #1280, plus up to 30 s of verification.
pub(crate) const ENV_CLAIM_TIMEOUT_SECS: &str = "AUGMENTAGENT_IMESSAGE_CLAIM_TIMEOUT_SECS";
const DEFAULT_CLAIM_TIMEOUT_SECS: i64 = 600;
/// How long an approved reply may wait for the Mac before it is dropped.
pub(crate) const ENV_OUTBOX_MAX_AGE_SECS: &str = "AUGMENTAGENT_IMESSAGE_OUTBOX_MAX_AGE_SECS";
const DEFAULT_OUTBOX_MAX_AGE_SECS: i64 = 3600;

pub(crate) const UNKNOWN_OUTCOME: &str = "iMessage send outcome unknown: the Mac sender claimed \
     it but never reported back, so it may or may not have been sent. Check Messages before \
     resending.";
pub(crate) const EXPIRED: &str = "iMessage send expired before the Mac sender picked it up. \
     Is the Mac awake and the sender installed?";

fn env_secs(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[derive(clap::Subcommand, Debug, Clone)]
pub(crate) enum OutboxOp {
    /// Hand the oldest queued reply to the calling sender. Prints one JSON
    /// line: `{"version":1,"item":null}` or the item to send.
    Claim {
        /// Accepted for clarity; output is always JSON.
        #[arg(long)]
        json: bool,
    },
    /// Report what the sender observed in chat.db for a claimed item.
    Complete {
        id: i64,
        #[arg(long, value_parser = ["sent", "failed"])]
        status: String,
        /// `message.error` from chat.db.
        #[arg(long)]
        error_code: Option<i64>,
        #[arg(long)]
        reason: Option<String>,
        /// `message.guid` of the sent row.
        #[arg(long)]
        message_guid: Option<String>,
    },
    /// Recent outbox rows: id, status, age and action. Never bodies or targets.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

/// Fail expired queued rows and stale claims, and flip their actions to
/// `error`. Returns the rows that changed so the daemon can notify.
pub(crate) fn reconcile_outbox(store: &Store) -> anyhow::Result<Vec<ImessageOutboxItem>> {
    let claim_timeout_ms = env_secs(ENV_CLAIM_TIMEOUT_SECS, DEFAULT_CLAIM_TIMEOUT_SECS) * 1000;
    let max_age_ms = env_secs(ENV_OUTBOX_MAX_AGE_SECS, DEFAULT_OUTBOX_MAX_AGE_SECS) * 1000;
    let mut changed = store.expire_imessage_outbox_claims(claim_timeout_ms)?;
    for row in &changed {
        store.finish_send_error(&row.action_id, UNKNOWN_OUTCOME, None, "imessage")?;
    }
    let expired = store.expire_imessage_outbox_queued(max_age_ms)?;
    for row in &expired {
        store.finish_send_error(&row.action_id, EXPIRED, None, "imessage")?;
    }
    changed.extend(expired);
    Ok(changed)
}

fn claim_json(item: Option<&ImessageOutboxItem>) -> String {
    let item = item.map(|i| {
        serde_json::json!({
            "id": i.id,
            "target": i.target,
            "target_kind": i.target_kind.as_str(),
            "service": i.service,
            "body": i.body,
        })
    });
    // Fixed key order: the contract is one exact line (#1304 AC-1).
    format!(
        "{{\"version\":1,\"item\":{}}}",
        item.unwrap_or(serde_json::Value::Null)
    )
}

/// Apply a sender report to the outbox row and its action.
pub(crate) fn complete(
    store: &Store,
    id: i64,
    outcome: &ImessageSendOutcome,
) -> anyhow::Result<ImessageOutboxItem> {
    let Some(before) = store.get_imessage_outbox(id)? else {
        anyhow::bail!("no outbox item {id}");
    };
    if !store.complete_imessage_outbox(id, outcome)? {
        anyhow::bail!(
            "outbox item {id} is {}, not claimed; nothing changed",
            before.status.as_str()
        );
    }
    let action = store.get_action_with_email(&before.action_id)?;
    match outcome {
        ImessageSendOutcome::Sent { message_guid } => {
            // A late report for a claim that had already been marked unknown
            // moves the action from that error to sent: it did go out.
            if !store.finish_send_sent(&before.action_id, "imessage")?
                && before.status == ImessageOutboxStatus::Unknown
            {
                store.update_action_status(&before.action_id, ActionStatus::Sent, None, None)?;
            }
            if let Some(a) = &action {
                let _ = store.mark_email_processed(&a.email.message_id, TriageResult::Reply);
                let sent_id = message_guid
                    .clone()
                    .unwrap_or_else(|| format!("imessage-outbox:{id}"));
                crate::record_self_send(
                    store,
                    Some(&sent_id),
                    a.email.thread_id.as_deref(),
                    a.email.account_entity_id.as_deref(),
                    Some(&before.action_id),
                );
            }
        }
        ImessageSendOutcome::Failed { error_code, reason } => {
            let msg = match error_code {
                Some(code) => format!("imessage error {code}"),
                None => format!("imessage send failed: {reason}"),
            };
            store.finish_send_error(&before.action_id, &msg, None, "imessage")?;
        }
    }
    Ok(store.get_imessage_outbox(id)?.expect("row exists"))
}

pub(crate) fn run_outbox(store: &Store, op: &OutboxOp) -> anyhow::Result<()> {
    match op {
        OutboxOp::Claim { .. } => {
            reconcile_outbox(store)?;
            let item = if augmentagent_channel_imessage::send_enabled() {
                store.claim_imessage_outbox()?
            } else {
                None
            };
            println!("{}", claim_json(item.as_ref()));
            Ok(())
        }
        OutboxOp::Complete {
            id,
            status,
            error_code,
            reason,
            message_guid,
        } => {
            let outcome = if status == "sent" {
                ImessageSendOutcome::Sent {
                    message_guid: message_guid.clone(),
                }
            } else {
                ImessageSendOutcome::Failed {
                    error_code: *error_code,
                    reason: reason.clone().unwrap_or_else(|| "no reason given".into()),
                }
            };
            let row = complete(store, *id, &outcome)?;
            println!(
                "{}",
                serde_json::json!({"version": 1, "id": row.id, "status": row.status.as_str()})
            );
            Ok(())
        }
        OutboxOp::List { limit } => {
            let now = chrono::Utc::now().timestamp_millis();
            println!("id\tstatus\tage_s\taction_id");
            for r in store.list_imessage_outbox(*limit)? {
                println!(
                    "{}\t{}\t{}\t{}",
                    r.id,
                    r.status.as_str(),
                    (now - r.created_at_ms) / 1000,
                    r.action_id
                );
            }
            Ok(())
        }
    }
}

pub(crate) fn set_allowlist(
    store: &Store,
    outbound: bool,
    allow: bool,
    identifier: &str,
) -> anyhow::Result<()> {
    let changed = match (outbound, allow) {
        (true, true) => store.allow_imessage_outbound(identifier)?,
        (true, false) => store.deny_imessage_outbound(identifier)?,
        (false, true) => store.allow_imessage_inbound(identifier)?,
        (false, false) => store.deny_imessage_inbound(identifier)?,
    };
    let list = if outbound { "outbound" } else { "inbound" };
    let verb = if allow { "added to" } else { "removed from" };
    if changed {
        println!("{verb} the {list} allowlist");
    } else {
        println!(
            "no change: already {}",
            if allow { "listed" } else { "absent" }
        );
    }
    Ok(())
}

pub(crate) fn print_allowlists(store: &Store) -> anyhow::Result<()> {
    for (label, outbound) in [("outbound", true), ("inbound", false)] {
        for id in store.list_imessage_allowlist(outbound)? {
            println!("{label}\t{id}");
        }
    }
    Ok(())
}

/// `imessage approve|skip <id>` — resolve one iMessage card through the
/// approver without a Discord session. The approver is built with only what
/// iMessage needs; any other platform is refused before it is touched.
pub(crate) async fn run_cli_resolve(
    store: std::sync::Arc<Store>,
    action_id: &str,
    approve: bool,
) -> anyhow::Result<()> {
    let Some(action) = store.get_action_with_email(action_id)? else {
        anyhow::bail!("no action {action_id}");
    };
    if action.email.platform != PLATFORM {
        anyhow::bail!(
            "action {action_id} is not an iMessage card (platform {}); resolve it from its own surface",
            action.email.platform
        );
    }
    let approver = crate::ReplyApprover {
        store: std::sync::Arc::clone(&store),
        gmail: std::sync::Arc::new(crate::ComposioClient::new(String::new())),
        calendar: std::sync::Arc::new(
            augmentagent_channel_calendar::ComposioCalendarClient::new(String::new()),
        ),
        linkedin: None,
        discord: None,
        slack: Default::default(),
        telegram: Default::default(),
        github: None,
        socialapi: None,
        reasoner: std::sync::Arc::new(augmentagent_channel_core::fallback::FallbackReasoner::claude_only()),
        draft_skill: String::new(),
        wiki_root: None,
        nudge: std::sync::OnceLock::new(),
        broker: std::sync::OnceLock::new(),
        imessage: ImessageSendConfig::from_env(),
    };
    let outcome = if approve {
        approver.run_approve(action_id).await
    } else {
        approver.run_skip(action_id).await
    };
    match outcome {
        ApprovalActionOutcome::Approved => {
            println!("approved: queued for the Mac sender");
            Ok(())
        }
        ApprovalActionOutcome::Skipped => {
            println!("skipped");
            Ok(())
        }
        ApprovalActionOutcome::Failed { message } => anyhow::bail!(message),
        ApprovalActionOutcome::AlreadyResolved { status, .. } => {
            anyhow::bail!("already resolved ({status})")
        }
        other => anyhow::bail!("unexpected outcome: {other:?}"),
    }
}

/// Tell the operator about every failed or unknown send once, through the
/// same flag-notice surface as other channels. Returns how many were posted.
pub(crate) async fn notify_outbox_failures(
    store: &Store,
    broker: &dyn augmentagent_approval_discord::ApprovalBroker,
) -> anyhow::Result<usize> {
    let mut posted = 0;
    for row in store.unnotified_imessage_outbox_failures()? {
        let Some(action) = store.get_action_with_email(&row.action_id)? else {
            store.mark_imessage_outbox_notified(row.id)?;
            continue;
        };
        let detail = action
            .action
            .error_message
            .clone()
            .or_else(|| row.error_detail.clone())
            .unwrap_or_else(|| row.status.as_str().to_string());
        let reason = format!("iMessage reply not delivered: {detail}");
        if let Err(e) = broker.post_flag_notice(&action.email, &reason).await {
            tracing::warn!(action_id = %row.action_id, "imessage failure notice failed: {e}");
            continue; // retried on the next tick
        }
        store.mark_imessage_outbox_notified(row.id)?;
        posted += 1;
    }
    Ok(posted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use augmentagent_channel_core::cooldown::CooldownLatch;
    use augmentagent_channel_core::fallback::FallbackReasoner;
    use augmentagent_channel_core::reasoner::{Reasoner, ReasonerOpts};
    use augmentagent_store::{Email, ImessageOutboxStatus, ImessageTargetKind};
    use tempfile::TempDir;

    const PHONE: &str = "+15555550100"; // pii-ok synthetic
    const SMS_PHONE: &str = "+15555550111"; // pii-ok synthetic

    struct Redraft;

    #[async_trait::async_trait]
    impl Reasoner for Redraft {
        async fn call(&self, _o: &ReasonerOpts, _m: &str) -> anyhow::Result<String> {
            Ok("a better reply".into())
        }
    }

    struct Fixture {
        store: Arc<Store>,
        approver: ReplyApprover,
        _dir: TempDir,
    }

    fn fixture(send_enabled: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(bundle.join("conversations")).unwrap();
        let index = serde_json::json!({
            PHONE: {"identifier": PHONE, "dir": "a", "title": "A",
                    "participants": [PHONE], "service": "iMessage"},
            SMS_PHONE: {"identifier": SMS_PHONE, "dir": "b", "title": "B",
                        "participants": [SMS_PHONE], "service": "SMS"},
            "chat900": {"identifier": "chat900", "dir": "c", "title": "G",
                        "participants": [PHONE, SMS_PHONE], "service": "iMessage"},
            "chat901": {"identifier": "chat901", "dir": "d", "title": "H",
                        "participants": [PHONE, SMS_PHONE], "service": "iMessage",
                        "chat_guid": "any;+;chat901"},
        });
        std::fs::write(
            bundle.join("conversations/index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();
        let store = Arc::new(Store::open(&dir.path().join("data.db")).unwrap());
        let latch = CooldownLatch::at(dir.path().join("latch.json"));
        let reasoner = FallbackReasoner::for_tests(
            vec![(
                augmentagent_channel_core::ProviderKind::Claude,
                Arc::new(Redraft) as Arc<dyn Reasoner>,
            )],
            latch,
        );
        let mut approver = crate::test_support::approver_with_store(Arc::clone(&store));
        approver.reasoner = Arc::new(reasoner);
        approver.imessage = ImessageSendConfig {
            bundle_dir: Some(bundle),
            send_enabled,
        };
        Fixture {
            store,
            approver,
            _dir: dir,
        }
    }

    fn seed(store: &Store, identifier: &str, draft: &str) -> String {
        seed_at(store, identifier, 7, draft)
    }

    fn seed_at(store: &Store, identifier: &str, idx: usize, draft: &str) -> String {
        let msg = format!("imessage:{identifier}:{idx}");
        store
            .upsert_email(&Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: msg.clone(),
                thread_id: Some(format!("imessage:{identifier}")),
                from: identifier.into(),
                subject: "[iMessage] A".into(),
                body: "are you free tonight?".into(),
                date: "2026-09-29T12:00:00Z".into(),
                account_entity_id: Some(PLATFORM.into()),
                platform: PLATFORM.into(),
                kind: "dm".into(),
            })
            .unwrap();
        store
            .log_action(
                &msg,
                Some(&format!("imessage:{identifier}")),
                identifier,
                "[iMessage] A",
                Some("are you free tonight?"),
                Some(draft),
                ActionStatus::Pending,
            )
            .unwrap()
    }

    fn status(store: &Store, id: &str) -> String {
        store
            .get_action_with_email(id)
            .unwrap()
            .unwrap()
            .action
            .status
    }

    fn outbox_len(store: &Store) -> usize {
        store.list_imessage_outbox(100).unwrap().len()
    }

    fn failed_message(o: ApprovalActionOutcome) -> String {
        match o {
            ApprovalActionOutcome::Failed { message } => message,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn approve_imessage_enqueues_once_and_never_reaches_gmail() {
        let f = fixture(true);
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let id = seed(&f.store, PHONE, "yes! 8pm? [assumes: owner is free]");
        let out = f.approver.run_approve(&id).await;
        assert!(matches!(out, ApprovalActionOutcome::Approved), "{out:?}");
        let rows = f.store.list_imessage_outbox(10).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.status, ImessageOutboxStatus::Queued);
        assert_eq!(row.action_id, id);
        assert_eq!(row.target, PHONE);
        assert_eq!(row.target_kind, ImessageTargetKind::Handle);
        assert_eq!(row.service, "iMessage");
        assert_eq!(
            row.body,
            augmentagent_approval_discord::strip_assumes_for_send(
                "yes! 8pm? [assumes: owner is free]"
            )
        );
        assert_eq!(status(&f.store, &id), "sending");
    }

    #[tokio::test]
    async fn approve_refuses_when_kill_switch_is_off() {
        let f = fixture(false);
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let id = seed(&f.store, PHONE, "hi");
        let msg = failed_message(f.approver.run_approve(&id).await);
        assert!(msg.contains("AUGMENTAGENT_IMESSAGE_SEND_ENABLED"), "{msg}");
        assert!(!msg.contains("draftId"));
        assert_eq!(outbox_len(&f.store), 0);
        assert_eq!(status(&f.store, &id), "pending");
    }

    #[tokio::test]
    async fn approve_refuses_conversation_not_on_outbound_allowlist() {
        let f = fixture(true);
        let id = seed(&f.store, PHONE, "hi");
        let msg = failed_message(f.approver.run_approve(&id).await);
        assert!(msg.contains("allow-outbound"), "{msg}");
        assert!(!msg.contains("draftId"));
        assert_eq!(outbox_len(&f.store), 0);
        assert_eq!(status(&f.store, &id), "pending");
    }

    #[tokio::test]
    async fn approve_names_the_target_refusal() {
        let f = fixture(true);
        for (ident, want) in [
            ("chat900", "group chat"),
            (SMS_PHONE, "SMS conversations are not supported"),
            ("+15555550122", "not found in the iMessage bundle"), // pii-ok synthetic
        ] {
            f.store.allow_imessage_outbound(ident).unwrap();
            let id = seed(&f.store, ident, "hi");
            let msg = failed_message(f.approver.run_approve(&id).await);
            assert!(msg.contains(want), "{ident}: {msg}");
            assert!(!msg.contains("draftId"));
            assert_eq!(status(&f.store, &id), "pending");
        }
        assert_eq!(outbox_len(&f.store), 0);
    }

    #[tokio::test]
    async fn group_with_guid_needs_its_own_allowlist_entry_then_queues_by_guid() {
        let f = fixture(true);
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let id = seed(&f.store, "chat901", "see you all there");
        let msg = failed_message(f.approver.run_approve(&id).await);
        assert!(msg.contains("allow-outbound chat901"), "{msg}");
        assert_eq!(outbox_len(&f.store), 0);
        f.store.allow_imessage_outbound("chat901").unwrap();
        assert!(matches!(
            f.approver.run_approve(&id).await,
            ApprovalActionOutcome::Approved
        ));
        let row = f.store.list_imessage_outbox(1).unwrap().remove(0);
        assert_eq!(row.target, "any;+;chat901");
        assert_eq!(row.target_kind, ImessageTargetKind::ChatGuid);
    }

    #[tokio::test]
    async fn approve_without_bundle_config_fails_cleanly() {
        let mut f = fixture(true);
        f.approver.imessage.bundle_dir = None;
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let id = seed(&f.store, PHONE, "hi");
        let msg = failed_message(f.approver.run_approve(&id).await);
        assert!(msg.contains("AUGMENTAGENT_IMESSAGE_REPO_DIR"), "{msg}");
        assert_eq!(outbox_len(&f.store), 0);
    }

    #[tokio::test]
    async fn concurrent_approves_queue_exactly_one_send() {
        let f = fixture(true);
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let id = seed(&f.store, PHONE, "hi");
        let (a, b) = tokio::join!(f.approver.run_approve(&id), f.approver.run_approve(&id));
        let approved = [&a, &b]
            .iter()
            .filter(|o| matches!(o, ApprovalActionOutcome::Approved))
            .count();
        let resolved = [&a, &b]
            .iter()
            .filter(|o| matches!(o, ApprovalActionOutcome::AlreadyResolved { .. }))
            .count();
        assert_eq!((approved, resolved), (1, 1), "{a:?} {b:?}");
        assert_eq!(outbox_len(&f.store), 1);
    }

    #[tokio::test]
    async fn skip_rejects_without_queueing() {
        let f = fixture(true);
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let id = seed(&f.store, PHONE, "hi");
        assert!(matches!(
            f.approver.run_skip(&id).await,
            ApprovalActionOutcome::Skipped
        ));
        assert_eq!(status(&f.store, &id), "rejected");
        assert_eq!(outbox_len(&f.store), 0);
        // A later approve on the skipped card does nothing.
        assert!(matches!(
            f.approver.run_approve(&id).await,
            ApprovalActionOutcome::AlreadyResolved { .. }
        ));
        assert_eq!(outbox_len(&f.store), 0);
    }

    #[tokio::test]
    async fn revise_stores_new_draft_without_queueing() {
        let f = fixture(true);
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let id = seed(&f.store, PHONE, "hi");
        match f.approver.run_revise(&id, "warmer").await {
            ApprovalActionOutcome::Revised { draft, email } => {
                assert_eq!(draft, "a better reply");
                assert_eq!(email.platform, PLATFORM);
            }
            other => panic!("expected Revised, got {other:?}"),
        }
        let a = f.store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(a.action.status, "pending");
        assert_eq!(a.action.draft_body.as_deref(), Some("a better reply"));
        assert_eq!(outbox_len(&f.store), 0);
    }

    #[derive(Default)]
    struct Notices(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl augmentagent_approval_discord::ApprovalBroker for Notices {
        async fn post_approval(
            &self,
            _: &str,
            _: &Email,
            _: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _: &Email,
            reason: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.0.lock().unwrap().push(reason.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn failed_and_unknown_sends_are_announced_once() {
        let f = fixture(true);
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let a = seed_at(&f.store, PHONE, 1, "one");
        let b_email = seed_at(&f.store, PHONE, 2, "two");
        f.approver.run_approve(&a).await;
        f.approver.run_approve(&b_email).await;
        let first = f.store.claim_imessage_outbox().unwrap().unwrap();
        complete(&f.store, first.id, &ImessageSendOutcome::Failed {
            error_code: Some(22), reason: "x".into() }).unwrap();
        f.store.claim_imessage_outbox().unwrap().unwrap();
        f.store
            .with_conn(|c| c.execute("UPDATE imessage_outbox SET claimed_at_ms = 1 WHERE status = 'claimed'", []))
            .unwrap();
        reconcile_outbox(&f.store).unwrap();

        let broker = Notices::default();
        assert_eq!(notify_outbox_failures(&f.store, &broker).await.unwrap(), 2);
        let posted = broker.0.lock().unwrap().clone();
        assert!(posted.iter().any(|m| m.contains("imessage error 22")), "{posted:?}");
        assert!(posted.iter().any(|m| m.contains("may or may not")), "{posted:?}");
        assert_eq!(notify_outbox_failures(&f.store, &broker).await.unwrap(), 0);
    }
}
