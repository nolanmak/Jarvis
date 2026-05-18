//! Render one `PlatformVariant` into a compact text fragment suitable for an
//! approval card. Each variant is approval-gated independently, so each gets
//! its own fragment (and, by the caller, its own card).

use crate::types::PlatformVariant;

/// A plain-text approval-card body for one variant. Discord-markdown-friendly
/// (bold header, fenced post bodies, an explicit over-limit warning). Kept in
/// this crate so the prompt/format and the preview can't drift apart.
pub fn variant_card(variant: &PlatformVariant) -> String {
    let mut out = String::new();
    let header = if variant.is_thread() {
        format!(
            "**{} — thread ({} posts)**",
            variant.platform.as_str(),
            variant.posts.len()
        )
    } else {
        format!("**{}**", variant.platform.as_str())
    };
    out.push_str(&header);
    out.push('\n');

    for (i, post) in variant.posts.iter().enumerate() {
        if variant.is_thread() {
            out.push_str(&format!("\n`{}/{}`\n", i + 1, variant.posts.len()));
        }
        out.push_str("```\n");
        out.push_str(post);
        out.push_str("\n```\n");
        let n = post.chars().count();
        out.push_str(&format!(
            "_{} chars / {} limit_\n",
            n,
            variant.platform.char_limit()
        ));
    }

    if variant.over_limit {
        out.push_str(
            "\n⚠️ One or more posts exceed the platform limit — edit before approving.\n",
        );
    }

    if let Some(m) = &variant.media {
        out.push_str(&format!(
            "\n🖼 media: {} ({}×{})",
            m.aspect_ratio, m.width, m.height
        ));
        if !m.alt_text.is_empty() {
            out.push_str(&format!("\nalt: {}", m.alt_text));
        }
        out.push('\n');
    }
    out
}

/// Convenience: one fragment per variant, in input order. The caller posts
/// each as its own independently-approvable card.
pub fn preview_all(variants: &[PlatformVariant]) -> Vec<String> {
    variants.iter().map(variant_card).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MediaSpec, Platform, PlatformVariant};

    #[test]
    fn single_post_card_has_header_and_count() {
        let v = PlatformVariant::new(
            Platform::Linkedin,
            vec!["a clean post".into()],
            None,
        );
        let card = variant_card(&v);
        assert!(card.contains("**linkedin**"));
        assert!(card.contains("a clean post"));
        assert!(card.contains("chars / 3000 limit"));
        assert!(!card.contains("⚠️"));
    }

    #[test]
    fn over_limit_emits_warning() {
        let v = PlatformVariant::new(
            Platform::Twitter,
            vec!["x".repeat(400)],
            None,
        );
        let card = variant_card(&v);
        assert!(card.contains("⚠️"));
        assert!(card.contains("exceed the platform limit"));
    }

    #[test]
    fn thread_card_numbers_each_post() {
        let v = PlatformVariant::new(
            Platform::Twitter,
            vec!["one".into(), "two".into()],
            None,
        );
        let card = variant_card(&v);
        assert!(card.contains("thread (2 posts)"));
        assert!(card.contains("`1/2`"));
        assert!(card.contains("`2/2`"));
    }

    #[test]
    fn media_block_rendered_when_present() {
        let v = PlatformVariant::new(
            Platform::Instagram,
            vec!["cap".into()],
            Some(MediaSpec::default_for(Platform::Instagram).with_alt("a photo")),
        );
        let card = variant_card(&v);
        assert!(card.contains("🖼 media: 1:1"));
        assert!(card.contains("alt: a photo"));
    }

    #[test]
    fn preview_all_one_fragment_each() {
        let vs = vec![
            PlatformVariant::new(Platform::Twitter, vec!["t".into()], None),
            PlatformVariant::new(Platform::Linkedin, vec!["l".into()], None),
        ];
        assert_eq!(preview_all(&vs).len(), 2);
    }
}
