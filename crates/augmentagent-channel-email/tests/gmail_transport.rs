//! #2388 — compose idempotency keys and Gmail draft reconciliation, proved
//! against synthetic drafts and a scripted Composio transport.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_channel_email::gmail::{
    compose_key, ComposeIdentity, ComposioClient, DraftCandidate, DraftReconciliation, GmailApi,
    GmailError, COMPOSE_KEY_HEADER,
};
use augmentagent_store::Email;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ENTITY: &str = "entity-synthetic";
const TO: &str = "Dana Example <dana@example.com>";

fn identity<'a>(
    thread_id: Option<&'a str>,
    to: &'a str,
    subject: &'a str,
    body: &'a str,
) -> ComposeIdentity<'a> {
    ComposeIdentity {
        entity_id: ENTITY,
        thread_id,
        to,
        subject,
        body,
    }
}

fn candidate(
    draft_id: &str,
    thread_id: Option<&str>,
    to: &str,
    subject: &str,
    key: Option<&str>,
) -> DraftCandidate {
    DraftCandidate {
        draft_id: draft_id.into(),
        thread_id: thread_id.map(str::to_string),
        to: to.into(),
        subject: subject.into(),
        compose_key: key.map(str::to_string),
    }
}

// ---- the key itself ----

#[test]
fn compose_key_is_stable_across_equivalent_compose_inputs() {
    let a = compose_key(&identity(
        Some("t-1"),
        "Dana Example <Dana@Example.com>, bo@example.org",
        "Re: Ground floor",
        "Body text",
    ));
    let b = compose_key(&identity(
        Some(" t-1 "),
        "bo@example.org ,  dana@example.com",
        "  ground FLOOR  ",
        "Body text",
    ));
    assert_eq!(a, b, "equivalent recipient/subject formatting must key the same");
    assert_eq!(a.len(), 64, "the key is an opaque sha256 hex digest: {a}");
    assert!(
        a.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "the key must be lower-case hex, not draft content: {a}"
    );
    assert!(
        !a.contains("Body") && !a.to_lowercase().contains("body"),
        "the key must not carry the draft body: {a}"
    );
}

#[test]
fn compose_key_changes_with_every_identity_field() {
    let base = identity(Some("t-1"), TO, "Ground floor", "Body text");
    let key = compose_key(&base);
    let mutations = [
        (
            "entity",
            ComposeIdentity {
                entity_id: "entity-other",
                ..base
            },
        ),
        (
            "thread",
            ComposeIdentity {
                thread_id: Some("t-2"),
                ..base
            },
        ),
        (
            "absent thread",
            ComposeIdentity {
                thread_id: None,
                ..base
            },
        ),
        (
            "recipient",
            ComposeIdentity {
                to: "someone-else@example.com",
                ..base
            },
        ),
        (
            "extra recipient",
            ComposeIdentity {
                to: "dana@example.com, bo@example.org",
                ..base
            },
        ),
        (
            "subject",
            ComposeIdentity {
                subject: "First floor",
                ..base
            },
        ),
        (
            "body",
            ComposeIdentity {
                body: "Body text, revised",
                ..base
            },
        ),
    ];
    for (field, mutated) in mutations {
        assert_ne!(
            key,
            compose_key(&mutated),
            "a changed {field} must change the compose key"
        );
    }
}

// ---- candidate narrowing ----

#[test]
fn candidate_narrowing_takes_the_thread_or_the_recipients_and_subject() {
    let on_thread = candidate("d-1", Some("t-1"), TO, "Ground floor", None);
    let standalone = candidate("d-2", None, "DANA@example.com", "Re: Ground floor", None);
    let elsewhere = candidate("d-3", None, "someone-else@example.com", "Ground floor", None);

    let threaded = identity(Some("t-1"), TO, "Ground floor", "Body text");
    assert!(on_thread.could_be(&threaded));
    assert!(!standalone.could_be(&threaded), "a reply belongs on its thread");

    let fresh = identity(None, TO, "Ground floor", "Body text");
    assert!(standalone.could_be(&fresh));
    assert!(!elsewhere.could_be(&fresh), "different recipients, different compose");
}

