//! Outbound file attachments for agent answers (#440).
//!
//! The wiki-ask reasoner delivers files by ending its answer with marker
//! lines, one per file:
//!
//! ```text
//! ATTACH: deliverables/scott-research.md
//! ```
//!
//! [`extract_attach_markers`] strips those lines from the text and validates
//! each referenced path against the wiki root. Validation is FAIL-CLOSED and
//! mirrors the write-side scope guard (`scripts/aa-wiki-scope-guard.sh`): the
//! reasoner can only Write inside the wiki root, so any marker resolving
//! outside it (traversal, symlink escape, absolute path elsewhere) is
//! refused — that rule is what stops a prompt-injected answer from
//! attaching `~/.env` or the sqlite store.
//!
//! Rejections are surfaced as visible `⚠️` notes in the posted text rather
//! than dropped silently, so a bad marker never reads as "attached".

use std::path::{Path, PathBuf};

use serenity::builder::CreateAttachment;
use tracing::warn;

/// Marker prefix. Matched at line start (after trimming leading whitespace),
/// so prose that merely mentions `ATTACH:` mid-sentence is left alone.
pub const ATTACH_MARKER_PREFIX: &str = "ATTACH:";

/// Per-file size cap. Discord's default bot upload limit is 8 MiB in guilds
/// without boosts; staying under it means an oversize file fails HERE with a
/// clear note instead of as an opaque Discord API 413.
pub const MAX_OUTBOUND_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

/// Max files per answer. Discord's own cap is 10; 5 keeps replies sane.
pub const MAX_OUTBOUND_ATTACHMENTS: usize = 5;

/// Result of scanning an answer for `ATTACH:` markers.
#[derive(Debug, Default)]
pub struct ExtractedAnswer {
    /// Answer text with marker lines removed.
    pub text: String,
    /// Canonicalized, validated file paths (all under the wiki root).
    pub files: Vec<PathBuf>,
    /// Human-readable reasons for every marker that was refused.
    pub notes: Vec<String>,
}

/// Scan `answer` for `ATTACH:` marker lines and validate each path against
/// `wiki_root`. With `wiki_root = None` (attachments not configured) every
/// marker is refused with a note — never silently swallowed.
pub fn extract_attach_markers(answer: &str, wiki_root: Option<&Path>) -> ExtractedAnswer {
    let mut text_lines: Vec<&str> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    for line in answer.lines() {
        let Some(rest) = line.trim().strip_prefix(ATTACH_MARKER_PREFIX) else {
            text_lines.push(line);
            continue;
        };
        let raw = rest.trim();
        if raw.is_empty() {
            notes.push("couldn't attach: empty ATTACH path".to_string());
            continue;
        }
        let Some(root) = wiki_root else {
            notes.push(format!(
                "couldn't attach `{raw}`: file delivery isn't configured (no wiki root)"
            ));
            continue;
        };
        match validate_under_root(root, raw) {
            Ok(path) => {
                if files.contains(&path) {
                    continue; // same file referenced twice — attach once
                }
                if files.len() >= MAX_OUTBOUND_ATTACHMENTS {
                    notes.push(format!(
                        "couldn't attach `{raw}`: max {MAX_OUTBOUND_ATTACHMENTS} files per answer"
                    ));
                    continue;
                }
                files.push(path);
            }
            Err(reason) => notes.push(format!("couldn't attach `{raw}`: {reason}")),
        }
    }

    ExtractedAnswer {
        text: text_lines.join("\n").trim().to_string(),
        files,
        notes,
    }
}

