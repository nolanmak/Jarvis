//! Read-only owner attention view for Live calls. No reasoning subprocess,
//! writes, importance reclassification, or tool approvals are introduced here.
use anyhow::Result;
use async_trait::async_trait;
use augmentagent_channel_calendar::{CalendarApi, ComposioCalendarClient, MeetingPayload};
use augmentagent_channel_core::phone_attention::{AttentionProgress, AttentionProvider};
use augmentagent_channel_email::gmail::{ComposioClient, GmailApi};
use augmentagent_store::{phone::PhoneOrigin, Account, Store};
use chrono::{Local, Utc};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::OnceCell;
const FRESH_MS: i64 = 300_000;
const PENDING_TRIAGE_QUERY: &str = "SELECT count(*) FROM emails WHERE platform='gmail' AND accountEntityId=?1 AND triageResult IS NULL";
#[derive(Clone)]
struct SourceView {
    name: String,
    checked_ms: Option<i64>,
    complete: bool,
    facts: Vec<String>,
    gap: Option<&'static str>,
}
impl SourceView {
    fn unavailable(name: String) -> Self {
        Self {
            name,
            checked_ms: None,
            complete: false,
            facts: vec![],
            gap: Some("could not be checked"),
        }
    }
}
#[async_trait]
trait Reads: Send + Sync {
    async fn calendar(&self, account: &Account) -> Result<Vec<String>>;
    async fn inbox(&self, account: &Account) -> Result<SourceView>;
}
struct ConnectedReads {
    store: Arc<Store>,
    calendar: ComposioCalendarClient,
    gmail: ComposioClient,
}
#[async_trait]
impl Reads for ConnectedReads {
    async fn calendar(&self, account: &Account) -> Result<Vec<String>> {
        let now = Utc::now();
        let events = self
            .calendar
            .list_events(
                &account.entity_id,
                "primary",
                now,
                now + chrono::Duration::days(3),
            )
            .await?;
        calendar_facts(&events, &account.entity_id)
    }
    async fn inbox(&self, account: &Account) -> Result<SourceView> {
        let now = Utc::now().timestamp_millis();
        let (last_ok,poll_ok,pending,mut facts)=self.store.with_conn(|c|{
            let (last_ok,poll_ok):(Option<i64>,Option<bool>)=c.query_row("SELECT lastOkPolledAt,lastPollOk FROM gmail_accounts WHERE entityId=?1 AND active=1",[&account.entity_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let pending:i64=c.query_row(PENDING_TRIAGE_QUERY,[&account.entity_id],|r|r.get(0))?;
            let facts=c.prepare("SELECT e.messageId,e.fromEmail,e.subject,a.status FROM actions a JOIN emails e ON e.messageId=a.messageId WHERE e.platform='gmail' AND e.accountEntityId=?1 AND a.status IN ('flagged','pending') ORDER BY CASE a.status WHEN 'flagged' THEN 0 ELSE 1 END,a.createdAt DESC LIMIT 3")?
                .query_map([&account.entity_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<augmentagent_store::rusqlite::Result<Vec<_>>>()?;
            Ok((last_ok,poll_ok,pending,facts))
        })?;
        let fresh =
            last_ok.is_some_and(|t| t <= now && now - t <= FRESH_MS) && poll_ok == Some(true);
        // A cold/stale mailbox gets one bounded read concurrently with calendar.
        // Do not advance the ingestion cursor or run a cheaper triage model.
        let (checked_ms, gap) = if fresh {
            (
                last_ok,
                if pending > 0 {
                    Some("has messages still awaiting normal triage")
                } else {
                    None
                },
            )
        } else {
            match self
                .gmail
                .fetch_with_query(&account.entity_id, "in:inbox newer_than:3d -from:me", 25)
                .await
            {
                Ok(messages) => {
                    // A bounded recent read cannot certify an unsynced inbox.
                    // Only re-observed messages may borrow its new check time.
                    let ids = messages
                        .iter()
                        .map(|m| m.message_id.as_str())
                        .collect::<std::collections::HashSet<_>>();
                    facts.retain(|(id, _, _, _)| ids.contains(id.as_str()));
                    (
                        Some(Utc::now().timestamp_millis()),
                        Some("has stale sync coverage; only a bounded recent inbox read completed"),
                    )
                }
                Err(_) => (
                    last_ok,
                    Some("could not refresh its stale or unavailable inbox sync"),
                ),
            }
        };
        Ok(SourceView {
            name: format!("{} inbox triage", account.email),
            checked_ms,
            complete: fresh && pending == 0,
            facts: facts
                .into_iter()
                .map(|(_, sender, subject, state)| {
                    if state == "pending" {
                        format!("A reply to {sender} about {subject} awaits your approval.")
                    } else {
                        format!("Flagged for your attention: {subject} from {sender}.")
                    }
                })
                .collect(),
            gap,
        })
    }
}
fn calendar_facts(
    events: &[augmentagent_channel_calendar::CalendarEvent],
    entity: &str,
) -> Result<Vec<String>> {
    let mut meetings = vec![];
    for event in events
        .iter()
        .filter(|e| e.status.as_deref() != Some("cancelled"))
    {
        let Some(payload) = MeetingPayload::from_event(event, entity, "primary") else {
            anyhow::bail!("calendar contains an event with unreadable timing")
        };
        if payload
            .attendees
            .iter()
            .any(|a| a.is_self && a.response_status.as_deref() == Some("declined"))
        {
            continue;
        }
        let all_day = event.start.as_ref().and_then(|s| s.date.clone());
        meetings.push((payload, all_day));
    }
    meetings.sort_by_key(|(p, _)| p.start);
    Ok(meetings
        .into_iter()
        .take(3)
        .map(|(p, all_day)| {
            let when = all_day
                .and_then(|date| chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok())
                .map(|date| date.format("%a %b %-d (all day)").to_string())
                .unwrap_or_else(|| {
                    p.start
                        .with_timezone(&Local)
                        .format("%a %b %-d at %-I:%M %p %Z")
                        .to_string()
                });
            format!("Upcoming: {} on {when}.", p.summary)
        })
        .collect())
}
// active=1 is the operator-enabled source set, not a health claim. Every
// enabled source remains required even when its read/sync fails; disabled
// historical connections do not become spurious missing-source notices.
fn accounts(store: &Store) -> Result<Vec<Account>> {
    Ok(store.with_conn(|c|c.prepare("SELECT id,connectionId,entityId,COALESCE(email,''),active FROM gmail_accounts WHERE active=1 ORDER BY id")?
        .query_map([],|r|Ok(Account{id:r.get(0)?,connection_id:r.get(1)?,entity_id:r.get(2)?,email:r.get(3)?,active:r.get(4)?}))?
        .collect::<augmentagent_store::rusqlite::Result<Vec<_>>>())?)
}
struct CachedSource {
    view: SourceView,
    expires_at: tokio::time::Instant,
}
#[derive(Hash, PartialEq, Eq)]
struct SourceKey {
    calendar: bool,
    account: String,
    connection: Option<String>,
    entity: String,
    email: String,
}
pub struct PhoneAttention {
    store: Arc<Store>,
    reads: Arc<dyn Reads>,
    cache: Mutex<HashMap<SourceKey, Arc<OnceCell<CachedSource>>>>,
}
impl PhoneAttention {
    pub fn new(store: Arc<Store>) -> Self {
        let key = augmentagent_channel_core::secret_loader::load_provider_key("COMPOSIO_API_KEY")
            .unwrap_or_default();
        Self {
            store: store.clone(),
            reads: Arc::new(ConnectedReads {
                store,
                calendar: ComposioCalendarClient::new(key.clone()),
                gmail: ComposioClient::new(key),
            }),
            cache: Mutex::new(HashMap::new()),
        }
    }
    async fn source(&self, account: &Account, calendar: bool) -> SourceView {
        let key = SourceKey {
            calendar,
            account: account.id.clone(),
            connection: account.connection_id.clone(),
            entity: account.entity_id.clone(),
            email: account.email.clone(),
        };
        let now = tokio::time::Instant::now();
        let cell = {
            let mut cache = self.cache.lock().unwrap();
            let expired = cache
                .get(&key)
                .and_then(|c| c.get())
                .is_some_and(|c| c.expires_at <= now);
            if expired {
                cache.remove(&key);
            }
            if cache.len() >= 64 && !cache.contains_key(&key) {
                cache.retain(|_, c| c.get().is_none_or(|v| v.expires_at > now));
            }
            cache.entry(key).or_default().clone()
        };
        let cached = cell
            .get_or_init(|| async {
                let started = tokio::time::Instant::now();
                let view = if calendar {
                    match tokio::time::timeout(Duration::from_secs(5), self.reads.calendar(account))
                        .await
                    {
                        Ok(Ok(facts)) => SourceView {
                            name: format!("{} calendar", account.email),
                            checked_ms: Some(Utc::now().timestamp_millis()),
                            complete: true,
                            facts,
                            gap: None,
                        },
                        _ => SourceView::unavailable(format!("{} calendar", account.email)),
                    }
                } else {
                    match tokio::time::timeout(Duration::from_secs(5), self.reads.inbox(account))
                        .await
                    {
                        Ok(Ok(view)) => view,
                        _ => SourceView::unavailable(format!("{} inbox triage", account.email)),
                    }
                };
                tracing::info!(
                    source = if calendar { "calendar" } else { "inbox" },
                    read_ms = started.elapsed().as_millis() as u64,
                    complete = view.complete,
                    "phone attention source read finished"
                );
                CachedSource {
                    expires_at: tokio::time::Instant::now()
                        + Duration::from_millis(if calendar && view.complete {
                            FRESH_MS as u64
                        } else {
                            1000
                        }),
                    view,
                }
            })
            .await;
        SourceView {
            name: cached.view.name.clone(),
            checked_ms: cached.view.checked_ms,
            complete: cached.view.complete,
            facts: cached.view.facts.clone(),
            gap: cached.view.gap,
        }
    }
}
impl PhoneAttention {
    fn validate_plan(
        &self,
        origin: &PhoneOrigin,
        revision: i64,
        configured: &[Account],
    ) -> Result<()> {
        let binding = self.store.surface_owner_binding(origin.owner.account())?;
        anyhow::ensure!(
            binding.is_some_and(|b| b.revision == revision
                && b.owner == origin.owner
                && b.is_control_conversation(origin.conversation.conversation_id())),
            "phone attention authorization changed during reads"
        );
        let identity = |rows: &[Account]| {
            rows.iter()
                .map(|a| {
                    (
                        a.id.clone(),
                        a.connection_id.clone(),
                        a.entity_id.clone(),
                        a.email.clone(),
                        a.active,
                    )
                })
                .collect::<Vec<_>>()
        };
        anyhow::ensure!(
            identity(&accounts(&self.store)?) == identity(configured),
            "attention source configuration changed during check"
        );
        Ok(())
    }
    async fn run_check(
        &self,
        origin: &PhoneOrigin,
        progress: AttentionProgress,
    ) -> Result<Option<String>> {
        let binding = self.store.surface_owner_binding(origin.owner.account())?;
        let binding = binding
            .filter(|b| {
                b.owner == origin.owner
                    && b.is_control_conversation(origin.conversation.conversation_id())
            })
            .ok_or_else(|| anyhow::anyhow!("phone attention origin is no longer authorized"))?;
        let configured = accounts(&self.store)?;
        if configured.is_empty() {
            return Ok(Some(
                "No calendar or inbox accounts are enabled, so I haven't checked them.".into(),
            ));
        }
        anyhow::ensure!(configured.len() <= 32, "too many attention sources");
        let plan = (0..configured.len())
            .flat_map(|i| [(i, true), (i, false)])
            .collect::<Vec<_>>();
        let mut views: Vec<Option<SourceView>> = vec![None; plan.len()];
        // Two retries of missing coverage only. The initial healthy-source
        // answer still has the same five-second concurrent source bounds.
        for attempt in 0..3 {
            self.validate_plan(origin, binding.revision, &configured)?;
            let now = Utc::now().timestamp_millis();
            let mut checks = FuturesUnordered::new();
            for (index, &(account, calendar)) in plan.iter().enumerate() {
                if views[index]
                    .as_ref()
                    .is_some_and(|v| fresh(v, now) && v.complete)
                {
                    continue;
                }
                let account = &configured[account];
                checks.push(async move { (index, self.source(account, calendar).await) });
            }
            while let Some((index, view)) = checks.next().await {
                views[index] = Some(view);
            }
            self.validate_plan(origin, binding.revision, &configured)?;
            let views_now = views.iter().flatten().cloned().collect::<Vec<_>>();
            let now = Utc::now().timestamp_millis();
            let complete = views_now.iter().all(|v| fresh(v, now) && v.complete);
            let actionable = views_now
                .iter()
                .any(|v| fresh(v, now) && !v.facts.is_empty());
            if complete || actionable || attempt == 2 {
                let mut answer = render(&views_now, now);
                if !complete && !actionable {
                    answer.push_str(" Those sources are still incomplete after two retries. I've stopped this check; we can keep talking.");
                }
                return Ok(Some(answer));
            }
            progress.source_gap(&missing_notice(&views_now, now));
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        unreachable!("bounded attention attempts always return")
    }
}
#[async_trait]
impl AttentionProvider for PhoneAttention {
    async fn check(&self, origin: &PhoneOrigin) -> Result<Option<String>> {
        self.run_check(origin, AttentionProgress::default()).await
    }
    async fn check_with_progress(
        &self,
        origin: &PhoneOrigin,
        progress: AttentionProgress,
    ) -> Result<Option<String>> {
        self.run_check(origin, progress).await
    }
}
fn fresh(view: &SourceView, now: i64) -> bool {
    view.checked_ms
        .is_some_and(|t| t <= now && now - t <= FRESH_MS)
}
fn missing_notice(views: &[SourceView], now: i64) -> String {
    let missing = views
        .iter()
        .filter(|v| !fresh(v, now) || !v.complete)
        .collect::<Vec<_>>();
    let pieces = missing
        .iter()
        .take(2)
        .map(|v| {
            let mut text = format!("{} {}", v.name, v.gap.unwrap_or("is incomplete"));
            if text.len() > 200 {
                let mut end = 200;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("…");
            }
            text
        })
        .collect::<Vec<_>>();
    format!("The attention check is incomplete. Still missing: {}{}. I'm continuing to check; I don't have verified findings yet.",
        pieces.join("; "), if missing.len() > 2 { "; and other required sources" } else { "" })
}

fn render(views: &[SourceView], now: i64) -> String {
    let mut facts = vec![];
    let mut gaps = vec![];
    let mut complete = !views.is_empty();
    let mut oldest: Option<i64> = None;
    for view in views {
        let fresh = fresh(view, now);
        if fresh {
            facts.extend(view.facts.iter().take(3).cloned());
            oldest = match (oldest, view.checked_ms) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        if !fresh || !view.complete {
            complete = false;
            gaps.push(format!(
                "{} {}",
                view.name,
                view.gap.unwrap_or(if fresh {
                    "is incomplete"
                } else {
                    "has no current source check"
                })
            ));
        }
    }
    let mut answer = if facts.is_empty() {
        if complete {
            "I found no pending triaged inbox items or upcoming calendar events in the next three days.".into()
        } else {
            "I can't give a complete attention check yet.".into()
        }
    } else {
        facts.join(" ")
    };
    if let Some(ms) = oldest.and_then(chrono::DateTime::from_timestamp_millis) {
        answer.push_str(&format!(" Sources checked as of {}. Calendar covers the next three days; inbox coverage is the unresolved triage view.",ms.with_timezone(&Local).format("%a %-I:%M %p %Z")));
    }
    if !gaps.is_empty() {
        answer.push_str(&format!(
            " Still missing: {}. I can't say there's nothing important in those sources.",
            gaps.join("; ")
        ));
    }
    answer
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_triage_lookup_work_is_bounded_to_the_selected_accounts_pending_rows() {
        use augmentagent_store::rusqlite::StatementStatus;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        store.with_conn(|c| {
            c.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<5000)
                INSERT INTO emails(messageId,fromEmail,subject,firstSeenAt,platform,accountEntityId,triageResult)
                SELECT 'other-'||i,'sender@example.test','Synthetic',0,'gmail','other',NULL FROM n;
                WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<5000)
                INSERT INTO emails(messageId,fromEmail,subject,firstSeenAt,platform,accountEntityId,triageResult)
                SELECT 'triaged-'||i,'sender@example.test','Synthetic',0,'gmail','selected','{}' FROM n;
                INSERT INTO emails(messageId,fromEmail,subject,firstSeenAt,platform,accountEntityId)
                VALUES('pending','sender@example.test','Synthetic',0,'gmail','selected'),
                      ('different-platform','sender@example.test','Synthetic',0,'slack','selected');")?;
            let mut query = c.prepare(PENDING_TRIAGE_QUERY)?;
            let pending: i64 = query.query_row(["selected"], |r| r.get(0))?;
            assert_eq!(pending, 1, "account and platform isolation must remain intact");
            let steps = query.get_status(StatementStatus::VmStep);
            assert!(steps < 100, "a phone source lookup must not scan unrelated or already triaged mail: {steps} SQLite VM steps");
            Ok(())
        }).unwrap();
    }
    #[test]
    fn all_day_dates_are_not_shifted_into_the_previous_local_evening() {
        let event=serde_json::from_value(serde_json::json!({"id":"synthetic","summary":"Synthetic deadline","start":{"date":"2026-11-03"},"end":{"date":"2026-11-04"}})).unwrap();
        let facts = calendar_facts(&[event], "synthetic").unwrap();
        assert!(facts[0].contains("Tue Nov 3 (all day)"));
    }
    #[test]
    fn verified_empty_requires_every_source_fresh_and_complete() {
        let good = SourceView {
            name: "synthetic calendar".into(),
            checked_ms: Some(1000),
            complete: true,
            facts: vec![],
            gap: None,
        };
        assert!(render(&[good], 1000).contains("no pending triaged"));
        for checked in [None, Some(0)] {
            let bad = SourceView {
                name: "synthetic inbox".into(),
                checked_ms: checked,
                complete: false,
                facts: vec![],
                gap: None,
            };
            let text = render(&[bad], FRESH_MS + 1);
            assert!(text.contains("can't give a complete"));
            assert!(!text.contains("I found no"));
        }
    }
    #[test]
    fn available_facts_precede_missing_source_notice_and_stale_facts_are_not_current() {
        let fresh = SourceView {
            name: "calendar".into(),
            checked_ms: Some(1000),
            complete: true,
            facts: vec!["Synthetic deadline Tuesday at 5 PM.".into()],
            gap: None,
        };
        let stale = SourceView {
            name: "inbox".into(),
            checked_ms: Some(0),
            complete: true,
            facts: vec!["OLD_PRIVATE_FACT".into()],
            gap: None,
        };
        let text = render(&[fresh, stale], FRESH_MS + 1);
        assert!(text.starts_with("Synthetic deadline"));
        assert!(text.contains("Still missing"));
        assert!(!text.contains("OLD_PRIVATE_FACT"));
    }
    struct FakeReads {
        calendar_calls: std::sync::atomic::AtomicUsize,
        inbox_calls: std::sync::atomic::AtomicUsize,
        slow_inbox: bool,
    }
    #[async_trait]
    impl Reads for FakeReads {
        async fn calendar(&self, _: &Account) -> Result<Vec<String>> {
            self.calendar_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(vec!["Synthetic deadline Tuesday at 5 PM.".into()])
        }
        async fn inbox(&self, _: &Account) -> Result<SourceView> {
            self.inbox_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(if self.slow_inbox { 10 } else { 2 })).await;
            Ok(SourceView {
                name: "synthetic inbox".into(),
                checked_ms: Some(Utc::now().timestamp_millis()),
                complete: true,
                facts: vec![],
                gap: None,
            })
        }
    }
    struct RecoveringReads {
        calendar_calls: std::sync::atomic::AtomicUsize,
        inbox_calls: std::sync::atomic::AtomicUsize,
        actionable: bool,
        failures: usize,
        first: tokio::sync::Notify,
    }
    #[async_trait]
    impl Reads for RecoveringReads {
        async fn calendar(&self, _: &Account) -> Result<Vec<String>> {
            self.calendar_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![])
        }
        async fn inbox(&self, _: &Account) -> Result<SourceView> {
            let attempt = self
                .inbox_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                self.first.notify_one();
            }
            if attempt < self.failures {
                anyhow::bail!("synthetic temporary source outage");
            }
            Ok(SourceView {
                name: "synthetic inbox".into(),
                checked_ms: Some(Utc::now().timestamp_millis()),
                complete: true,
                facts: if self.actionable {
                    vec!["Synthetic approval deadline tomorrow at 5 PM.".into()]
                } else {
                    vec![]
                },
                gap: None,
            })
        }
    }
    async fn recovering_check(actionable: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        let reads = Arc::new(RecoveringReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            actionable,
            failures: 1,
            first: Default::default(),
        });
        let (_dir, provider, origin) = fixture(reads.clone());
        let started = tokio::time::Instant::now();
        let answer = provider.check(&origin).await.unwrap().unwrap();
        assert!(
            if actionable {
                answer.contains("Synthetic approval deadline tomorrow at 5 PM.")
            } else {
                answer.contains("I found no pending triaged inbox items")
            },
            "an incomplete no-facts check must keep working until its required source recovers: {answer}"
        );
        assert!(started.elapsed() <= Duration::from_secs(36));
        assert_eq!(
            reads.calendar_calls.load(SeqCst),
            1,
            "do not refetch a healthy source just to create progress"
        );
        assert_eq!(
            reads.inbox_calls.load(SeqCst),
            2,
            "retry the failed source once, then stop after a verified result"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn incomplete_check_retries_until_a_verified_fact_arrives() {
        recovering_check(true).await;
    }
    #[tokio::test(start_paused = true)]
    async fn incomplete_check_retries_and_finishes_when_sources_are_verified_empty() {
        recovering_check(false).await;
    }
    #[tokio::test(start_paused = true)]
    async fn persistent_source_failure_has_bounded_retries_and_never_becomes_all_clear() {
        use std::sync::atomic::Ordering::SeqCst;
        let reads = Arc::new(RecoveringReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            actionable: false,
            failures: usize::MAX,
            first: Default::default(),
        });
        let (_dir, provider, origin) = fixture(reads.clone());
        let started = tokio::time::Instant::now();
        let answer = provider.check(&origin).await.unwrap().unwrap();
        assert!(answer.contains("Still missing") && !answer.contains("I found no"));
        assert!(started.elapsed() <= Duration::from_secs(36));
        assert_eq!(reads.calendar_calls.load(SeqCst), 1);
        assert_eq!(
            reads.inbox_calls.load(SeqCst),
            3,
            "make two bounded retries, then report the unresolved source honestly"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn healthy_verified_empty_check_finishes_without_background_retries() {
        use std::sync::atomic::Ordering::SeqCst;
        let reads = Arc::new(RecoveringReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            actionable: false,
            failures: 0,
            first: Default::default(),
        });
        let (_dir, provider, origin) = fixture(reads.clone());
        let started = tokio::time::Instant::now();
        assert!(provider
            .check(&origin)
            .await
            .unwrap()
            .unwrap()
            .contains("I found no pending triaged inbox items"));
        assert!(started.elapsed() <= Duration::from_secs(5));
        assert_eq!(reads.calendar_calls.load(SeqCst), 1);
        assert_eq!(reads.inbox_calls.load(SeqCst), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn overlapping_incomplete_checks_share_the_retry_and_keep_the_healthy_snapshot() {
        use std::sync::atomic::Ordering::SeqCst;
        let reads = Arc::new(RecoveringReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            actionable: true,
            failures: 1,
            first: Default::default(),
        });
        let (_dir, provider, origin) = fixture(reads.clone());
        let (a, b) = tokio::join!(provider.check(&origin), provider.check(&origin));
        assert!(a.unwrap().unwrap().contains("Synthetic approval deadline"));
        assert!(b.unwrap().unwrap().contains("Synthetic approval deadline"));
        assert_eq!(reads.calendar_calls.load(SeqCst), 1);
        assert_eq!(reads.inbox_calls.load(SeqCst), 2);
    }
    async fn invalidate_during_retry(revoke: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        let reads = Arc::new(RecoveringReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            actionable: true,
            failures: 1,
            first: Default::default(),
        });
        let (_dir, provider, origin) = fixture(reads.clone());
        let provider = Arc::new(provider);
        let task_provider = provider.clone();
        let task_origin = origin.clone();
        let task = tokio::spawn(async move { task_provider.check(&task_origin).await });
        reads.first.notified().await;
        if revoke {
            provider
                .store
                .unbind_surface_owner(origin.owner.account())
                .unwrap();
        } else {
            provider
                .store
                .with_conn(|c| c.execute("UPDATE gmail_accounts SET active=0", []))
                .unwrap();
        }
        assert!(
            task.await.unwrap().is_err(),
            "a changed source plan or authority cancels the check before another read"
        );
        assert_eq!(reads.inbox_calls.load(SeqCst), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn revoked_owner_cannot_resume_an_incomplete_check() {
        invalidate_during_retry(true).await;
    }
    #[tokio::test(start_paused = true)]
    async fn changed_source_plan_cannot_resume_an_incomplete_check() {
        invalidate_during_retry(false).await;
    }
    #[tokio::test(start_paused = true)]
    async fn reconnected_source_requires_its_own_fresh_snapshot() {
        use std::sync::atomic::Ordering::SeqCst;
        let reads = Arc::new(FakeReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            slow_inbox: false,
        });
        let (_dir, provider, origin) = fixture(reads.clone());
        provider.check(&origin).await.unwrap();
        provider
            .store
            .with_conn(|c| {
                c.execute(
                    "UPDATE gmail_accounts SET connectionId='new-connection' WHERE active=1",
                    [],
                )
            })
            .unwrap();
        provider.check(&origin).await.unwrap();
        assert_eq!(
            reads.calendar_calls.load(SeqCst),
            2,
            "a reconnected source cannot borrow the prior connection's check time"
        );
        assert_eq!(reads.inbox_calls.load(SeqCst), 2);
    }
    fn fixture(reads: Arc<dyn Reads>) -> (tempfile::TempDir, PhoneAttention, PhoneOrigin) {
        use augmentagent_store::{
            owner::ControlConversationKind, SurfaceAccountRef, SurfaceConversationRef,
            SurfaceOwnerRef, SurfacePlatform,
        };
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "synthetic-account")
                .unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
        let conversation = SurfaceConversationRef::new(account, "owner-chat", None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&conversation, ControlConversationKind::Direct, 0)
            .unwrap();
        store
            .upsert_google_gmail_account("owner@example.test")
            .unwrap();
        store
            .upsert_google_gmail_account("retired@example.test")
            .unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE gmail_accounts SET active=0 WHERE email='retired@example.test'",
                    [],
                )
            })
            .unwrap();
        (
            dir,
            PhoneAttention {
                store,
                reads,
                cache: Mutex::new(HashMap::new()),
            },
            PhoneOrigin {
                owner,
                conversation,
            },
        )
    }
    #[tokio::test(start_paused = true)]
    async fn parallel_checks_share_source_refresh_and_finish_in_two_seconds() {
        let reads = Arc::new(FakeReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            slow_inbox: false,
        });
        let (_dir, provider, origin) = fixture(reads.clone());
        let start = tokio::time::Instant::now();
        let (a, b) = tokio::join!(provider.check(&origin), provider.check(&origin));
        assert!(start.elapsed() <= Duration::from_secs(2));
        let answer = a.unwrap().unwrap();
        assert!(answer.starts_with("Synthetic deadline"));
        assert!(
            !answer.contains("retired"),
            "disabled historical accounts are outside this check's configured source set"
        );
        assert!(b.unwrap().unwrap().starts_with("Synthetic deadline"));
        assert_eq!(
            reads
                .calendar_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            reads.inbox_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "overlapping calls must not duplicate an external refresh"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn unavailable_inbox_is_bounded_and_does_not_hide_calendar_facts() {
        let reads = Arc::new(FakeReads {
            calendar_calls: 0.into(),
            inbox_calls: 0.into(),
            slow_inbox: true,
        });
        let (_dir, provider, origin) = fixture(reads);
        let start = tokio::time::Instant::now();
        let result = provider.check(&origin).await.unwrap().unwrap();
        assert!(start.elapsed() <= Duration::from_secs(5));
        assert!(result.starts_with("Synthetic deadline"));
        assert!(result.contains("Still missing"));
        assert!(!result.contains("I found no"));
        provider
            .store
            .unbind_surface_owner(origin.owner.account())
            .unwrap();
        assert!(provider.check(&origin).await.is_err());
    }
}
