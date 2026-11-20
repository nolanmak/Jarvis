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

/// Images downloaded for one prompt (#2332). Each lives in a temp file named
/// `aa-img-…`, the prefix that lets a call without file tools read exactly
/// that file. The files are removed when this is dropped, so keep it alive
/// until the model calls that use the prompt have finished.
#[derive(Default)]
pub struct PromptImages {
    files: Vec<tempfile::NamedTempFile>,
}

impl PromptImages {
    /// Most images taken from one message.
    pub const MAX: usize = 4;
    /// Larger files are skipped: a draft should not wait on a big download.
    pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

    /// Whether `ext` (any case) is a format the models can open.
    pub fn supports(ext: &str) -> bool {
        IMAGE_EXT_ALLOWLIST.contains(&ext.to_ascii_lowercase().as_str())
    }

    /// An empty temp file for a download to write into.
    pub fn new_file(ext: &str) -> std::io::Result<tempfile::NamedTempFile> {
        tempfile::Builder::new()
            .prefix("aa-img-")
            .suffix(&format!(".{}", ext.to_ascii_lowercase()))
            .tempfile_in(std::env::temp_dir())
    }

    /// Keep a downloaded file. Ignored once [`Self::MAX`] are held.
    pub fn push(&mut self, file: tempfile::NamedTempFile) {
        if self.files.len() < Self::MAX {
            self.files.push(file);
        }
    }

    /// Write `bytes` to a new file and keep it. False when the format is
    /// unsupported, the size is out of range, the limit is reached or the
    /// write failed.
    pub fn push_bytes(&mut self, ext: &str, bytes: &[u8]) -> bool {
        if !Self::supports(ext)
            || bytes.is_empty()
            || bytes.len() as u64 > Self::MAX_BYTES
            || self.files.len() >= Self::MAX
        {
            return false;
        }
        let Ok(mut file) = Self::new_file(ext) else {
            return false;
        };
        if std::io::Write::write_all(&mut file, bytes).is_err() {
            return false;
        }
        self.files.push(file);
        true
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Text to append to a prompt: one `IMAGE:` line per file, or nothing.
    pub fn prompt_lines(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        let mut text = String::from(
            "\n\nImages attached to the latest message (open each one before deciding or drafting):\n",
        );
        for file in &self.files {
            text.push_str(&image_marker_line(file.path()));
            text.push('\n');
        }
        text
    }
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
    fn prompt_images_hold_supported_files_up_to_the_limit_and_clean_up() {
        let mut images = PromptImages::default();
        assert_eq!(images.prompt_lines(), "");
        assert!(!images.push_bytes("heic", b"x"), "unsupported format");
        assert!(!images.push_bytes("png", b""), "empty file");
        assert!(images.push_bytes("PNG", b"\x89PNG one"));
        for _ in 0..5 {
            images.push_bytes("jpg", b"jpeg");
        }
        assert_eq!(images.len(), PromptImages::MAX);
        let lines = images.prompt_lines();
        let seen = extract_image_markers(&lines).images;
        assert_eq!(seen.len(), PromptImages::MAX);
        for path in &seen {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("aa-img-"), "{name}");
        }
        drop(images);
        assert!(seen.iter().all(|path| !path.exists()), "files are removed on drop");
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
