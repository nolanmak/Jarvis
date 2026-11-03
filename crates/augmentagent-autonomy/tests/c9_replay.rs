//! #1833 C9 — the motivating incident, replayed end to end: an
//! `actions`-tier trusted contact texts "not seeing an invite for this
//! meeting today". Fake planner, the real candidate lookup and guest
//! executor against a mock Google Calendar, the real store, and an
//! executor that queues the iMessage outbox exactly like Approve does.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::autonomy::{STEP_ADD_GUEST, STEP_SEND};
use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_autonomy::calendar::{CalendarOps, GoogleCalendarOps};
use augmentagent_autonomy::registry::{ADD_GUEST, SEND_DETAILS};
use augmentagent_autonomy::{PlanStatus, StepStatus, Tier};
use augmentagent_channel_calendar::GoogleCalendar;
use chrono::{DateTime, Utc};
use common::*;
use mockito::{Matcher, Server};
use serde_json::json;

const PRIMARY: &str = "owner@example.test";
const SECONDARY: &str = "owner.work@example.test";
const TEXT: &str = "not seeing an invite for this meeting today, only for next Monday morning";
const EVENT_PATH: &str = "/calendars/primary/events/ev-sec-1";

struct Tokens;
#[async_trait]
impl augmentagent_channel_calendar::GoogleTokens for Tokens {
    async fn access_token(&self, email: &str) -> Result<String, String> {
        Ok(format!("at-{email}"))
    }
}

fn now() -> DateTime<Utc> {
    // 10:00 in New York on the day of the meeting.
    DateTime::parse_from_rfc3339("2026-11-03T15:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn attendees() -> serde_json::Value {
    json!([
        {"email": SECONDARY, "self": true, "organizer": true, "responseStatus": "accepted"},
        {"email": "alex@example.test", "displayName": "Alex", "responseStatus": "accepted", "optional": true},
        {"email": "room-1@resource.example.test", "resource": true, "responseStatus": "accepted"},
    ])
}

fn event(etag: &str, attendees: serde_json::Value) -> serde_json::Value {
    json!({
        "kind": "calendar#event", "id": "ev-sec-1", "etag": etag, "status": "confirmed",
        "summary": "Design review",
        "start": {"dateTime": "2026-11-03T13:00:00-05:00"},
        "end": {"dateTime": "2026-11-03T13:30:00-05:00"},
        "organizer": {"email": SECONDARY, "self": true},
        "hangoutLink": "https://meet.example.test/abc-defg-hij",
        "attendees": attendees,
    })
}

struct Replay {
    server: mockito::ServerGuard,
    store: Arc<augmentagent_store::Store>,
    _dir: tempfile::TempDir,
    inner: Arc<Recording>,
    exec: Arc<FakeExecutor>,
    planner: Arc<FakePlanner>,
    broker: augmentagent_autonomy::broker::AutonomyBroker,
}

async fn replay(secondary_events: serde_json::Value) -> Replay {
    let server = Server::new_async().await;
    let (dir, store) = store();
    store.upsert_google_gmail_account(PRIMARY).unwrap();
    store.upsert_google_gmail_account(SECONDARY).unwrap();
    let mut server = server;
    for (account, items) in [(PRIMARY, json!([])), (SECONDARY, secondary_events)] {
        server
            .mock("GET", "/calendars/primary/events")
            .match_header("authorization", format!("Bearer at-{account}").as_str())
            .match_query(Matcher::Any)
            .with_body(json!({"items": items}).to_string())
            .create_async()
            .await;
    }
    let cal: Arc<dyn CalendarOps> = Arc::new(GoogleCalendarOps {
        google: GoogleCalendar::new(Arc::new(Tokens)).with_base_url(server.url()),
        store: Arc::clone(&store),
        tz: tz(),
        now: Some(now()),
    });
    let planner = FakePlanner::new(
        r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.add_guest"]}"#,
    );
    let g = gate(
        &store,
        true,
        snapshot(contact(Tier::Actions, &[ADD_GUEST, SEND_DETAILS])),
        Some(cal),
        Some(planner.clone()),
    );
    let inner = Recording::new(true);
    let exec = FakeExecutor::new(Arc::clone(&store));
    let broker = broker(Arc::clone(&inner), g, Arc::clone(&exec));
    Replay {
        server,
        store,
        _dir: dir,
        inner,
        exec,
        planner,
        broker,
    }
}

fn inbound(phone: &str) -> augmentagent_store::Email {
    imessage_email(phone, 12, TEXT, &minutes_ago(3))
}

const HEDGE: &str = "Let me check on it and get the invite sorted out.";

#[tokio::test]
async fn incident_resolves_without_a_click() {
    let mut r = replay(json!([event("\"e1\"", attendees())])).await;
    let get = r
        .server
        .mock("GET", EVENT_PATH)
        .match_header("authorization", format!("Bearer at-{SECONDARY}").as_str())
        .with_header("etag", "\"e1\"")
        .with_body(event("\"e1\"", attendees()).to_string())
        .expect(1)
        .create_async()
        .await;
    let mut with_guest = attendees();
    with_guest
        .as_array_mut()
        .unwrap()
        .push(json!({"email": WORK_EMAIL}));
    let patch = r
        .server
        .mock("PATCH", EVENT_PATH)
        .match_header("if-match", "\"e1\"")
        .match_query(Matcher::UrlEncoded("sendUpdates".into(), "all".into()))
        // The PATCH preserves every attendee and adds the work address.
        .match_body(Matcher::Json(json!({"attendees": with_guest.clone()})))
        .with_header("etag", "\"e2\"")
        .with_body(event("\"e2\"", with_guest).to_string())
        .expect(1)
        .create_async()
        .await;

    let email = inbound(PHONE);
    let action = seed(&r.store, &email, HEDGE, Some((true, &[])));
    r.broker
        .post_approval(&action, &email, HEDGE)
        .await
        .unwrap();
    get.assert_async().await;
    patch.assert_async().await;

    // The planner was asked once and returned add_guest on the candidate
    // found on the SECONDARY account.
    assert_eq!(r.planner.calls(), 1);
    // The template text, not the hedge, went out on the inbound thread.
    let expected = format!(
        "Added you to Design review (Tue 3 Nov, 1:00 PM) — invite sent to {WORK_EMAIL}. Join: https://meet.example.test/abc-defg-hij"
    );
    assert_eq!(
        outbox_bodies(&r.store),
        vec![(PHONE.to_string(), expected.clone())]
    );
    assert_eq!(r.exec.approvals(), vec![action.clone()]);
    // The plan is executed with 2 steps.
    let plan = r
        .store
        .autonomous_plans_with_notice(augmentagent_autonomy::NoticeStatus::Posted)
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].status, PlanStatus::Executed);
    assert_eq!(plan[0].action_id, action);
    let steps = r.store.autonomous_steps(plan[0].id).unwrap();
    let shape: Vec<(&str, StepStatus)> =
        steps.iter().map(|s| (s.kind.as_str(), s.status)).collect();
    assert_eq!(
        shape,
        vec![
            (STEP_ADD_GUEST, StepStatus::Executed),
            (STEP_SEND, StepStatus::Executed)
        ]
    );
    assert_eq!(steps[0].account.as_deref(), Some(SECONDARY));
    // No card; one notice, with Undo.
    assert!(r.inner.cards().is_empty(), "{:?}", r.inner.calls());
    let notices = r.inner.notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].undo_available());
    assert!(notices[0].text().contains(&expected));
}

