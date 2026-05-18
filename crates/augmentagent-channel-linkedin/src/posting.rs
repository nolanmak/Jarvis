//! LinkedIn feed posting via Voyager `contentcreation/normShares` (#51 / #77).
//!
//! Phase 1 scope (per #77 §8): Voyager-only **text post** + **single-image**
//! post. Everything else — video, polls, scheduling, articles, multi-image,
//! browser fallback — is `Refs #51 — deferred` to Phase 2.
//!
//! Auth re-use: `VoyagerClient` already holds a `LinkedInAuth` (cookies +
//! member urn + csrf). `normShares` needs a strict subset of the DM path's
//! auth, plus a few content-creation-only headers ([`posting_headers`]).
//!
//! Quirks captured from public reverse-engineering (`snlagr/linkedin-post-
//! update`, `tomquirk/linkedin-api`) + web-app traffic:
//! - `commentaryV2.{text,attributes}` is the canonical body shape; mentions
//!   / hashtags ride in `attributes` (Phase 1 leaves it empty).
//! - `visibleToConnectionsOnly` toggles PUBLIC vs CONNECTIONS_ONLY.
//! - Image upload is a 3-step dance: register → presigned PUT → reference
//!   the returned `digitalmediaAsset` urn in `media`.
//! - `contentcreation/normShares` 400s without `x-li-track` /
//!   `x-li-page-instance`; the DM path omits them, so we add a separate
//!   header builder rather than widening `base_headers`.

use serde::Serialize;
use uuid::Uuid;

use crate::api::{find_string_field, LinkedInError, VoyagerClient};

/// A created share's `urn:li:share:7...` (19-digit numeric id, urn-wrapped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareUrn(pub String);

/// Audience scope for a new post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Anyone on/off LinkedIn — `visibleToConnectionsOnly: false`.
    Public,
    /// 1st-degree connections only — `visibleToConnectionsOnly: true`.
    ConnectionsOnly,
}

impl Visibility {
    fn connections_only(self) -> bool {
        matches!(self, Visibility::ConnectionsOnly)
    }

    /// Parse the CLI `--visibility` value. Defaults handled by the caller.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "public" => Some(Self::Public),
            "connections" | "connections_only" | "connectionsonly" => {
                Some(Self::ConnectionsOnly)
            }
            _ => None,
        }
    }
}

/// One feed post the caller wants published. Borrows its payload so the
/// caller keeps ownership of the (potentially large) image bytes.
#[derive(Debug, Clone)]
pub struct PostDraft<'a> {
    pub text: &'a str,
    /// Optional single image. Phase 1 caps at one; multi-image is Phase 2.
    pub image: Option<&'a [u8]>,
    pub image_filename: Option<&'a str>,
    pub visibility: Visibility,
}

impl<'a> PostDraft<'a> {
    /// Text-only public post — the common case.
    pub fn text(text: &'a str) -> Self {
        Self {
            text,
            image: None,
            image_filename: None,
            visibility: Visibility::Public,
        }
    }
}

/// Default `x-li-track` client version. Values aren't validated by LinkedIn
/// (per #77 open-question 1) so a reasonable fixed default is safest; an env
/// override lets us bump it without a recompile if LinkedIn ever starts
/// rejecting "stale" versions.
const DEFAULT_CLIENT_VERSION: &str = "1.13.32099";

/// Voyager media-upload register endpoint. Renamed historically by LinkedIn
/// (`voyagerMediaUploadMetadata` → `voyagerVideoDashMediaUploadMetadata`);
/// env override per #77 §3 caveat.
pub const DEFAULT_MEDIA_UPLOAD_PATH: &str =
    "/voyager/api/voyagerVideoDashMediaUploadMetadata";

const NORMSHARES_PATH: &str = "/voyager/api/contentcreation/normShares";

/// Origin for Voyager calls. Defaults to `https://www.linkedin.com`; the
/// `AUGMENTAGENT_LINKEDIN_BASE_URL` override exists so the wiremock-backed
/// `normshares_body_shape` integration test can point the client at a local
/// mock without a network call. Production never sets this.
fn base_url() -> String {
    std::env::var("AUGMENTAGENT_LINKEDIN_BASE_URL")
        .unwrap_or_else(|_| "https://www.linkedin.com".to_string())
}

// =============================================================================
// Body builder (snapshot-tested in tests/normshares_body_shape.rs)
// =============================================================================

