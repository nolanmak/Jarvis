//! #1833 C4 (D8) — add or remove one guest on an event the owner organizes.
//!
//! Both executors read the event, refuse unless the account is the organizer
//! (v1 is organizer-only, OT5), rebuild the **full** attendee list from that
//! read (every attendee object passed back unchanged; `attendeesOmitted` is
//! never used, it is only safe for self-RSVP) and PATCH it with
//! `If-Match: <etag>` and `sendUpdates=all`. A 412 (the event changed under
//! us) is retried once from a fresh read. A PATCH whose outcome is unknown
//! (transport error, 5xx, unreadable body, or a 2xx whose attendee list does
//! not show the change) is [`GuestOutcome::Unconfirmed`]: the caller records
//! the step as `unknown` and never retries it blindly.

use serde_json::Value;

use crate::gcal::{path_segment, CalendarError};
use crate::google_direct::{CallOptions, GoogleCalendar};

/// What a guest edit did. Errors where nothing was written (read failed,
/// 4xx, second 412, bad input) are `Err(CalendarError)` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestOutcome {
    /// Google confirmed the guest is on the event. `attendees` is the
    /// updated list (lowercased emails) Google returned.
    Added {
        etag: Option<String>,
        attendees: Vec<String>,
    },
    /// Google confirmed the guest is off the event.
    Removed {
        etag: Option<String>,
        attendees: Vec<String>,
    },
    /// Nothing to do: already a guest (add) or not a guest (remove).
    Noop,
    /// The account does not organize this event; nothing was written.
    NotOrganizer,
    /// Remove refused: the guest already accepted ("Remove them in Calendar
    /// if needed"). Nothing was written.
    AlreadyAccepted,
    /// The PATCH may or may not have landed.
    Unconfirmed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Add,
    Remove,
}

impl GoogleCalendar {
    /// D8 `calendar.add_guest`: invite `email` to the event as `account`.
    pub async fn add_guest(
        &self,
        account: &str,
        calendar_id: &str,
        event_id: &str,
        email: &str,
    ) -> Result<GuestOutcome, CalendarError> {
        self.edit_guest(account, calendar_id, event_id, email, Edit::Add)
            .await
    }

    /// D8 undo: take `email` off the event as `account`, unless they
    /// already accepted.
    pub async fn remove_guest(
        &self,
        account: &str,
        calendar_id: &str,
        event_id: &str,
        email: &str,
    ) -> Result<GuestOutcome, CalendarError> {
        self.edit_guest(account, calendar_id, event_id, email, Edit::Remove)
            .await
    }

