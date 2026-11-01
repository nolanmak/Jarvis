use anyhow::{Context, Result};
use augmentagent_channel_core::{
    decision::DecisionKind,
    ingest::{spawn_ingest, IngestTrigger},
    reasoner::Reasoner,
};
use augmentagent_channel_whatsapp_history::{self as history, Config};
use augmentagent_store::Store;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(clap::Subcommand)]
pub enum Op {
    /// Pull an optional Git feed and import searchable history once; no LLM calls.
    PollOnce,
    /// Owner policy for existing and future WhatsApp chats, groups included
    /// (#1763). Every reply still needs Approve; explicit denials win.
    ContactDefaults {
        #[arg(long, required = true, action = clap::ArgAction::Set)]
        inbound: bool,
        #[arg(long, required = true, action = clap::ArgAction::Set)]
        outbound: bool,
    },
    /// Draft reply cards for a chat (its JID), lifting any denial.
    AllowInbound { jid: String },
    /// Never draft reply cards for a chat, even while defaults are on.
    DenyInbound { jid: String },
    /// Allow approved replies for a chat, lifting any denial.
    AllowOutbound { jid: String },
    /// Refuse approved replies for a chat, even while defaults are on.
    DenyOutbound { jid: String },
    /// Print defaults, explicit entries and denials for both directions.
    Allowlist,
}

/// #1765 — Approve on a WhatsApp archive card. Nothing is sent: the action
/// becomes `approved` with `MANUAL_SEND_PENDING`, the owner gets the text to
/// paste, and the archive later confirms it (`reply::WhatsappReplier`).
pub fn approve(
    store: &Store,
    action_id: &str,
    action: &augmentagent_store::ActionWithEmail,
) -> augmentagent_approval_discord::ApprovalActionOutcome {
    use augmentagent_approval_discord::ApprovalActionOutcome as Outcome;
    let Some(draft) = action.action.draft_body.as_deref() else {
        return Outcome::Failed { message: "no draft body on action; nothing to send".into() };
    };
    let text = augmentagent_approval_discord::strip_assumes_for_send(draft);
    if text.trim().is_empty() {
        return Outcome::Failed { message: "draft is empty after removing card markers".into() };
    }
    match store.mark_manual_send_pending(action_id) {
        Ok(true) => {}
        Ok(false) => {
            return match store.get_action_with_email(action_id) {
                Ok(Some(row)) => Outcome::AlreadyResolved {
                    status: row.action.status,
                    detail: row.action.error_message,
                },
                _ => Outcome::NotFound,
            }
        }
        Err(e) => return Outcome::Failed { message: format!("recording the approval failed: {e}") },
    }
    // Keep the text the owner will paste on the row; confirmation matches it.
    if let Err(e) = store.with_conn(|c| {
        c.execute("UPDATE actions SET draftBody = ?2 WHERE id = ?1", augmentagent_store::rusqlite::params![action_id, text])
    }) {
        warn!(action_id, "storing the paste-ready text failed: {e}");
    }
    Outcome::ManualSendReady {
        link: action.email.thread_id.as_deref().and_then(history::reply::chat_link),
        text,
    }
}

/// Owner-only policy commands (#1763). Never on a reasoner allowlist.
pub fn policy_command(store: &Store, op: &Op) -> Result<()> {
    let checked = |jid: &str| -> Result<String> {
        let jid = jid.trim();
        anyhow::ensure!(
            augmentagent_store::whatsapp_default_coverable(jid),
            "{jid:?} is not a WhatsApp person or group JID (<digits>@s.whatsapp.net, <digits>@lid, <id>@g.us)"
        );
        Ok(jid.to_string())
    };
    match op {
        Op::PollOnce => unreachable!("poll-once is async; see poll_command"),
        Op::ContactDefaults { inbound, outbound } => {
            store.set_whatsapp_contact_defaults(*inbound, *outbound)?
        }
        Op::AllowInbound { jid } => store.allow_whatsapp_inbound(&checked(jid)?)?,
        Op::DenyInbound { jid } => store.deny_whatsapp_inbound(&checked(jid)?)?,
        Op::AllowOutbound { jid } => store.allow_whatsapp_outbound(&checked(jid)?)?,
        Op::DenyOutbound { jid } => store.deny_whatsapp_outbound(&checked(jid)?)?,
        Op::Allowlist => {}
    }
    print!("{}", render_policy(store)?);
    Ok(())
}