/// Build the canonical `normShares` POST body (#77 §1). `media_urn` is the
/// `urn:li:digitalmediaAsset:...` from a completed image upload, or `None`
/// for a pure-text post.
///
/// Pure function — no I/O — so the body shape is unit-/snapshot-testable
/// without a network mock.
pub fn build_normshares_body(
    text: &str,
    visibility: Visibility,
    media_urn: Option<&str>,
) -> serde_json::Value {
    let media = match media_urn {
        Some(urn) => serde_json::json!([{
            "category": "IMAGE",
            "mediaUrn": urn,
            "tapTargets": [],
            "thumbnails": [],
            "$type": "com.linkedin.voyager.feed.shared.ShareImage"
        }]),
        None => serde_json::json!([]),
    };
    serde_json::json!({
        "visibleToConnectionsOnly": visibility.connections_only(),
        "externalAudienceProviders": [],
        "commentaryV2": {
            "text": text,
            "attributes": []
        },
        "origin": "FEED_DETAIL",
        "allowedCommentersScope": "ALL",
        "postState": "PUBLISHED",
        "media": media
    })
}

// =============================================================================
// Header split (#77 §1 — messaging vs posting)
// =============================================================================

/// Content-creation header set: `base_headers()` minus the messaging
/// referer, plus `x-li-track`, a fresh `x-li-page-instance`, `x-li-lang`,
/// and `referer: /feed/`.
pub(crate) fn posting_headers(
    client: &VoyagerClient,
) -> Result<reqwest::header::HeaderMap, LinkedInError> {
    use reqwest::header::{HeaderName, HeaderValue};
    let mut h = client.base_headers()?;
    let mut set = |name: &'static str, val: String| -> Result<(), LinkedInError> {
        let name = HeaderName::from_static(name);
        let value = HeaderValue::from_str(&val)
            .map_err(|e| LinkedInError::Config(format!("{name}: {e}")))?;
        h.insert(name, value);
        Ok(())
    };
    // Swap the messaging referer for the feed referer.
    set("referer", "https://www.linkedin.com/feed/".into())?;
    let client_version = std::env::var("AUGMENTAGENT_LINKEDIN_CLIENT_VERSION")
        .unwrap_or_else(|_| DEFAULT_CLIENT_VERSION.to_string());
    let x_li_track = serde_json::json!({
        "clientVersion": client_version,
        "mpVersion": client_version,
        "osName": "web",
        "timezoneOffset": -7,
        "timezone": "America/Los_Angeles",
        "deviceFormFactor": "DESKTOP",
        "mpName": "voyager-web",
        "displayDensity": 2,
        "displayWidth": 2560,
        "displayHeight": 1440
    })
    .to_string();
    set("x-li-track", x_li_track)?;
    // Fresh per request (#77 open-question 2 — per-request is safer).
    set(
        "x-li-page-instance",
        format!("urn:li:page:d_flagship3_feed;{}", Uuid::new_v4()),
    )?;
    set("x-li-lang", "en_US".into())?;
    Ok(h)
}

// =============================================================================
// Image upload (3-step Voyager media dance — #77 §3)
// =============================================================================

#[derive(Debug, serde::Deserialize)]
struct RegisterUploadResponse {
    data: RegisterUploadData,
}

#[derive(Debug, serde::Deserialize)]
struct RegisterUploadData {
    value: RegisterUploadValue,
}

#[derive(Debug, serde::Deserialize)]
struct RegisterUploadValue {
    urn: String,
    #[serde(rename = "singleUploadUrl")]
    single_upload_url: String,
}

#[derive(Serialize)]
struct RegisterUploadBody<'a> {
    #[serde(rename = "mediaUploadType")]
    media_upload_type: &'a str,
    #[serde(rename = "fileSize")]
    file_size: usize,
    filename: &'a str,
}

