//! #1224 — calendar invite RSVP: the `calendar rsvp` command, the
//! auto-accept setting, the auto-accept pass the calendar poll runs, and the
//! approver arms that execute an RSVP card or an auto-accept notice's
//! Decline button. Planning (which invites, accept vs tentative) is pure and
//! lives in `augmentagent_channel_calendar::rsvp`; this module owns the side
//! effects.

use super::*;
use augmentagent_channel_calendar::rsvp::{
    auto_accept_account_key, auto_accept_enabled, auto_rsvp_message_id, plan_auto_accept,
    self_response, AccountEvents, AUTO_ACCEPT_GLOBAL_KEY,
};
use augmentagent_channel_calendar::{
    CalendarApi, CalendarError, CalendarEvent, ComposioCalendarClient, InviteDecision,
    MeetingPayload, RsvpRequest, RsvpResponse,
};
use chrono::{DateTime, Local, Utc};

/// `emails.kind` of a `calendar rsvp --post true` approval card.
pub(crate) const RSVP_CARD_KIND: &str = "rsvp";
/// `emails.kind` of an auto-accept receipt row (its notice's Decline button
/// targets it).
pub(crate) const RSVP_AUTO_KIND: &str = "rsvp_auto";
/// Terminal line appended to an auto-accept receipt once Decline ran.
const DECLINED_MARKER: &str = "\ndeclined via Discord at ";

const AUTO_ACCEPT_ENV: &str = "AUGMENTAGENT_CALENDAR_AUTO_ACCEPT";
const AUTO_ACCEPT_DAYS_ENV: &str = "AUGMENTAGENT_CALENDAR_AUTO_ACCEPT_DAYS";

fn composio_client() -> Result<ComposioCalendarClient> {
    let key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    Ok(ComposioCalendarClient::new(key))
}

fn when(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let s = start.with_timezone(&Local);
    format!(
        "{} {}–{}",
        s.format("%a %b %-d"),
        s.format("%H:%M"),
        end.with_timezone(&Local).format("%H:%M %Z")
    )
}

/// The effective auto-accept setting for one account, and where it came
/// from (`account` / `global` / `env` / `default`).
pub(crate) fn auto_accept_for(store: &Store, email: &str) -> (bool, &'static str) {
    let account = if email.trim().is_empty() {
        None
    } else {
        store.get_config(&auto_accept_account_key(email)).ok().flatten()
    };
    let global = store.get_config(AUTO_ACCEPT_GLOBAL_KEY).ok().flatten();
    let env = std::env::var(AUTO_ACCEPT_ENV).ok();
    auto_accept_enabled(account.as_deref(), global.as_deref(), env.as_deref())
}

/// `calendar set-auto-accept` — write the setting for one account, or the
/// global default when `--account` is absent.
pub(crate) fn run_set_auto_accept(store: Arc<Store>, on: bool, account: Option<String>) -> Result<()> {
    let value = if on { "on" } else { "off" };
    match account {
        Some(email) => {
            let accounts = store.get_active_gmail_accounts()?;
            let acct = accounts
                .iter()
                .find(|a| a.email.eq_ignore_ascii_case(email.trim()))
                .with_context(|| format!("no active account matching {email}"))?;
            store.set_config(&auto_accept_account_key(&acct.email), value)?;
            println!("calendar auto-accept {value} for {}", acct.email);
        }
        None => {
            store.set_config(AUTO_ACCEPT_GLOBAL_KEY, value)?;
            println!("calendar auto-accept {value} for every account without its own setting");
        }
    }
    Ok(())
}

/// `calendar status` — per-account auto-accept state and its source.
pub(crate) fn run_status(store: Arc<Store>, json: bool) -> Result<()> {
    let accounts = store.get_active_gmail_accounts()?;
    let rows: Vec<(String, bool, &'static str)> = accounts
        .iter()
        .map(|a| {
            let (on, source) = auto_accept_for(&store, &a.email);
            (a.email.clone(), on, source)
        })
        .collect();
    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|(email, on, source)| {
                serde_json::json!({"email": email, "auto_accept": on, "source": source})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"accounts": v}))?);
        return Ok(());
    }
    println!("calendar auto-accept (account setting > global > ${AUTO_ACCEPT_ENV} > off):");
    for (email, on, source) in rows {
        let email = if email.is_empty() { "(no address)".to_string() } else { email };
        println!("  {email}: {} ({source})", if on { "on" } else { "off" });
    }
    Ok(())
}