pub fn render_policy(store: &Store) -> Result<String> {
    let mut out = String::new();
    for (label, outbound) in [("inbound", false), ("outbound", true)] {
        let default = if store.whatsapp_contact_default(outbound)? {
            "all WhatsApp chats, groups included (approval required for replies)"
        } else {
            "explicit allowlist only"
        };
        out.push_str(&format!("{label} defaults\t{default}\n"));
        let (entries, denials) = store.list_whatsapp_policy(outbound)?;
        for jid in entries {
            out.push_str(&format!("{label}\t{jid}\n"));
        }
        for jid in denials {
            out.push_str(&format!("{label} denied\t{jid}\n"));
        }
    }
    Ok(out)
}

async fn poll(config: &Config, store: Arc<Store>) -> Result<history::Report> {
    if let Err(e) = history::refresh(config).await {
        warn!("{e:#}; reading the last on-disk WhatsApp bundle");
    }
    let directory = config.directory.clone();
    tokio::task::spawn_blocking(move || history::poll_once(&directory, &store)).await?
}

pub async fn poll_command(store: Arc<Store>) -> Result<()> {
    let config = Config::load()?.context("AUGMENTAGENT_WHATSAPP_HISTORY_DIR is required")?;
    let report = poll(&config, store).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

pub async fn run_loop<R: Reasoner + 'static>(
    config: Config,
    store: Arc<Store>,
    reasoner: Arc<R>,
    broker: Arc<dyn augmentagent_approval_discord::ApprovalBroker>,
    wiki_root: Option<PathBuf>,
    wiki_schema: Option<String>,
    shutdown: CancellationToken,
) -> Result<()> {
    let wiki_capture = augmentagent_channel_imessage::history_wiki_capture_enabled();
    let poll_interval = history::poll_interval();
    let mut interval = tokio::time::interval(poll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // #1764 — reply cards for chats the owner's policy covers.
    let replier = history::reply::WhatsappReplier::new(
        Arc::clone(&store),
        Arc::clone(&reasoner) as Arc<dyn Reasoner>,
        broker,
        wiki_root.clone(),
    );
    info!(
        wiki_capture,
        poll_secs = poll_interval.as_secs(),
        "WhatsApp history poller started"
    );
    loop {
        tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = interval.tick() => {} }
        let report = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            result = poll(&config, Arc::clone(&store)) => match result {
                Ok(report) => report,
                Err(e) => { warn!("WhatsApp history poll failed: {e:#}"); continue; }
            }
        };
        info!(
            inserted = report.inserted,
            conversations = report.conversations_with_new,
            skipped = report.skipped,
            "WhatsApp history poll complete"
        );
        let replies = replier.handle_deltas(&report.deltas).await;
        if replies != Default::default() {
            info!(?replies, "WhatsApp reply cards");
        }
        if !wiki_capture {
            continue;
        }
        let (Some(root), Some(schema)) = (&wiki_root, &wiki_schema) else {
            continue;
        };
        for delta in report.deltas.iter().filter(|d| !d.first_run) {
            spawn_ingest(
                Arc::clone(&reasoner),
                root.clone(),
                schema.clone(),
                history::capture_email(delta),
                DecisionKind::Capture,
                Some("WhatsApp history sync".into()),
                None,
                IngestTrigger::WhatsappHistory,
            );
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn whatsapp_history_allowlist_prints_defaults_entries_and_denials() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("d.db")).unwrap();
        let group = "120363001234567890@g.us";
        policy_command(&store, &Op::ContactDefaults { inbound: true, outbound: false }).unwrap();
        policy_command(&store, &Op::DenyInbound { jid: group.into() }).unwrap();
        policy_command(&store, &Op::AllowOutbound { jid: "14155550123@s.whatsapp.net".into() }).unwrap(); // pii-ok synthetic
        let text = render_policy(&store).unwrap();
        assert!(text.contains("inbound defaults\tall WhatsApp chats, groups included"), "{text}");
        assert!(text.contains("outbound defaults\texplicit allowlist only"), "{text}");
        assert!(text.contains(&format!("inbound denied\t{group}")), "{text}");
        assert!(text.contains("outbound\t14155550123@s.whatsapp.net"), "{text}"); // pii-ok synthetic
        assert!(!store.is_whatsapp_inbound_allowed(group).unwrap());
    }

    #[test]
    fn policy_commands_refuse_broadcast_and_malformed_jids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("d.db")).unwrap();
        for jid in ["status@broadcast", "1@newsletter", "Ski Trip", ""] {
            let err = policy_command(&store, &Op::AllowInbound { jid: jid.into() }).unwrap_err();
            assert!(err.to_string().contains("not a WhatsApp person or group JID"), "{err}");
        }
        assert_eq!(store.list_whatsapp_policy(false).unwrap(), (vec![], vec![]));
    }
}

