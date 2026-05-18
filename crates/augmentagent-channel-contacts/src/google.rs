//! Backend A — Google People API via the Composio Google grant (#62).
//!
//! Reuses the *same* Composio v3 REST surface the Gmail / Calendar / Drive
//! channels already use (`POST /api/v3/tools/execute/{action}` with
//! `x-api-key` + `{user_id, arguments}`). We call
//! `GOOGLE_PEOPLE_LIST_CONNECTIONS` (Composio's wrapper over
//! `people.connections.list`) with
//! `personFields=names,emailAddresses,phoneNumbers,addresses,organizations,birthdays,metadata`
//! and the persisted `syncToken` for delta pulls.
//!
//! The Composio People response is JSON (not vCard); we map it into the
//! shared [`VCard`] shape so the upsert engine stays backend-agnostic.
//!
//! Composio retry/backoff policy is duplicated (intentionally — same note as
//! `augmentagent-channel-gdrive/src/composio.rs`: extracting a shared crate
//! would mean editing the prod email path).

use async_trait::async_trait;
use serde_json::Value;

use crate::source::{ContactsError, ContactsPull, ContactsSource};
use crate::vcard::VCard;

const PERSON_FIELDS: &str =
    "names,emailAddresses,phoneNumbers,addresses,organizations,birthdays,metadata";

pub struct GooglePeopleSource {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    entity_id: String,
}

impl GooglePeopleSource {
    pub fn new(api_key: String, entity_id: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: "https://backend.composio.dev".into(),
            api_key,
            entity_id,
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    async fn execute(
        &self,
        action: &str,
        arguments: Value,
    ) -> Result<Value, ContactsError> {
        let url = format!("{}/api/v3/tools/execute/{}", self.base_url, action);
        let body = serde_json::json!({
            "user_id": self.entity_id,
            "arguments": arguments,
        });
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.json::<Value>().await.map_err(Into::into);
                    }
                    let text = resp.text().await.unwrap_or_default();
                    let retryable =
                        status.as_u16() == 429 || status.is_server_error();
                    if retryable && attempt < MAX_ATTEMPTS {
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(ContactsError::Backend(format!(
                        "{action} → {status}: {text}"
                    )));
                }
                Err(e)
                    if attempt < MAX_ATTEMPTS
                        && (e.is_timeout() || e.is_connect() || e.is_request()) =>
                {
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(ContactsError::Http(e)),
            }
        }
    }
}

async fn backoff(attempt: u32) {
    let ms = 300u64 * (1u64 << attempt.min(5));
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[async_trait]
impl ContactsSource for GooglePeopleSource {
    fn backend_id(&self) -> &'static str {
        "google_people"
    }

    async fn list_contacts(
        &self,
        since_token: Option<&str>,
    ) -> Result<ContactsPull, ContactsError> {
        let mut cards = Vec::new();
        let mut page_token: Option<String> = None;
        let mut next_sync_token: Option<String> = None;

        // People API caps at ~1000/page; loop on nextPageToken. With
        // syncToken set the server returns only changed contacts.
        for _ in 0..50 {
            let mut args = serde_json::json!({
                "resourceName": "people/me",
                "personFields": PERSON_FIELDS,
                "pageSize": 1000,
                "requestSyncToken": true,
            });
            if let Some(t) = since_token {
                args["syncToken"] = Value::String(t.to_string());
            }
            if let Some(pt) = &page_token {
                args["pageToken"] = Value::String(pt.clone());
            }

            let v = self
                .execute("GOOGLE_PEOPLE_LIST_CONNECTIONS", args)
                .await?;
            let root = drill(&v);

            if let Some(arr) =
                root.get("connections").and_then(|c| c.as_array())
            {
                for person in arr {
                    if let Some(card) = person_to_vcard(person) {
                        cards.push(card);
                    }
                }
            }
            if let Some(st) = root
                .get("nextSyncToken")
                .and_then(|s| s.as_str())
            {
                next_sync_token = Some(st.to_string());
            }
            match root
                .get("nextPageToken")
                .and_then(|s| s.as_str())
            {
                Some(pt) if !pt.is_empty() => page_token = Some(pt.to_string()),
                _ => break,
            }
        }

        Ok(ContactsPull {
            cards,
            next_sync_token,
        })
    }
}

/// Composio nests the Google payload under `data` / `data.response_data`.
/// Drill to the object that actually carries `connections`.
fn drill(v: &Value) -> &Value {
    for path in [
        v.get("data")
            .and_then(|d| d.get("response_data")),
        v.get("data"),
        Some(v),
    ]
    .into_iter()
    .flatten()
    {
        if path.get("connections").is_some()
            || path.get("nextSyncToken").is_some()
        {
            return path;
        }
    }
    v
}