// ---- reconciliation outcomes, over synthetic drafts ----

#[tokio::test]
async fn reconciliation_reports_an_absent_draft_and_leaves_creation_to_the_caller() {
    let key = compose_key(&identity(Some("t-1"), TO, "Ground floor", "Body text"));
    let fake = FakeGmail::finding(Ok(vec![
        // Same thread, same subject, same recipients — but not this compose.
        candidate("d-unkeyed", Some("t-1"), TO, "Ground floor", None),
        candidate("d-other-key", Some("t-1"), TO, "Ground floor", Some("not-this-one")),
        // This compose's key, but on another thread.
        candidate("d-elsewhere", Some("t-9"), TO, "Ground floor", Some(&key)),
    ]));
    let identity = identity(Some("t-1"), TO, "Ground floor", "Body text");

    assert_eq!(
        fake.reconcile_compose_draft(ENTITY, &identity).await,
        DraftReconciliation::Absent
    );
    assert_eq!(
        fake.create_keyed_draft(ENTITY, TO, "Ground floor", "Body text", Some("t-1"), &key)
            .await
            .unwrap(),
        "draft-1",
        "an absent draft must leave the ordinary creation path open"
    );
}

#[tokio::test]
async fn reconciliation_adopts_the_single_draft_carrying_the_key() {
    let key = compose_key(&identity(Some("t-1"), TO, "Ground floor", "Body text"));
    let fake = FakeGmail::finding(Ok(vec![
        candidate("d-unkeyed", Some("t-1"), TO, "Ground floor", None),
        candidate("d-keyed", Some("t-1"), TO, "Ground floor", Some(&key)),
    ]));
    let identity = identity(Some("t-1"), TO, "Ground floor", "Body text");

    match fake.reconcile_compose_draft(ENTITY, &identity).await {
        DraftReconciliation::Adopted(c) => assert_eq!(c.draft_id, "d-keyed"),
        other => panic!("one matching header must be adopted, got {other:?}"),
    }
    assert_eq!(fake.created(), 0, "an adopted draft must not be re-created");
}

#[tokio::test]
async fn reconciliation_reports_duplicate_keys_as_ambiguous() {
    let key = compose_key(&identity(None, TO, "Ground floor", "Body text"));
    let fake = FakeGmail::finding(Ok(vec![
        candidate("d-first", None, TO, "Ground floor", Some(&key)),
        candidate("d-second", None, TO, "Ground floor", Some(&key)),
    ]));
    let identity = identity(None, TO, "Ground floor", "Body text");

    match fake.reconcile_compose_draft(ENTITY, &identity).await {
        DraftReconciliation::Ambiguous(found) => {
            let ids: Vec<&str> = found.iter().map(|c| c.draft_id.as_str()).collect();
            assert_eq!(ids, ["d-first", "d-second"]);
        }
        other => panic!("duplicate keys must be ambiguous, got {other:?}"),
    }
}

#[tokio::test]
async fn reconciliation_reports_a_failed_lookup_distinctly_from_an_absent_draft() {
    let fake = FakeGmail::finding(Err("GMAIL_LIST_DRAFTS → 503".into()));
    let identity = identity(Some("t-1"), TO, "Ground floor", "Body text");

    match fake.reconcile_compose_draft(ENTITY, &identity).await {
        DraftReconciliation::LookupFailed(why) => assert!(why.contains("503"), "{why}"),
        other => panic!("a lookup failure must not read as absent, got {other:?}"),
    }
}

// ---- trait defaults keep legacy fakes and callers compiling ----

