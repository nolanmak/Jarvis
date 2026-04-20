//! LinkedIn voyager (internal web) API client.
//!
//! Narrow scope: list recent DM threads + send a reply to an existing thread.
//! No new-thread creation, no attachments, no group-send — v1 scope.
//!
//! Quirks learned from reverse-engineering (this codebase's reconnaissance
//! captured via the claude_intercept proxy on 2026-04-19):
//! - `messengerConversations` responds to `GET` with a `queryId` + `variables`
//!   tuple in the query string; mailboxUrn is the user's own fsd_profile urn.
//! - `createMessage` POST wants `trackingId` as the **raw 16 bytes of a UUID**
//!   encoded as a Latin-1 string (NOT the hyphenated form). The `originToken`
//!   is the hyphenated form of the same UUID. Diverging these => HTTP 400.
//! - queryIds (`messengerConversations.74c17e85...`) can rotate on LinkedIn
//!   deploys — we default to a known-good id and expose an env override for
//!   hotfixes without a rebuild.

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

use crate::auth::LinkedInAuth;
use crate::types::{Dm, MemberUrn};

/// queryId as observed in captures on 2026-04-19. If LinkedIn rotates it,
/// override via `AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID` without a
/// recompile.
pub const DEFAULT_CONVERSATIONS_QUERY_ID: &str =
    "messengerConversations.74c17e85611b60b7ba2700481151a316";

#[derive(Debug, Error)]
pub enum LinkedInError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth expired (401/403); re-run `augmentagent linkedin login`")]
    AuthExpired,
    #[error("voyager: {status}: {body}")]
    Voyager { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(String),
}

#[async_trait]
pub trait LinkedInApi: Send + Sync {
    /// List the most recent 1-on-1 DM threads with the last message of each.
    /// Group chats are filtered out (v1 doesn't draft for group).
    async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError>;

    /// Send a reply on an existing conversation. Returns the backendUrn of
    /// the new message on success.
    async fn send_message(
        &self,
        conversation_urn: &str,
        text: &str,
    ) -> Result<String, LinkedInError>;
}

pub struct VoyagerClient {
    http: reqwest::Client,
    auth: LinkedInAuth,
    conversations_query_id: String,
}

impl VoyagerClient {
    pub fn new(auth: LinkedInAuth) -> Self {
        let query_id = std::env::var("AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID")
            .unwrap_or_else(|_| DEFAULT_CONVERSATIONS_QUERY_ID.to_string());
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            auth,
            conversations_query_id: query_id,
        }
    }

    fn base_headers(&self) -> Result<reqwest::header::HeaderMap, LinkedInError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut h = HeaderMap::new();
        let mut set = |name: &'static str, val: String| -> Result<(), LinkedInError> {
            let name = HeaderName::from_static(name);
            let value = HeaderValue::from_str(&val)
                .map_err(|e| LinkedInError::Config(format!("{name}: {e}")))?;
            h.insert(name, value);
            Ok(())
        };
        set("cookie", self.auth.cookie_header())?;
        set(
            "csrf-token",
            self.auth
                .csrf_token()
                .map_err(|e| LinkedInError::Config(e.to_string()))?,
        )?;
        set("x-restli-protocol-version", "2.0.0".into())?;
        set(
            "x-li-accept",
            "application/vnd.linkedin.normalized+json+2.1".into(),
        )?;
        set("x-li-query-accept", "application/graphql".into())?;
        set("accept", "*/*".into())?;
        set("referer", "https://www.linkedin.com/messaging/".into())?;
        set("origin", "https://www.linkedin.com".into())?;
        Ok(h)
    }
}

#[async_trait]
impl LinkedInApi for VoyagerClient {
    async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
        let mailbox_urn = &self.auth.member_urn;
        let encoded_mailbox = urlencode_restli(mailbox_urn);
        let url = format!(
            "https://www.linkedin.com/voyager/api/voyagerMessagingGraphQL/graphql\
             ?queryId={qid}&variables=(mailboxUrn:{mbox})",
            qid = self.conversations_query_id,
            mbox = encoded_mailbox,
        );