    async fn edit_guest(
        &self,
        account: &str,
        calendar_id: &str,
        event_id: &str,
        email: &str,
        edit: Edit,
    ) -> Result<GuestOutcome, CalendarError> {
        let guest = email.trim().to_ascii_lowercase();
        if guest.is_empty() || !guest.contains('@') || guest.chars().any(char::is_whitespace) {
            return Err(CalendarError::Invalid(format!(
                "guest must be one email address, got {email:?}"
            )));
        }
        if calendar_id.trim().is_empty() || event_id.trim().is_empty() {
            return Err(CalendarError::Invalid(
                "calendar id and event id are required".into(),
            ));
        }
        let path = format!(
            "calendars/{}/events/{}",
            path_segment(calendar_id),
            path_segment(event_id)
        );
        // Attempt 0, then at most one retry after a 412.
        let mut attempt = 0;
        loop {
            let read = self
                .call_with(account, "GET", &path, &[], None, CallOptions::default())
                .await?;
            let event = read.body;
            if !is_organizer(&event, account) {
                return Ok(GuestOutcome::NotOrganizer);
            }
            if event.get("attendeesOmitted").and_then(Value::as_bool) == Some(true) {
                // Writing back a partial list would drop the omitted guests.
                return Err(CalendarError::Invalid(format!(
                    "{path}: Google returned a partial attendee list (attendeesOmitted)"
                )));
            }
            let etag = read.etag.filter(|e| !e.trim().is_empty()).ok_or_else(|| {
                CalendarError::Decode(format!("{path}: event read carried no etag"))
            })?;
            let mut attendees: Vec<Value> = match event.get("attendees") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(list)) => list.clone(),
                Some(other) => {
                    return Err(CalendarError::Decode(format!(
                        "{path}: attendees is not a list ({other})"
                    )))
                }
            };
            let existing = attendees
                .iter()
                .position(|a| attendee_email(a).as_deref() == Some(guest.as_str()));
            match (edit, existing) {
                (Edit::Add, Some(_)) | (Edit::Remove, None) => return Ok(GuestOutcome::Noop),
                (Edit::Add, None) => attendees.push(serde_json::json!({ "email": guest })),
                (Edit::Remove, Some(i)) => {
                    let accepted = attendees[i]
                        .get("responseStatus")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.eq_ignore_ascii_case("accepted"));
                    if accepted {
                        return Ok(GuestOutcome::AlreadyAccepted);
                    }
                    attendees.remove(i);
                }
            }
            let patch = serde_json::json!({ "attendees": attendees });
            let opts = CallOptions {
                if_match: Some(&etag),
                unconfirmed_writes: true,
            };
            let result = self
                .call_with(
                    account,
                    "PATCH",
                    &path,
                    &[("sendUpdates", "all")],
                    Some(patch),
                    opts,
                )
                .await;
            match result {
                Ok(resp) => return Ok(confirm(edit, &guest, resp.body, resp.etag)),
                Err(CalendarError::PreconditionFailed { .. }) if attempt == 0 => {
                    attempt += 1;
                    continue;
                }
                Err(CalendarError::Unconfirmed(m)) => return Ok(GuestOutcome::Unconfirmed(m)),
                Err(e) => return Err(e),
            }
        }
    }
}

/// The account organizes the event: Google marks the organizer block
/// `self`, or it names the account.
fn is_organizer(event: &Value, account: &str) -> bool {
    let Some(org) = event.get("organizer") else {
        return false;
    };
    org.get("self").and_then(Value::as_bool) == Some(true)
        || org
            .get("email")
            .and_then(Value::as_str)
            .is_some_and(|e| e.trim().eq_ignore_ascii_case(account.trim()))
}

fn attendee_email(a: &Value) -> Option<String> {
    a.get("email")
        .and_then(Value::as_str)
        .map(|e| e.trim().to_ascii_lowercase())
}

