//! Discord REST response types. Partial — we deserialize only fields the
//! channel pipeline needs; `#[serde(default)]` + `allow unknown fields` lets
//! Discord add keys without breaking us.

use serde::Deserialize;

/// A DM or group-DM channel (`GET /users/@me/channels`).
#[derive(Debug, Clone, Deserialize)]
pub struct DmChannel {
    pub id: String,
    /// `1` = 1:1 DM, `3` = group DM.
    #[serde(rename = "type")]
    pub channel_type: u8,
    #[serde(default)]
    pub recipients: Vec<User>,
    #[serde(default)]
    pub last_message_id: Option<String>,
}

impl DmChannel {
    /// Human-friendly label — recipients joined by `, ` (or "group DM" for
    /// unnamed type-3 channels with no recipients in the payload).
    pub fn display_name(&self) -> String {
        if self.recipients.is_empty() {
            return format!("channel {}", self.id);
        }
        self.recipients
            .iter()
            .map(|r| r.display_label())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn is_one_to_one(&self) -> bool {
        self.channel_type == 1
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildChannel {
    pub id: String,
    pub name: String,
    /// `0` = text channel (only kind we read from).
    #[serde(rename = "type")]
    pub channel_type: u8,
    #[serde(default)]
    pub parent_id: Option<String>,
}

impl GuildChannel {
    pub fn is_text(&self) -> bool {
        self.channel_type == 0
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

impl User {
    pub fn display_label(&self) -> String {
        self.global_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.username.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub channel_id: String,
    pub author: User,
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Discord message type. `0` = default user message; other types (system
    /// messages like "user pinned a message", "user joined") we skip.
    #[serde(rename = "type", default)]
    pub message_type: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub url: String,
    pub size: u64,
    /// Discord populates `content_type` for most attachments (e.g.
    /// `text/plain; charset=utf-8`). It's optional so we fall back to the
    /// filename extension when missing.
    #[serde(default)]
    pub content_type: Option<String>,
}

impl Attachment {
    /// `true` if this attachment is an image the reasoner can view — either
    /// an `image/*` mime type or an extension on the shared allowlist
    /// (`augmentagent_channel_core::images::IMAGE_EXT_ALLOWLIST`). Images are
    /// downloaded to `/tmp/aa-img-*` and referenced via `IMAGE:` marker
    /// lines rather than inlined (they aren't UTF-8).
    pub fn is_image_like(&self) -> bool {
        if let Some(ct) = self.content_type.as_deref() {
            let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            if ct.starts_with("image/") {
                return true;
            }
        }
        std::path::Path::new(&self.filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                augmentagent_channel_core::images::IMAGE_EXT_ALLOWLIST
                    .contains(&e.to_ascii_lowercase().as_str())
            })
            .unwrap_or(false)
    }

    /// `true` if this attachment looks like a plain-text file we can usefully
    /// inline into the agent prompt — either a `text/*` mime type or one of a
    /// short allowlist of common extensions.
    pub fn is_text_like(&self) -> bool {
        if let Some(ct) = self.content_type.as_deref() {
            let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            if ct.starts_with("text/") {
                return true;
            }
            if matches!(
                ct.as_str(),
                "application/json"
                    | "application/xml"
                    | "application/x-yaml"
                    | "application/yaml"
            ) {
                return true;
            }
        }
        let lower = self.filename.to_ascii_lowercase();
        matches!(
            std::path::Path::new(&lower)
                .extension()
                .and_then(|e| e.to_str()),
            Some(
                "txt"
                    | "md"
                    | "markdown"
                    | "log"
                    | "csv"
                    | "tsv"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "toml"
                    | "xml"
                    | "ini"
                    | "conf"
                    | "rs"
                    | "py"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "sh"
            )
        )
    }
}

impl Message {
    /// `true` iff this is a normal user-sent text message (not a system event).
    pub fn is_default_user_message(&self) -> bool {
        self.message_type == 0 && !self.author.bot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(filename: &str, content_type: Option<&str>) -> Attachment {
        Attachment {
            id: "1".into(),
            filename: filename.into(),
            url: "https://cdn.discordapp.com/x".into(),
            size: 10,
            content_type: content_type.map(str::to_string),
        }
    }

    #[test]
    fn is_text_like_recognizes_text_mime() {
        assert!(att("notes", Some("text/plain; charset=utf-8")).is_text_like());
        assert!(att("notes", Some("text/markdown")).is_text_like());
    }

    #[test]
    fn is_text_like_recognizes_json_and_yaml_mime() {
        assert!(att("data", Some("application/json")).is_text_like());
        assert!(att("data", Some("application/x-yaml")).is_text_like());
    }

    #[test]
    fn is_text_like_recognizes_text_extensions() {
        for f in [
            "notes.txt",
            "README.md",
            "out.log",
            "config.toml",
            "main.rs",
            "build.sh",
        ] {
            assert!(att(f, None).is_text_like(), "{f} should be text-like");
        }
    }

    #[test]
    fn is_text_like_rejects_binary() {
        assert!(!att("image.png", Some("image/png")).is_text_like());
        assert!(!att("doc.pdf", Some("application/pdf")).is_text_like());
        assert!(!att("blob.bin", None).is_text_like());
    }
}