fn rsvp_human(req: &RsvpRequest, ev: &CalendarEvent) -> String {
    let p = MeetingPayload::from_event(ev, &req.entity_id, &req.calendar_id);
    let mut out = format!(
        "RSVP {} to calendar invite:\n• title:     {}\n",
        req.response.as_str(),
        req.summary
    );
    if let Some(p) = &p {
        out.push_str(&format!("• when:      {}\n", when(p.start, p.end)));
        if let Some(org) = &p.organizer_email {
            out.push_str(&format!("• organizer: {org}\n"));
        }
    }
    out.push_str(&format!("• account:   {}\n", req.account_email));
    if req.event_id != ev.id {
        out.push_str("• scope:     the whole recurring series\n");
    }
    out
}

/// `calendar rsvp` — answer an invite. Accept/tentative run immediately (a
/// direct owner instruction; low blast radius, one click to change).
/// Decline is visible to the organizer, so it only previews unless
/// `--post true` raises an approval card; `--post true` cards any response.
pub(crate) async fn run_rsvp(
    store: Arc<Store>,
    event_id: String,
    response: String,
    account: Option<String>,
    series: bool,
    post: bool,
) -> Result<()> {
    let response = RsvpResponse::parse(&response)
        .context("--response must be accepted, declined, or tentative")?;
    let client = composio_client()?;
    let accounts = store.get_active_gmail_accounts()?;
    let candidates: Vec<_> = match &account {
        Some(email) => {
            let acct = accounts
                .iter()
                .find(|a| a.email.eq_ignore_ascii_case(email.trim()))
                .with_context(|| format!("no active account matching {email}"))?;
            vec![acct.clone()]
        }
        None => accounts.clone(),
    };
    let mut found = None;
    let mut errors = Vec::new();
    for acct in &candidates {
        match client.get_event(&acct.entity_id, "primary", &event_id).await {
            Ok(ev) if self_response(&ev).is_some() || ev.organizer.as_ref().and_then(|o| o.self_) == Some(true) => {
                found = Some((acct.clone(), ev));
                break;
            }
            Ok(_) => errors.push(format!("{}: not an attendee", acct.email)),
            Err(e) => errors.push(format!("{}: {e}", acct.email)),
        }
    }
    let Some((acct, ev)) = found else {
        anyhow::bail!(
            "event {event_id} was not found as an invite on {} — use the event_id from \
             `calendar list-events --json true` ({})",
            if account.is_some() { "that account" } else { "any connected account" },
            errors.join("; ")
        );
    };
    if ev.organizer.as_ref().and_then(|o| o.self_) == Some(true) {
        anyhow::bail!("{} organizes this event; an organizer does not RSVP", acct.email);
    }
    let target = match (&ev.recurring_event_id, series) {
        (Some(master), true) if !master.is_empty() => master.clone(),
        _ => ev.id.clone(),
    };
    let summary = MeetingPayload::from_event(&ev, &acct.entity_id, "primary")
        .map(|p| p.summary)
        .unwrap_or_else(|| "(no title)".into());
    let req = RsvpRequest {
        entity_id: acct.entity_id.clone(),
        account_email: acct.email.clone(),
        calendar_id: "primary".into(),
        event_id: target,
        response,
        summary,
    };
    let human = rsvp_human(&req, &ev);

    if !post && response == RsvpResponse::Declined {
        println!("{human}");
        println!(
            "(preview only — a decline notifies the organizer, so it needs approval: \
             re-run with --post true to surface the Discord card)"
        );
        return Ok(());
    }
    if !post {
        client
            .respond(&req.entity_id, &req.calendar_id, &req.event_id, response, true)
            .await
            .map_err(|e| anyhow::anyhow!("rsvp failed: {e}"))?;
        println!("RSVP sent: {} for \"{}\" on {}", response.as_str(), req.summary, req.account_email);
        return Ok(());
    }

    let token = std::env::var("DISCORD_BOT_TOKEN")
        .context("DISCORD_BOT_TOKEN required for --post (set it in the daemon's .env)")?;
    let cid: u64 = std::env::var("DISCORD_CHANNEL_ID")
        .context("DISCORD_CHANNEL_ID required for --post")?
        .parse()
        .context("DISCORD_CHANNEL_ID must be numeric")?;
    let payload = serde_json::to_string_pretty(&req)?;
    let message_id = format!("gcal-rsvp:{}", uuid::Uuid::new_v4());
    let inbound = augmentagent_store::Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: message_id.clone(),
        thread_id: None,
        from: acct.email.clone(),
        subject: format!("RSVP {}: {}", response.as_str(), req.summary),
        body: payload.clone(),
        date: Utc::now().to_rfc3339(),
        account_entity_id: Some(acct.entity_id.clone()),
        platform: "gcal".into(),
        kind: RSVP_CARD_KIND.into(),
    };
    store.upsert_email(&inbound).context("upsert rsvp proposal row")?;
    let action_id = store
        .log_action(
            &message_id,
            None,
            &inbound.from,
            &inbound.subject,
            Some(&payload),
            Some(&human),
            ActionStatus::Pending,
        )
        .context("log rsvp action row")?;
    let http = serenity::http::Http::new(&token);
    serenity::all::ChannelId::new(cid)
        .send_message(&http, approval_message(&action_id, &inbound, &human, 0))
        .await
        .context("post rsvp approval card")?;
    println!("approval card posted: action_id={action_id}");
    println!("{human}");
    Ok(())
}

