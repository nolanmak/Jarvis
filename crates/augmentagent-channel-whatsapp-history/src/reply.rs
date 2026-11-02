//! Reply cards for the WhatsApp archive (#1764), and confirming the replies
//! the owner pastes by hand (#1765).
//!
//! After each archive poll, every chat the owner's policy covers (#1763) whose
//! newest entry is from someone else gets triaged, and a `reply` becomes one
//! approval card. Triage, draft and card come from the shared
//! [`CardDrafter`] used by iMessage, so groups get the same higher bar
//! ([`GROUP_TRIAGE_POLICY`](augmentagent_channel_imessage::GROUP_TRIAGE_POLICY)).
//!
//! There is no WhatsApp sender. Approve marks the card `approved` with
//! `MANUAL_SEND_PENDING` and hands the owner the text; when a `me` entry
//! with that text arrives in the archive the action becomes `sent`. Any other
//! `me` entry is a manual reply and retires the pending card.
//!
//! Never drafted: first sync of a chat, the owner's own newest entry,
//! media-only entries, groups the owner left, and catch-up older than
//! [`ENV_MAX_AGE_HOURS`] — WhatsApp Desktop only receives while it runs, so
//! a sync after it was closed can deliver days-old messages.

use std::sync::Arc;
use std::time::Duration;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::Reasoner;
use augmentagent_channel_imessage::{
    context_body, CardDrafter, Handled, ImessageReplyConfig, MessageEntry, ReplyStats,
};
use augmentagent_store::{Email, Store};
use tracing::{info, warn};

use crate::{display_sender, email, is_group, Delta, THREAD_PREFIX};

pub const ENV_MAX_AGE_HOURS: &str = "AUGMENTAGENT_WHATSAPP_REPLY_MAX_AGE_HOURS";
pub const ENV_MANUAL_SEND_WINDOW_MINS: &str = "AUGMENTAGENT_WHATSAPP_MANUAL_SEND_WINDOW_MINS";
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
const DEFAULT_MANUAL_SEND_WINDOW: Duration = Duration::from_secs(120 * 60);
/// Clock skew allowed between the Mac's message time and the daemon's
/// approval time when matching a pasted reply.
const SKEW_MS: i64 = 5 * 60_000;

fn env_duration(name: &str, unit_secs: u64, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(|n| Duration::from_secs(n * unit_secs))
        .unwrap_or(default)
}

pub fn max_age() -> Duration {
    env_duration(ENV_MAX_AGE_HOURS, 3600, DEFAULT_MAX_AGE)
}