/// "Executed" means Google returned 2xx **with** the updated attendee list
/// showing the change; anything less is an unknown outcome.
fn confirm(edit: Edit, guest: &str, body: Value, etag: Option<String>) -> GuestOutcome {
    let Some(list) = body.get("attendees").and_then(Value::as_array) else {
        if edit == Edit::Remove && body.get("id").is_some() && body.get("attendees").is_none() {
            // Google omits `attendees` when the list is empty.
            return GuestOutcome::Removed {
                etag,
                attendees: Vec::new(),
            };
        }
        return GuestOutcome::Unconfirmed(
            "Google accepted the update but returned no attendee list".into(),
        );
    };
    let attendees: Vec<String> = list.iter().filter_map(attendee_email).collect();
    let present = attendees.iter().any(|e| e == guest);
    match (edit, present) {
        (Edit::Add, true) => GuestOutcome::Added { etag, attendees },
        (Edit::Remove, false) => GuestOutcome::Removed { etag, attendees },
        (Edit::Add, false) => GuestOutcome::Unconfirmed(format!(
            "Google accepted the update but {guest} is not on the returned attendee list"
        )),
        (Edit::Remove, true) => GuestOutcome::Unconfirmed(format!(
            "Google accepted the update but {guest} is still on the returned attendee list"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mockito::{Matcher, Server};
    use serde_json::json;
    use std::sync::Arc;

    const OWNER: &str = "owner@example.test";
    const GUEST: &str = "teammate@example.test";
    const PATH: &str = "/calendars/primary/events/ev1";

    struct Tokens;
    #[async_trait]
    impl crate::google_direct::GoogleTokens for Tokens {
        async fn access_token(&self, email: &str) -> Result<String, String> {
            assert_eq!(email, OWNER);
            Ok("at-1".into())
        }
    }

    fn client(base: String) -> GoogleCalendar {
        GoogleCalendar::new(Arc::new(Tokens)).with_base_url(base)
    }

    /// Attendee objects carry fields the daemon never models (`optional`,
    /// `comment`, `additionalGuests`, `id`); a PATCH must hand every one back.
    fn existing_attendees() -> Value {
        json!([
            {"email": OWNER, "self": true, "organizer": true, "responseStatus": "accepted"},
            {"email": "alex@example.test", "displayName": "Alex", "responseStatus": "accepted", "optional": true, "comment": "may be late"},
            {"email": "room-1@resource.example.test", "resource": true, "responseStatus": "accepted"},
            {"email": "sam@example.test", "responseStatus": "needsAction", "additionalGuests": 1, "id": "att-sam"},
        ])
    }

    fn event(etag: &str, attendees: Value, organizer_self: bool) -> Value {
        json!({
            "kind": "calendar#event",
            "id": "ev1",
            "etag": etag,
            "status": "confirmed",
            "summary": "Design review",
            "organizer": if organizer_self { json!({"email": OWNER, "self": true}) } else { json!({"email": "someone-else@example.test"}) },
            "attendees": attendees,
        })
    }

    fn with_guest(mut list: Value, guest: Value) -> Value {
        list.as_array_mut().unwrap().push(guest);
        list
    }

    async fn get_mock(server: &mut Server, body: &Value) -> mockito::Mock {
        server
            .mock("GET", PATH)
            .match_header("authorization", "Bearer at-1")
            .with_header("etag", body["etag"].as_str().unwrap())
            .with_body(body.to_string())
            .expect(1)
            .create_async()
            .await
    }

    #[tokio::test]
    async fn add_guest_patch_body_preserves_all_attendees() {
        let mut server = Server::new_async().await;
        let got = event("\"3300000000000001\"", existing_attendees(), true);
        let get = get_mock(&mut server, &got).await;
        let expected_list = with_guest(existing_attendees(), json!({"email": GUEST}));
        let patched = event("\"3300000000000002\"", expected_list.clone(), true);
        let patch = server
            .mock("PATCH", PATH)
            .match_header("authorization", "Bearer at-1")
            .match_header("if-match", "\"3300000000000001\"")
            .match_query(Matcher::AllOf(vec![Matcher::UrlEncoded(
                "sendUpdates".into(),
                "all".into(),
            )]))
            // Exact body: every attendee object as read, plus the guest, and
            // nothing else (no attendeesOmitted, no other event fields).
            .match_body(Matcher::Json(json!({"attendees": expected_list})))
            .with_header("etag", "\"3300000000000002\"")
            .with_body(patched.to_string())
            .expect(1)
            .create_async()
            .await;

        let out = client(server.url())
            .add_guest(OWNER, "primary", "ev1", " Teammate@Example.test ")
            .await
            .unwrap();
        get.assert_async().await;
        patch.assert_async().await;
        assert_eq!(
            out,
            GuestOutcome::Added {
                etag: Some("\"3300000000000002\"".into()),
                attendees: vec![
                    OWNER.into(),
                    "alex@example.test".into(),
                    "room-1@resource.example.test".into(),
                    "sam@example.test".into(),
                    GUEST.into(),
                ],
            }
        );
    }

    #[tokio::test]
    async fn add_guest_refuses_non_organizer() {
        let mut server = Server::new_async().await;
        let got = event("\"e1\"", existing_attendees(), false);
        let get = get_mock(&mut server, &got).await;
        let patch = server
            .mock("PATCH", PATH)
            .match_query(Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let out = client(server.url())
            .add_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap();
        assert_eq!(out, GuestOutcome::NotOrganizer);
        get.assert_async().await;
        patch.assert_async().await;

        // Undo is organizer-only too.
        let get = get_mock(&mut server, &got).await;
        let out = client(server.url())
            .remove_guest(OWNER, "primary", "ev1", "sam@example.test")
            .await
            .unwrap();
        assert_eq!(out, GuestOutcome::NotOrganizer);
        get.assert_async().await;
        patch.assert_async().await;
    }

    #[tokio::test]
    async fn add_guest_noop_when_present() {
        let mut server = Server::new_async().await;
        let list = with_guest(
            existing_attendees(),
            json!({"email": "TEAMMATE@example.test", "responseStatus": "needsAction"}),
        );
        let got = event("\"e1\"", list, true);
        let get = get_mock(&mut server, &got).await;
        let patch = server
            .mock("PATCH", PATH)
            .match_query(Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let out = client(server.url())
            .add_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap();
        assert_eq!(out, GuestOutcome::Noop);
        get.assert_async().await;
        patch.assert_async().await;
    }

    #[tokio::test]
    async fn add_guest_retries_once_on_412() {
        let mut server = Server::new_async().await;
        // First read; the PATCH against its etag loses a race (412).
        let first = event("\"v1\"", existing_attendees(), true);
        let get1 = get_mock(&mut server, &first).await;
        // Second read sees the concurrent edit (a new attendee) and a new
        // etag; the retry must be built from it, not from the stale list.
        let fresh_list = with_guest(
            existing_attendees(),
            json!({"email": "new@example.test", "responseStatus": "needsAction"}),
        );
        let second = event("\"v2\"", fresh_list.clone(), true);
        let get2 = get_mock(&mut server, &second).await;
        let stale = server
            .mock("PATCH", PATH)
            .match_header("if-match", "\"v1\"")
            .match_query(Matcher::Any)
            .with_status(412)
            .with_body(r#"{"error":{"message":"Precondition Failed"}}"#)
            .expect(1)
            .create_async()
            .await;
        let retry_list = with_guest(fresh_list, json!({"email": GUEST}));
        let retried = server
            .mock("PATCH", PATH)
            .match_header("if-match", "\"v2\"")
            .match_query(Matcher::UrlEncoded("sendUpdates".into(), "all".into()))
            .match_body(Matcher::Json(json!({"attendees": retry_list})))
            .with_body(event("\"v3\"", retry_list.clone(), true).to_string())
            .expect(1)
            .create_async()
            .await;
        let out = client(server.url())
            .add_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap();
        assert!(
            matches!(&out, GuestOutcome::Added { etag: Some(e), .. } if e == "\"v3\""),
            "{out:?}"
        );
        get1.assert_async().await;
        get2.assert_async().await;
        stale.assert_async().await;
        retried.assert_async().await;

        // Only once: a second 412 is an error (nothing written), not a loop.
        let mut server = Server::new_async().await;
        let _get = server
            .mock("GET", PATH)
            .with_body(event("\"v1\"", existing_attendees(), true).to_string())
            .expect(2)
            .create_async()
            .await;
        let always = server
            .mock("PATCH", PATH)
            .match_query(Matcher::Any)
            .with_status(412)
            .expect(2)
            .create_async()
            .await;
        let err = client(server.url())
            .add_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap_err();
        assert!(
            matches!(err, CalendarError::PreconditionFailed { .. }),
            "{err}"
        );
        always.assert_async().await;
    }

    /// Serves GETs of `get_body` and drops the connection on any other
    /// request after reading it: a PATCH whose response never arrives.
    async fn get_then_hang_up(get_body: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&writes);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let body = get_body.clone();
                let w = Arc::clone(&w);
                tokio::spawn(async move {
                    loop {
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") {
                            match sock.read(&mut byte).await {
                                Ok(1) => head.push(byte[0]),
                                _ => return,
                            }
                        }
                        let head_s = String::from_utf8_lossy(&head).to_ascii_lowercase();
                        let len = head_s
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        let mut rest = vec![0u8; len];
                        if len > 0 && sock.read_exact(&mut rest).await.is_err() {
                            return;
                        }
                        if head_s.starts_with("get ") {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            if sock.write_all(resp.as_bytes()).await.is_err() {
                                return;
                            }
                        } else {
                            w.fetch_add(1, Ordering::SeqCst);
                            return; // hang up without a response
                        }
                    }
                });
            }
        });
        (format!("http://{addr}"), writes)
    }

    #[tokio::test]
    async fn add_guest_transport_error_is_unconfirmed() {
        use std::sync::atomic::Ordering;
        let got = event("\"e1\"", existing_attendees(), true).to_string();
        let (base, writes) = get_then_hang_up(got).await;
        let out = client(base)
            .add_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap();
        assert!(matches!(out, GuestOutcome::Unconfirmed(_)), "{out:?}");
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "an unknown-outcome PATCH is never retried"
        );

        // 5xx and an unreadable 2xx body are unknown outcomes too.
        for (status, body) in [
            (503, r#"{"error":{"message":"backend"}}"#),
            (200, "{not json"),
        ] {
            let mut server = Server::new_async().await;
            let _get = get_mock(&mut server, &event("\"e1\"", existing_attendees(), true)).await;
            let patch = server
                .mock("PATCH", PATH)
                .match_query(Matcher::Any)
                .with_status(status)
                .with_body(body)
                .expect(1)
                .create_async()
                .await;
            let out = client(server.url())
                .add_guest(OWNER, "primary", "ev1", GUEST)
                .await
                .unwrap();
            assert!(
                matches!(out, GuestOutcome::Unconfirmed(_)),
                "{status}: {out:?}"
            );
            patch.assert_async().await;
        }

        // A 2xx whose attendee list lacks the guest is not a confirmation.
        let mut server = Server::new_async().await;
        let _get = get_mock(&mut server, &event("\"e1\"", existing_attendees(), true)).await;
        let _patch = server
            .mock("PATCH", PATH)
            .match_query(Matcher::Any)
            .with_body(event("\"e2\"", existing_attendees(), true).to_string())
            .create_async()
            .await;
        let out = client(server.url())
            .add_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap();
        assert!(matches!(out, GuestOutcome::Unconfirmed(_)), "{out:?}");
    }

    #[tokio::test]
    async fn undo_removes_only_that_attendee() {
        let mut server = Server::new_async().await;
        let list = with_guest(
            existing_attendees(),
            json!({"email": GUEST, "responseStatus": "needsAction"}),
        );
        let got = event("\"u1\"", list, true);
        let get = get_mock(&mut server, &got).await;
        let patch = server
            .mock("PATCH", PATH)
            .match_header("if-match", "\"u1\"")
            .match_query(Matcher::UrlEncoded("sendUpdates".into(), "all".into()))
            .match_body(Matcher::Json(json!({"attendees": existing_attendees()})))
            .with_body(event("\"u2\"", existing_attendees(), true).to_string())
            .expect(1)
            .create_async()
            .await;
        let out = client(server.url())
            .remove_guest(OWNER, "primary", "ev1", "TEAMMATE@example.test")
            .await
            .unwrap();
        get.assert_async().await;
        patch.assert_async().await;
        assert_eq!(
            out,
            GuestOutcome::Removed {
                etag: Some("\"u2\"".into()),
                attendees: vec![
                    OWNER.into(),
                    "alex@example.test".into(),
                    "room-1@resource.example.test".into(),
                    "sam@example.test".into(),
                ],
            }
        );

        // Not on the event: nothing to undo, nothing written.
        let mut server = Server::new_async().await;
        let _get = get_mock(&mut server, &event("\"u1\"", existing_attendees(), true)).await;
        let none = server
            .mock("PATCH", PATH)
            .match_query(Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let out = client(server.url())
            .remove_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap();
        assert_eq!(out, GuestOutcome::Noop);
        none.assert_async().await;
    }

    #[tokio::test]
    async fn undo_refused_after_accept() {
        let mut server = Server::new_async().await;
        let list = with_guest(
            existing_attendees(),
            json!({"email": GUEST, "responseStatus": "accepted"}),
        );
        let get = get_mock(&mut server, &event("\"u1\"", list, true)).await;
        let patch = server
            .mock("PATCH", PATH)
            .match_query(Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let out = client(server.url())
            .remove_guest(OWNER, "primary", "ev1", GUEST)
            .await
            .unwrap();
        assert_eq!(out, GuestOutcome::AlreadyAccepted);
        get.assert_async().await;
        patch.assert_async().await;
    }
}
