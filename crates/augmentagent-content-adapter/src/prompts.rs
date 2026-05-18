//! System-prompt assembly. Each platform call gets: shared rules + that one
//! platform's section + an optional `<voice>` block. Kept as compile-time
//! includes so a deploy can't drift from the prompts the tests pin.

use crate::types::{Platform, SourceDraft};

const SHARED: &str = include_str!("../prompts/shared_system.md");
const TWITTER: &str = include_str!("../prompts/twitter.md");
const LINKEDIN: &str = include_str!("../prompts/linkedin.md");
const INSTAGRAM: &str = include_str!("../prompts/instagram.md");

fn platform_section(p: Platform) -> &'static str {
    match p {
        Platform::Twitter => TWITTER,
        Platform::Linkedin => LINKEDIN,
        Platform::Instagram => INSTAGRAM,
    }
}

/// Full system prompt for one platform call.
pub fn system_prompt(platform: Platform) -> String {
    format!("{SHARED}\n\n{}", platform_section(platform))
}

/// The user message: the source draft, plus an optional voice sample the
/// shared prompt tells the model to weight heavily.
pub fn user_message(platform: Platform, src: &SourceDraft) -> String {
    let mut m = format!(
        "Adapt this for {plat}.\n\n<source_draft>\n{body}\n</source_draft>",
        plat = platform.as_str(),
        body = src.body.trim(),
    );
    if let Some(v) = &src.voice_sample {
        if !v.trim().is_empty() {
            m.push_str(&format!("\n\n<voice>\n{}\n</voice>", v.trim()));
        }
    }
    if let Some(mi) = &src.media_intent {
        if !mi.trim().is_empty() {
            m.push_str(&format!(
                "\n\n<media_intent>\n{}\n</media_intent>\n(If the post warrants an image, you may also reply with a final line `ALT: <alt text>` describing it for accessibility.)",
                mi.trim()
            ));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_includes_shared_and_platform() {
        let sp = system_prompt(Platform::Linkedin);
        assert!(sp.contains("Cross-Platform Content Adapter"));
        assert!(sp.contains("Platform: LinkedIn"));
        assert!(!sp.contains("Platform: X / Twitter"));
    }

    #[test]
    fn user_message_embeds_voice_and_media() {
        let src = SourceDraft::new("ship it")
            .with_voice("I write terse.")
            .with_media_intent("whiteboard photo");
        let m = user_message(Platform::Twitter, &src);
        assert!(m.contains("<source_draft>\nship it"));
        assert!(m.contains("<voice>\nI write terse."));
        assert!(m.contains("<media_intent>\nwhiteboard photo"));
    }

    #[test]
    fn user_message_omits_empty_optionals() {
        let m = user_message(Platform::Instagram, &SourceDraft::new("hi"));
        assert!(!m.contains("<voice>"));
        assert!(!m.contains("<media_intent>"));
    }
}