/// Where an auto-accept notice goes: Discord with a Decline button when the
/// bot is configured, else the routed calendar-alert sink as plain text.
pub(crate) enum NoticeSurface {
    Discord { http: serenity::http::Http, channel: u64 },
    Text(Arc<dyn augmentagent_channel_calendar::AlertSink>),
    None,
}

impl NoticeSurface {
    pub(crate) fn from_env() -> Self {
        let token = std::env::var("DISCORD_BOT_TOKEN").ok().filter(|t| !t.trim().is_empty());
        let channel = std::env::var("DISCORD_CHANNEL_ID").ok().and_then(|c| c.trim().parse().ok());
        if let (Some(token), Some(channel)) = (token, channel) {
            return Self::Discord { http: serenity::http::Http::new(&token), channel };
        }
        match build_calendar_alert_sink() {
            Some(sink) => Self::Text(sink),
            None => Self::None,
        }
    }
}

pub(crate) fn notice_text(d: &InviteDecision) -> String {
    let head = match d.response {
        RsvpResponse::Tentative => "🤔 **Auto-RSVP: tentative** (time conflict)",
        _ => "✅ **Auto-accepted invite**",
    };
    let mut out = format!(
        "{head}\n**{}** — {}{}\nfrom {} · on {}",
        d.summary,
        when(d.start, d.end),
        if d.recurring { " (recurring series — answered for every occurrence)" } else { "" },
        d.organizer.as_deref().unwrap_or("(unknown organizer)"),
        d.account_email,
    );
    if !d.conflicts.is_empty() {
        out.push_str(&format!("\n⚠ overlaps {}", d.conflicts.join(", ")));
    }
    out
}

#[derive(Debug, Default)]
pub(crate) struct AutoAcceptReport {
    pub planned: usize,
    pub answered: usize,
    pub already_handled: usize,
    pub failed: usize,
}