pub fn manual_send_window() -> Duration {
    env_duration(ENV_MANUAL_SEND_WINDOW_MINS, 60, DEFAULT_MANUAL_SEND_WINDOW)
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn entry_ms(entry: &MessageEntry) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// The card email for a delta at `now_ms`, or `None` when no card is owed.
pub fn card_email(delta: &Delta, now_ms: i64, max_age: Duration) -> Option<Email> {
    let conv = &delta.conversation;
    if delta.first_run || conv.active == Some(false) {
        return None;
    }
    let (idx, newest) = delta.entries.last()?;
    if newest.sender == "me" || newest.body.trim().is_empty() {
        return None;
    }
    let age_ms = now_ms - entry_ms(newest)?;
    if age_ms > max_age.as_millis() as i64 {
        return None;
    }
    let mut card = email(conv, *idx, newest);
    let shown: Vec<(usize, MessageEntry)> = delta
        .entries
        .iter()
        .map(|(i, e)| {
            let mut e = e.clone();
            e.sender = display_sender(conv, &e.sender);
            (*i, e)
        })
        .collect();
    card.body = context_body(&shown);
    if is_group(conv) {
        card.subject = format!("WhatsApp group: {}", conv.title);
    } else {
        card.subject = format!("WhatsApp: {}", conv.title);
    }
    Some(card)
}

pub struct WhatsappReplier {
    pub store: Arc<Store>,
    pub reasoner: Arc<dyn Reasoner>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: ImessageReplyConfig,
    pub max_age: Duration,
    pub manual_send_window: Duration,
}

impl WhatsappReplier {
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<dyn Reasoner>,
        approvals: Arc<dyn ApprovalBroker>,
        wiki_root: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            store,
            reasoner,
            approvals,
            config: ImessageReplyConfig {
                skill_dir: "skills/whatsapp-triage".into(),
                wiki_root,
            },
            max_age: max_age(),
            manual_send_window: manual_send_window(),
        }
    }

    /// Confirm pasted replies and retire cards the owner answered by hand.
    /// Returns `(own sends, manual replies)`.
    fn reconcile_own_entries(&self, delta: &Delta, thread: &str) -> anyhow::Result<(usize, usize)> {
        let mine: Vec<&MessageEntry> = delta
            .entries
            .iter()
            .map(|(_, e)| e)
            .filter(|e| e.sender == "me")
            .collect();
        if mine.is_empty() {
            return Ok((0, 0));
        }
        let mut pending = self.store.manual_sends_pending_for_thread(thread)?;
        let window = self.manual_send_window.as_millis() as i64;
        let (mut own, mut manual) = (0, 0);
        for entry in mine {
            let at = entry_ms(entry);
            let hit = pending.iter().position(|(_, text, approved_ms)| {
                normalized(text) == normalized(&entry.body)
                    && at.is_some_and(|at| at >= approved_ms - SKEW_MS && at <= approved_ms + window)
            });
            match hit {
                Some(i) => {
                    let (id, _, _) = pending.remove(i);
                    if self.store.confirm_manual_send(&id)? {
                        info!(action_id = %id, "whatsapp manual send confirmed from the archive");
                    }
                    own += 1;
                }
                None => manual += 1,
            }
        }
        Ok((own, manual))
    }

    pub async fn handle_deltas(&self, deltas: &[Delta]) -> ReplyStats {
        self.handle_deltas_at(deltas, chrono::Utc::now().timestamp_millis()).await
    }

    pub async fn handle_deltas_at(&self, deltas: &[Delta], now_ms: i64) -> ReplyStats {
        let mut stats = ReplyStats::default();
        for delta in deltas {
            if delta.first_run {
                continue;
            }
            let thread = format!("{THREAD_PREFIX}{}", delta.conversation.identifier);
            match self.reconcile_own_entries(delta, &thread) {
                Ok((own, manual)) => {
                    stats.own_sends += own;
                    stats.manual_replies += manual;
                    if manual > 0 {
                        match self.store.mark_pending_drafts_superseded_by_thread_except_kind(
                            &thread,
                            "superseded by manual reply",
                            augmentagent_channel_imessage::reply::COMPOSE_KIND,
                        ) {
                            Ok(ids) => stats.superseded += ids.len(),
                            Err(e) => {
                                warn!("whatsapp supersede on manual reply failed: {e:#}");
                                stats.errors += 1;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("whatsapp own-entry reconcile failed: {e:#}");
                    stats.errors += 1;
                }
            }
            let Some(card) = card_email(delta, now_ms, self.max_age) else {
                continue;
            };
            match self.store.is_whatsapp_inbound_allowed(&delta.conversation.identifier) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    warn!("whatsapp inbound policy read failed: {e:#}");
                    stats.errors += 1;
                    continue;
                }
            }
            let drafter = CardDrafter {
                store: &self.store,
                reasoner: self.reasoner.as_ref(),
                approvals: self.approvals.as_ref(),
                config: &self.config,
            };
            match drafter
                .draft(card, is_group(&delta.conversation), "superseded by a newer WhatsApp message")
                .await
            {
                Ok(Handled::Card { superseded }) => {
                    stats.cards += 1;
                    stats.superseded += superseded;
                }
                Ok(Handled::Skipped) => stats.skipped += 1,
                Ok(Handled::Flagged) => stats.flagged += 1,
                Err(e) => {
                    warn!("whatsapp reply card failed: {e:#}");
                    stats.errors += 1;
                }
            }
        }
        // Only after this poll's archive entries had their chance to confirm:
        // expiring first would mark a paste the archive just delivered as never sent.
        let cutoff = now_ms - self.manual_send_window.as_millis() as i64;
        match self.store.expire_manual_sends(cutoff) {
            Ok(ids) if !ids.is_empty() => {
                warn!(count = ids.len(), "whatsapp manual sends never showed up in the archive; marked error, nothing resent")
            }
            Ok(_) => {}
            Err(e) => {
                warn!("whatsapp manual-send expiry failed: {e:#}");
                stats.errors += 1;
            }
        }
        stats
    }
}