        let resp = self
            .http
            .get(&url)
            .headers(self.base_headers()?)
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        let payload: MailboxResponse = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("mailbox json: {e}")))?;

        let my_urn = self.auth.member_urn.as_str();
        let mut out = Vec::new();
        for conv in payload.data.messenger_conversations_by_sync_token.elements {
            if conv.group_chat {
                continue;
            }
            if let Some(dm) = build_dm(conv, my_urn) {
                out.push(dm);
            }
        }
        Ok(out)
    }

    async fn send_message(
        &self,
        conversation_urn: &str,
        text: &str,
    ) -> Result<String, LinkedInError> {
        let url = "https://www.linkedin.com/voyager/api/voyagerMessagingDashMessengerMessages\
                   ?action=createMessage";

        // LinkedIn wants trackingId as raw 16 UUID bytes (Latin-1) and
        // originToken as the hyphenated form of the same UUID. Diverging
        // these => 400.
        let id = Uuid::new_v4();
        let origin_token = id.to_string();
        // Convert 16 raw bytes to a Latin-1 string — each byte becomes a
        // codepoint 0..255. serde_json escapes non-ASCII as \uXXXX, which is
        // exactly what LinkedIn's captured browser traffic sends.
        let tracking_id: String = id.as_bytes().iter().map(|b| *b as char).collect();

        let body = serde_json::json!({
            "message": {
                "body": { "attributes": [], "text": text },
                "conversationUrn": conversation_urn,
                "originToken": origin_token,
                "renderContentUnions": [],
            },
            "mailboxUrn": self.auth.member_urn,
            "trackingId": tracking_id,
            "dedupeByClientGeneratedToken": false,
        });

        let resp = self
            .http
            .post(url)
            .headers(self.base_headers()?)
            .header("content-type", "text/plain;charset=UTF-8")
            .body(serde_json::to_vec(&body).expect("serialize send body"))
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }

        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("send json: {e}")))?;
        Ok(find_string_field(&v, "backendUrn").unwrap_or_default())
    }
}

/// URL-encode only the rest.li tuple punctuation. The urn slugs are already
/// URL-safe; we just need `:`, `,`, `(`, `)` escaped.
fn urlencode_restli(s: &str) -> String {
    s.replace('(', "%28")
        .replace(')', "%29")
        .replace(':', "%3A")
        .replace(',', "%2C")
}

fn find_string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(s)) = m.get(key) {
                if !s.is_empty() {
                    return Some(s.clone());
                }
            }
            for (_, vv) in m {
                if let Some(s) = find_string_field(vv, key) {
                    return Some(s);
                }
            }
            None
        }
        serde_json::Value::Array(a) => {
            for vv in a {
                if let Some(s) = find_string_field(vv, key) {
                    return Some(s);
                }
            }
            None
        }
        _ => None,
    }
}

// --- Response types (partial shapes; unknown fields ignored) ---

#[derive(Debug, Deserialize)]
struct MailboxResponse {
    data: MailboxData,
}

#[derive(Debug, Deserialize)]
struct MailboxData {
    #[serde(rename = "messengerConversationsBySyncToken")]
    messenger_conversations_by_sync_token: ConversationsList,
}

#[derive(Debug, Deserialize)]
struct ConversationsList {
    #[serde(default)]
    elements: Vec<Conversation>,
}

#[derive(Debug, Deserialize)]
struct Conversation {
    #[serde(rename = "entityUrn", default)]
    entity_urn: String,
    #[serde(rename = "lastActivityAt", default)]
    last_activity_at: i64,
    #[serde(rename = "conversationParticipants", default)]
    participants: Vec<Participant>,
    #[serde(default)]
    messages: MessagesBlock,
    #[serde(rename = "groupChat", default)]
    group_chat: bool,
}

#[derive(Debug, Deserialize)]
struct Participant {
    #[serde(rename = "hostIdentityUrn", default)]
    host_identity_urn: String,
    #[serde(rename = "participantType", default)]
    participant_type: ParticipantType,
}

#[derive(Debug, Default, Deserialize)]
struct ParticipantType {
    #[serde(default)]
    member: Option<Member>,
}

#[derive(Debug, Deserialize)]
struct Member {
    #[serde(rename = "firstName", default)]
    first_name: Option<AttributedText>,
    #[serde(rename = "lastName", default)]
    last_name: Option<AttributedText>,
}

#[derive(Debug, Default, Deserialize)]
struct AttributedText {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct MessagesBlock {
    #[serde(default)]
    elements: Vec<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(rename = "backendUrn", default)]
    backend_urn: String,
    #[serde(rename = "deliveredAt", default)]
    delivered_at: i64,
    #[serde(default)]
    body: Option<AttributedText>,
    #[serde(default)]
    actor: Option<Participant>,
}

