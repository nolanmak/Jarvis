//! Media-spec derivation. The adapter does not render pixels; it decides the
//! per-platform sizing and lifts the model's `ALT:` line (if any) into the
//! `MediaSpec`. No image intent in the source ⇒ no `MediaSpec` at all
//! (text-only post), never an invented one.

use crate::types::{MediaSpec, Platform, SourceDraft};

/// Pull a trailing `ALT: ...` line the platform call may have appended, and
/// return (post_body_without_alt, alt_text).
pub fn split_alt(reply: &str) -> (String, Option<String>) {
    // Scan from the end for a line starting with `ALT:` (case-insensitive).
    let mut alt: Option<String> = None;
    let mut kept: Vec<&str> = Vec::new();
    for line in reply.lines() {
        let t = line.trim_start();
        if alt.is_none()
            && t.len() >= 4
            && t[..4].eq_ignore_ascii_case("alt:")
        {
            alt = Some(t[4..].trim().to_string());
            continue;
        }
        kept.push(line);
    }
    (kept.join("\n").trim().to_string(), alt)
}

/// Build the `MediaSpec` for a platform, given the source's media intent and
/// any model-written alt text. Returns `None` when the source declared no
/// image intent (text-only post).
pub fn media_for(
    platform: Platform,
    src: &SourceDraft,
    model_alt: Option<&str>,
) -> Option<MediaSpec> {
    src.media_intent.as_ref().filter(|m| !m.trim().is_empty())?;
    let spec = MediaSpec::default_for(platform);
    let alt = model_alt
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .or_else(|| src.media_intent.clone())
        .unwrap_or_default();
    Some(spec.with_alt(alt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_alt_extracts_trailing_line() {
        let (body, alt) = split_alt("the post body\nmore body\nALT: a chart of MRR");
        assert_eq!(body, "the post body\nmore body");
        assert_eq!(alt.as_deref(), Some("a chart of MRR"));
    }

    #[test]
    fn split_alt_none_when_absent() {
        let (body, alt) = split_alt("just a post");
        assert_eq!(body, "just a post");
        assert!(alt.is_none());
    }

    #[test]
    fn no_media_intent_means_no_spec() {
        let src = SourceDraft::new("text only");
        assert!(media_for(Platform::Twitter, &src, None).is_none());
    }

    #[test]
    fn media_spec_prefers_model_alt() {
        let src = SourceDraft::new("x").with_media_intent("whiteboard");
        let spec = media_for(Platform::Instagram, &src, Some("annotated whiteboard"))
            .unwrap();
        assert_eq!(spec.alt_text, "annotated whiteboard");
        assert_eq!(spec.aspect_ratio, "1:1");
    }

    #[test]
    fn media_spec_falls_back_to_intent_for_alt() {
        let src = SourceDraft::new("x").with_media_intent("the team photo");
        let spec = media_for(Platform::Linkedin, &src, None).unwrap();
        assert_eq!(spec.alt_text, "the team photo");
    }
}