/// `https://wa.me/<digits>` for a phone-addressed chat; groups and `@lid`
/// chats have no addressable link (#1765).
pub fn chat_link(thread_id: &str) -> Option<String> {
    let jid = thread_id.strip_prefix(THREAD_PREFIX)?;
    let digits = jid.strip_suffix("@s.whatsapp.net")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("https://wa.me/{digits}"))
}

/// Whether an action row is a WhatsApp archive card.
pub fn is_history_card(email: &Email) -> bool {
    email.platform == "whatsapp" && email.account_entity_id.as_deref() == Some(crate::ACCOUNT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::reasoner::ReasonerOpts;
    use augmentagent_channel_imessage::{group_notes, Conversation};
    use augmentagent_store::ActionStatus;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const DM: &str = "14155550123@s.whatsapp.net"; // pii-ok synthetic
    const GROUP: &str = "120363001234567890@g.us";
    const MEMBER: &str = "111111111111111@lid"; // pii-ok synthetic
    const NOW: &str = "2026-11-01T12:00:00-04:00";
    const REPLY: &str = r#"{"decision":"reply","reason":"question"}"#;
    const SKIP: &str = r#"{"decision":"skip","reason":"chatter"}"#;

    struct Scripted(Mutex<VecDeque<String>>, Mutex<Vec<String>>);

    impl Scripted {
        fn new(r: &[&str]) -> Arc<Self> {
            Arc::new(Self(Mutex::new(r.iter().map(|s| s.to_string()).collect()), Mutex::new(vec![])))
        }
        fn prompts(&self) -> Vec<String> {
            self.1.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Reasoner for Scripted {
        async fn call(&self, o: &ReasonerOpts, m: &str) -> anyhow::Result<String> {
            if o.system_prompt.starts_with("You select evidence for a messaging draft") {
                return Ok(r#"{"action":"ready","facts":[],"gaps":[]}"#.into());
            }
            self.1.lock().unwrap().push(m.to_string());
            Ok(self.0.lock().unwrap().pop_front().unwrap_or_else(|| SKIP.into()))
        }
    }

    #[derive(Default)]
    struct Broker {
        cards: Mutex<Vec<(String, Email, String)>>,
        flags: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl ApprovalBroker for Broker {
        async fn post_approval(&self, id: &str, e: &Email, d: &str) -> Result<(), ApprovalError> {
            self.cards.lock().unwrap().push((id.into(), e.clone(), d.into()));
            Ok(())
        }
        async fn post_flag_notice(&self, _e: &Email, _r: &str) -> Result<(), ApprovalError> {
            *self.flags.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn now_ms() -> i64 {
        chrono::DateTime::parse_from_rfc3339(NOW).unwrap().timestamp_millis()
    }

    fn at(minutes_before_now: i64) -> String {
        (chrono::DateTime::parse_from_rfc3339(NOW).unwrap() - chrono::Duration::minutes(minutes_before_now))
            .to_rfc3339()
    }

    fn dm() -> Conversation {
        Conversation {
            identifier: DM.into(),
            dir: "jane".into(),
            title: "Jane".into(),
            participants: vec!["+14155550123".into()], // pii-ok synthetic
            service: "WhatsApp".into(),
            ..Default::default()
        }
    }

    fn group() -> Conversation {
        Conversation {
            identifier: GROUP.into(),
            dir: "ski".into(),
            title: "Ski Trip".into(),
            participants: vec!["+14155550999".into(), MEMBER.into()], // pii-ok synthetic
            participant_names: [("+14155550999".to_string(), "Jo".to_string()), (MEMBER.to_string(), "Ana".to_string())] // pii-ok synthetic
                .into_iter()
                .collect(),
            identity: [(MEMBER.to_string(), "+14155550777".to_string())].into_iter().collect(), // pii-ok synthetic
            service: "WhatsApp".into(),
            ..Default::default()
        }
    }

    fn entry(sender: &str, body: &str, minutes_ago: i64) -> MessageEntry {
        MessageEntry { timestamp: at(minutes_ago), sender: sender.into(), body: body.into(), attachments: vec![] }
    }

    fn delta(conversation: Conversation, entries: Vec<MessageEntry>, first_run: bool) -> Delta {
        Delta { conversation, entries: entries.into_iter().enumerate().map(|(i, e)| (i + 5, e)).collect(), first_run }
    }

    struct Fx {
        store: Arc<Store>,
        broker: Arc<Broker>,
        _dir: tempfile::TempDir,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        Fx { store: Arc::new(Store::open(dir.path().join("d.db")).unwrap()), broker: Arc::new(Broker::default()), _dir: dir }
    }

    fn replier(f: &Fx, r: Arc<Scripted>) -> WhatsappReplier {
        WhatsappReplier {
            store: Arc::clone(&f.store),
            reasoner: r,
            approvals: f.broker.clone(),
            config: ImessageReplyConfig { skill_dir: "skills/whatsapp-triage".into(), wiki_root: None },
            max_age: DEFAULT_MAX_AGE,
            manual_send_window: DEFAULT_MANUAL_SEND_WINDOW,
        }
    }

    /// What `poll_once` stores before replies run.
    fn ingest(store: &Store, d: &Delta) {
        for (i, e) in &d.entries {
            let row = email(&d.conversation, *i, e);
            store.upsert_email_backfill(&row, 1).unwrap();
            store.record_reply_recipients(&row).unwrap();
        }
    }

    async fn run(f: &Fx, r: Arc<Scripted>, d: Delta) -> ReplyStats {
        ingest(&f.store, &d);
        replier(f, r).handle_deltas_at(&[d], now_ms()).await
    }

    fn statuses(store: &Store) -> Vec<(String, String)> {
        store
            .with_conn(|c| {
                let mut s = c.prepare("SELECT id, status FROM actions ORDER BY createdAt, id")?;
                let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect()
            })
            .unwrap()
    }

    #[tokio::test]
    async fn allowlisted_dm_message_posts_exactly_one_card() {
        let f = fx();
        f.store.allow_whatsapp_inbound(DM).unwrap();
        let stats = run(&f, Scripted::new(&[REPLY, "yes, 7 works"]), delta(dm(), vec![entry(DM, "dinner at 7?", 1)], false)).await;
        assert_eq!(stats.cards, 1);
        let cards = f.broker.cards.lock().unwrap();
        let (id, card, draft) = &cards[0];
        assert_eq!(draft, "yes, 7 works");
        assert_eq!(card.subject, "WhatsApp: Jane");
        assert_eq!(card.from, format!("Jane <{DM}>"));
        let row = f.store.get_action_with_email(id).unwrap().unwrap();
        assert_eq!(row.action.status, "pending");
        assert!(is_history_card(&row.email), "{:?}", row.email.account_entity_id);
        assert_eq!(row.email.thread_id.as_deref(), Some(&*format!("{THREAD_PREFIX}{DM}")));
    }

    #[tokio::test]
    async fn group_inbound_addressed_to_owner_raises_card_with_every_member_in_to() {
        let f = fx();
        f.store.allow_whatsapp_inbound(GROUP).unwrap();
        let r = Scripted::new(&[REPLY, "Yes, I can drive."]);
        let d = delta(group(), vec![entry(MEMBER, "Sam, can you drive Friday?", 2)], false);
        let stats = run(&f, r.clone(), d).await;
        assert_eq!(stats.cards, 1);
        let prompts = r.prompts();
        let (policy, note) = group_notes("whatsapp");
        assert!(prompts[0].contains(&policy));
        assert!(prompts[1].contains(&note));
        let card = &f.broker.cards.lock().unwrap()[0].1;
        assert_eq!(card.subject, "WhatsApp group: Ski Trip");
        assert_eq!(card.to, "Jo <+14155550999>, Ana <+14155550777>"); // pii-ok synthetic
        assert_eq!(card.from, "Ana <+14155550777>"); // pii-ok synthetic
        assert!(card.body.contains("] Ana <+14155550777>\nSam, can you drive"), "{}", card.body); // pii-ok synthetic
    }

    #[tokio::test]
    async fn group_chatter_not_addressed_to_owner_raises_no_card_and_no_flag() {
        let f = fx();
        f.store.allow_whatsapp_inbound(GROUP).unwrap();
        let r = Scripted::new(&[SKIP]);
        let stats = run(&f, r.clone(), delta(group(), vec![entry(MEMBER, "lol", 1)], false)).await;
        assert_eq!((stats.cards, stats.skipped, stats.flagged), (0, 1, 0));
        assert_eq!(r.prompts().len(), 1, "triage only");
        assert_eq!(*f.broker.flags.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn owner_defaults_draft_dms_and_groups_with_no_per_chat_step_unless_denied() {
        let f = fx();
        f.store.set_whatsapp_contact_defaults(true, false).unwrap();
        let r = Scripted::new(&[REPLY, "a", REPLY, "b"]);
        assert_eq!(run(&f, r.clone(), delta(dm(), vec![entry(DM, "hi?", 1)], false)).await.cards, 1);
        assert_eq!(run(&f, r.clone(), delta(group(), vec![entry(MEMBER, "Sam?", 1)], false)).await.cards, 1);
        f.store.deny_whatsapp_inbound(GROUP).unwrap();
        let before = r.prompts().len();
        let mut d = delta(group(), vec![entry(MEMBER, "Sam??", 1)], false);
        d.entries[0].0 = 40;
        assert_eq!(run(&f, r.clone(), d).await.cards, 0);
        assert_eq!(r.prompts().len(), before, "denied group costs nothing");
    }

    #[tokio::test]
    async fn not_allowlisted_chat_costs_zero_reasoner_calls() {
        let f = fx();
        let r = Scripted::new(&[REPLY, "x"]);
        let stats = run(&f, r.clone(), delta(dm(), vec![entry(DM, "hi?", 1)], false)).await;
        assert_eq!(stats, ReplyStats::default());
        assert!(r.prompts().is_empty());
    }

    #[tokio::test]
    async fn first_run_stale_catch_up_media_only_left_group_and_own_newest_never_draft() {
        let f = fx();
        f.store.set_whatsapp_contact_defaults(true, false).unwrap();
        let mut left = group();
        left.active = Some(false);
        let media = MessageEntry { attachments: vec!["[attachment: whatsapp-media IMG.jpg]".into()], ..entry(DM, "", 1) };
        let cases = vec![
            delta(dm(), vec![entry(DM, "first sync", 1)], true),
            delta(dm(), vec![entry(DM, "from two days ago", 48 * 60)], false),
            delta(dm(), vec![media], false),
            delta(left, vec![entry(MEMBER, "Sam?", 1)], false),
            delta(dm(), vec![entry(DM, "q?", 3), entry("me", "answered", 1)], false),
        ];
        let r = Scripted::new(&[REPLY, "x", REPLY, "x", REPLY, "x", REPLY, "x", REPLY, "x"]);
        for d in cases {
            ingest(&f.store, &d);
            let stats = replier(&f, r.clone()).handle_deltas_at(&[d], now_ms()).await;
            assert_eq!(stats.cards, 0);
        }
        assert!(r.prompts().is_empty());
    }

    #[test]
    fn catch_up_age_is_configurable_and_boundaries_hold() {
        let d = delta(dm(), vec![entry(DM, "q?", 60)], false);
        assert!(card_email(&d, now_ms(), Duration::from_secs(2 * 3600)).is_some());
        assert!(card_email(&d, now_ms(), Duration::from_secs(30 * 60)).is_none());
    }

    #[tokio::test]
    async fn newer_message_supersedes_the_pending_card_and_manual_reply_retires_it() {
        let f = fx();
        f.store.allow_whatsapp_inbound(DM).unwrap();
        let r = Scripted::new(&[REPLY, "one", REPLY, "two"]);
        run(&f, r.clone(), delta(dm(), vec![entry(DM, "q1?", 5)], false)).await;
        let mut d2 = delta(dm(), vec![entry(DM, "q2?", 4)], false);
        d2.entries[0].0 = 9;
        let stats = run(&f, r.clone(), d2).await;
        assert_eq!((stats.cards, stats.superseded), (1, 1));
        let mut d3 = delta(dm(), vec![entry("me", "typed it myself", 1)], false);
        d3.entries[0].0 = 10;
        let stats = run(&f, r, d3).await;
        assert_eq!((stats.manual_replies, stats.superseded), (1, 1));
        let s: Vec<String> = statuses(&f.store).into_iter().map(|(_, s)| s).collect();
        assert_eq!(s, vec!["superseded", "superseded"]);
    }

    /// One pending card on DM, approved for a manual send at `approved_min_ago`.
    fn approved_card(f: &Fx, text: &str, approved_min_ago: i64) -> String {
        let e = email(&dm(), 1, &entry(DM, "q?", 30));
        f.store.upsert_email(&e).unwrap();
        let id = f.store
            .log_action(&e.message_id, e.thread_id.as_deref(), &e.from, &e.subject, Some(&e.body), Some(text), ActionStatus::Pending)
            .unwrap();
        assert!(f.store.mark_manual_send_pending(&id).unwrap());
        let approved_ms = now_ms() - approved_min_ago * 60_000;
        f.store
            .with_conn(|c| c.execute("UPDATE actions SET updatedAt = ?2 WHERE id = ?1", augmentagent_store::rusqlite::params![id, approved_ms]))
            .unwrap();
        id
    }

    fn status(store: &Store, id: &str) -> (String, Option<String>) {
        let a = store.get_action_with_email(id).unwrap().unwrap().action;
        (a.status, a.error_message)
    }

    #[tokio::test]
    async fn matching_me_entry_within_window_marks_action_sent_and_is_not_a_manual_reply() {
        let f = fx();
        let id = approved_card(&f, "On my  way!", 10);
        let stats = run(&f, Scripted::new(&[]), delta(dm(), vec![entry("me", "On my way!\n", 8)], false)).await;
        assert_eq!((stats.own_sends, stats.manual_replies), (1, 0));
        assert_eq!(status(&f.store, &id), ("sent".into(), None));
    }

    #[tokio::test]
    async fn different_me_entry_supersedes_as_manual_reply_and_leaves_approval_waiting() {
        let f = fx();
        let id = approved_card(&f, "On my way!", 10);
        let e = email(&dm(), 2, &entry(DM, "later?", 20));
        f.store.upsert_email(&e).unwrap();
        let pending = f.store
            .log_action(&e.message_id, e.thread_id.as_deref(), &e.from, &e.subject, None, Some("d"), ActionStatus::Pending)
            .unwrap();
        let stats = run(&f, Scripted::new(&[]), delta(dm(), vec![entry("me", "something else", 5)], false)).await;
        assert_eq!((stats.own_sends, stats.manual_replies), (0, 1));
        assert_eq!(status(&f.store, &pending), ("superseded".into(), Some("superseded by manual reply".into())));
        assert_eq!(status(&f.store, &id).0, "approved");
    }

    #[tokio::test]
    async fn unconfirmed_approval_expires_after_window_without_resend() {
        let f = fx();
        let id = approved_card(&f, "On my way!", 121);
        let fresh = approved_card(&f, "Later", 5);
        let stats = replier(&f, Scripted::new(&[])).handle_deltas_at(&[], now_ms()).await;
        assert_eq!(stats.errors, 0);
        assert_eq!(status(&f.store, &id), ("error".into(), Some(augmentagent_store::MANUAL_SEND_UNCONFIRMED.into())));
        assert_eq!(status(&f.store, &fresh).0, "approved");
        assert!(f.broker.cards.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn paste_arriving_in_the_poll_that_crosses_the_window_still_confirms() {
        // Approved 125 min ago, pasted at +100 min; the archive only delivers it now.
        let f = fx();
        let id = approved_card(&f, "On my way!", 125);
        let stats = run(&f, Scripted::new(&[]), delta(dm(), vec![entry("me", "On my way!", 25)], false)).await;
        assert_eq!((stats.own_sends, stats.manual_replies), (1, 0));
        assert_eq!(status(&f.store, &id), ("sent".into(), None));
    }

    #[tokio::test]
    async fn late_archive_confirms_an_already_expired_row_within_its_original_window() {
        let f = fx();
        let id = approved_card(&f, "On my way!", 200);
        replier(&f, Scripted::new(&[])).handle_deltas_at(&[], now_ms()).await;
        assert_eq!(status(&f.store, &id).0, "error");
        // Pasted 100 min after approval, but WhatsApp Desktop was closed until now.
        let stats = run(&f, Scripted::new(&[]), delta(dm(), vec![entry("me", "On my way!", 100)], false)).await;
        assert_eq!(stats.own_sends, 1);
        assert_eq!(status(&f.store, &id), ("sent".into(), None));
        // Outside the original window never confirms.
        let late = approved_card(&f, "Later", 300);
        replier(&f, Scripted::new(&[])).handle_deltas_at(&[], now_ms()).await;
        let mut d = delta(dm(), vec![entry("me", "Later", 10)], false);
        d.entries[0].0 = 30;
        assert_eq!(run(&f, Scripted::new(&[]), d).await.own_sends, 0);
        assert_eq!(status(&f.store, &late).0, "error");
        assert_eq!(f.store.manual_send_counts().unwrap(), (0, 1));
    }

    #[test]
    fn phone_dm_card_includes_wa_me_link_group_and_lid_cards_do_not() {
        assert_eq!(chat_link(&format!("{THREAD_PREFIX}{DM}")).as_deref(), Some("https://wa.me/14155550123")); // pii-ok synthetic
        assert_eq!(chat_link(&format!("{THREAD_PREFIX}{GROUP}")), None);
        assert_eq!(chat_link(&format!("{THREAD_PREFIX}{MEMBER}")), None);
        assert_eq!(chat_link(&format!("imessage:{DM}")), None);
    }

    #[tokio::test]
    async fn polling_the_same_bundle_twice_changes_nothing_the_second_time() {
        let f = fx();
        f.store.set_whatsapp_contact_defaults(true, false).unwrap();
        let root = f._dir.path().join("bundle");
        let conv_dir = root.join("conversations/jane");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::write(
            root.join("conversations/index.json"),
            serde_json::json!({DM: {"identifier": DM, "dir": "jane", "title": "Jane", "path": "conversations/jane/messages.md",
                                    "participants": ["+14155550123"], "service": "WhatsApp"}}).to_string(), // pii-ok synthetic
        )
        .unwrap();
        let md = |entries: &str| format!("---\ntitle: Jane\n---\n{entries}");
        let first = format!("### [{}] +14155550123\nhello\n", at(10)); // pii-ok synthetic
        std::fs::write(conv_dir.join("messages.md"), md(&first)).unwrap();
        crate::poll_once(&root, &f.store).unwrap(); // first sync: history only
        let second = format!("{first}\n### [{}] +14155550123\nfree tonight?\n\n### [{}] +14155550123\nphone number verify 123456\n", at(2), at(1)); // pii-ok synthetic
        std::fs::write(conv_dir.join("messages.md"), md(&second)).unwrap();
        let rep = replier(&f, Scripted::new(&[REPLY, "yes"]));
        let report = crate::poll_once(&root, &f.store).unwrap();
        assert_eq!(rep.handle_deltas_at(&report.deltas, now_ms()).await.cards, 1);
        let card = f.broker.cards.lock().unwrap()[0].1.clone();
        assert!(card.body.contains("free tonight?") && !card.body.contains("verify"), "sensitive entry never reaches triage: {}", card.body);
        let before = statuses(&f.store);
        let report = crate::poll_once(&root, &f.store).unwrap();
        assert_eq!(rep.handle_deltas_at(&report.deltas, now_ms()).await, ReplyStats::default());
        assert_eq!(statuses(&f.store), before);
    }
}