#[tokio::test]
async fn a_fake_that_models_only_the_legacy_draft_api_still_reconciles_safely() {
    let fake = LegacyFake;
    assert_eq!(
        fake.create_draft(ENTITY, TO, "Ground floor", "Body text", None)
            .await
            .unwrap(),
        "legacy-draft"
    );
    assert_eq!(
        fake.create_keyed_draft(ENTITY, TO, "Ground floor", "Body text", None, "key-abc")
            .await
            .unwrap(),
        "legacy-draft",
        "the keyed path must fall back to create_draft for fakes that don't model it"
    );
    let identity = identity(None, TO, "Ground floor", "Body text");
    assert_eq!(
        fake.reconcile_compose_draft(ENTITY, &identity).await,
        DraftReconciliation::Absent
    );
}

// ---- the Composio transport ----

#[tokio::test]
async fn a_queued_compose_draft_stamps_the_compose_key_header() {
    let (addr, seen) = scripted_composio(vec![
        json!({"successful": true, "data": {"response_data": {"id": "d-keyed"}}}).to_string(),
    ])
    .await;
    let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

    let id = client
        .create_keyed_draft(
            ENTITY,
            "dana@example.com",
            "Ground floor",
            "Body text",
            Some("t-1"),
            "key-abc",
        )
        .await
        .expect("a keyed draft must be created");
    assert_eq!(id, "d-keyed");

    let request = requests(&seen).remove(0);
    assert!(
        request.contains(COMPOSE_KEY_HEADER) && request.contains("key-abc"),
        "a queued compose must stamp its key: {request}"
    );
}

#[tokio::test]
async fn an_ordinary_draft_creation_stays_header_free() {
    let (addr, seen) = scripted_composio(vec![
        json!({"successful": true, "data": {"response_data": {"id": "d-plain"}}}).to_string(),
    ])
    .await;
    let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

    let id = client
        .create_draft(ENTITY, "dana@example.com", "Ground floor", "Body text", None)
        .await
        .expect("ordinary draft creation must be unchanged");
    assert_eq!(id, "d-plain");

    let request = requests(&seen).remove(0);
    assert!(
        !request.contains(COMPOSE_KEY_HEADER),
        "an unqueued draft carries no compose key: {request}"
    );
}

#[tokio::test]
async fn composio_lookup_adopts_only_the_draft_whose_header_matches_exactly() {
    let identity = identity(Some("t-1"), TO, "Ground floor", "Body text");
    let key = compose_key(&identity);
    let listing = json!({"successful": true, "data": {"drafts": [
        synthetic_draft("d-elsewhere", "t-9", TO, "Ground floor", Some(&key)),
        synthetic_draft("d-unkeyed", "t-1", TO, "Ground floor", None),
        synthetic_draft("d-keyed", "t-1", TO, "Ground floor", Some(&key)),
    ]}});
    let (addr, seen) = scripted_composio(vec![listing.to_string()]).await;
    let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

    match client.reconcile_compose_draft(ENTITY, &identity).await {
        DraftReconciliation::Adopted(c) => {
            assert_eq!(c.draft_id, "d-keyed");
            assert_eq!(c.thread_id.as_deref(), Some("t-1"));
        }
        other => panic!("the keyed draft on the target thread must be adopted, got {other:?}"),
    }
    assert!(
        requests(&seen)[0].contains("GMAIL_LIST_DRAFTS"),
        "the lookup must search the mailbox's drafts"
    );
}

#[tokio::test]
async fn a_composio_lookup_failure_is_not_an_absent_draft() {
    let (addr, _seen) = scripted_composio(vec![
        json!({"successful": false, "error": "Requested entity was not found.", "log_id": "lg1"})
            .to_string(),
    ])
    .await;
    let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));
    let identity = identity(Some("t-1"), TO, "Ground floor", "Body text");

    match client.reconcile_compose_draft(ENTITY, &identity).await {
        DraftReconciliation::LookupFailed(why) => {
            assert!(why.contains("Requested entity was not found."), "{why}")
        }
        other => panic!("a refused listing must not read as absent, got {other:?}"),
    }
}

// ---- fixtures ----

