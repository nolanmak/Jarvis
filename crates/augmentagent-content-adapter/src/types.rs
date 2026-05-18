//! Core value types for the compose-once adapter.

use serde::{Deserialize, Serialize};

/// The target platforms the adapter knows how to render for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Twitter,
    Linkedin,
    Instagram,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Twitter => "twitter",
            Self::Linkedin => "linkedin",
            Self::Instagram => "instagram",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "twitter" | "x" => Some(Self::Twitter),
            "linkedin" | "li" => Some(Self::Linkedin),
            "instagram" | "ig" => Some(Self::Instagram),
            _ => None,
        }
    }

    /// Hard character ceiling for a single post on this platform. Used by the
    /// preview to flag an over-long variant before it reaches approval.
    pub fn char_limit(self) -> usize {
        match self {
            Self::Twitter => 280,
            Self::Linkedin => 3000,
            Self::Instagram => 2200,
        }
    }
}

/// Image/media sizing + alt-text spec for a platform. The adapter does not
/// produce pixels — it produces the *spec* a downstream renderer/uploader
/// honors, plus model-written alt text grounded in the source draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaSpec {
    /// Recommended aspect ratio, e.g. "1:1", "1.91:1", "9:16".
    pub aspect_ratio: String,
    /// Recommended pixel width × height (longest sensible default).
    pub width: u32,
    pub height: u32,
    /// Accessibility alt text. Empty when the source carried no image intent.
    #[serde(default)]
    pub alt_text: String,
}

impl MediaSpec {
    /// Platform-native default sizing. Alt text is filled later (or left
    /// empty when the post is text-only).
    pub fn default_for(platform: Platform) -> Self {
        let (ar, w, h) = match platform {
            // X in-timeline image.
            Platform::Twitter => ("1.91:1", 1600u32, 837u32),
            // LinkedIn link/image post.
            Platform::Linkedin => ("1.91:1", 1200, 627),
            // Instagram square feed default.
            Platform::Instagram => ("1:1", 1080, 1080),
        };
        Self {
            aspect_ratio: ar.into(),
            width: w,
            height: h,
            alt_text: String::new(),
        }
    }

    pub fn with_alt(mut self, alt: impl Into<String>) -> Self {
        self.alt_text = alt.into();
        self
    }
}

/// The user's idea, written once. `media_intent` is a free-text hint ("photo
/// of the whiteboard", "chart of MRR") the adapter turns into per-platform
/// `MediaSpec` alt text — never invented when empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDraft {
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_intent: Option<String>,
    /// Optional sample of the user's writing voice, weighted heavily by the
    /// adapter prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_sample: Option<String>,
}

impl SourceDraft {
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            media_intent: None,
            voice_sample: None,
        }
    }
    pub fn with_media_intent(mut self, m: impl Into<String>) -> Self {
        self.media_intent = Some(m.into());
        self
    }
    pub fn with_voice(mut self, v: impl Into<String>) -> Self {
        self.voice_sample = Some(v.into());
        self
    }
}

/// One platform's rendered output. `posts` is a Vec so an X thread is just
/// `posts.len() > 1`; single-post platforms always have exactly one element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformVariant {
    pub platform: Platform,
    pub posts: Vec<String>,
    pub media: Option<MediaSpec>,
    /// True if any post exceeds the platform's hard char limit (surfaced as a
    /// warning on the approval card; never silently truncated).
    pub over_limit: bool,
}

impl PlatformVariant {
    pub fn new(platform: Platform, posts: Vec<String>, media: Option<MediaSpec>) -> Self {
        let limit = platform.char_limit();
        let over_limit = posts.iter().any(|p| p.chars().count() > limit);
        Self {
            platform,
            posts,
            media,
            over_limit,
        }
    }

    /// Whether this variant is an X-style multi-post thread.
    pub fn is_thread(&self) -> bool {
        self.posts.len() > 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_parse_and_aliases() {
        assert_eq!(Platform::parse("x"), Some(Platform::Twitter));
        assert_eq!(Platform::parse("LI"), Some(Platform::Linkedin));
        assert_eq!(Platform::parse("ig"), Some(Platform::Instagram));
        assert_eq!(Platform::parse("myspace"), None);
    }

    #[test]
    fn over_limit_flagged() {
        let long = "a".repeat(300);
        let v = PlatformVariant::new(Platform::Twitter, vec![long], None);
        assert!(v.over_limit);
        let ok = PlatformVariant::new(Platform::Twitter, vec!["short".into()], None);
        assert!(!ok.over_limit);
    }

    #[test]
    fn thread_detection() {
        let v = PlatformVariant::new(
            Platform::Twitter,
            vec!["one".into(), "two".into()],
            None,
        );
        assert!(v.is_thread());
    }

    #[test]
    fn media_defaults_differ_per_platform() {
        assert_eq!(MediaSpec::default_for(Platform::Instagram).aspect_ratio, "1:1");
        assert_eq!(MediaSpec::default_for(Platform::Twitter).aspect_ratio, "1.91:1");
    }
}
