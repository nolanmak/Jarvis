//! Evidence-backed urgency classification. Sender emphasis alone is not urgency.
use augmentagent_store::{
    alert_schedule::AlertDetails,
    owner_alerts::{NewOwnerAlert, Urgency},
    Email, Store,
};
use serde::{Deserialize, Serialize};

pub fn context(
    store: &Store,
    email: &Email,
    now: i64,
) -> anyhow::Result<(Vec<MeetingContext>, Option<Urgency>)> {
    let meetings = store
        .owner_alert_meetings(email.account_entity_id.as_deref().unwrap_or_default(), now)?
        .into_iter()
        .map(|s| serde_json::from_str(&s))
        .collect::<Result<Vec<_>, _>>()?;
    let priority = store.owner_alert_priority(&crate::gmail::extract_bare_email(&email.from))?;
    Ok((meetings, priority))
}

pub fn prompt_context(store: &Store, email: &Email, now: i64) -> anyhow::Result<String> {
    let (meetings, priority) = context(store, email, now)?;
    Ok(format!("\nOwner-alert context: current UTC time {}; owner timezone {}. Priority override: {}.\n<meeting_context>{}</meeting_context>\nUse only supported request/deadline evidence. Include the alert field required by the system prompt.",
        chrono::DateTime::from_timestamp_millis(now).map(|d|d.to_rfc3339()).unwrap_or_default(),
        timezone(),priority.map(|p|p.as_str()).unwrap_or("none"),
        augmentagent_channel_core::prompt::sanitize_untrusted(&serde_json::to_string(&meetings)?)))
}

pub fn timezone() -> String {
    std::env::var("AUGMENTAGENT_ALERT_TIMEZONE").unwrap_or_else(|_| "UTC".into())
}

pub async fn backfill_tick<R: augmentagent_channel_core::Reasoner + ?Sized>(
    store: &Store,
    reasoner: &R,
    wiki_root: Option<std::path::PathBuf>,
    now: i64,
) -> anyhow::Result<usize> {
    let opts = augmentagent_channel_core::reasoner::triage_opts(wiki_root.clone());
    let mut assessed = 0;
    for email in store.owner_alert_backfill_candidates(now, 10)? {
        let result: anyhow::Result<()> = async {
            let hint = wiki_root
                .as_ref()
                .map(|root| {
                    let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                    augmentagent_wiki::WikiReader::new(&layout).triage_hint(&email)
                })
                .unwrap_or_default();
            let mut prompt =
                augmentagent_channel_core::prompt::triage_user_message(&email, "", &hint);
            prompt.push_str(&prompt_context(store, &email, now)?);
            let raw = reasoner.call(&opts, &prompt).await?;
            persist_assessment(store, &email, &raw, now)?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => assessed += 1,
            Err(error) => {
                store.record_owner_alert_assessment_failure(&email.message_id, now)?;
                tracing::warn!(message_id=%email.message_id,
                    "owner alert assessment failed; bounded retry recorded: {error:#}");
            }
        }
    }
    Ok(assessed)
}

pub async fn run_backfill<R: augmentagent_channel_core::Reasoner + 'static>(
    store: std::sync::Arc<Store>,
    reasoner: std::sync::Arc<R>,
    wiki_root: Option<std::path::PathBuf>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tokio::select! {
            _=shutdown.cancelled()=>return,
            _=timer.tick()=>{
                tokio::select!{
                    _=shutdown.cancelled()=>return,
                    result=backfill_tick(&store,reasoner.as_ref(),wiki_root.clone(),chrono::Utc::now().timestamp_millis())=>{
                        if let Err(error)=result {tracing::warn!("owner alert reevaluation failed: {error:#}");}
                    }
                }
            }
        }
    }
}