/// The auto-accept pass the calendar poll runs (#1224). Lists the next
/// `AUGMENTAGENT_CALENDAR_AUTO_ACCEPT_DAYS` (default 60) on every active
/// account, plans answers for pending invites on the accounts with the
/// setting on, writes each answer once (dedup row keyed by account+event),
/// and posts a notice per answer. Dry-run prints the plan and writes nothing.
pub(crate) async fn auto_accept_pass(
    store: &Store,
    api: &dyn CalendarApi,
    surface: &NoticeSurface,
    dry_run: bool,
) -> Result<AutoAcceptReport> {
    let accounts = store.get_active_gmail_accounts()?;
    let settings: Vec<bool> = accounts.iter().map(|a| auto_accept_for(store, &a.email).0).collect();
    let mut report = AutoAcceptReport::default();
    if !settings.iter().any(|on| *on) {
        return Ok(report);
    }
    let days = std::env::var(AUTO_ACCEPT_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(60)
        .clamp(1, 180);
    let now = Utc::now();
    let mut listed: Vec<(usize, Vec<CalendarEvent>)> = Vec::new();
    for (i, acct) in accounts.iter().enumerate() {
        match api
            .list_events(&acct.entity_id, "primary", now, now + chrono::Duration::days(days))
            .await
        {
            Ok(events) => listed.push((i, events)),
            Err(e) => warn!(account = %acct.email, "calendar auto-accept: list failed, account skipped: {e}"),
        }
    }
    let inputs: Vec<AccountEvents<'_>> = listed
        .iter()
        .map(|(i, events)| AccountEvents {
            entity_id: &accounts[*i].entity_id,
            email: &accounts[*i].email,
            auto_accept: settings[*i],
            events,
        })
        .collect();
    let plan = plan_auto_accept(&inputs, now);
    report.planned = plan.len();
    for d in &plan {
        let message_id = auto_rsvp_message_id(&d.entity_id, &d.target_event_id);
        if store.email_first_seen_at(&message_id)?.is_some() {
            report.already_handled += 1;
            continue;
        }
        let text = notice_text(d);
        if dry_run {
            println!("[dry-run] would auto-RSVP {}:\n{text}\n", d.response.as_str());
            continue;
        }
        if let Err(e) = api
            .respond(&d.entity_id, "primary", &d.target_event_id, d.response, true)
            .await
        {
            report.failed += 1;
            warn!(account = %d.account_email, event = %d.target_event_id, "calendar auto-accept: rsvp failed, will retry next poll: {e}");
            continue;
        }
        report.answered += 1;
        let action_id = match record_auto_rsvp(store, d, &message_id, &text) {
            Ok(id) => id,
            Err(e) => {
                // The answer is written; only the receipt/Decline button is
                // missing. Say so in the notice rather than staying silent.
                warn!(event = %d.target_event_id, "calendar auto-accept: receipt not saved: {e:#}");
                post_text(surface, &format!("{text}\n(receipt not saved — decline in Google Calendar if needed)")).await;
                continue;
            }
        };
        match surface {
            NoticeSurface::Discord { http, channel } => {
                let msg = augmentagent_approval_discord::auto_rsvp_notice_message(&action_id, &text);
                if let Err(e) = serenity::all::ChannelId::new(*channel).send_message(http, msg).await {
                    warn!(%action_id, "calendar auto-accept: notice not posted: {e}");
                }
            }
            _ => post_text(surface, &text).await,
        }
    }
    Ok(report)
}

async fn post_text(surface: &NoticeSurface, text: &str) {
    match surface {
        NoticeSurface::Discord { http, channel } => {
            let msg = serenity::all::CreateMessage::new().content(text);
            if let Err(e) = serenity::all::ChannelId::new(*channel).send_message(http, msg).await {
                warn!("calendar auto-accept: notice not posted: {e}");
            }
        }
        NoticeSurface::Text(sink) => {
            if let Err(e) = sink.send(text).await {
                warn!("calendar auto-accept: notice not posted: {e:#}");
            }
        }
        NoticeSurface::None => info!("calendar auto-accept: {text}"),
    }
}

/// Persist the dedup + receipt rows for one written answer: the `emails`
/// row (its body is the request that was executed) and a `sent` action row,
/// which the notice's Decline button targets. `sent` keeps it out of the
/// pending-card nudge queue.
fn record_auto_rsvp(store: &Store, d: &InviteDecision, message_id: &str, text: &str) -> Result<String> {
    let req = d.request();
    let body = serde_json::to_string_pretty(&req)?;
    let email = augmentagent_store::Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: message_id.to_string(),
        thread_id: None,
        from: d.account_email.clone(),
        subject: format!("Auto-RSVP {}: {}", d.response.as_str(), d.summary),
        body: body.clone(),
        date: Utc::now().to_rfc3339(),
        account_entity_id: Some(d.entity_id.clone()),
        platform: "gcal".into(),
        kind: RSVP_AUTO_KIND.into(),
    };
    store.upsert_email(&email)?;
    store.mark_email_processed(message_id, TriageResult::Skip)?;
    let action_id = store.log_action(
        message_id,
        None,
        &email.from,
        &email.subject,
        Some(&body),
        Some(text),
        ActionStatus::Sent,
    )?;
    Ok(action_id)
}

fn parse_request(action: &augmentagent_store::ActionWithEmail) -> Result<RsvpRequest, String> {
    serde_json::from_str(&action.email.body).map_err(|e| format!("rsvp payload parse failed: {e}"))
}

fn describe_calendar_error(e: CalendarError) -> String {
    match e {
        CalendarError::Forbidden { message } => format!(
            "calendar write scope missing — re-consent the Google connection with \
             calendar.events: {message}"
        ),
        other => format!("rsvp: {other}"),
    }
}

