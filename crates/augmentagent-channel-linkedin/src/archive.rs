//! Read coverage is independent of reply eligibility. These page APIs retain
//! group conversations, owner messages and bodyless events (#1681).
use crate::{LinkedInAuth, VoyagerClient};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, time::Duration};

pub const MAILBOX_QUERY: &str = "messengerConversations.9501074288a12f3ae9e3c7ea243bccbf";

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("LinkedIn session rejected; refresh credentials")]
    Auth,
    #[error(
        "LinkedIn redirected the API request; session or request compatibility needs verification"
    )]
    Redirect,
    #[error("LinkedIn rate limited the read; retry after {0} seconds")]
    RateLimited(u64),
    #[error("LinkedIn API returned HTTP {0}")]
    Status(u16),
    #[error("LinkedIn request timed out")]
    Timeout,
    #[error("LinkedIn transport failed")]
    Transport,
    #[error("LinkedIn response shape: {0}")]
    Shape(&'static str),
    #[error("LinkedIn pagination did not advance")]
    CursorLoop,
    #[error("invalid LinkedIn archive configuration")]
    Config,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Participant {
    pub urn: String,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub urn: String,
    pub title: String,
    pub group: bool,
    pub participants: Vec<Participant>,
    pub last_activity_ms: i64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub urn: String,
    pub conversation: String,
    pub sender: String,
    pub from_me: bool,
    pub delivered_at_ms: i64,
    pub body: String,
    pub kind: String,
    pub content: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque provider value. Do not log it or infer its format.
    pub next: Option<String>,
}

/// One request per call: the orchestrator owns budgets, cancellation and durable
/// Retry-After deferrals. No automatic retries can hide a failed page.
pub struct ArchiveReader {
    http: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    mailbox: String,
    base: String,
    query: String,
}
impl ArchiveReader {
    pub fn new(auth: LinkedInAuth) -> Result<Self, ArchiveError> {
        Self::build(
            auth,
            "https://www.linkedin.com/voyager/api".into(),
            Duration::from_secs(30),
        )
    }
    /// Local mock endpoint only. Production never accepts a URL from a cursor.
    #[doc(hidden)]
    pub fn for_test(auth: LinkedInAuth, base: String) -> Result<Self, ArchiveError> {
        Self::for_test_with_timeout(auth, base, Duration::from_secs(30))
    }
    #[doc(hidden)]
    pub fn for_test_with_timeout(
        auth: LinkedInAuth,
        base: String,
        timeout: Duration,
    ) -> Result<Self, ArchiveError> {
        let u = reqwest::Url::parse(&base).map_err(|_| ArchiveError::Config)?;
        if u.scheme() != "http"
            || !matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        {
            return Err(ArchiveError::Config);
        }
        Self::build(auth, base, timeout)
    }
    fn build(auth: LinkedInAuth, base: String, timeout: Duration) -> Result<Self, ArchiveError> {
        auth.validate().map_err(|_| ArchiveError::Config)?;
        let mailbox = auth.member_urn.clone();
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .build()
            .map_err(|_| ArchiveError::Config)?;
        let mut headers = VoyagerClient::new(auth)
            .base_headers()
            .map_err(|_| ArchiveError::Config)?;
        for name in ["cookie", "csrf-token"] {
            if let Some(v) = headers.get_mut(name) {
                v.set_sensitive(true);
            }
        }
        let query = std::env::var("AUGMENTAGENT_LINKEDIN_ARCHIVE_QUERY_ID")
            .unwrap_or_else(|_| MAILBOX_QUERY.into());
        // IDs are identifiers, never URL fragments or arbitrary query strings.
        if !query.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
            return Err(ArchiveError::Config);
        }
        Ok(Self {
            http,
            headers,
            mailbox,
            base,
            query,
        })
    }
    pub fn mailbox(&self) -> &str {
        &self.mailbox
    }

    async fn get(&self, url: String) -> Result<Value, ArchiveError> {
        let response = self
            .http
            .get(url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        match status.as_u16() {
            401 | 403 => return Err(ArchiveError::Auth),
            300..=399 => return Err(ArchiveError::Redirect),
            429 => {
                let now = time::OffsetDateTime::now_utc().unix_timestamp();
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| retry_after(v, now))
                    .unwrap_or(60);
                return Err(ArchiveError::RateLimited(delay));
            }
            _ if !status.is_success() => return Err(ArchiveError::Status(status.as_u16())),
            _ => {}
        }
        // Stream with a finite body cap; Content-Length alone is not trusted.
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if body.len() + chunk.len() > 16 * 1024 * 1024 {
                return Err(ArchiveError::Shape("page exceeds 16 MiB"));
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| ArchiveError::Shape("invalid JSON"))?;
        if value
            .get("errors")
            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
        {
            return Err(ArchiveError::Shape("GraphQL error envelope"));
        }
        Ok(value)
    }
    pub async fn conversations(
        &self,
        category: &str,
        cursor: Option<&str>,
    ) -> Result<Page<Conversation>, ArchiveError> {
        if category.is_empty() || !category.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return Err(ArchiveError::Config);
        }
        let continuation = cursor
            .map(|c| format!(",nextCursor:{}", scalar(c)))
            .unwrap_or_default();
        let url = format!("{}/voyagerMessagingGraphQL/graphql?queryId={}&variables=(query:(predicateUnions:List((conversationCategoryPredicate:(category:{})))),count:20,mailboxUrn:{}{})",
            self.base, self.query, category, scalar(&self.mailbox), continuation);
        let value = self.get(url).await?;
        let root = value
            .pointer("/data/messengerConversationsByCategoryQuery")
            .ok_or(ArchiveError::Shape("missing conversation page"))?;
        let items = elements(root)?
            .iter()
            .map(parse_conversation)
            .collect::<Result<_, _>>()?;
        Ok(Page {
            items,
            next: next_cursor(root, cursor)?,
        })
    }
    pub async fn categories(&self) -> Result<Vec<String>, ArchiveError> {
        let url = format!("{}/voyagerMessagingGraphQL/graphql?queryId=messengerMailboxCounts.fc528a5a81a76dff212a4a3d2d48e84b&variables=(mailboxUrn:{})", self.base, scalar(&self.mailbox));
        let value = self.get(url).await?;
        let root = value
            .pointer("/data/messengerMailboxCountsByMailbox")
            .ok_or(ArchiveError::Shape("missing mailbox categories"))?;
        let mut categories = elements(root)?
            .iter()
            .map(|v| required(v, "category").map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?;
        categories.sort();
        categories.dedup();
        if categories.is_empty() {
            return Err(ArchiveError::Shape("empty category inventory"));
        }
        Ok(categories)
    }
    pub async fn conversation(&self, urn: &str) -> Result<Conversation, ArchiveError> {
        let url = format!("{}/voyagerMessagingGraphQL/graphql?queryId=messengerConversations.fb3eab135970d880d59c5fad1bc695f9&variables=(messengerConversationsId:{},count:20)", self.base, scalar(urn));
        let value = self.get(url).await?;
        let conv = parse_conversation(
            value
                .pointer("/data/messengerConversationsById")
                .ok_or(ArchiveError::Shape("missing conversation detail"))?,
        )?;
        if conv.urn != urn {
            return Err(ArchiveError::Shape("conversation identity mismatch"));
        }
        Ok(conv)
    }
    /// A read permission never grants group automation. Recheck at send time,
    /// including stale cards and conversations whose membership changed.
    pub async fn require_direct(&self, urn: &str) -> Result<(), ArchiveError> {
        let conv = self.conversation(urn).await?;
        if conv.group
            || conv.participants.len() != 2
            || !conv.participants.iter().any(|p| p.urn == self.mailbox)
        {
            return Err(ArchiveError::Shape(
                "group or unknown conversation is not eligible for automated replies",
            ));
        }
        Ok(())
    }
    pub async fn messages(
        &self,
        conversation: &str,
        cursor: Option<&str>,
    ) -> Result<Page<Message>, ArchiveError> {
        if conversation.is_empty() {
            return Err(ArchiveError::Config);
        }
        let mut url = reqwest::Url::parse(&format!(
            "{}/voyagerMessagingDashMessengerMessages",
            self.base
        ))
        .map_err(|_| ArchiveError::Config)?;
        url.query_pairs_mut()
            .append_pair("q", "conversation")
            .append_pair("conversationUrn", conversation)
            .append_pair("count", "50");
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("nextCursor", cursor);
        }
        let value = self.get(url.into()).await?;
        let mut unique = std::collections::BTreeMap::new();
        for value in elements(&value)? {
            let item = parse_message(value, conversation, &self.mailbox)?;
            if let Some(previous) = unique.get(&item.urn) {
                if previous != &item {
                    return Err(ArchiveError::Shape(
                        "conflicting copies of message identity",
                    ));
                }
            } else {
                unique.insert(item.urn.clone(), item);
            }
        }
        let mut items: Vec<_> = unique.into_values().collect();
        items.sort_by(|a, b| (a.delivered_at_ms, &a.urn).cmp(&(b.delivered_at_ms, &b.urn)));
        Ok(Page {
            items,
            next: next_cursor(&value, cursor)?,
        })
    }
}
fn transport_error(error: reqwest::Error) -> ArchiveError {
    if error.is_timeout() {
        ArchiveError::Timeout
    } else {
        ArchiveError::Transport
    }
}
fn scalar(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
fn required<'a>(v: &'a Value, key: &str) -> Result<&'a str, ArchiveError> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(ArchiveError::Shape("missing stable identity"))
}
fn elements(v: &Value) -> Result<&Vec<Value>, ArchiveError> {
    v.get("elements")
        .and_then(Value::as_array)
        .ok_or(ArchiveError::Shape("missing elements"))
}
fn next_cursor(v: &Value, previous: Option<&str>) -> Result<Option<String>, ArchiveError> {
    // Missing metadata is not proof of exhaustion.
    let next = v
        .pointer("/metadata/nextCursor")
        .ok_or(ArchiveError::Shape("missing continuation metadata"))?;
    if next.is_null() {
        return Ok(None);
    }
    let s = next
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(ArchiveError::Shape("invalid continuation"))?;
    if Some(s) == previous {
        return Err(ArchiveError::CursorLoop);
    }
    Ok(Some(s.into()))
}
fn text(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str().or_else(|| x.get("text").and_then(Value::as_str)))
        .unwrap_or("")
        .into()
}
fn parse_conversation(v: &Value) -> Result<Conversation, ArchiveError> {
    let mut seen = BTreeSet::new();
    let participants = v
        .get("conversationParticipants")
        .and_then(Value::as_array)
        .ok_or(ArchiveError::Shape("missing membership"))?
        .iter()
        .map(|p| {
            let urn = required(p, "hostIdentityUrn")?.to_string();
            let member = p.pointer("/participantType/member");
            let name = format!(
                "{} {}",
                text(member.and_then(|m| m.get("firstName"))),
                text(member.and_then(|m| m.get("lastName")))
            )
            .trim()
            .to_string();
            Ok(Participant { urn, name })
        })
        .collect::<Result<Vec<_>, ArchiveError>>()?
        .into_iter()
        .filter(|p| seen.insert(p.urn.clone()))
        .collect();
    Ok(Conversation {
        urn: required(v, "entityUrn")?.into(),
        title: text(v.get("title")),
        group: v
            .get("groupChat")
            .and_then(Value::as_bool)
            .ok_or(ArchiveError::Shape("unknown conversation kind"))?,
        participants,
        last_activity_ms: v
            .get("lastActivityAt")
            .and_then(Value::as_i64)
            .ok_or(ArchiveError::Shape("missing conversation timestamp"))?,
    })
}
fn parse_message(v: &Value, conversation: &str, owner: &str) -> Result<Message, ArchiveError> {
    if let Some(scope) = v.get("conversationUrn") {
        if scope.as_str() != Some(conversation) {
            return Err(ArchiveError::Shape(
                "message conversation identity mismatch",
            ));
        }
    }
    let sender = v
        .get("senderUrn")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/actor/hostIdentityUrn").and_then(Value::as_str))
        .or_else(|| v.get("actorUrn").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .ok_or(ArchiveError::Shape("missing sender"))?;
    let sender = sender
        .strip_prefix("urn:li:msg_messagingParticipant:")
        .unwrap_or(sender)
        .to_string();
    let body = text(v.get("body"));
    Ok(Message {
        urn: required(v, "backendUrn")?.into(),
        conversation: conversation.into(),
        from_me: sender == owner,
        sender,
        delivered_at_ms: v
            .get("deliveredAt")
            .and_then(Value::as_i64)
            .ok_or(ArchiveError::Shape("missing message timestamp"))?,
        kind: if body.is_empty() { "non_text" } else { "text" }.into(),
        body,
        content: v
            .get("renderContent")
            .or_else(|| v.get("renderContentUnions"))
            .cloned()
            .unwrap_or(Value::Null),
    })
}
fn retry_after(value: &str, now_secs: i64) -> u64 {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return seconds;
    }
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822)
        .map(|date| date.unix_timestamp().saturating_sub(now_secs).max(0) as u64)
        .unwrap_or(60)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_supports_seconds_and_dates() {
        assert_eq!(retry_after("120", 0), 120);
        assert_eq!(retry_after("Thu, 01 Jan 1970 00:02:00 GMT", 0), 120);
        assert_eq!(retry_after("Thu, 01 Jan 1970 00:02:00 GMT", 130), 0);
        assert_eq!(retry_after("bad", 0), 60);
    }
}
