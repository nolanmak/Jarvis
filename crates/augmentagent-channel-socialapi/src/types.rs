//! Serde models for the SocialAPI.ai REST API.
//!
//! These deliberately model a *pragmatic subset* of each response — enough for
//! posting and inbox triage. Unknown fields are ignored on deserialize (no
//! `deny_unknown_fields`) so the API can add fields without breaking us.

use serde::{Deserialize, Serialize};

/// A connected social account ("brand") behind the SocialAPI.ai key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub brand_id: String,
    /// Underlying platform, e.g. `"instagram"`, `"twitter"`, `"linkedin"`.
    pub platform: String,
    pub display_name: String,
    /// Public handle / username on the platform (without a leading `@`).
    pub handle: String,
}

/// Response from `POST /accounts/connect` — an OAuth URL to send the user to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectResponse {
    pub auth_url: String,
}

/// One destination for a post. A single create-post call can fan out to many.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostTarget {
    pub account_id: String,
    /// Platform discriminator, mirrors [`Account::platform`].
    pub platform: String,
}

/// Body for `POST /posts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePostRequest {
    pub targets: Vec<PostTarget>,
    pub body: String,
    /// Optional attached media (urns/ids returned from the upload-url flow).
    /// Omitted from the wire when `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub media: Option<Vec<String>>,
}

/// Response from `POST /posts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePostResponse {
    pub id: String,
}

/// A comment on one of our posts (inbox item).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub post_id: String,
    pub author: String,
    pub text: String,
    pub created_at: String,
}

/// A direct-message thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    /// Account this conversation belongs to.
    pub account_id: String,
    /// Other party's handle / display name.
    pub with: String,
    /// Messages in the thread, oldest first. May be empty in list views.
    #[serde(default)]
    pub messages: Vec<DmMessage>,
}

/// A single message inside a [`Conversation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmMessage {
    pub id: String,
    pub author: String,
    pub text: String,
    pub created_at: String,
}

/// Body for replying to a comment or DM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyRequest {
    pub text: String,
}

/// Body for `POST /media/upload-url` — request a presigned upload slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MediaUploadRequest {
    /// MIME type of the media to be uploaded, e.g. `"image/png"`.
    pub content_type: String,
    /// Size in bytes (some backends require it to presign).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size_bytes: Option<u64>,
}

/// Response from `POST /media/upload-url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MediaUploadResponse {
    /// Opaque media id to reference in [`CreatePostRequest::media`].
    pub media_id: String,
    /// Presigned URL the caller PUTs the bytes to.
    pub upload_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_post_request_omits_none_media() {
        let req = CreatePostRequest {
            targets: vec![PostTarget {
                account_id: "acc_1".into(),
                platform: "twitter".into(),
            }],
            body: "hello".into(),
            media: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("media").is_none());
        assert_eq!(v["body"], "hello");
        assert_eq!(v["targets"][0]["account_id"], "acc_1");
    }

    #[test]
    fn create_post_request_includes_some_media() {
        let req = CreatePostRequest {
            targets: vec![],
            body: "x".into(),
            media: Some(vec!["m_1".into()]),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["media"][0], "m_1");
    }

    #[test]
    fn media_upload_request_snake_case() {
        let req = MediaUploadRequest {
            content_type: "image/png".into(),
            size_bytes: Some(42),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["content_type"], "image/png");
        assert_eq!(v["size_bytes"], 42);
    }

    #[test]
    fn account_round_trip() {
        let a = Account {
            id: "id".into(),
            brand_id: "b".into(),
            platform: "instagram".into(),
            display_name: "Brand".into(),
            handle: "brand".into(),
        };
        let j = serde_json::to_string(&a).unwrap();
        let back: Account = serde_json::from_str(&j).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn conversation_defaults_empty_messages() {
        let c: Conversation = serde_json::from_value(serde_json::json!({
            "id": "c1", "account_id": "a1", "with": "someone"
        }))
        .unwrap();
        assert!(c.messages.is_empty());
    }
}
