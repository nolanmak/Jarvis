//! Reply target for an ingested iMessage row (#1302).
//!
//! Ingested rows carry `thread_id = "imessage:<identifier>"`, where the
//! identifier is `chat.chat_identifier` from the Mac. For a 1:1 iMessage chat
//! that is the other person's handle, which is exactly what the sender
//! addresses (`send … to participant <handle>`; spike #1280). Groups can only
//! be addressed by the chat guid copied from `chat.db`, never one built from a
//! handle — the prefix changed to `any;-;` on macOS 26.

use augmentagent_store::ImessageTargetKind;

use crate::bundle::Conversation;

pub const THREAD_PREFIX: &str = "imessage:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImessageTarget {
    /// What the sender passes to Messages: a handle or a chat guid.
    pub target: String,
    pub kind: ImessageTargetKind,
    /// `chat_identifier`; the key used by the allowlists.
    pub conversation: String,
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    NotImessage,
    InvalidHandle,
    UnknownConversation,
    GroupUnsupported,
    ServiceUnsupported(String),
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImessage => write!(f, "not an iMessage thread"),
            Self::InvalidHandle => write!(f, "invalid iMessage handle"),
            Self::UnknownConversation => write!(f, "conversation not found in the iMessage bundle"),
            Self::GroupUnsupported => {
                write!(
                    f,
                    "group chat has no exported chat guid, so it cannot be addressed"
                )
            }
            Self::ServiceUnsupported(s) => {
                write!(
                    f,
                    "{s} conversations are not supported; only iMessage can be sent"
                )
            }
        }
    }
}

impl std::error::Error for TargetError {}

/// The `chat_identifier` inside an `imessage:` thread id.
pub fn conversation_identifier(thread_id: &str) -> Result<&str, TargetError> {
    thread_id
        .strip_prefix(THREAD_PREFIX)
        .ok_or(TargetError::NotImessage)
}

fn valid_handle(h: &str) -> bool {
    !h.trim().is_empty() && !h.contains([';', '\n', '\r', '\0'])
}

/// Resolve where a reply on `thread_id` goes, using the bundle index.
pub fn resolve_target(
    thread_id: &str,
    conversations: &[Conversation],
) -> Result<ImessageTarget, TargetError> {
    let identifier = conversation_identifier(thread_id)?;
    if !valid_handle(identifier) {
        return Err(TargetError::InvalidHandle);
    }
    let conv = conversations
        .iter()
        .find(|c| c.identifier == identifier)
        .ok_or(TargetError::UnknownConversation)?;
    if conv.service != "iMessage" {
        return Err(TargetError::ServiceUnsupported(conv.service.clone()));
    }
    let (target, kind) = if conv.is_group() {
        match conv.chat_guid.as_deref().filter(|g| !g.trim().is_empty()) {
            Some(guid) if !guid.contains(['\n', '\r', '\0']) => {
                (guid.to_string(), ImessageTargetKind::ChatGuid)
            }
            _ => return Err(TargetError::GroupUnsupported),
        }
    } else {
        (identifier.to_string(), ImessageTargetKind::Handle)
    };
    Ok(ImessageTarget {
        target,
        kind,
        conversation: identifier.to_string(),
        service: conv.service.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(identifier: &str, service: &str, participants: &[&str]) -> Conversation {
        Conversation {
            identifier: identifier.into(),
            dir: "d".into(),
            title: "t".into(),
            participants: participants.iter().map(|s| s.to_string()).collect(),
            service: service.into(),
            chat_guid: None,
        }
    }

    const PHONE: &str = "+15555550100"; // pii-ok synthetic
    const MAIL: &str = "person@example.com";

    #[test]
    fn reply_target_accepts_one_to_one_imessage_handle() {
        let convs = [conv(PHONE, "iMessage", &[PHONE])];
        let t = resolve_target(&format!("imessage:{PHONE}"), &convs).unwrap();
        assert_eq!(t.target, PHONE);
        assert_eq!(t.kind, ImessageTargetKind::Handle);
        assert_eq!(t.conversation, PHONE);
        assert_eq!(t.service, "iMessage");
    }

    #[test]
    fn reply_target_accepts_email_handle() {
        let convs = [conv(MAIL, "iMessage", &[MAIL])];
        let t = resolve_target(&format!("imessage:{MAIL}"), &convs).unwrap();
        assert_eq!(t.target, MAIL);
    }

    #[test]
    fn group_by_chat_identifier_is_refused_without_guid() {
        let convs = [conv("chat123456", "iMessage", &[PHONE, MAIL])];
        assert_eq!(
            resolve_target("imessage:chat123456", &convs),
            Err(TargetError::GroupUnsupported)
        );
    }

    #[test]
    fn group_by_participant_count_is_refused_without_guid() {
        let convs = [conv("ab12cd", "iMessage", &[PHONE, MAIL])];
        assert_eq!(
            resolve_target("imessage:ab12cd", &convs),
            Err(TargetError::GroupUnsupported)
        );
    }

    #[test]
    fn sms_and_rcs_are_refused() {
        for svc in ["SMS", "RCS"] {
            let convs = [conv(PHONE, svc, &[PHONE])];
            assert_eq!(
                resolve_target(&format!("imessage:{PHONE}"), &convs),
                Err(TargetError::ServiceUnsupported(svc.into()))
            );
        }
    }

    #[test]
    fn unknown_conversation_is_refused() {
        let convs = [conv(MAIL, "iMessage", &[MAIL])];
        assert_eq!(
            resolve_target(&format!("imessage:{PHONE}"), &convs),
            Err(TargetError::UnknownConversation)
        );
    }

    #[test]
    fn thread_without_prefix_is_not_imessage() {
        let convs = [conv(PHONE, "iMessage", &[PHONE])];
        assert_eq!(resolve_target(PHONE, &convs), Err(TargetError::NotImessage));
        assert_eq!(
            resolve_target(&format!("telegram:{PHONE}"), &convs),
            Err(TargetError::NotImessage)
        );
    }

    #[test]
    fn empty_or_malformed_handle_is_invalid() {
        for bad in ["", "  ", "a;b", "a\nb"] {
            let convs = [conv(bad, "iMessage", &[bad])];
            assert_eq!(
                resolve_target(&format!("imessage:{bad}"), &convs),
                Err(TargetError::InvalidHandle),
                "{bad:?}"
            );
        }
    }
}
