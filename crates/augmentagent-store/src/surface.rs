//! Stable, transport-neutral references for conversations and their owner.
//!
//! Provider IDs are opaque. In particular, a WhatsApp JID is never parsed as
//! a Discord snowflake or compacted into one. The storage keys use length
//! prefixes to avoid ambiguity when an ID itself contains `:`, `@`, or `|`.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SurfaceRefError {
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error("surface does not support {0}")]
    UnsupportedCapability(&'static str),
}

fn required<'a>(value: &'a str, name: &'static str) -> Result<&'a str, SurfaceRefError> {
    if value.trim().is_empty() {
        Err(SurfaceRefError::Invalid(name))
    } else {
        Ok(value)
    }
}

fn deserialize_required<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    required(&value, "surface identifier").map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn deserialize_optional_required<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(id) = &value {
        required(id, "surface identifier").map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn part(value: &str) -> String {
    format!("{}:{value}", value.len())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SurfacePlatform(String);

impl SurfacePlatform {
    pub fn new(value: impl Into<String>) -> Result<Self, SurfaceRefError> {
        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(SurfaceRefError::Invalid("surface platform"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SurfacePlatform {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceAccountRef {
    platform: SurfacePlatform,
    #[serde(deserialize_with = "deserialize_required")]
    account_id: String,
}

impl SurfaceAccountRef {
    pub fn new(
        platform: SurfacePlatform,
        account_id: impl Into<String>,
    ) -> Result<Self, SurfaceRefError> {
        let account_id = account_id.into();
        required(&account_id, "surface account ID")?;
        Ok(Self {
            platform,
            account_id,
        })
    }

    pub fn platform(&self) -> &SurfacePlatform {
        &self.platform
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceOwnerRef {
    account: SurfaceAccountRef,
    #[serde(deserialize_with = "deserialize_required")]
    sender_id: String,
}

impl SurfaceOwnerRef {
    pub fn new(
        account: SurfaceAccountRef,
        sender_id: impl Into<String>,
    ) -> Result<Self, SurfaceRefError> {
        let sender_id = sender_id.into();
        required(&sender_id, "surface owner ID")?;
        Ok(Self { account, sender_id })
    }

    pub fn account(&self) -> &SurfaceAccountRef {
        &self.account
    }

    pub fn sender_id(&self) -> &str {
        &self.sender_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceConversationRef {
    account: SurfaceAccountRef,
    #[serde(deserialize_with = "deserialize_required")]
    conversation_id: String,
    #[serde(default, deserialize_with = "deserialize_optional_required")]
    thread_id: Option<String>,
}

impl SurfaceConversationRef {
    pub fn new(
        account: SurfaceAccountRef,
        conversation_id: impl Into<String>,
        thread_id: Option<String>,
    ) -> Result<Self, SurfaceRefError> {
        let conversation_id = conversation_id.into();
        required(&conversation_id, "surface conversation ID")?;
        if let Some(thread) = &thread_id {
            required(thread, "surface thread ID")?;
        }
        Ok(Self {
            account,
            conversation_id,
            thread_id,
        })
    }

    pub fn account(&self) -> &SurfaceAccountRef {
        &self.account
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    /// Stable across restart and unambiguous even for IDs with separators.
    pub fn storage_key(&self) -> String {
        let thread = self
            .thread_id
            .as_ref()
            .map(|v| format!("S{}", part(v)))
            .unwrap_or_else(|| "N".into());
        format!(
            "v1|{}|{}|{}|{thread}",
            part(self.account.platform.as_str()),
            part(self.account.account_id()),
            part(&self.conversation_id)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceTurnRef {
    conversation: SurfaceConversationRef,
    #[serde(deserialize_with = "deserialize_required")]
    turn_id: String,
}

impl SurfaceTurnRef {
    pub fn new(
        conversation: SurfaceConversationRef,
        turn_id: impl Into<String>,
    ) -> Result<Self, SurfaceRefError> {
        let turn_id = turn_id.into();
        required(&turn_id, "surface turn ID")?;
        Ok(Self {
            conversation,
            turn_id,
        })
    }

    pub fn conversation(&self) -> &SurfaceConversationRef {
        &self.conversation
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn storage_key(&self) -> String {
        format!(
            "turn|{}|{}",
            self.conversation.storage_key(),
            part(&self.turn_id)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceMessageRef {
    conversation: SurfaceConversationRef,
    #[serde(deserialize_with = "deserialize_required")]
    message_id: String,
}

impl SurfaceMessageRef {
    pub fn new(
        conversation: SurfaceConversationRef,
        message_id: impl Into<String>,
    ) -> Result<Self, SurfaceRefError> {
        let message_id = message_id.into();
        required(&message_id, "surface message ID")?;
        Ok(Self {
            conversation,
            message_id,
        })
    }

    pub fn conversation(&self) -> &SurfaceConversationRef {
        &self.conversation
    }

    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    pub fn storage_key(&self) -> String {
        format!(
            "message|{}|{}",
            self.conversation.storage_key(),
            part(&self.message_id)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceReplyTarget {
    conversation: SurfaceConversationRef,
    #[serde(default, deserialize_with = "deserialize_optional_required")]
    quoted_message_id: Option<String>,
}

impl SurfaceReplyTarget {
    pub fn new(
        conversation: SurfaceConversationRef,
        quoted_message_id: Option<String>,
    ) -> Result<Self, SurfaceRefError> {
        if let Some(id) = &quoted_message_id {
            required(id, "quoted surface message ID")?;
        }
        Ok(Self {
            conversation,
            quoted_message_id,
        })
    }

    pub fn conversation(&self) -> &SurfaceConversationRef {
        &self.conversation
    }

    pub fn quoted_message_id(&self) -> Option<&str> {
        self.quoted_message_id.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceCapability {
    Query,
    Approve,
    Send,
    Schedule,
    ModelControl,
    ProcessControl,
    MediaRead,
    MediaWrite,
    History,
    Notifications,
    Voice,
}

impl SurfaceCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Approve => "approve",
            Self::Send => "send",
            Self::Schedule => "schedule",
            Self::ModelControl => "model_control",
            Self::ProcessControl => "process_control",
            Self::MediaRead => "media_read",
            Self::MediaWrite => "media_write",
            Self::History => "history",
            Self::Notifications => "notifications",
            Self::Voice => "voice",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceCapabilities(BTreeSet<SurfaceCapability>);

impl SurfaceCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = SurfaceCapability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    pub fn require(&self, capability: SurfaceCapability) -> Result<(), SurfaceRefError> {
        if self.0.contains(&capability) {
            Ok(())
        } else {
            Err(SurfaceRefError::UnsupportedCapability(capability.as_str()))
        }
    }
}