impl ReplyApprover {
    /// Approve on a `calendar rsvp --post true` card: write the stored
    /// response.
    pub(crate) async fn approve_gcal_rsvp(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let req = match parse_request(&action) {
            Ok(r) => r,
            Err(message) => {
                let _ = self.store.update_action_status(action_id, ActionStatus::Error, None, Some(&message));
                return ApprovalActionOutcome::Failed { message };
            }
        };
        match self
            .calendar
            .respond(&req.entity_id, &req.calendar_id, &req.event_id, req.response, true)
            .await
        {
            Ok(()) => {
                let body = format!(
                    "{}\nresponded: {}",
                    action.action.draft_body.clone().unwrap_or_default(),
                    req.response.as_str()
                );
                let _ = self.store.update_action_status(action_id, ActionStatus::Sent, Some(&body), None);
                let _ = self.store.mark_email_processed(&action.email.message_id, TriageResult::Reply);
                ApprovalActionOutcome::CalendarResponded {
                    account: req.account_email,
                    response: req.response.as_str().into(),
                    summary: req.summary,
                }
            }
            Err(e) => {
                let message = describe_calendar_error(e);
                let _ = self.store.update_action_status(action_id, ActionStatus::Error, None, Some(&message));
                ApprovalActionOutcome::Failed { message }
            }
        }
    }