/// Map one People `Person` resource into our [`VCard`]. Empty fields stay
/// empty (no invention).
fn person_to_vcard(p: &Value) -> Option<VCard> {
    let mut card = VCard::default();

    if let Some(name) = p
        .get("names")
        .and_then(|n| n.as_array())
        .and_then(|a| a.first())
    {
        card.full_name = name
            .get("displayName")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
    }
    if let Some(arr) = p.get("emailAddresses").and_then(|e| e.as_array()) {
        for e in arr {
            if let Some(v) = e.get("value").and_then(|s| s.as_str()) {
                if !v.is_empty() {
                    card.emails.push(v.to_string());
                }
            }
        }
    }
    if let Some(arr) = p.get("phoneNumbers").and_then(|e| e.as_array()) {
        for ph in arr {
            if let Some(v) = ph.get("value").and_then(|s| s.as_str()) {
                if !v.is_empty() {
                    card.phones.push(v.to_string());
                }
            }
        }
    }
    if let Some(addr) = p
        .get("addresses")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
    {
        let formatted = addr
            .get("formattedValue")
            .and_then(|s| s.as_str())
            .map(|s| s.replace('\n', ", "))
            .filter(|s| !s.is_empty());
        card.address = formatted;
    }
    if let Some(org) = p
        .get("organizations")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
    {
        card.organization = org
            .get("name")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        card.title = org
            .get("title")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
    }
    if let Some(bday) = p
        .get("birthdays")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("date"))
    {
        let y = bday.get("year").and_then(|x| x.as_i64());
        let m = bday.get("month").and_then(|x| x.as_i64());
        let d = bday.get("day").and_then(|x| x.as_i64());
        if let (Some(m), Some(d)) = (m, d) {
            card.birthday = Some(match y {
                Some(y) => format!("{y:04}-{m:02}-{d:02}"),
                None => format!("--{m:02}{d:02}"),
            });
        }
    }
    card.uid = p
        .get("resourceName")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    if card.is_empty() {
        None
    } else {
        Some(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_people_person_to_vcard() {
        let p = serde_json::json!({
            "resourceName": "people/c123",
            "names": [{ "displayName": "Jane Doe" }],
            "emailAddresses": [{ "value": "jane@x.com" }],
            "phoneNumbers": [{ "value": "+1 415 555 2671" }],
            "addresses": [{ "formattedValue": "123 Main St\nAnytown CA" }],
            "organizations": [{ "name": "Acme", "title": "Staff Engineer" }],
            "birthdays": [{ "date": { "month": 3, "day": 12 } }]
        });
        let c = person_to_vcard(&p).unwrap();
        assert_eq!(c.full_name, "Jane Doe");
        assert_eq!(c.emails, vec!["jane@x.com"]);
        assert_eq!(c.phones, vec!["+1 415 555 2671"]);
        assert_eq!(c.address.as_deref(), Some("123 Main St, Anytown CA"));
        assert_eq!(c.organization.as_deref(), Some("Acme"));
        assert_eq!(c.title.as_deref(), Some("Staff Engineer"));
        assert_eq!(c.birthday.as_deref(), Some("--0312"));
        assert_eq!(c.uid, "people/c123");
    }

    #[test]
    fn empty_person_dropped() {
        assert!(person_to_vcard(&serde_json::json!({})).is_none());
    }

    #[test]
    fn drill_finds_nested_connections() {
        let v = serde_json::json!({
            "data": { "response_data": { "connections": [], "nextSyncToken": "tok" } }
        });
        let r = drill(&v);
        assert_eq!(r.get("nextSyncToken").and_then(|s| s.as_str()), Some("tok"));
    }

    #[tokio::test]
    async fn list_contacts_parses_composio_envelope() {
        let mut server = mockito::Server::new_async().await;
        let body = serde_json::json!({
            "data": { "response_data": {
                "connections": [
                    { "names": [{ "displayName": "A B" }],
                      "phoneNumbers": [{ "value": "+1 415 555 0001" }] }
                ],
                "nextSyncToken": "sync-9"
            }}
        })
        .to_string();
        let _m = server
            .mock(
                "POST",
                "/api/v3/tools/execute/GOOGLE_PEOPLE_LIST_CONNECTIONS",
            )
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        let src = GooglePeopleSource::new("k".into(), "ent".into())
            .with_base_url(server.url());
        let pull = src.list_contacts(None).await.unwrap();
        assert_eq!(pull.cards.len(), 1);
        assert_eq!(pull.cards[0].full_name, "A B");
        assert_eq!(pull.next_sync_token.as_deref(), Some("sync-9"));
    }
}