pub fn persist_assessment(
    store: &Store,
    email: &Email,
    raw: &str,
    now: i64,
) -> anyhow::Result<Option<String>> {
    let blob = augmentagent_channel_core::decision::extract_json_blob(raw)
        .ok_or_else(|| anyhow::anyhow!("missing triage JSON"))?;
    let value: serde_json::Value = serde_json::from_str(&blob)?;
    let Some(value) = value.get("alert").filter(|v| !v.is_null()) else {
        anyhow::ensure!(
            value.get("alert").is_some()
                || matches!(
                    value.get("decision").and_then(|v| v.as_str()),
                    Some("reply" | "flag" | "skip")
                ),
            "missing alert assessment or valid triage decision"
        );
        store.mark_owner_alert_assessed(&email.message_id, now)?;
        return Ok(None);
    };
    let a: Assessment = serde_json::from_value(value.clone())?;
    let (meetings, priority) = context(store, email, now)?;
    let zone = timezone();
    let plan = assess(email, &a, &meetings, priority, &zone, now)?;
    let delay = std::env::var("AUGMENTAGENT_ALERT_TEXT_DELAY_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n >= 0 && *n <= 86400)
        .unwrap_or(600)
        * 1000;
    let id = plan
        .map(|p| p.persist(store, email, &zone, now, delay))
        .transpose()?;
    store.mark_owner_alert_assessed(&email.message_id, now)?;
    Ok(id)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Assessment {
    pub action: String,
    pub request_evidence: String,
    pub reason: String,
    pub urgency: String,
    pub consequence_evidence: String,
    pub due: Option<String>,
    pub deadline_evidence: String,
    pub meeting_id: Option<String>,
    pub meeting_evidence: String,
    pub preparation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingContext {
    pub id: String,
    pub account_id: String,
    pub summary: String,
    pub start_ms: i64,
    pub participants: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AlertPlan {
    pub action: String,
    pub reason: String,
    pub evidence: String,
    pub urgency: Urgency,
    pub due_at_ms: Option<i64>,
    pub deadline_kind: String,
    pub meeting_id: Option<String>,
    pub meeting_start_ms: Option<i64>,
    pub meeting_url: Option<String>,
    pub reply_resolves: bool,
}

fn normal(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn supported(body: &str, evidence: &str) -> bool {
    evidence.trim().chars().count() >= 12 && normal(body).contains(&normal(evidence))
}

pub fn assess(
    email: &Email,
    a: &Assessment,
    meetings: &[MeetingContext],
    priority: Option<Urgency>,
    _timezone: &str,
    now: i64,
) -> anyhow::Result<Option<AlertPlan>> {
    if priority == Some(Urgency::Routine)
        || a.action.trim().len() < 8
        || a.reason.trim().is_empty()
        || !supported(&email.body, &a.request_evidence)
    {
        return Ok(None);
    }
    let sender = crate::gmail::extract_bare_email(&email.from).to_lowercase();
    let meeting = meetings.iter().find(|m| {
        Some(m.id.as_str()) == a.meeting_id.as_deref()
            && email.account_entity_id.as_deref() == Some(m.account_id.as_str())
            && m.participants
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&sender))
            && supported(&email.body, &a.meeting_evidence)
    });
    // A date or vague "tomorrow" alone never invents a clock time.
    let clock = regex::Regex::new(
        r"(?i)(\b\d{1,2}:\d{2}\b|\b\d{1,2}\s*(?:am|pm)\b|\bnoon\b|\bmidnight\b)",
    )?;
    let explicit_due =
        if supported(&email.body, &a.deadline_evidence) && clock.is_match(&a.deadline_evidence) {
            a.due
                .as_deref()
                .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                .map(|d| d.timestamp_millis())
        } else {
            None
        };
    let due = explicit_due.or_else(|| meeting.map(|m| m.start_ms));
    if due.is_some_and(|d| d <= now) {
        return Ok(None);
    }
    let consequential = supported(&email.body, &a.consequence_evidence);
    if priority.is_none()
        && due.is_none()
        && !(consequential && matches!(a.urgency.as_str(), "high" | "critical"))
    {
        return Ok(None);
    }
    let urgency = priority.unwrap_or_else(|| {
        if due.is_some_and(|d| d - now <= 3_600_000) || (a.urgency == "critical" && consequential) {
            Urgency::Critical
        } else {
            Urgency::High
        }
    });
    Ok(Some(AlertPlan {
        action: a.action.chars().take(400).collect(),
        reason: a.reason.chars().take(400).collect(),
        evidence: a.request_evidence.clone(),
        urgency,
        due_at_ms: due,
        deadline_kind: if explicit_due.is_some() {
            "explicit"
        } else if meeting.is_some() {
            "inferred_preparation"
        } else {
            "unknown"
        }
        .into(),
        meeting_id: meeting.map(|m| m.id.clone()),
        meeting_start_ms: meeting.map(|m| m.start_ms),
        meeting_url: meeting.and_then(|m| m.url.clone()),
        reply_resolves: !a.preparation && meeting.is_none(),
    }))
}

impl AlertPlan {
    pub fn persist(
        &self,
        store: &Store,
        email: &Email,
        timezone: &str,
        now: i64,
        delay_ms: i64,
    ) -> anyhow::Result<String> {
        let id = format!("gmail:{}", email.message_id);
        let mut source = reqwest::Url::parse("https://mail.google.com/mail/u/0/")?;
        if let Some(address) = store
            .get_active_gmail_accounts()?
            .into_iter()
            .find(|a| Some(a.entity_id.as_str()) == email.account_entity_id.as_deref())
            .map(|a| a.email)
            .filter(|s| !s.is_empty())
        {
            source.query_pairs_mut().append_pair("authuser", &address);
        }
        source.set_fragment(Some(&format!(
            "all/{}",
            email.thread_id.as_deref().unwrap_or(&email.message_id)
        )));
        let sender: String = email.from.chars().take(120).collect();
        let inserted = store.insert_owner_alert(
            &NewOwnerAlert {
                id: &id,
                source_url: source.as_str(),
                sender: &sender,
                action: &self.action,
                reason: &self.reason,
                urgency: self.urgency,
                due_at_ms: self.due_at_ms,
                timezone,
                meeting_id: self.meeting_id.as_deref(),
                expires_at_ms: self.due_at_ms.unwrap_or(now + 86_400_000),
                text_after_ms: now + delay_ms,
            },
            now,
        )?;
        store.attach_owner_alert_details(
            &id,
            &AlertDetails {
                message_id: email.message_id.clone(),
                thread_id: email.thread_id.clone(),
                account_id: email.account_entity_id.clone(),
                subject: email.subject.chars().take(180).collect(),
                evidence: self.evidence.clone(),
                deadline_kind: self.deadline_kind.clone(),
                meeting_start_ms: self.meeting_start_ms,
                meeting_url: self.meeting_url.clone(),
                reply_resolves: self.reply_resolves,
            },
            now,
        )?;
        if !inserted {
            // New calendar evidence may supply a formerly unknown deadline;
            // preserve acknowledgment/resolution and the original escalation timer.
            store.with_conn(|c|c.execute("UPDATE owner_alerts SET due_at_ms=?2,meeting_id=?3,
                urgency=?4,action=?5,reason=?6,expires_at_ms=COALESCE(?2,expires_at_ms),updated_at_ms=?7,
                text_after_ms=CASE WHEN ?4='critical' THEN MIN(text_after_ms,?7) ELSE text_after_ms END
                WHERE id=?1 AND state!='resolved'",augmentagent_store::rusqlite::params![id,self.due_at_ms,self.meeting_id,
                    self.urgency.as_str(),self.action,self.reason,now]))?;
            let metadata = AlertDetails {
                message_id: email.message_id.clone(),
                thread_id: email.thread_id.clone(),
                account_id: email.account_entity_id.clone(),
                subject: email.subject.chars().take(180).collect(),
                evidence: self.evidence.clone(),
                deadline_kind: self.deadline_kind.clone(),
                meeting_start_ms: self.meeting_start_ms,
                meeting_url: self.meeting_url.clone(),
                reply_resolves: self.reply_resolves,
            };
            store.with_conn(|c| {
                c.execute(
                    "UPDATE owner_alert_details SET payload=?2 WHERE alert_id=?1",
                    augmentagent_store::rusqlite::params![
                        id,
                        serde_json::to_string(&metadata).unwrap()
                    ],
                )
            })?;
        }
        store.enqueue_owner_alert_texts(now)?;
        Ok(id)
    }
}