#[cfg(test)]
mod approve_tests {
    use super::*;
    use augmentagent_approval_discord::ApprovalActionOutcome;
    use augmentagent_store::ActionStatus;

    const DM: &str = "14155550123@s.whatsapp.net"; // pii-ok synthetic
    const GROUP: &str = "120363001234567890@g.us";

    fn seed(store: &Store, jid: &str, draft: &str) -> String {
        let email = augmentagent_store::Email {
            message_id: format!("{}{jid}:3", history::THREAD_PREFIX),
            thread_id: Some(format!("{}{jid}", history::THREAD_PREFIX)),
            from: "Jane <+14155550123>".into(), // pii-ok synthetic
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: "WhatsApp: Jane".into(),
            body: "dinner?".into(),
            date: "2026-11-01T12:00:00-04:00".into(),
            account_entity_id: Some(history::ACCOUNT.into()),
            platform: "whatsapp".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&email).unwrap();
        store
            .log_action(&email.message_id, email.thread_id.as_deref(), &email.from, &email.subject,
                        Some(&email.body), Some(draft), ActionStatus::Pending)
            .unwrap()
    }

    fn fixture() -> (Arc<Store>, crate::ReplyApprover, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("d.db")).unwrap());
        let approver = crate::test_support::approver_with_store(Arc::clone(&store));
        (store, approver, dir)
    }

    #[tokio::test]
    async fn approve_history_card_has_no_side_effects_and_shows_copyable_text() {
        let (store, approver, _d) = fixture();
        let id = seed(&store, DM, "yes, 7 works");
        let outbox_before: i64 = store.with_conn(|c| c.query_row("SELECT COUNT(*) FROM imessage_outbox", [], |r| r.get(0))).unwrap();
        // Routed through the real Approve ladder: never Gmail, never the sidecar.
        let out = approver.run_approve(&id).await;
        let ApprovalActionOutcome::ManualSendReady { text, link } = out else { panic!("{out:?}") };
        assert_eq!(text, "yes, 7 works");
        assert_eq!(link.as_deref(), Some("https://wa.me/14155550123")); // pii-ok synthetic
        let row = store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(row.action.status, "approved");
        assert_eq!(row.action.error_message.as_deref(), Some(augmentagent_store::MANUAL_SEND_PENDING));
        let outbox_after: i64 = store.with_conn(|c| c.query_row("SELECT COUNT(*) FROM imessage_outbox", [], |r| r.get(0))).unwrap();
        assert_eq!(outbox_before, outbox_after);
        let described = augmentagent_approval_discord::outcome::describe(&ApprovalActionOutcome::ManualSendReady { text, link });
        assert!(described.contains("not sent automatically") && described.contains("```\nyes, 7 works\n```"), "{described}");
        // A second click never re-approves.
        assert!(matches!(approver.run_approve(&id).await, ApprovalActionOutcome::AlreadyResolved { .. }));
    }

    #[tokio::test]
    async fn approve_strips_assume_markers_and_group_cards_have_no_link() {
        let (store, approver, _d) = fixture();
        let draft = augmentagent_approval_discord::append_assumes_marker("See you all there.", &["Friday".to_string()]);
        let id = seed(&store, GROUP, &draft);
        let ApprovalActionOutcome::ManualSendReady { text, link } = approver.run_approve(&id).await else { panic!() };
        assert_eq!(text.trim(), "See you all there.");
        assert_eq!(link, None);
        let row = store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(row.action.draft_body.as_deref().map(str::trim), Some("See you all there."), "stored text is what gets matched");
    }

    #[tokio::test]
    async fn live_whatsapp_rows_never_take_the_history_path() {
        let (store, approver, _d) = fixture();
        let id = seed(&store, DM, "hi");
        store.with_conn(|c| c.execute("UPDATE emails SET accountEntityId = 'whatsapp:+14155550000'", [])).unwrap(); // pii-ok synthetic
        let out = approver.run_approve(&id).await;
        assert!(!matches!(out, ApprovalActionOutcome::ManualSendReady { .. }), "{out:?}");
        assert_ne!(store.get_action_with_email(&id).unwrap().unwrap().action.error_message.as_deref(), Some(augmentagent_store::MANUAL_SEND_PENDING));
    }
}