fn build_dm(conv: Conversation, my_urn: &str) -> Option<Dm> {
    let msg = conv.messages.elements.into_iter().next()?;
    let text = msg
        .body
        .as_ref()
        .map(|b| b.text.clone())
        .unwrap_or_default();
    if text.is_empty() {
        return None;
    }

    let (peer_name, peer_urn) = conv
        .participants
        .iter()
        .find(|p| p.host_identity_urn != my_urn)
        .map(|p| {
            let m = p.participant_type.member.as_ref();
            let first = m
                .and_then(|x| x.first_name.as_ref())
                .map(|t| t.text.clone())
                .unwrap_or_default();
            let last = m
                .and_then(|x| x.last_name.as_ref())
                .map(|t| t.text.clone())
                .unwrap_or_default();
            let full = format!("{first} {last}").trim().to_string();
            (full, p.host_identity_urn.clone())
        })
        .unwrap_or_else(|| ("(unknown)".into(), String::new()));

    let actor_urn = msg
        .actor
        .as_ref()
        .map(|a| a.host_identity_urn.clone())
        .unwrap_or_default();

    Some(Dm {
        message_urn: msg.backend_urn,
        conversation_urn: conv.entity_urn,
        peer_name,
        peer_urn: MemberUrn(peer_urn),
        sender_urn: MemberUrn(actor_urn),
        text,
        delivered_at_ms: if msg.delivered_at != 0 {
            msg.delivered_at
        } else {
            conv.last_activity_at
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restli_encodes_tuple_punctuation() {
        assert_eq!(urlencode_restli("(a:b,c:d)"), "%28a%3Ab%2Cc%3Ad%29");
    }

    #[test]
    fn tracking_id_is_16_latin1_bytes_of_uuid() {
        // The exact invariant that cost us a 400 during the prototype: raw
        // UUID bytes serialized as Latin-1 characters, not hyphenated text.
        let id = Uuid::parse_str("82bc98f6-0676-4f2c-a56e-4cd976e3f7e8").unwrap();
        let tracking: String = id.as_bytes().iter().map(|b| *b as char).collect();
        assert_eq!(tracking.chars().count(), 16);
        // First byte of this UUID is 0x82 → codepoint 130.
        assert_eq!(tracking.chars().next().unwrap() as u32, 0x82);
    }

    #[test]
    fn build_dm_picks_non_self_participant() {
        let conv = Conversation {
            entity_urn: "urn:li:msg_conversation:xyz".into(),
            last_activity_at: 100,
            participants: vec![
                Participant {
                    host_identity_urn: "urn:li:fsd_profile:ME".into(),
                    participant_type: ParticipantType {
                        member: Some(Member {
                            first_name: Some(AttributedText { text: "Me".into() }),
                            last_name: Some(AttributedText { text: "Self".into() }),
                        }),
                    },
                },
                Participant {
                    host_identity_urn: "urn:li:fsd_profile:PEER".into(),
                    participant_type: ParticipantType {
                        member: Some(Member {
                            first_name: Some(AttributedText { text: "Tony".into() }),
                            last_name: Some(AttributedText { text: "Siu".into() }),
                        }),
                    },
                },
            ],
            messages: MessagesBlock {
                elements: vec![Message {
                    backend_urn: "urn:li:messagingMessage:m1".into(),
                    delivered_at: 200,
                    body: Some(AttributedText { text: "hello".into() }),
                    actor: Some(Participant {
                        host_identity_urn: "urn:li:fsd_profile:PEER".into(),
                        participant_type: ParticipantType::default(),
                    }),
                }],
            },
            group_chat: false,
        };
        let dm = build_dm(conv, "urn:li:fsd_profile:ME").unwrap();
        assert_eq!(dm.peer_name, "Tony Siu");
        assert_eq!(dm.peer_urn.0, "urn:li:fsd_profile:PEER");
        assert_eq!(dm.text, "hello");
    }

    #[test]
    fn build_dm_drops_empty_body() {
        let conv = Conversation {
            entity_urn: "urn:li:msg_conversation:xyz".into(),
            last_activity_at: 100,
            participants: vec![],
            messages: MessagesBlock {
                elements: vec![Message {
                    backend_urn: "m".into(),
                    delivered_at: 200,
                    body: None,
                    actor: None,
                }],
            },
            group_chat: false,
        };
        assert!(build_dm(conv, "me").is_none());
    }
}