#[tokio::test]
async fn untrusted_number_gets_byte_identical_card() {
    let mut r = replay(json!([event("\"e1\"", attendees())])).await;
    let patch = r
        .server
        .mock("PATCH", EVENT_PATH)
        .match_query(Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let email = inbound(OTHER_PHONE);
    let action = seed(&r.store, &email, HEDGE, Some((true, &[])));
    r.broker
        .post_approval(&action, &email, HEDGE)
        .await
        .unwrap();
    assert_eq!(r.inner.cards(), vec![plain(&action, &email, HEDGE)]);
    assert!(outbox_bodies(&r.store).is_empty());
    assert_eq!(r.planner.calls(), 0);
    patch.assert_async().await;
}

#[tokio::test]
async fn meeting_not_found_is_evidence_incomplete() {
    let mut r = replay(json!([])).await;
    let patch = r
        .server
        .mock("PATCH", EVENT_PATH)
        .match_query(Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let email = inbound(PHONE);
    let action = seed(&r.store, &email, HEDGE, Some((true, &[])));
    r.broker
        .post_approval(&action, &email, HEDGE)
        .await
        .unwrap();
    match r.inner.cards().as_slice() {
        [Call::Noted {
            action_id,
            draft,
            note,
        }] => {
            assert_eq!(action_id, &action);
            assert_eq!(draft, HEDGE);
            assert!(note.starts_with("evidence_incomplete"), "{note}");
        }
        other => panic!("{other:?}"),
    }
    assert!(outbox_bodies(&r.store).is_empty());
    patch.assert_async().await;
}

#[tokio::test]
async fn owner_texted_first_is_owner_took_over() {
    let mut r = replay(json!([event("\"e1\"", attendees())])).await;
    let patch = r
        .server
        .mock("PATCH", EVENT_PATH)
        .match_query(Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let email = inbound(PHONE);
    let action = seed(&r.store, &email, HEDGE, Some((true, &[])));
    owner_texted(&r.store, PHONE, 13, &minutes_ago(1));
    r.broker
        .post_approval(&action, &email, HEDGE)
        .await
        .unwrap();
    match r.inner.cards().as_slice() {
        [Call::Noted { note, .. }] => assert!(note.starts_with("owner_took_over"), "{note}"),
        other => panic!("{other:?}"),
    }
    assert!(outbox_bodies(&r.store).is_empty());
    assert!(r.exec.approvals().is_empty());
    patch.assert_async().await;
}