    /// Decline on an auto-accept notice: replace the automatic answer with
    /// a decline (the organizer is notified). Idempotent per receipt row.
    pub(crate) async fn run_calendar_decline(&self, action_id: &str) -> ApprovalActionOutcome {
        let action = match self.store.get_action_with_email(action_id) {
            Ok(Some(a)) => a,
            Ok(None) => return ApprovalActionOutcome::NotFound,
            Err(e) => return ApprovalActionOutcome::Failed { message: format!("load action: {e}") },
        };
        if action.email.platform != "gcal" || action.email.kind != RSVP_AUTO_KIND {
            return ApprovalActionOutcome::Failed {
                message: "Decline only applies to a calendar auto-accept notice".into(),
            };
        }
        let draft = action.action.draft_body.clone().unwrap_or_default();
        if draft.contains(DECLINED_MARKER) {
            return ApprovalActionOutcome::AlreadyResolved { status: "declined".into(), detail: None };
        }
        let req = match parse_request(&action) {
            Ok(r) => r,
            Err(message) => return ApprovalActionOutcome::Failed { message },
        };
        if let Err(e) = self
            .calendar
            .respond(&req.entity_id, &req.calendar_id, &req.event_id, RsvpResponse::Declined, true)
            .await
        {
            return ApprovalActionOutcome::Failed { message: describe_calendar_error(e) };
        }
        let body = format!("{draft}{DECLINED_MARKER}{}", Utc::now().to_rfc3339());
        if let Err(e) = self.store.update_action_status(action_id, ActionStatus::Sent, Some(&body), None) {
            return ApprovalActionOutcome::Failed {
                message: format!("the invite was declined, but recording it failed: {e}"),
            };
        }
        ApprovalActionOutcome::CalendarResponded {
            account: req.account_email,
            response: "declined".into(),
            summary: req.summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(response: RsvpResponse) -> InviteDecision {
        InviteDecision {
            entity_id: "ent".into(),
            account_email: "owner@example.test".into(),
            target_event_id: "evt-1".into(),
            recurring: false,
            summary: "Synthetic sync".into(),
            start: "2026-10-20T15:00:00Z".parse().unwrap(),
            end: "2026-10-20T16:00:00Z".parse().unwrap(),
            organizer: Some("organizer@example.test".into()),
            response,
            conflicts: if response == RsvpResponse::Tentative {
                vec!["\"Busy\" (Tue 11:00–12:00)".into()]
            } else {
                vec![]
            },
        }
    }

    /// The provider side of one RSVP through Composio's proxy; returns the
    /// PATCH mock (expected exactly once).
    async fn mock_proxy_rsvp(
        server: &mut mockito::ServerGuard,
        event_id: &str,
        response: &str,
        send_updates: &str,
    ) -> mockito::Mock {
        server
            .mock("GET", "/api/v3/connected_accounts")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(r#"{"items":[{"id":"ca_test"}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/api/v3/tools/execute/proxy")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"method": "GET"})))
            .with_status(200)
            .with_body(serde_json::json!({"status": 200, "data": {"id": event_id,
                "attendees": [{"email": "owner@example.test", "self": true, "responseStatus": "accepted"}]}}).to_string())
            .create_async()
            .await;
        server
            .mock("POST", "/api/v3/tools/execute/proxy")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "method": "PATCH",
                "endpoint": format!("https://www.googleapis.com/calendar/v3/calendars/primary/events/{event_id}"),
                "parameters": [{"name": "sendUpdates", "value": send_updates, "type": "query"}],
                "body": {"attendeesOmitted": true, "attendees": [{"email": "owner@example.test", "responseStatus": response}]},
            })))
            .with_status(200)
            .with_body(serde_json::json!({"status": 200, "data": {"id": event_id,
                "attendees": [{"email": "owner@example.test", "self": true, "responseStatus": response}]}}).to_string())
            .expect(1)
            .create_async()
            .await
    }

    fn store() -> (Arc<Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Arc::new(Store::open(dir.path().join("t.db")).unwrap()), dir)
    }

    #[test]
    fn notice_says_what_was_answered_and_why() {
        let t = notice_text(&decision(RsvpResponse::Accepted));
        assert!(t.contains("Auto-accepted") && t.contains("Synthetic sync") && t.contains("organizer@example.test"), "{t}");
        let t = notice_text(&decision(RsvpResponse::Tentative));
        assert!(t.contains("tentative") && t.contains("overlaps \"Busy\""), "{t}");
    }

    #[test]
    fn auto_rsvp_receipt_is_sent_status_and_deduplicates() {
        let (store, _d) = store();
        let d = decision(RsvpResponse::Accepted);
        let mid = auto_rsvp_message_id(&d.entity_id, &d.target_event_id);
        assert!(store.email_first_seen_at(&mid).unwrap().is_none());
        let id = record_auto_rsvp(&store, &d, &mid, "notice").unwrap();
        assert!(store.email_first_seen_at(&mid).unwrap().is_some());
        let row = store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(row.action.status, "sent", "a pending row would enter the nudge queue");
        assert_eq!(row.email.kind, RSVP_AUTO_KIND);
        let req: RsvpRequest = serde_json::from_str(&row.email.body).unwrap();
        assert_eq!(req.event_id, "evt-1");
    }

    #[test]
    fn setting_resolution_reads_account_then_global() {
        let (store, _d) = store();
        store.set_config(AUTO_ACCEPT_GLOBAL_KEY, "on").unwrap();
        assert_eq!(auto_accept_for(&store, "owner@example.test"), (true, "global"));
        store.set_config(&auto_accept_account_key("Owner@Example.test"), "off").unwrap();
        assert_eq!(auto_accept_for(&store, "owner@example.test"), (false, "account"));
    }

    /// Decline on a notice writes `declined` to the provider once; a second
    /// click is AlreadyResolved and makes no provider call.
    #[tokio::test]
    async fn decline_button_declines_once() {
        let (store, _d) = store();
        let d = decision(RsvpResponse::Accepted);
        let mid = auto_rsvp_message_id(&d.entity_id, &d.target_event_id);
        let id = record_auto_rsvp(&store, &d, &mid, "notice").unwrap();
        let mut server = mockito::Server::new_async().await;
        let m = mock_proxy_rsvp(&mut server, "evt-1", "declined", "all").await;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar = Arc::new(ComposioCalendarClient::new("k".into()).with_base_url(server.url()));
        let out = approver.calendar_decline(&id).await;
        assert!(matches!(out, ApprovalActionOutcome::CalendarResponded { ref response, .. } if response == "declined"), "{out:?}");
        let again = approver.calendar_decline(&id).await;
        assert!(matches!(again, ApprovalActionOutcome::AlreadyResolved { .. } | ApprovalActionOutcome::Failed { .. }), "{again:?}");
        m.assert_async().await;
    }

    /// Approve on a `calendar rsvp --post true` card writes the stored
    /// response and records it.
    #[tokio::test]
    async fn rsvp_card_approve_writes_the_response() {
        let (store, _d) = store();
        let req = RsvpRequest {
            entity_id: "ent".into(),
            account_email: "owner@example.test".into(),
            calendar_id: "primary".into(),
            event_id: "evt-2".into(),
            response: RsvpResponse::Declined,
            summary: "Synthetic".into(),
        };
        let body = serde_json::to_string(&req).unwrap();
        let email = augmentagent_store::Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "gcal-rsvp:test".into(),
            thread_id: None,
            from: "owner@example.test".into(),
            subject: "RSVP declined: Synthetic".into(),
            body: body.clone(),
            date: Utc::now().to_rfc3339(),
            account_entity_id: Some("ent".into()),
            platform: "gcal".into(),
            kind: RSVP_CARD_KIND.into(),
        };
        store.upsert_email(&email).unwrap();
        let id = store
            .log_action(&email.message_id, None, &email.from, &email.subject, Some(&body), Some("RSVP declined"), ActionStatus::Pending)
            .unwrap();
        let mut server = mockito::Server::new_async().await;
        let m = mock_proxy_rsvp(&mut server, "evt-2", "declined", "all").await;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar = Arc::new(ComposioCalendarClient::new("k".into()).with_base_url(server.url()));
        let out = approver.approve(&id).await;
        assert!(matches!(out, ApprovalActionOutcome::CalendarResponded { .. }), "{out:?}");
        m.assert_async().await;
        assert_eq!(store.get_action_with_email(&id).unwrap().unwrap().action.status, "sent");
    }

    /// Owner-authorized live round trip for #1456 + #1224 on two of the
    /// owner's own accounts (QA config from env, nothing hardcoded):
    /// organizer creates a short future event inviting itself and a second
    /// owned account through the real approver; the organizer ends up
    /// accepted; auto-accept answers the invitee; Decline flips it;
    /// `calendar rsvp` sets tentative; the event is always deleted.
    #[tokio::test]
    #[ignore = "owner-authorized live self-invite; needs JARVIS_CALENDAR_QA_ACCOUNTS_JSON (organizer, invitee) and COMPOSIO_API_KEY"]
    async fn live_rsvp_round_trip() -> Result<()> {
        #[derive(serde::Deserialize)]
        struct Qa { email: String, entity_id: String }
        augmentagent_channel_core::state_dir::isolate_for_tests();
        let key = std::env::var("COMPOSIO_API_KEY")?;
        let qa: Vec<Qa> = serde_json::from_str(&std::env::var("JARVIS_CALENDAR_QA_ACCOUNTS_JSON")?)?;
        anyhow::ensure!(qa.len() >= 2, "configure organizer then invitee");
        let (org, inv) = (&qa[0], &qa[1]);
        let (store, _d) = store();
        store.with_conn(|c| {
            for (i, a) in [org, inv].iter().enumerate() {
                c.execute(
                    "INSERT INTO gmail_accounts (id, connectionId, email, entityId, active, createdAt) VALUES (?1, 'qa', ?2, ?3, 1, 0)",
                    rusqlite::params![format!("qa-{i}"), a.email, a.entity_id],
                )?;
            }
            Ok(())
        })?;
        store.set_config(&auto_accept_account_key(&inv.email), "on")?;
        store.set_config(&auto_accept_account_key(&org.email), "off")?;
        let api = ComposioCalendarClient::new(key.clone());

        // Inside a short auto-accept horizon (run with
        // AUGMENTAGENT_CALENDAR_AUTO_ACCEPT_DAYS=2) so real invites further
        // out are never touched; a 5-minute slot.
        let start = Utc::now() + chrono::Duration::hours(26);
        let start = DateTime::from_timestamp(start.timestamp() - start.timestamp() % 300, 0).unwrap();
        let summary = format!("[TEST #1224] {}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
        let body = serde_json::json!({"summary": summary, "start_datetime": start.to_rfc3339(),
            "duration_minutes": 5, "attendees": [inv.email], "create_meeting_room": false,
            "description": "Owner-authorized RSVP QA. Temporary; deleted after verification."}).to_string();
        let email = augmentagent_store::Email {
            message_id: format!("gcal-create:{}", uuid::Uuid::new_v4()), thread_id: None,
            from: org.email.clone(), to: String::new(), cc: String::new(), attachments: Vec::new(),
            subject: summary.clone(), body: body.clone(), date: start.to_rfc3339(),
            account_entity_id: Some(org.entity_id.clone()), platform: "gcal".into(), kind: "create_event".into(),
        };
        store.upsert_email(&email)?;
        let action = store.log_action(&email.message_id, None, &email.from, &email.subject, Some(&body), Some("RSVP QA"), ActionStatus::Pending)?;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar = Arc::new(ComposioCalendarClient::new(key.clone()));
        let ApprovalActionOutcome::CalendarCreated { event_id, .. } = approver.approve(&action).await else {
            anyhow::bail!("create failed; search the calendar for {summary} before retrying");
        };
        println!("created {event_id} ({summary})");

        let status_on = |events: &[CalendarEvent]| -> Option<String> {
            events.iter().find(|e| e.summary.as_deref() == Some(summary.as_str()))
                .and_then(|e| self_response(e).map(str::to_string))
        };
        let window = (start - chrono::Duration::minutes(1), start + chrono::Duration::minutes(6));
        let verify: Result<()> = async {
            let receipt = store.get_action_with_email(&action)?.unwrap().action.draft_body.unwrap_or_default();
            println!("create receipt:\n{receipt}");
            let org_status = status_on(&api.list_events(&org.entity_id, "primary", window.0, window.1).await?);
            println!("#1456 organizer self status: {org_status:?}");
            anyhow::ensure!(org_status.as_deref().is_none_or(|s| s == "accepted"), "organizer left at {org_status:?}");

            // Invitee copy can take a few seconds to appear.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
            loop {
                let s = status_on(&api.list_events(&inv.entity_id, "primary", window.0, window.1).await?);
                if s.as_deref() == Some("needsAction") { break; }
                anyhow::ensure!(tokio::time::Instant::now() < deadline, "invite never reached invitee: {s:?}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            // Guard: the plan must be exactly this test invite, or nothing real
            // gets answered.
            let preview = auto_accept_pass(&store, &api, &NoticeSurface::None, true).await?;
            anyhow::ensure!(preview.planned == 1, "plan touches more than the test invite: {preview:?}");
            let report = auto_accept_pass(&store, &api, &NoticeSurface::None, false).await?;
            println!("auto-accept report: {report:?}");
            let s = status_on(&api.list_events(&inv.entity_id, "primary", window.0, window.1).await?);
            println!("#1224 invitee after auto-accept: {s:?}");
            anyhow::ensure!(matches!(s.as_deref(), Some("accepted" | "tentative")), "auto-accept did not answer: {s:?}");
            let again = auto_accept_pass(&store, &api, &NoticeSurface::None, false).await?;
            anyhow::ensure!(again.answered == 0, "second pass re-answered: {again:?}");

            let receipt_row = store.get_action_with_email(&approver_auto_row(&store, &inv.entity_id)?)?.unwrap();
            let out = approver.calendar_decline(&receipt_row.action.id).await;
            println!("decline outcome: {out:?}");
            anyhow::ensure!(matches!(out, ApprovalActionOutcome::CalendarResponded { .. }), "{out:?}");
            let s = status_on(&api.list_events(&inv.entity_id, "primary", window.0, window.1).await?);
            println!("#1224 invitee after Decline: {s:?}");
            anyhow::ensure!(s.as_deref() == Some("declined"));

            let inv_event = api.list_events(&inv.entity_id, "primary", window.0, window.1).await?
                .into_iter().find(|e| e.summary.as_deref() == Some(summary.as_str())).context("invitee copy")?;
            run_rsvp(store.clone(), inv_event.id.clone(), "maybe".into(), Some(inv.email.clone()), false, false).await?;
            let s = status_on(&api.list_events(&inv.entity_id, "primary", window.0, window.1).await?);
            println!("#1224 invitee after `calendar rsvp --response maybe`: {s:?}");
            anyhow::ensure!(s.as_deref() == Some("tentative"));
            Ok(())
        }.await;

        let cleanup = api_delete(&key, &org.entity_id, &event_id).await;
        println!("cleanup: {cleanup:?}");
        cleanup?;
        verify
    }

    fn approver_auto_row(store: &Store, entity_id: &str) -> Result<String> {
        Ok(store.with_conn(|c| c.query_row(
            "SELECT a.id FROM actions a JOIN emails e ON e.messageId = a.messageId \
             WHERE e.kind = 'rsvp_auto' AND e.accountEntityId = ?1 ORDER BY a.createdAt DESC LIMIT 1",
            rusqlite::params![entity_id], |r| r.get::<_, String>(0)))?)
    }

    async fn api_delete(key: &str, entity: &str, event_id: &str) -> Result<()> {
        let v: serde_json::Value = reqwest::Client::new()
            .post("https://backend.composio.dev/api/v3/tools/execute/GOOGLECALENDAR_DELETE_EVENT")
            .header("x-api-key", key)
            .json(&serde_json::json!({"user_id": entity, "arguments": {"calendar_id": "primary", "event_id": event_id}}))
            .send().await?.json().await?;
        anyhow::ensure!(v["successful"] == true, "delete failed: {}", v["error"]);
        Ok(())
    }

}
