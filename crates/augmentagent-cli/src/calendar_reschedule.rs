//! #2039 — `calendar reschedule`: move an event the owner organizes to a new
//! time. The command only previews or posts an approval card; Google is
//! changed only when the owner clicks Approve (`approve_gcal_reschedule`).
//! The checks and the PATCH live in
//! `augmentagent_channel_calendar::reschedule`.

use super::*;
use augmentagent_channel_calendar::reschedule::plan_reschedule;
use augmentagent_channel_calendar::{
    AlertCandidate, CalendarApi, CalendarError, ComposioCalendarClient, RescheduleOutcome,
    RescheduleRequest, RESCHEDULE_CARD_KIND,
};
use chrono::{DateTime, Local, Utc};

/// "Tue Nov 18 15:00–15:30 EST" in the host's local time.
pub(crate) fn when_local(start: &str, end: &str) -> String {
    let parse = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Local))
    };
    match (parse(start), parse(end)) {
        (Some(s), Some(e)) => format!(
            "{} {}–{}",
            s.format("%a %b %-d"),
            s.format("%H:%M"),
            e.format("%H:%M %Z")
        ),
        _ => format!("{start} – {end}"),
    }
}

pub(crate) fn reschedule_human(req: &RescheduleRequest, conflicts: &[String]) -> String {
    let mut out = format!(
        "Move calendar event ({} on Approve):\n\
         • title:   {}\n\
         • from:    {}\n\
         • to:      {}\n\
         • guests:  {}\n\
         • account: {}\n",
        if req.notify {
            "guests are emailed the new time"
        } else {
            "guests are NOT emailed"
        },
        req.summary,
        when_local(&req.from_start, &req.from_end),
        when_local(&req.new_start, &req.new_end),
        req.guest_count,
        req.account_email,
    );
    for line in conflicts {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Busy events at the new time, other than the one being moved.
async fn conflicts_at(client: &ComposioCalendarClient, req: &RescheduleRequest) -> Vec<String> {
    let (Ok(s), Ok(e)) = (
        DateTime::parse_from_rfc3339(&req.new_start),
        DateTime::parse_from_rfc3339(&req.new_end),
    ) else {
        return Vec::new();
    };
    let (s, e) = (s.with_timezone(&Utc), e.with_timezone(&Utc));
    match client
        .list_events(&req.entity_id, &req.calendar_id, s, e)
        .await
    {
        Ok(events) => events
            .iter()
            .filter(|ev| ev.id != req.event_id)
            .filter_map(|ev| AlertCandidate::from_event(ev, &req.entity_id, &req.calendar_id))
            .filter(|c| !(c.all_day || c.transparent || c.declined_by_self))
            .filter(|c| c.payload.start < e && s < c.payload.end)
            .map(|c| {
                format!(
                    "⚠ conflicts with \"{}\" ({}–{})",
                    c.payload.summary,
                    c.payload.start.with_timezone(&Local).format("%H:%M"),
                    c.payload.end.with_timezone(&Local).format("%H:%M"),
                )
            })
            .collect(),
        Err(e) => vec![format!("(conflict check unavailable: {e})")],
    }
}

/// `calendar reschedule` — preview, or with `--post true` raise the card.
pub(crate) async fn run_reschedule(
    store: Arc<Store>,
    event_id: String,
    start: String,
    duration_min: Option<i64>,
    account: Option<String>,
    notify: bool,
    post: bool,
) -> Result<()> {
    let new_start = DateTime::parse_from_rfc3339(&start).context(
        "--start must be RFC3339 with offset, e.g. 2026-07-10T15:00:00-04:00 \
         (compute the date from `calendar list-events`'s now: header)",
    )?;
    let key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let client = ComposioCalendarClient::new(key);
    let accounts = store.get_active_gmail_accounts()?;
    let candidates: Vec<_> = match &account {
        Some(email) => vec![accounts
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(email.trim()))
            .with_context(|| format!("no active account matching {email}"))?
            .clone()],
        None => accounts.clone(),
    };
    // Prefer the account that organizes the event: an invite between two
    // owned accounts shares its id, and only the organizer copy can move.
    let mut organizer = None;
    let mut attendee = None;
    let mut errors = Vec::new();
    for acct in &candidates {
        match client
            .get_event(&acct.entity_id, "primary", &event_id)
            .await
        {
            Ok(ev) if ev.organizer.as_ref().and_then(|o| o.self_) == Some(true) => {
                organizer = Some((acct.clone(), ev));
                break;
            }
            Ok(ev) => {
                attendee.get_or_insert((acct.clone(), ev));
            }
            Err(e) => errors.push(format!("{}: {e}", acct.email)),
        }
    }
    let Some((acct, ev)) = organizer.or(attendee) else {
        anyhow::bail!(
            "event {event_id} was not found on {} — use the event_id from \
             `calendar list-events --json true` ({})",
            if account.is_some() {
                "that account"
            } else {
                "any connected account"
            },
            errors.join("; ")
        );
    };
    let req = plan_reschedule(
        &ev,
        &acct.entity_id,
        &acct.email,
        "primary",
        new_start,
        duration_min,
        notify,
        Utc::now(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    let conflicts = conflicts_at(&client, &req).await;
    let human = reschedule_human(&req, &conflicts);
    let payload = serde_json::to_string_pretty(&req)?;

    if !post {
        println!("{human}");
        println!("(preview only — nothing written; re-run with --post true to surface the approval card)");
        return Ok(());
    }

    let token = std::env::var("DISCORD_BOT_TOKEN")
        .context("DISCORD_BOT_TOKEN required for --post (set it in the daemon's .env)")?;
    let cid: u64 = std::env::var("DISCORD_CHANNEL_ID")
        .context("DISCORD_CHANNEL_ID required for --post")?
        .parse()
        .context("DISCORD_CHANNEL_ID must be numeric")?;
    let (inbound, action_id) = record_card(&store, &req, &payload, &human)?;
    let http = serenity::http::Http::new(&token);
    serenity::all::ChannelId::new(cid)
        .send_message(&http, approval_message(&action_id, &inbound, &human, 0))
        .await
        .context("post reschedule approval card")?;
    println!("approval card posted: action_id={action_id}");
    println!("{human}");
    Ok(())
}

/// Write the proposal (emails row = machine payload, actions row = pending).
pub(crate) fn record_card(
    store: &Store,
    req: &RescheduleRequest,
    payload: &str,
    human: &str,
) -> Result<(augmentagent_store::Email, String)> {
    let message_id = format!("gcal-reschedule:{}", uuid::Uuid::new_v4());
    let inbound = augmentagent_store::Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: message_id.clone(),
        thread_id: None,
        from: req.account_email.clone(),
        subject: format!("Move event: {}", req.summary),
        body: payload.to_string(),
        date: Utc::now().to_rfc3339(),
        account_entity_id: Some(req.entity_id.clone()),
        platform: "gcal".into(),
        kind: RESCHEDULE_CARD_KIND.into(),
    };
    store
        .upsert_email(&inbound)
        .context("upsert reschedule proposal row")?;
    let action_id = store
        .log_action(
            &message_id,
            None,
            &inbound.from,
            &inbound.subject,
            Some(payload),
            Some(human),
            ActionStatus::Pending,
        )
        .context("log reschedule action row")?;
    Ok((inbound, action_id))
}

impl ReplyApprover {
    /// Approve on a `calendar reschedule --post true` card. Claims the row
    /// `pending → sending` first so a double click writes once.
    pub(crate) async fn approve_gcal_reschedule(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let req: RescheduleRequest = match serde_json::from_str(&action.email.body) {
            Ok(r) => r,
            Err(e) => {
                let message = format!("reschedule payload parse failed: {e}");
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Error,
                    None,
                    Some(&message),
                );
                return ApprovalActionOutcome::Failed { message };
            }
        };
        match self
            .store
            .claim_action_for_send(action_id, ActionStatus::Pending, decision_source())
        {
            Ok(true) => {}
            Ok(false) => {
                let status = self
                    .store
                    .get_action_with_email(action_id)
                    .ok()
                    .flatten()
                    .map(|a| a.action.status)
                    .unwrap_or_else(|| "unknown".into());
                return ApprovalActionOutcome::AlreadyResolved {
                    status,
                    detail: None,
                };
            }
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("claim reschedule action: {e}"),
                };
            }
        }
        let when = when_local(&req.new_start, &req.new_end);
        match self.calendar.reschedule(&req).await {
            Ok(outcome) => {
                let already_there = outcome == RescheduleOutcome::AlreadyThere;
                let body = format!(
                    "{}\nrescheduled: {}{}",
                    action.action.draft_body.clone().unwrap_or_default(),
                    when,
                    if already_there {
                        " (already there)"
                    } else {
                        ""
                    }
                );
                if let Err(e) = self.store.update_action_status(
                    action_id,
                    ActionStatus::Sent,
                    Some(&body),
                    None,
                ) {
                    return ApprovalActionOutcome::Failed {
                        message: format!("the event was moved, but recording it failed: {e}"),
                    };
                }
                let _ = self
                    .store
                    .mark_email_processed(&action.email.message_id, TriageResult::Reply);
                tracing::info!(action_id, event_id = %req.event_id, already_there, "gcal event rescheduled");
                ApprovalActionOutcome::CalendarRescheduled {
                    account: req.account_email,
                    summary: req.summary,
                    when,
                    notified: req.notify && !already_there,
                    already_there,
                }
            }
            Err(e) => {
                let message = match e {
                    CalendarError::Forbidden { message } => format!(
                        "calendar write scope missing — re-consent the Google connection with \
                         calendar.events: {message}"
                    ),
                    other => format!("reschedule: {other}"),
                };
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Error,
                    None,
                    Some(&message),
                );
                ApprovalActionOutcome::Failed { message }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER: &str = "owner@example.test";
    const ENDPOINT: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events/ev1";

    fn store() -> (Arc<Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Arc::new(Store::open(dir.path().join("t.db")).unwrap()), dir)
    }

    fn req() -> RescheduleRequest {
        RescheduleRequest {
            entity_id: "ent".into(),
            account_email: OWNER.into(),
            calendar_id: "primary".into(),
            event_id: "ev1".into(),
            summary: "Sync with Sam".into(),
            from_start: "2030-11-13T14:00:00-05:00".into(),
            from_end: "2030-11-13T14:30:00-05:00".into(),
            new_start: "2030-11-13T15:30:00-05:00".into(),
            new_end: "2030-11-13T16:00:00-05:00".into(),
            notify: true,
            guest_count: 1,
        }
    }

    fn event(start: &str, end: &str) -> serde_json::Value {
        json!({
            "id": "ev1", "status": "confirmed", "summary": "Sync with Sam",
            "start": {"dateTime": start, "timeZone": "America/New_York"},
            "end": {"dateTime": end, "timeZone": "America/New_York"},
            "organizer": {"email": OWNER, "self": true},
        })
    }

    async fn mock_google(
        server: &mut mockito::ServerGuard,
        current: serde_json::Value,
        patches: usize,
    ) -> mockito::Mock {
        server
            .mock("GET", "/api/v3/connected_accounts")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(r#"{"items":[{"id":"ca_test","status":"ACTIVE"}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/api/v3/tools/execute/proxy")
            .match_body(mockito::Matcher::PartialJson(
                json!({"method": "GET", "endpoint": ENDPOINT}),
            ))
            .with_status(200)
            .with_body(json!({"status": 200, "data": current}).to_string())
            .create_async()
            .await;
        server
            .mock("POST", "/api/v3/tools/execute/proxy")
            .match_body(mockito::Matcher::PartialJson(
                json!({"method": "PATCH", "endpoint": ENDPOINT}),
            ))
            .with_status(200)
            .with_body(
                json!({"status": 200,
                "data": event("2030-11-13T15:30:00-05:00", "2030-11-13T16:00:00-05:00")})
                .to_string(),
            )
            .expect(patches)
            .create_async()
            .await
    }

    fn card(store: &Store) -> String {
        let r = req();
        let payload = serde_json::to_string(&r).unwrap();
        record_card(store, &r, &payload, &reschedule_human(&r, &[]))
            .unwrap()
            .1
    }

    // AC5 + AC7 + AC8 — Approve moves the event once; a second click is
    // AlreadyResolved and makes no provider call.
    #[tokio::test]
    async fn approve_moves_the_event_once() {
        let (store, _d) = store();
        let id = card(&store);
        let mut server = mockito::Server::new_async().await;
        let patch = mock_google(
            &mut server,
            event("2030-11-13T14:00:00-05:00", "2030-11-13T14:30:00-05:00"),
            1,
        )
        .await;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar =
            Arc::new(ComposioCalendarClient::new("k".into()).with_base_url(server.url()));

        let out = approver.approve(&id).await;
        match &out {
            ApprovalActionOutcome::CalendarRescheduled {
                account,
                summary,
                notified,
                already_there,
                ..
            } => {
                assert_eq!(account, OWNER);
                assert_eq!(summary, "Sync with Sam");
                assert!(*notified && !*already_there);
            }
            other => panic!("{other:?}"),
        }
        let row = store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(row.action.status, "sent");
        assert!(row
            .action
            .draft_body
            .unwrap_or_default()
            .contains("\nrescheduled: "));

        let again = approver.approve(&id).await;
        assert!(
            matches!(again, ApprovalActionOutcome::AlreadyResolved { .. }),
            "{again:?}"
        );
        patch.assert_async().await;
    }

    // AC5 — the event moved since the card: nothing written, row is error.
    #[tokio::test]
    async fn approve_refuses_a_moved_event() {
        let (store, _d) = store();
        let id = card(&store);
        let mut server = mockito::Server::new_async().await;
        let patch = mock_google(
            &mut server,
            event("2030-11-13T17:00:00-05:00", "2030-11-13T17:30:00-05:00"),
            0,
        )
        .await;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar =
            Arc::new(ComposioCalendarClient::new("k".into()).with_base_url(server.url()));

        let out = approver.approve(&id).await;
        assert!(
            matches!(&out, ApprovalActionOutcome::Failed { message } if message.contains("re-propose")),
            "{out:?}"
        );
        assert_eq!(
            store
                .get_action_with_email(&id)
                .unwrap()
                .unwrap()
                .action
                .status,
            "error"
        );
        patch.assert_async().await;
    }

    // AC5 — already at the target: success without a write.
    #[tokio::test]
    async fn approve_when_already_there_writes_nothing() {
        let (store, _d) = store();
        let id = card(&store);
        let mut server = mockito::Server::new_async().await;
        let patch = mock_google(
            &mut server,
            event("2030-11-13T15:30:00-05:00", "2030-11-13T16:00:00-05:00"),
            0,
        )
        .await;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar =
            Arc::new(ComposioCalendarClient::new("k".into()).with_base_url(server.url()));

        let out = approver.approve(&id).await;
        assert!(
            matches!(
                out,
                ApprovalActionOutcome::CalendarRescheduled {
                    already_there: true,
                    ..
                }
            ),
            "{out:?}"
        );
        patch.assert_async().await;
    }

    // AC4 — the card says from → to and whether guests are emailed.
    #[test]
    fn card_text_shows_from_to_and_notify() {
        let h = reschedule_human(&req(), &["⚠ conflicts with \"Lunch\" (15:30–16:30)".into()]);
        assert!(
            h.contains("• from:") && h.contains("• to:") && h.contains("guests are emailed"),
            "{h}"
        );
        assert!(h.contains("⚠ conflicts with \"Lunch\""), "{h}");
        let quiet = reschedule_human(
            &RescheduleRequest {
                notify: false,
                ..req()
            },
            &[],
        );
        assert!(quiet.contains("NOT emailed"), "{quiet}");
    }

    /// Owner-authorized live round trip on ONE owned Google-direct account
    /// (`JARVIS_CALENDAR_QA_ACCOUNT=<email>`, nothing hardcoded): create a
    /// guest-less event 3 days out, approve a reschedule card (no guest
    /// email), check Google shows the new time, check a second click and a
    /// stale card both write nothing, and always delete the event.
    #[tokio::test]
    #[ignore = "owner-authorized live calendar write; needs JARVIS_CALENDAR_QA_ACCOUNT and the Google OAuth env"]
    async fn live_reschedule_round_trip() -> Result<()> {
        let _ = dotenvy::dotenv(); // the operator's Google OAuth client
                                   // Bind the real credential store BEFORE isolating state: it lives
                                   // under XDG_STATE_HOME, which isolation redirects.
        let google = || -> Result<augmentagent_channel_calendar::GoogleCalendar> {
            let tokens = augmentagent_channel_email::google::GoogleGmail::from_env()?;
            Ok(augmentagent_channel_calendar::GoogleCalendar::new(
                Arc::new(tokens),
            ))
        };
        let client = || -> Result<ComposioCalendarClient> {
            Ok(
                ComposioCalendarClient::new("unused-for-google-direct".into())
                    .with_google(google()?),
            )
        };
        let (api, approver_client, cleaner) = (client()?, client()?, google()?);
        augmentagent_channel_core::state_dir::isolate_for_tests();
        let email = std::env::var("JARVIS_CALENDAR_QA_ACCOUNT")?;
        let entity = format!("google:{email}");
        let (store, _d) = store();
        let start = Utc::now() + chrono::Duration::days(3);
        let start =
            DateTime::from_timestamp(start.timestamp() - start.timestamp() % 900, 0).unwrap();
        let summary = format!(
            "[TEST #2039] {}",
            &uuid::Uuid::new_v4().simple().to_string()[..10]
        );
        let draft = augmentagent_channel_calendar::EventDraft {
            summary: summary.clone(),
            start_datetime: start.to_rfc3339(),
            duration_minutes: 15,
            attendees: vec![],
            description: Some(
                "Owner-authorized reschedule QA. Temporary; deleted after verification.".into(),
            ),
            create_meeting_room: false, recurrence: vec![], time_zone: None,
        };
        let created = api.create_event(&entity, "primary", &draft).await?;
        let event_id = created.id.clone().context("created event id")?;
        println!("created {event_id} ({summary}) at {}", start.to_rfc3339());

        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar = Arc::new(approver_client);
        let verify: Result<()> = async {
            let ev = api.get_event(&entity, "primary", &event_id).await?;
            let target = (start + chrono::Duration::minutes(75)).fixed_offset();
            let req = plan_reschedule(&ev, &entity, &email, "primary", target, None, false, Utc::now())
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("plan: {} → {} (notify={})", req.from_start, req.new_start, req.notify);
            let payload = serde_json::to_string(&req)?;
            let (_, id) = record_card(&store, &req, &payload, &reschedule_human(&req, &[]))?;
            let out = approver.approve(&id).await;
            println!("approve outcome: {}", augmentagent_approval_discord::outcome::describe(&out));
            anyhow::ensure!(matches!(out, ApprovalActionOutcome::CalendarRescheduled { already_there: false, .. }), "{out:?}");

            let after = api.get_event(&entity, "primary", &event_id).await?;
            let s = after.start.as_ref().and_then(|t| t.to_utc());
            let e = after.end.as_ref().and_then(|t| t.to_utc());
            println!("google now shows: {s:?} – {e:?}");
            anyhow::ensure!(s == Some(target.with_timezone(&Utc)), "start not moved");
            anyhow::ensure!(e == Some(target.with_timezone(&Utc) + chrono::Duration::minutes(15)), "length not kept");

            let again = approver.approve(&id).await;
            println!("second click: {again:?}");
            anyhow::ensure!(matches!(again, ApprovalActionOutcome::AlreadyResolved { .. }), "{again:?}");

            // A stale card: planned from the ORIGINAL time to somewhere else.
            let stale = RescheduleRequest {
                new_start: (start + chrono::Duration::minutes(150)).to_rfc3339(),
                new_end: (start + chrono::Duration::minutes(165)).to_rfc3339(),
                ..req.clone()
            };
            let (_, sid) = record_card(&store, &stale, &serde_json::to_string(&stale)?, "stale")?;
            let out = approver.approve(&sid).await;
            println!("stale card: {out:?}");
            anyhow::ensure!(matches!(&out, ApprovalActionOutcome::Failed { message } if message.contains("re-propose")), "{out:?}");
            let still = api.get_event(&entity, "primary", &event_id).await?;
            anyhow::ensure!(still.start.as_ref().and_then(|t| t.to_utc()) == Some(target.with_timezone(&Utc)), "stale card moved it");
            Ok(())
        }
        .await;

        let cleanup = cleaner
            .call_no_content(
                &email,
                "DELETE",
                &format!("calendars/primary/events/{event_id}"),
            )
            .await;
        println!("cleanup: {cleanup:?}");
        cleanup?;
        verify
    }

    // AC1 — the CLI surface parses.
    #[test]
    fn reschedule_command_parses() {
        let cli = Cli::try_parse_from([
            "augmentagent",
            "calendar",
            "reschedule",
            "--event-id",
            "ev1",
            "--start",
            "2030-11-13T15:30:00-05:00",
            "--duration-min",
            "45",
            "--notify",
            "false",
            "--post",
            "true",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Calendar {
                op:
                    CalendarOp::Reschedule {
                        event_id,
                        duration_min,
                        notify,
                        post,
                        ..
                    },
            } => {
                assert_eq!(event_id, "ev1");
                assert_eq!(duration_min, Some(45));
                assert!(!notify && post);
            }
            _ => panic!("wrong command"),
        }
    }
}