fn synthetic_draft(
    draft_id: &str,
    thread_id: &str,
    to: &str,
    subject: &str,
    key: Option<&str>,
) -> serde_json::Value {
    let mut headers = vec![
        json!({"name": "To", "value": to}),
        json!({"name": "Subject", "value": subject}),
    ];
    if let Some(k) = key {
        headers.push(json!({"name": COMPOSE_KEY_HEADER, "value": k}));
    }
    json!({
        "id": draft_id,
        "message": {"id": format!("m-{draft_id}"), "threadId": thread_id, "payload": {"headers": headers}},
    })
}

/// A Gmail fake that models the legacy draft API plus one scripted lookup
/// result, leaving the reconciliation decision to the trait default.
struct FakeGmail {
    found: Result<Vec<DraftCandidate>, String>,
    created: Mutex<usize>,
}

impl FakeGmail {
    fn finding(found: Result<Vec<DraftCandidate>, String>) -> Self {
        Self {
            found,
            created: Mutex::new(0),
        }
    }

    fn created(&self) -> usize {
        *self.created.lock().unwrap()
    }
}

#[async_trait]
impl GmailApi for FakeGmail {
    async fn fetch_unread(&self, _e: &str, _limit: u32) -> Result<Vec<Email>, GmailError> {
        Ok(Vec::new())
    }
    async fn fetch_with_query(
        &self,
        _e: &str,
        _q: &str,
        _limit: u32,
    ) -> Result<Vec<Email>, GmailError> {
        Ok(Vec::new())
    }
    async fn create_draft(
        &self,
        _e: &str,
        _to: &str,
        _subject: &str,
        _body: &str,
        _thread: Option<&str>,
    ) -> Result<String, GmailError> {
        let mut n = self.created.lock().unwrap();
        *n += 1;
        Ok(format!("draft-{n}"))
    }
    async fn send_draft(&self, _e: &str, _d: &str) -> Result<Option<String>, GmailError> {
        Ok(None)
    }
    async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), GmailError> {
        Ok(())
    }
    async fn find_compose_drafts(
        &self,
        _e: &str,
        identity: &ComposeIdentity<'_>,
    ) -> Result<Vec<DraftCandidate>, GmailError> {
        match &self.found {
            Ok(found) => Ok(found
                .iter()
                .filter(|c| c.could_be(identity))
                .cloned()
                .collect()),
            Err(message) => Err(GmailError::Composio {
                message: message.clone(),
            }),
        }
    }
}

/// A fake written before #2388: it implements only the methods the trait has
/// always required, so compiling it at all is the compatibility assertion.
struct LegacyFake;

#[async_trait]
impl GmailApi for LegacyFake {
    async fn fetch_unread(&self, _e: &str, _limit: u32) -> Result<Vec<Email>, GmailError> {
        Ok(Vec::new())
    }
    async fn fetch_with_query(
        &self,
        _e: &str,
        _q: &str,
        _limit: u32,
    ) -> Result<Vec<Email>, GmailError> {
        Ok(Vec::new())
    }
    async fn create_draft(
        &self,
        _e: &str,
        _to: &str,
        _subject: &str,
        _body: &str,
        _thread: Option<&str>,
    ) -> Result<String, GmailError> {
        Ok("legacy-draft".into())
    }
    async fn send_draft(&self, _e: &str, _d: &str) -> Result<Option<String>, GmailError> {
        Ok(None)
    }
    async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), GmailError> {
        Ok(())
    }
}

/// A scripted Composio: answers `responses` in request order (the last entry
/// repeats) and records every request it saw, headers and body.
async fn scripted_composio(responses: Vec<String>) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let responses = Arc::new(responses);
    let seen_c = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let responses = Arc::clone(&responses);
            let seen = Arc::clone(&seen_c);
            tokio::spawn(async move {
                let mut raw: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read headers, then the Content-Length body: one read can
                // return a partial request.
                while let Ok(n) = socket.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&raw);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if raw.len() >= head_end + 4 + len {
                            break;
                        }
                    }
                }
                let body = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(String::from_utf8_lossy(&raw).into_owned());
                    responses[(seen.len() - 1).min(responses.len() - 1)].clone()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, seen)
}

fn requests(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock().unwrap().clone()
}
