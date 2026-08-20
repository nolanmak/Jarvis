//! Inbound image attachments through the provider seam (#655 follow-up).
//!
//! Channels that receive image attachments (Discord query mode, Discord DM
//! subscriptions) download them to `/tmp/aa-img-<msgid>-<idx>.<ext>` and
//! reference them in the user message with one marker line per file:
//!
//! ```text
//! IMAGE: /tmp/aa-img-1234-0.png
//! ```
//!
//! What each provider does with the marker:
//!
//! - **claude** — nothing to translate: the marker line stays in the prompt
//!   and the CLI's `Read` tool renders images natively (the wiki-ask scope
//!   guard carve-out for `/tmp/aa-img-*` paths has existed since #441).
//! - **codex** — [`extract_image_markers`] strips the lines and the adapter
//!   passes each path via `codex exec -i <path>`, which attaches the image
//!   to the prompt itself. This is what makes image turns survive a
//!   claude→codex failover.
//! - **gemini / cerebras** — markers are stripped and replaced with an
//!   honest `[image attached but not viewable by this provider]` note, so a
//!   JSON-emitting preset never chokes on a path it can't open and the
//!   model never hallucinates having seen the picture.
//!
//! Marker matching is line-anchored (same posture as the `ATTACH:` outbound
//! markers, #440): prose that merely mentions `IMAGE:` mid-sentence is left
//! alone. A marker whose path fails validation (missing file, non-image
//! extension) is left in the text untouched — for claude that is still a
//! readable hint, for the others it is inert prose.

use std::path::{Path, PathBuf};

/// Marker prefix, matched at line start after trimming leading whitespace.
pub const IMAGE_MARKER_PREFIX: &str = "IMAGE:";

/// Extensions accepted as images. Mirrors the Discord-side filter and the
/// mimetypes `codex exec -i` / the Claude `Read` tool actually render.
pub const IMAGE_EXT_ALLOWLIST: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Result of scanning a user message for `IMAGE:` markers.
#[derive(Debug, Default)]
pub struct ExtractedImages {
    /// Message with the valid marker lines removed.
    pub text: String,
    /// Validated image paths, in marker order.
    pub images: Vec<PathBuf>,
}

fn is_valid_image_path(path: &Path) -> bool {
    let ext_ok = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXT_ALLOWLIST.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    ext_ok && path.is_file()
}

/// Split `IMAGE:` marker lines out of `user_msg`. Invalid markers stay in
/// the text verbatim (see module docs for why that is the safe default).
pub fn extract_image_markers(user_msg: &str) -> ExtractedImages {
    let mut text_lines: Vec<&str> = Vec::new();
    let mut images: Vec<PathBuf> = Vec::new();
    for line in user_msg.lines() {
        if let Some(rest) = line.trim().strip_prefix(IMAGE_MARKER_PREFIX) {
            let path = PathBuf::from(rest.trim());
            if is_valid_image_path(&path) {
                images.push(path);
                continue;
            }
        }
        text_lines.push(line);
    }
    ExtractedImages {
        text: text_lines.join("\n"),
        images,
    }
}

/// Replace every valid marker with a visible degradation note — used by
/// providers that cannot view images at all (gemini json mode, cerebras).
/// Returns the original string unchanged (no allocation churn beyond the
/// scan) when there are no valid markers.
pub fn strip_markers_with_note(user_msg: &str) -> String {
    let extracted = extract_image_markers(user_msg);
    if extracted.images.is_empty() {
        return user_msg.to_string();
    }
    let note = format!(
        "[{} image attachment(s) were included but are NOT viewable by this \
         provider — answer from the text alone and say so if the images were \
         essential]",
        extracted.images.len()
    );
    if extracted.text.trim().is_empty() {
        note
    } else {
        format!("{}\n\n{note}", extracted.text)
    }
}

/// Format a marker line for channels to append to a user message.
pub fn image_marker_line(path: &Path) -> String {
    format!("{IMAGE_MARKER_PREFIX} {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn tmp_image(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"\x89PNG fake").unwrap();
        p
    }

    #[test]
    fn extracts_valid_markers_and_leaves_prose_alone() {
        let dir = tempfile::tempdir().unwrap();
        let img = tmp_image(&dir, "aa-img-1-0.png");
        let msg = format!(
            "what is on this screenshot?\nIMAGE: {}\nAlso: IMAGE: is a word I typed mid-question.",
            img.display()
        );
        let got = extract_image_markers(&msg);
        assert_eq!(got.images, vec![img]);
        assert!(got.text.contains("what is on this screenshot?"));
        assert!(
            got.text.contains("Also: IMAGE: is a word"),
            "mid-sentence mention must survive: {}",
            got.text
        );
    }

    #[test]
    fn invalid_markers_stay_in_text() {
        let dir = tempfile::tempdir().unwrap();
        let not_image = dir.path().join("notes.txt");
        std::fs::write(&not_image, "text").unwrap();
        let missing = dir.path().join("gone.png");
        let msg = format!("IMAGE: {}\nIMAGE: {}", not_image.display(), missing.display());
        let got = extract_image_markers(&msg);
        assert!(got.images.is_empty());
        assert_eq!(got.text.lines().count(), 2, "both bad markers kept as text");
    }

    #[test]
    fn strip_with_note_degrades_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let img = tmp_image(&dir, "aa-img-2-0.jpg");
        let msg = format!("triage this\nIMAGE: {}", img.display());
        let got = strip_markers_with_note(&msg);
        assert!(got.starts_with("triage this"));
        assert!(got.contains("NOT viewable"));
        assert!(!got.contains("aa-img-2-0.jpg"), "path removed: {got}");

        // No markers → unchanged.
        assert_eq!(strip_markers_with_note("plain question"), "plain question");
    }
}
