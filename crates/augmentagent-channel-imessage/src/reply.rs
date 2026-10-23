//! Reply cards for inbound iMessages (#1306).
//!
//! After each bundle poll, every conversation the operator opted in with
//! `augmentagent imessage allow-inbound` whose newest entry is from the
//! other person gets triaged; a `reply` decision becomes a pending action
//! with a draft and one approval card, the same flow as Telegram. Approve
//! then queues the send (#1303). Groups, SMS/RCS and the first sync of a
//! bundle never produce cards.

use std::path::PathBuf;
use std::sync::Arc;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Email, Store, TriageResult, NUDGE_INTERVAL_MS};
use tracing::{info, warn};

use crate::bundle::synthetic_imessage_email;
use crate::sync::PollDelta;

/// Entries of context shown to triage and draft, newest last.
const CONTEXT_ENTRIES: usize = 10;

#[derive(Debug, Clone)]
pub struct ImessageReplyConfig {
    pub skill_dir: PathBuf,
    pub wiki_root: Option<PathBuf>,
}

impl Default for ImessageReplyConfig {
    fn default() -> Self {
        Self {
            skill_dir: PathBuf::from("skills/imessage-triage"),
            wiki_root: None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplyStats {
    pub own_sends: usize,
    pub manual_replies: usize,
    pub cards: usize,
    pub skipped: usize,
    pub flagged: usize,
    pub superseded: usize,
    pub errors: usize,
}

pub struct ImessageReplier {
    pub store: Arc<Store>,
    pub reasoner: Arc<dyn Reasoner>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: ImessageReplyConfig,
}

/// The card email for a delta, or `None` when no card is owed: first sync,
/// group, not iMessage, or the newest entry is the operator's own.
pub fn reply_email(delta: &PollDelta) -> Option<Email> {
    let conv = &delta.conversation;
    if delta.first_run || conv.service != "iMessage" || conv.is_group() {
        return None;
    }
    let (idx, newest) = delta.new_entries.last()?;
    if newest.sender == "me" {
        return None;
    }
    let mut email = synthetic_imessage_email(conv, *idx, newest);
    let start = delta.new_entries.len().saturating_sub(CONTEXT_ENTRIES);
    email.body = delta.new_entries[start..]
        .iter()
        .map(|(_, e)| {
            let mut s = format!("### [{}] {}\n{}", e.timestamp, e.sender, e.body);
            for a in &e.attachments {
                s.push('\n');
                s.push_str(a);
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(email)
}

enum Handled {
    Card { superseded: usize },
    Skipped,
    Flagged,
}

/// True when every new entry is the agent's own send coming back, so the
/// delta holds nothing new to learn from (#1307).
pub fn only_own_sends(store: &Store, delta: &PollDelta) -> bool {
    let (own, _) = split_own_entries(store, delta);
    own > 0 && own == delta.new_entries.len()
}

/// How close an entry's timestamp must be to an outbox row's completion for
/// the entry to be that send. The bundle carries no message guid.
const OWN_SEND_WINDOW_MS: i64 = 10 * 60_000;

/// In the self-chat, each send also arrives as an incoming copy from the
/// operator's own handle. A newest entry that matches a sent outbox row is
/// that echo, not someone writing in.
fn newest_is_echo_of_own_send(store: &Store, delta: &PollDelta) -> bool {
    let Some((_, newest)) = delta.new_entries.last() else {
        return false;
    };
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&newest.timestamp) else {
        return false;
    };
    let ts = ts.timestamp_millis();
    let conv = &delta.conversation;
    std::iter::once(&conv.identifier)
        .chain(conv.chat_guid.as_ref())
        .filter_map(|t| store.sent_imessage_outbox_for_target(t, 0).ok())
        .flatten()
        .any(|r| {
            r.body == newest.body
                && r.completed_at_ms
                    .is_some_and(|c| (ts - c).abs() <= OWN_SEND_WINDOW_MS)
        })
}

/// Split the delta's `me` entries into (agent's own sends, manual replies).
/// Each sent outbox row accounts for at most one entry.
fn split_own_entries(store: &Store, delta: &PollDelta) -> (usize, usize) {
    let mine: Vec<&crate::bundle::MessageEntry> = delta
        .new_entries
        .iter()
        .map(|(_, e)| e)
        .filter(|e| e.sender == "me")
        .collect();
    if mine.is_empty() {
        return (0, 0);
    }
    let conv = &delta.conversation;
    let mut rows = Vec::new();
    for target in std::iter::once(&conv.identifier).chain(conv.chat_guid.as_ref()) {
        match store.sent_imessage_outbox_for_target(target, 0) {
            Ok(r) => rows.extend(r),
            Err(e) => warn!("imessage outbox lookup failed: {e}"),
        }
    }
    let mut used = std::collections::HashSet::new();
    let mut own = 0;
    for entry in &mine {
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&entry.timestamp) else {
            continue;
        };
        let ts = ts.timestamp_millis();
        let hit = rows.iter().find(|r| {
            !used.contains(&r.id)
                && r.body == entry.body
                && r.completed_at_ms
                    .is_some_and(|c| (ts - c).abs() <= OWN_SEND_WINDOW_MS)
        });
        if let Some(r) = hit {
            used.insert(r.id);
            own += 1;
        }
    }
    (own, mine.len() - own)
}

impl ImessageReplier {
    pub async fn handle_deltas(&self, deltas: &[PollDelta]) -> ReplyStats {
        let mut stats = ReplyStats::default();
        for delta in deltas {
            if !delta.first_run {
                let (own, manual) = split_own_entries(&self.store, delta);
                stats.own_sends += own;
                stats.manual_replies += manual;
                if manual > 0 {
                    // The operator answered by hand; the pending draft is stale.
                    let thread = format!("imessage:{}", delta.conversation.identifier);
                    match self
                        .store
                        .mark_pending_drafts_superseded_by_thread(&thread, "superseded by manual reply")
                    {
                        Ok(ids) => stats.superseded += ids.len(),
                        Err(e) => {
                            warn!("imessage supersede on manual reply failed: {e}");
                            stats.errors += 1;
                        }
                    }
                }
            }
            let Some(email) = reply_email(delta) else {
                continue;
            };
            if newest_is_echo_of_own_send(&self.store, delta) {
                continue;
            }
            match self
                .store
                .is_imessage_inbound_allowed(&delta.conversation.identifier)
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    warn!("imessage inbound allowlist read failed: {e}");
                    stats.errors += 1;
                    continue;
                }
            }
            match self.handle_one(email).await {
                Ok(Handled::Card { superseded }) => {
                    stats.cards += 1;
                    stats.superseded += superseded;
                }
                Ok(Handled::Skipped) => stats.skipped += 1,
                Ok(Handled::Flagged) => stats.flagged += 1,
                Err(e) => {
                    warn!("imessage reply card failed: {e:#}");
                    stats.errors += 1;
                }
            }
        }
        stats
    }

    fn log(&self, email: &Email, draft: Option<&str>, status: ActionStatus) -> anyhow::Result<String> {
        Ok(self.store.log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            draft,
            status,
        )?)
    }

    async fn handle_one(&self, email: Email) -> anyhow::Result<Handled> {
        let triage = triage_opts(self.config.wiki_root.clone());
        let raw = self
            .reasoner
            .call(&triage, &triage_user_message(&email, "", ""))
            .await?;
        let decision = parse_decision(&raw)?;
        match decision.decision {
            DecisionKind::Reply => {}
            DecisionKind::Flag => {
                self.log(&email, None, ActionStatus::Flagged)?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                let reason = decision.reason.as_deref().unwrap_or("flagged");
                if let Err(e) = self.approvals.post_flag_notice(&email, reason).await {
                    warn!("imessage flag notice failed: {e}");
                }
                return Ok(Handled::Flagged);
            }
            _ => {
                self.log(&email, None, ActionStatus::Skipped)?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                return Ok(Handled::Skipped);
            }
        }

        let skill = std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
            .unwrap_or_default();
        let draft_opts = draft_opts(skill, self.config.wiki_root.clone());
        let drafted = self
            .reasoner
            .call(&draft_opts, &draft_user_message(&email, "", "", "", "", ""))
            .await?
            .trim()
            .to_string();
        if drafted.is_empty() {
            anyhow::bail!("draft came back empty");
        }
        // One live card per conversation: the newer message replaces it.
        let thread = email.thread_id.clone().unwrap_or_default();
        let superseded = self
            .store
            .mark_pending_drafts_superseded_by_thread(&thread, "superseded by a newer iMessage")?
            .len();
        let action_id = self.log(&email, Some(&drafted), ActionStatus::Pending)?;
        if let Err(e) = self.approvals.post_approval(&action_id, &email, &drafted).await {
            self.store.update_action_status(
                &action_id,
                ActionStatus::Error,
                None,
                Some(&format!("post_approval: {e}")),
            )?;
            anyhow::bail!("post_approval: {e}");
        }
        if let Err(e) = self
            .store
            .record_nudge(&action_id, chrono::Utc::now().timestamp_millis() + NUDGE_INTERVAL_MS)
        {
            warn!(action_id, "record_nudge after post_approval failed: {e}");
        }
        info!(action_id, "imessage approval card posted");
        Ok(Handled::Card { superseded })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Conversation, MessageEntry};
    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::reasoner::ReasonerOpts;
    use std::sync::Mutex;

    const PHONE: &str = "+15555550100"; // pii-ok synthetic

    struct Scripted(Mutex<std::collections::VecDeque<String>>, Mutex<usize>);

    impl Scripted {
        fn new(r: &[&str]) -> Arc<Self> {
            Arc::new(Self(
                Mutex::new(r.iter().map(|s| s.to_string()).collect()),
                Mutex::new(0),
            ))
        }
        fn calls(&self) -> usize {
            *self.1.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl Reasoner for Scripted {
        async fn call(&self, _o: &ReasonerOpts, _m: &str) -> anyhow::Result<String> {
            *self.1.lock().unwrap() += 1;
            Ok(self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"decision":"skip","reason":"stub"}"#.into()))
        }
    }

    #[derive(Default)]
    struct Broker {
        cards: Mutex<Vec<(String, String)>>,
        flags: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl ApprovalBroker for Broker {
        async fn post_approval(&self, id: &str, _e: &Email, d: &str) -> Result<(), ApprovalError> {
            self.cards.lock().unwrap().push((id.into(), d.into()));
            Ok(())
        }
        async fn post_flag_notice(&self, _e: &Email, _r: &str) -> Result<(), ApprovalError> {
            *self.flags.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn conv(identifier: &str, service: &str, participants: &[&str]) -> Conversation {
        Conversation {
            identifier: identifier.into(),
            dir: "d".into(),
            title: "Alex".into(),
            participants: participants.iter().map(|s| s.to_string()).collect(),
            service: service.into(),
            chat_guid: None,
            newest_entry: None,
        }
    }

    fn entry(sender: &str, body: &str) -> MessageEntry {
        MessageEntry {
            timestamp: "2026-09-29T12:00:00-04:00".into(),
            sender: sender.into(),
            body: body.into(),
            attachments: vec![],
        }
    }

    fn delta(c: Conversation, entries: &[(&str, &str)], first_run: bool) -> PollDelta {
        PollDelta {
            conversation: c,
            new_entries: entries
                .iter()
                .enumerate()
                .map(|(i, (s, b))| (i + 3, entry(s, b)))
                .collect(),
            first_run,
        }
    }

    fn one_to_one(entries: &[(&str, &str)]) -> PollDelta {
        delta(conv(PHONE, "iMessage", &[PHONE]), entries, false)
    }

    struct Fx {
        store: Arc<Store>,
        broker: Arc<Broker>,
        _dir: tempfile::TempDir,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("data.db")).unwrap());
        Fx {
            store,
            broker: Arc::new(Broker::default()),
            _dir: dir,
        }
    }

    fn replier(f: &Fx, reasoner: Arc<Scripted>) -> ImessageReplier {
        ImessageReplier {
            store: Arc::clone(&f.store),
            reasoner,
            approvals: f.broker.clone(),
            config: ImessageReplyConfig::default(),
        }
    }

    /// Mirrors what `poll_once` has already stored before replies run.
    fn ingest(store: &Store, d: &PollDelta) {
        for (idx, e) in &d.new_entries {
            store
                .upsert_email(&synthetic_imessage_email(&d.conversation, *idx, e))
                .unwrap();
        }
    }

    const REPLY: &str = r#"{"decision":"reply","reason":"question"}"#;

    fn pending(store: &Store) -> Vec<String> {
        store
            .with_conn(|c| {
                let mut s = c.prepare("SELECT id FROM actions WHERE status = 'pending'")?;
                let rows = s.query_map([], |r| r.get(0))?;
                rows.collect()
            })
            .unwrap()
    }

    #[tokio::test]
    async fn allowlisted_inbound_message_posts_exactly_one_card() {
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        let d = one_to_one(&[(PHONE, "free tonight?")]);
        ingest(&f.store, &d);
        let r = Scripted::new(&[REPLY, "yes! what time?"]);
        let stats = replier(&f, r).handle_deltas(&[d]).await;
        assert_eq!(stats.cards, 1);
        let cards = f.broker.cards.lock().unwrap().clone();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].1, "yes! what time?");
        let a = f.store.get_action_with_email(&cards[0].0).unwrap().unwrap();
        assert_eq!(a.action.status, "pending");
        assert_eq!(a.action.draft_body.as_deref(), Some("yes! what time?"));
        assert_eq!(a.email.platform, "imessage");
        assert_eq!(a.email.thread_id.as_deref(), Some(&*format!("imessage:{PHONE}")));
    }

    #[tokio::test]
    async fn conversation_not_on_inbound_allowlist_costs_nothing() {
        let f = fx();
        let d = one_to_one(&[(PHONE, "free tonight?")]);
        ingest(&f.store, &d);
        let r = Scripted::new(&[REPLY, "x"]);
        let stats = replier(&f, r.clone()).handle_deltas(&[d]).await;
        assert_eq!(stats, ReplyStats::default());
        assert_eq!(r.calls(), 0);
        assert!(f.broker.cards.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn newest_entry_from_me_posts_no_card() {
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        let d = one_to_one(&[(PHONE, "free tonight?"), ("me", "yes")]);
        ingest(&f.store, &d);
        let r = Scripted::new(&[REPLY, "x"]);
        replier(&f, r.clone()).handle_deltas(&[d]).await;
        assert_eq!(r.calls(), 0);
        assert!(f.broker.cards.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_delta_list_posts_nothing() {
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        let r = Scripted::new(&[REPLY, "x"]);
        let stats = replier(&f, r.clone()).handle_deltas(&[]).await;
        assert_eq!(stats, ReplyStats::default());
    }

    #[tokio::test]
    async fn new_message_supersedes_the_pending_card_on_the_thread() {
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        let d1 = one_to_one(&[(PHONE, "free tonight?")]);
        ingest(&f.store, &d1);
        let r = Scripted::new(&[REPLY, "yes", REPLY, "yes, 8?"]);
        let rep = replier(&f, r);
        rep.handle_deltas(&[d1]).await;
        let first = pending(&f.store);
        assert_eq!(first.len(), 1);
        let mut d2 = one_to_one(&[(PHONE, "actually tomorrow?")]);
        d2.new_entries[0].0 = 9;
        ingest(&f.store, &d2);
        let stats = rep.handle_deltas(&[d2]).await;
        assert_eq!(stats.superseded, 1);
        let now = pending(&f.store);
        assert_eq!(now.len(), 1);
        assert_ne!(now[0], first[0]);
        let old = f.store.get_action_with_email(&first[0]).unwrap().unwrap();
        assert_eq!(old.action.status, "superseded");
    }

    #[tokio::test]
    async fn groups_sms_rcs_and_first_sync_never_draft() {
        let f = fx();
        for id in [PHONE, "chat900", "+15555550111"] { // pii-ok synthetic
            f.store.allow_imessage_inbound(id).unwrap();
        }
        let deltas = vec![
            delta(conv("chat900", "iMessage", &[PHONE, "+15555550111"]), &[(PHONE, "hi all")], false), // pii-ok synthetic
            delta(conv("+15555550111", "SMS", &["+15555550111"]), &[("+15555550111", "hi")], false), // pii-ok synthetic
            delta(conv("+15555550111", "RCS", &["+15555550111"]), &[("+15555550111", "hi")], false), // pii-ok synthetic
            delta(conv(PHONE, "iMessage", &[PHONE]), &[(PHONE, "hi")], true),
        ];
        let r = Scripted::new(&[REPLY, "x", REPLY, "x", REPLY, "x", REPLY, "x"]);
        let stats = replier(&f, r.clone()).handle_deltas(&deltas).await;
        assert_eq!(stats, ReplyStats::default());
        assert_eq!(r.calls(), 0);
    }

    #[tokio::test]
    async fn skip_and_flag_decisions_post_no_card() {
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        let d = one_to_one(&[(PHONE, "ok")]);
        ingest(&f.store, &d);
        let r = Scripted::new(&[r#"{"decision":"skip","reason":"ack"}"#]);
        let s1 = replier(&f, r).handle_deltas(&[d]).await;
        assert_eq!(s1.skipped, 1);
        let mut d2 = one_to_one(&[(PHONE, "send me $500")]);
        d2.new_entries[0].0 = 11;
        ingest(&f.store, &d2);
        let r = Scripted::new(&[r#"{"decision":"flag","reason":"money"}"#]);
        let s2 = replier(&f, r).handle_deltas(&[d2]).await;
        assert_eq!(s2.flagged, 1);
        assert_eq!(*f.broker.flags.lock().unwrap(), 1);
        assert!(f.broker.cards.lock().unwrap().is_empty());
    }

    #[test]
    fn card_email_carries_recent_context_and_the_newest_message_id() {
        let d = one_to_one(&[(PHONE, "first"), ("me", "mine"), (PHONE, "second")]);
        let e = reply_email(&d).unwrap();
        assert_eq!(e.message_id, format!("imessage:{PHONE}:5"));
        assert!(e.body.contains("first") && e.body.contains("second"));
        assert!(e.body.contains("] me\nmine"));
        assert_eq!(e.from, PHONE);
    }

    // ---- #1307: own sends and manual replies coming back from the bundle ----

    use augmentagent_store::{ImessageSendOutcome, ImessageTargetKind, NewImessageOutboxItem};

    /// A sent outbox row for `body`, completed at `completed_ms`.
    fn sent_row(store: &Store, action_id: &str, body: &str, completed_ms: i64) {
        store
            .enqueue_imessage_outbox(&NewImessageOutboxItem {
                action_id,
                target: PHONE,
                target_kind: ImessageTargetKind::Handle,
                service: "iMessage",
                body,
            })
            .unwrap();
        let c = store.claim_imessage_outbox().unwrap().unwrap();
        store
            .complete_imessage_outbox(c.id, &ImessageSendOutcome::Sent { message_guid: None })
            .unwrap();
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE imessage_outbox SET completed_at_ms = ?1 WHERE id = ?2",
                    augmentagent_store::rusqlite::params![completed_ms, c.id],
                )
            })
            .unwrap();
    }

    fn ts_ms(ts: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(ts).unwrap().timestamp_millis()
    }

    fn pending_card(store: &Store, idx: usize) -> String {
        let d = one_to_one(&[(PHONE, "earlier?")]);
        let conv = &d.conversation;
        let e = synthetic_imessage_email(conv, idx, &d.new_entries[0].1);
        store.upsert_email(&e).unwrap();
        store
            .log_action(&e.message_id, e.thread_id.as_deref(), &e.from, &e.subject,
                        Some(&e.body), Some("draft"), ActionStatus::Pending)
            .unwrap()
    }

    const TS: &str = "2026-09-29T12:00:00-04:00";

    #[tokio::test]
    async fn own_sent_message_is_not_treated_as_new_inbound() {
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        let card = pending_card(&f.store, 1);
        sent_row(&f.store, "sent-action", "on my way", ts_ms(TS) + 2_000);
        let d = one_to_one(&[("me", "on my way")]);
        ingest(&f.store, &d);
        let r = Scripted::new(&[REPLY, "x"]);
        let stats = replier(&f, r.clone()).handle_deltas(&[d]).await;
        assert_eq!(stats.own_sends, 1);
        assert_eq!(stats.manual_replies, 0);
        assert_eq!(stats.cards, 0);
        assert_eq!(r.calls(), 0);
        let a = f.store.get_action_with_email(&card).unwrap().unwrap();
        assert_eq!(a.action.status, "pending", "own send must not retire other cards");
    }

    #[tokio::test]
    async fn same_body_outside_the_window_is_a_manual_reply() {
        let f = fx();
        let card = pending_card(&f.store, 1);
        sent_row(&f.store, "sent-action", "on my way", ts_ms(TS) - 11 * 60_000);
        let d = one_to_one(&[("me", "on my way")]);
        ingest(&f.store, &d);
        let stats = replier(&f, Scripted::new(&[])).handle_deltas(&[d]).await;
        assert_eq!((stats.own_sends, stats.manual_replies), (0, 1));
        let a = f.store.get_action_with_email(&card).unwrap().unwrap();
        assert_eq!(a.action.status, "superseded");
    }

    #[tokio::test]
    async fn manual_reply_supersedes_pending_card_with_reason() {
        let f = fx();
        let card = pending_card(&f.store, 1);
        let d = one_to_one(&[("me", "typed it myself")]);
        ingest(&f.store, &d);
        let stats = replier(&f, Scripted::new(&[])).handle_deltas(&[d]).await;
        assert_eq!(stats.manual_replies, 1);
        let a = f.store.get_action_with_email(&card).unwrap().unwrap();
        assert_eq!(a.action.status, "superseded");
        assert_eq!(a.action.error_message.as_deref(), Some("superseded by manual reply"));
    }

    #[tokio::test]
    async fn manual_reply_leaves_a_sending_action_alone() {
        let f = fx();
        let card = pending_card(&f.store, 1);
        f.store
            .claim_action_for_send(&card, ActionStatus::Pending, "t")
            .unwrap();
        let d = one_to_one(&[("me", "typed it myself")]);
        ingest(&f.store, &d);
        replier(&f, Scripted::new(&[])).handle_deltas(&[d]).await;
        let a = f.store.get_action_with_email(&card).unwrap().unwrap();
        assert_eq!(a.action.status, "sending");
    }

    #[tokio::test]
    async fn one_outbox_row_matches_one_entry_only() {
        let f = fx();
        sent_row(&f.store, "sent-action", "ok", ts_ms(TS));
        let d = one_to_one(&[("me", "ok"), ("me", "ok")]);
        ingest(&f.store, &d);
        let stats = replier(&f, Scripted::new(&[])).handle_deltas(&[d]).await;
        assert_eq!((stats.own_sends, stats.manual_replies), (1, 1));
    }

    #[tokio::test]
    async fn polling_the_same_bundle_twice_changes_nothing_the_second_time() {
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        let root = f._dir.path().join("bundle");
        let conv_dir = root.join("conversations").join("alex");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::write(
            root.join("conversations/index.json"),
            serde_json::json!({PHONE: {"identifier": PHONE, "dir": "alex", "title": "Alex",
                                       "participants": [PHONE], "service": "iMessage"}})
                .to_string(),
        )
        .unwrap();
        let md = |entries: &str| format!("---\ntitle: Alex\n---\n{entries}");
        std::fs::write(conv_dir.join("messages.md"),
                       md("### [2026-09-29T11:00:00-04:00] +15555550100\nhello\n")).unwrap(); // pii-ok
        let bundle = crate::Bundle::open(&root);
        crate::poll_once(&bundle, &f.store).unwrap(); // first sync: history only
        std::fs::write(conv_dir.join("messages.md"), md(
            "### [2026-09-29T11:00:00-04:00] +15555550100\nhello\n\n\
             ### [2026-09-29T12:00:00-04:00] +15555550100\nfree tonight?\n")).unwrap(); // pii-ok
        let rep = replier(&f, Scripted::new(&[REPLY, "yes"]));
        let (_, d1) = crate::poll_once(&bundle, &f.store).unwrap();
        assert_eq!(rep.handle_deltas(&d1).await.cards, 1);
        let snapshot = |s: &Store| -> Vec<(String, String)> {
            s.with_conn(|c| {
                let mut st = c.prepare("SELECT id, status FROM actions ORDER BY id")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect()
            })
            .unwrap()
        };
        let emails = |s: &Store| -> i64 {
            s.with_conn(|c| c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get(0))).unwrap()
        };
        let (before, n_before) = (snapshot(&f.store), emails(&f.store));
        let (_, d2) = crate::poll_once(&bundle, &f.store).unwrap();
        assert_eq!(rep.handle_deltas(&d2).await, ReplyStats::default());
        assert_eq!(snapshot(&f.store), before);
        assert_eq!(emails(&f.store), n_before);
    }

    #[tokio::test]
    async fn self_chat_echo_of_an_agent_send_posts_no_card() {
        // In the self-chat every send also arrives as an incoming copy from
        // the operator's own handle; it must not trigger a reply to itself.
        let f = fx();
        f.store.allow_imessage_inbound(PHONE).unwrap();
        sent_row(&f.store, "sent-action", "on my way", ts_ms(TS));
        let d = one_to_one(&[("me", "on my way"), (PHONE, "on my way")]);
        ingest(&f.store, &d);
        let r = Scripted::new(&[REPLY, "x"]);
        let stats = replier(&f, r.clone()).handle_deltas(&[d]).await;
        assert_eq!(r.calls(), 0);
        assert_eq!(stats.cards, 0);
    }

    #[test]
    fn delta_of_only_own_sends_is_recognised_for_capture_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("d.db")).unwrap();
        sent_row(&store, "a", "on my way", ts_ms(TS));
        let own = one_to_one(&[("me", "on my way")]);
        let mixed = one_to_one(&[("me", "on my way"), (PHONE, "great")]);
        assert!(only_own_sends(&store, &own));
        assert!(!only_own_sends(&store, &mixed));
    }
}