/// Resolve `raw` (relative to `root`, or absolute) and require the
/// canonicalized result to live under the canonicalized root. Symlinks are
/// followed BEFORE the containment check, so a link inside the wiki pointing
/// at `/home/user/.env` is refused.
fn validate_under_root(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let p = Path::new(raw);
    let candidate = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("wiki root unavailable ({e})"))?;
    let canon = candidate
        .canonicalize()
        .map_err(|_| "file not found under the wiki".to_string())?;
    if !canon.starts_with(&canon_root) {
        return Err("path is outside the wiki root".to_string());
    }
    let meta = std::fs::metadata(&canon).map_err(|e| format!("unreadable ({e})"))?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > MAX_OUTBOUND_ATTACHMENT_BYTES {
        return Err(format!(
            "{} bytes exceeds the {} MiB cap",
            meta.len(),
            MAX_OUTBOUND_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    Ok(canon)
}

/// One-stop preparation for posting: extract markers, read the surviving
/// files into serenity attachments, and fold every refusal/read failure into
/// the posted text as a `⚠️` line. Returns `(posted_text, attachments)`.
///
/// The posted text is never empty when there is something to deliver: an
/// answer that was ONLY markers gets a `📎` placeholder line so the Discord
/// send (which rejects empty content-with-files less gracefully than you'd
/// hope) always has a body.
pub async fn prepare_answer_delivery(
    answer: &str,
    wiki_root: Option<&Path>,
) -> (String, Vec<CreateAttachment>) {
    let extracted = extract_attach_markers(answer, wiki_root);
    let mut notes = extracted.notes;

    let mut attachments = Vec::with_capacity(extracted.files.len());
    for path in &extracted.files {
        match CreateAttachment::path(path).await {
            Ok(a) => attachments.push(a),
            Err(e) => {
                warn!("outbound attachment read failed for {}: {e}", path.display());
                notes.push(format!(
                    "couldn't attach `{}`: read failed ({e})",
                    path.display()
                ));
            }
        }
    }

    let mut text = extracted.text;
    if !notes.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        for (i, note) in notes.iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            text.push_str("\u{26a0}\u{fe0f} ");
            text.push_str(note);
        }
    }
    if text.is_empty() && !attachments.is_empty() {
        text = "\u{1f4ce} file attached".to_string();
    }

    (text, attachments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn plain_answer_passes_through_untouched() {
        let dir = root();
        let out = extract_attach_markers("hello\nworld", Some(dir.path()));
        assert_eq!(out.text, "hello\nworld");
        assert!(out.files.is_empty());
        assert!(out.notes.is_empty());
    }

    #[test]
    fn marker_is_stripped_and_file_resolved() {
        let dir = root();
        fs::write(dir.path().join("report.md"), "# hi").unwrap();
        let out = extract_attach_markers(
            "Here is the report.\nATTACH: report.md",
            Some(dir.path()),
        );
        assert_eq!(out.text, "Here is the report.");
        assert_eq!(out.files.len(), 1);
        assert!(out.files[0].ends_with("report.md"));
        assert!(out.notes.is_empty());
    }

    #[test]
    fn indented_marker_counts_but_mid_sentence_mention_does_not() {
        let dir = root();
        fs::write(dir.path().join("a.md"), "x").unwrap();
        let out = extract_attach_markers(
            "  ATTACH: a.md\nuse ATTACH: like this in prose",
            Some(dir.path()),
        );
        assert_eq!(out.files.len(), 1);
        assert_eq!(out.text, "use ATTACH: like this in prose");
    }

    #[test]
    fn traversal_is_refused() {
        let dir = root();
        let out = extract_attach_markers("ATTACH: ../../etc/passwd", Some(dir.path()));
        assert!(out.files.is_empty());
        assert_eq!(out.notes.len(), 1, "traversal must produce a note");
    }

    #[test]
    fn absolute_path_outside_root_is_refused() {
        let dir = root();
        let out = extract_attach_markers("ATTACH: /etc/hostname", Some(dir.path()));
        assert!(out.files.is_empty());
        assert_eq!(out.notes.len(), 1);
        assert!(out.notes[0].contains("outside the wiki root"), "{:?}", out.notes);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused() {
        let dir = root();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("sneaky.md")).unwrap();
        let out = extract_attach_markers("ATTACH: sneaky.md", Some(dir.path()));
        assert!(out.files.is_empty());
        assert!(out.notes[0].contains("outside the wiki root"), "{:?}", out.notes);
    }

    #[test]
    fn missing_file_and_directory_are_refused() {
        let dir = root();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        let out = extract_attach_markers(
            "ATTACH: nope.md\nATTACH: subdir",
            Some(dir.path()),
        );
        assert!(out.files.is_empty());
        assert_eq!(out.notes.len(), 2);
    }

    #[test]
    fn oversize_file_is_refused() {
        let dir = root();
        let big = dir.path().join("big.bin");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_OUTBOUND_ATTACHMENT_BYTES + 1).unwrap();
        let out = extract_attach_markers("ATTACH: big.bin", Some(dir.path()));
        assert!(out.files.is_empty());
        assert!(out.notes[0].contains("cap"), "{:?}", out.notes);
    }

    #[test]
    fn duplicate_markers_attach_once_and_cap_is_enforced() {
        let dir = root();
        let mut answer = String::new();
        for i in 0..7 {
            let name = format!("f{i}.md");
            fs::write(dir.path().join(&name), "x").unwrap();
            answer.push_str(&format!("ATTACH: {name}\n"));
        }
        answer.push_str("ATTACH: f0.md\n"); // duplicate
        let out = extract_attach_markers(&answer, Some(dir.path()));
        assert_eq!(out.files.len(), MAX_OUTBOUND_ATTACHMENTS);
        // two over-cap files noted, duplicate silently deduped
        assert_eq!(out.notes.len(), 7 - MAX_OUTBOUND_ATTACHMENTS);
    }

    #[test]
    fn no_wiki_root_refuses_with_note() {
        let out = extract_attach_markers("ATTACH: report.md", None);
        assert!(out.files.is_empty());
        assert!(out.notes[0].contains("no wiki root"), "{:?}", out.notes);
    }

    #[tokio::test]
    async fn prepare_appends_notes_and_placeholder() {
        let dir = root();
        fs::write(dir.path().join("ok.md"), "content").unwrap();
        // marker-only answer: placeholder body + one warning for the bad path
        let (text, files) =
            prepare_answer_delivery("ATTACH: ok.md\nATTACH: missing.md", Some(dir.path())).await;
        assert_eq!(files.len(), 1);
        assert!(text.contains("\u{26a0}\u{fe0f}"), "warning note missing: {text}");
        assert!(text.contains("missing.md"));
        // answer with prose keeps the prose first
        let (text2, files2) =
            prepare_answer_delivery("Summary here.\nATTACH: ok.md", Some(dir.path())).await;
        assert_eq!(files2.len(), 1);
        assert!(text2.starts_with("Summary here."));
        assert!(!text2.contains("ATTACH:"));
    }
}