/// Steps 1+2: register the upload, PUT the bytes to the presigned CDN URL,
/// return the `urn:li:digitalmediaAsset:...` to reference in `normShares`.
async fn upload_image(
    client: &VoyagerClient,
    bytes: &[u8],
    filename: Option<&str>,
) -> Result<String, LinkedInError> {
    let media_path = std::env::var("AUGMENTAGENT_LINKEDIN_MEDIA_UPLOAD_PATH")
        .unwrap_or_else(|_| DEFAULT_MEDIA_UPLOAD_PATH.to_string());
    let register_url = format!("{}{media_path}?action=upload", base_url());
    let filename = filename.unwrap_or("augmentagent-upload.png");

    // --- Step 1: register ---
    let body = RegisterUploadBody {
        media_upload_type: "FEEDSHARE_IMAGE",
        file_size: bytes.len(),
        filename,
    };
    let resp = client
        .http
        .post(&register_url)
        .headers(posting_headers(client)?)
        .header("content-type", "application/json; charset=UTF-8")
        .body(serde_json::to_vec(&body).expect("serialize register body"))
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
    let reg: RegisterUploadResponse = resp
        .json()
        .await
        .map_err(|e| LinkedInError::Decode(format!("register upload json: {e}")))?;

    // --- Step 2: presigned PUT (no cookie/csrf — auth is in the URL sig) ---
    let put = client
        .http
        .put(&reg.data.value.single_upload_url)
        .header("content-type", guess_mime(filename))
        .body(bytes.to_vec())
        .send()
        .await?;
    if !put.status().is_success() {
        let st = put.status().as_u16();
        let body = put.text().await.unwrap_or_default();
        return Err(LinkedInError::Voyager { status: st, body });
    }
    Ok(reg.data.value.urn)
}

fn guess_mime(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

// =============================================================================
// create_share — the orchestration the trait method delegates to
// =============================================================================

/// Implementation behind `VoyagerClient::create_share`. Rate-limit preflight
/// is enforced one layer up (channel / CLI dispatch reads
/// `linkedin_action_log`); this is the raw 1-3-call wire sequence.
pub(crate) async fn create_share_impl(
    client: &VoyagerClient,
    draft: PostDraft<'_>,
) -> Result<ShareUrn, LinkedInError> {
    // 1. optional image upload (register → PUT → asset urn).
    let media_urn = match draft.image {
        Some(bytes) => Some(upload_image(client, bytes, draft.image_filename).await?),
        None => None,
    };
    // 2. POST normShares.
    let body = build_normshares_body(draft.text, draft.visibility, media_urn.as_deref());
    let normshares_url = format!("{}{NORMSHARES_PATH}", base_url());
    let resp = client
        .http
        .post(&normshares_url)
        .headers(posting_headers(client)?)
        .header("content-type", "application/json; charset=UTF-8")
        .body(serde_json::to_vec(&body).expect("serialize normShares body"))
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
    // 3. extract entityUrn (data.entityUrn, else recursive scan).
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| LinkedInError::Decode(format!("normShares json: {e}")))?;
    let urn = v
        .get("data")
        .and_then(|d| d.get("entityUrn"))
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .or_else(|| find_string_field(&v, "entityUrn"))
        .ok_or_else(|| LinkedInError::Decode("normShares: no entityUrn in response".into()))?;
    Ok(ShareUrn(urn))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only_body_has_empty_media_and_public() {
        let b = build_normshares_body("hello world", Visibility::Public, None);
        assert_eq!(b["visibleToConnectionsOnly"], false);
        assert_eq!(b["commentaryV2"]["text"], "hello world");
        assert_eq!(b["commentaryV2"]["attributes"], serde_json::json!([]));
        assert_eq!(b["media"], serde_json::json!([]));
        assert_eq!(b["postState"], "PUBLISHED");
        assert_eq!(b["origin"], "FEED_DETAIL");
    }

    #[test]
    fn connections_only_flips_flag() {
        let b = build_normshares_body("x", Visibility::ConnectionsOnly, None);
        assert_eq!(b["visibleToConnectionsOnly"], true);
    }

    #[test]
    fn image_body_references_media_urn() {
        let b = build_normshares_body(
            "with pic",
            Visibility::Public,
            Some("urn:li:digitalmediaAsset:D5610AQH"),
        );
        let media = b["media"].as_array().unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0]["category"], "IMAGE");
        assert_eq!(media[0]["mediaUrn"], "urn:li:digitalmediaAsset:D5610AQH");
        assert_eq!(
            media[0]["$type"],
            "com.linkedin.voyager.feed.shared.ShareImage"
        );
    }

    #[test]
    fn visibility_parse_round_trip() {
        assert_eq!(Visibility::parse("public"), Some(Visibility::Public));
        assert_eq!(
            Visibility::parse("CONNECTIONS"),
            Some(Visibility::ConnectionsOnly)
        );
        assert_eq!(Visibility::parse("nonsense"), None);
    }

    #[test]
    fn guess_mime_covers_common_types() {
        assert_eq!(guess_mime("a.PNG"), "image/png");
        assert_eq!(guess_mime("a.jpeg"), "image/jpeg");
        assert_eq!(guess_mime("a.bin"), "application/octet-stream");
    }
}
