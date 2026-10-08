//! Opt-in owner self-invite QA for #1436. Exercises the real approver and
//! Composio/Google, but does not pretend to be a live Discord button click.
use super::*;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct Account {
    email: String,
    entity_id: String,
}

async fn execute(key: &str, entity: &str, slug: &str, args: Value) -> Result<Value> {
    let response = reqwest::Client::new()
        .post(format!(
            "https://backend.composio.dev/api/v3/tools/execute/{slug}"
        ))
        .header("x-api-key", key)
        .timeout(Duration::from_secs(20))
        .json(&json!({"user_id":entity,"arguments":args}))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    anyhow::ensure!(
        response["successful"] == true,
        "{slug} rejected: {}",
        response["error"]
    );
    Ok(response)
}

#[tokio::test]
#[ignore = "owner-authorized live self-invites; requires explicit QA account config and API key"]
async fn live_calendar_self_invite_and_cleanup() -> Result<()> {
    augmentagent_channel_core::state_dir::isolate_for_tests();
    let key = std::env::var("COMPOSIO_API_KEY")?;
    let accounts: Vec<Account> =
        serde_json::from_str(&std::env::var("JARVIS_CALENDAR_QA_ACCOUNTS_JSON")?)?;
    anyhow::ensure!(
        accounts.len() == 3,
        "configure organizer followed by exactly two owned attendee accounts"
    );
    let report = PathBuf::from(std::env::var("JARVIS_CALENDAR_QA_REPORT")?);
    let dir = tempfile::tempdir()?;
    let store = Arc::new(Store::open(dir.path().join("qa.db"))?);
    let summary = format!("[TEST #1436] Rust approval QA {}", uuid::Uuid::new_v4());
    let start = chrono::Utc::now() + chrono::Duration::days(1);
    let start = chrono::DateTime::from_timestamp(start.timestamp(), 0).unwrap();
    let end = start + chrono::Duration::minutes(5);
    let email = augmentagent_store::Email {
        message_id: format!("gcal-create:{}", uuid::Uuid::new_v4()), thread_id: None,
        from: accounts[0].email.clone(), to: String::new(), cc: String::new(),
        attachments: Vec::new(), subject: summary.clone(),
        body: json!({"summary":summary,"start_datetime":start.with_timezone(&chrono::FixedOffset::west_opt(4*3600).unwrap()).to_rfc3339(),
            "duration_minutes":5,"attendees":[accounts[1].email,accounts[2].email],"create_meeting_room":true,
            "description":"Owner-authorized self-invite QA. Temporary; removed after verification."}).to_string(),
        date: start.to_rfc3339(), account_entity_id: Some(accounts[0].entity_id.clone()),
        platform:"gcal".into(), kind:"create_event".into(),
    };
    store.upsert_email(&email)?;
    let action = store.log_action(
        &email.message_id,
        None,
        &email.from,
        &email.subject,
        Some(&email.body),
        Some("Calendar QA"),
        ActionStatus::Pending,
    )?;
    let mut approver = test_support::approver_with_store(store.clone());
    approver.calendar = Arc::new(augmentagent_channel_calendar::ComposioCalendarClient::new(
        key.clone(),
    ));
    let outcome = approver.approve(&action).await;
    let ApprovalActionOutcome::CalendarCreated {
        event_id,
        html_link,
    } = outcome
    else {
        anyhow::bail!("real approval did not confirm creation: {outcome:?}; inspect the test title before retrying");
    };
    // Write the cleanup capability before any assertion/readback can fail.
    let mut evidence = json!({"summary":summary,"event_id":event_id,"html_link":html_link,
        "expected_start":start.to_rfc3339(),"expected_end":end.to_rfc3339(),"cleanup":"pending"});
    std::fs::write(&report, serde_json::to_vec_pretty(&evidence)?)?;
    let verify: Result<()> = async {
        anyhow::ensure!(store.get_action_with_email(&action)?.unwrap().action.status == "sent");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let mut complete = true;
            let mut observations = Vec::new();
            for (ix, account) in accounts.iter().enumerate() {
                let listed = execute(&key, &account.entity_id, "GOOGLECALENDAR_EVENTS_LIST", json!({
                    "calendarId":"primary","timeMin":(start-chrono::Duration::minutes(1)).to_rfc3339(),
                    "timeMax":(end+chrono::Duration::minutes(1)).to_rfc3339(),"singleEvents":true,"maxResults":250
                })).await?;
                let events: Vec<&Value> = listed["data"]["items"].as_array().context("missing event list")?
                    .iter().filter(|e| e["summary"] == summary).collect();
                if events.is_empty() { complete = false; continue; }
                anyhow::ensure!(events.len() == 1, "duplicate QA events on account {ix}");
                let event = events[0];
                anyhow::ensure!(event["id"] == event_id);
                anyhow::ensure!(chrono::DateTime::parse_from_rfc3339(event["start"]["dateTime"].as_str().context("start missing")?)? == start);
                anyhow::ensure!(chrono::DateTime::parse_from_rfc3339(event["end"]["dateTime"].as_str().context("end missing")?)? == end);
                anyhow::ensure!(event["organizer"]["email"] == accounts[0].email);
                anyhow::ensure!(event["hangoutLink"].as_str().is_some_and(|s| s.starts_with("https://meet.google.com/")));
                for attendee in &accounts[1..] {
                    anyhow::ensure!(event["attendees"].as_array().context("attendees missing")?.iter().any(|a| a["email"] == attendee.email));
                }
                let mut observation = json!({"account_index":ix,"event_verified":true,"calendar_log_id":listed["log_id"]});
                if ix > 0 {
                    let mail = execute(&key, &account.entity_id, "GMAIL_FETCH_EMAILS", json!({
                        "query":format!("subject:\"{summary}\" newer_than:1d"),"max_results":10,"verbose":false,"include_payload":false
                    })).await?;
                    let delivered = mail["data"]["messages"].as_array().is_some_and(|m| !m.is_empty());
                    complete &= delivered;
                    observation["invitation_delivered"] = json!(delivered);
                    observation["mail_log_id"] = mail["log_id"].clone();
                }
                observations.push(observation);
            }
            evidence["observations"] = json!(observations);
            if complete { return Ok(()); }
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "calendar/inbox propagation exceeded 120 seconds");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }.await;
    // Always attempt cleanup, including when verification fails.
    let cleanup: Result<()> = async {
        execute(&key, &accounts[0].entity_id, "GOOGLECALENDAR_DELETE_EVENT", json!({"calendar_id":"primary","event_id":event_id})).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let mut complete = true;
            for account in &accounts {
                let listed = execute(&key, &account.entity_id, "GOOGLECALENDAR_EVENTS_LIST", json!({
                    "calendarId":"primary","timeMin":(start-chrono::Duration::minutes(1)).to_rfc3339(),
                    "timeMax":(end+chrono::Duration::minutes(1)).to_rfc3339(),"singleEvents":true,"maxResults":250,"showDeleted":true
                })).await?;
                complete &= !listed["data"]["items"].as_array().context("missing event list")?
                    .iter().any(|e| e["id"] == event_id && e["status"] != "cancelled");
            }
            if complete { return Ok(()); }
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "cleanup propagation exceeded 120 seconds");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }.await;
    evidence["verification"] = json!(verify.as_ref().map(|_| "passed").map_err(|e| e.to_string()));
    evidence["cleanup"] = json!(cleanup
        .as_ref()
        .map(|_| "cancelled on all three calendars")
        .map_err(|e| e.to_string()));
    std::fs::write(report, serde_json::to_vec_pretty(&evidence)?)?;
    cleanup?;
    verify
}
