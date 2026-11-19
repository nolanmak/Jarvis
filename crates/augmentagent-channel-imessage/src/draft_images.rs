//! Image attachments for triage and drafting (#2332).
//!
//! The exporter records an image as a line of text,
//! `[attachment: image/jpeg IMG_001.jpeg s3://bucket/key]`, so the model knew
//! a picture existed and nothing about what was in it. This downloads the
//! images of the message being answered and hands them to the model as
//! `IMAGE:` lines ([`augmentagent_channel_core::images`]).
//!
//! Best effort and strictly additive: with no bucket configured, a format the
//! models cannot open, or a failed download, the prompt is what it was
//! before. A missing image must never stop a draft.
use augmentagent_channel_core::images::{image_marker_line, IMAGE_EXT_ALLOWLIST};
use augmentagent_channel_journal::s3::AttachmentSource;
use augmentagent_store::Email;
use tracing::debug;

/// Most images taken from one message.
pub const MAX_IMAGES: usize = 4;
/// Larger files are skipped: a draft should not wait on a big download.
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// An image reference parsed from an attachment line.
#[derive(Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub uri: String,
    /// Lower-case extension, one the models can open.
    pub ext: String,
}

/// The fetchable images of a message, in order, at most [`MAX_IMAGES`].
pub fn image_refs(email: &Email) -> Vec<ImageRef> {
    email
        .attachments
        .iter()
        .filter_map(|line| parse(line))
        .take(MAX_IMAGES)
        .collect()
}

fn parse(line: &str) -> Option<ImageRef> {
    let inner = line.trim().strip_prefix("[attachment: ")?.strip_suffix(']')?;
    let (mime, rest) = inner.split_once(' ')?;
    if !mime.starts_with("image/") {
        return None;
    }
    // The URI is the last token: file names may contain spaces, keys never do.
    let uri = rest.rsplit(' ').next()?;
    if !uri.starts_with("s3://") {
        return None;
    }
    let ext = uri.rsplit_once('.')?.1.to_ascii_lowercase();
    IMAGE_EXT_ALLOWLIST
        .contains(&ext.as_str())
        .then(|| ImageRef { uri: uri.to_string(), ext })
}

/// Downloaded images. The files are removed when this is dropped, so keep it
/// alive until the model calls that use the prompt have finished.
#[derive(Default)]
pub struct DraftImages {
    files: Vec<tempfile::NamedTempFile>,
}

impl DraftImages {
    /// Download the message's images. Never fails.
    pub async fn fetch(email: &Email) -> Self {
        let refs = image_refs(email);
        if refs.is_empty() {
            return Self::default();
        }
        let source = match AttachmentSource::from_env() {
            Ok(source) => source,
            Err(error) => {
                debug!("image attachments not fetched: {error}");
                return Self::default();
            }
        };
        let mut files = Vec::new();
        for image in refs {
            // The `aa-img-` prefix is what lets a call without file tools
            // read exactly this file (reasoner::image_read_grants).
            let file = match tempfile::Builder::new()
                .prefix("aa-img-")
                .suffix(&format!(".{}", image.ext))
                .tempfile_in(std::env::temp_dir())
            {
                Ok(file) => file,
                Err(error) => {
                    debug!("image attachment temp file failed: {error}");
                    continue;
                }
            };
            match source.fetch(&image.uri, file.path()).await {
                Ok(bytes) if bytes > 0 && bytes <= MAX_IMAGE_BYTES => files.push(file),
                Ok(bytes) => debug!("image attachment skipped: {bytes} bytes"),
                Err(error) => debug!("image attachment download failed: {error}"),
            }
        }
        Self { files }
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

    fn email(attachments: &[&str]) -> Email {
        Email {
            message_id: "imessage:fixture:1".into(),
            thread_id: None,
            from: "sender".into(),
            to: String::new(),
            cc: String::new(),
            attachments: attachments.iter().map(|s| s.to_string()).collect(),
            subject: "iMessage: Fixture".into(),
            body: String::new(),
            date: "2026-11-19T12:00:00Z".into(),
            account_entity_id: None,
            platform: "imessage".into(),
            kind: "dm".into(),
        }
    }

    #[test]
    fn only_fetchable_images_in_formats_the_models_open_are_taken() {
        let refs = image_refs(&email(&[
            "[attachment: image/jpeg IMG_001.jpeg s3://bucket/conversations/a/1-IMG_001.jpeg]",
            // A name with spaces.
            "[attachment: image/png Screen Shot 1.PNG s3://bucket/conversations/a/attachments/2-screen-shot-1.png]",
            // Not uploaded: nothing to fetch.
            "[attachment: image/jpeg IMG_002.jpeg]",
            // A format no model here can open.
            "[attachment: image/heic IMG_003.HEIC s3://bucket/conversations/a/3-IMG_003.HEIC]",
            // Not an image.
            "[attachment: application/pdf plan.pdf s3://bucket/conversations/a/4-plan.pdf]",
            "a line of ordinary text mentioning s3://bucket/x.png",
        ]));
        assert_eq!(
            refs,
            [
                ImageRef {
                    uri: "s3://bucket/conversations/a/1-IMG_001.jpeg".into(),
                    ext: "jpeg".into()
                },
                ImageRef {
                    uri: "s3://bucket/conversations/a/attachments/2-screen-shot-1.png".into(),
                    ext: "png".into()
                },
            ]
        );
    }

    #[test]
    fn at_most_four_images_are_taken_from_one_message() {
        let lines: Vec<String> = (0..9)
            .map(|n| format!("[attachment: image/png p{n}.png s3://bucket/conversations/a/{n}.png]"))
            .collect();
        let refs = image_refs(&email(&lines.iter().map(String::as_str).collect::<Vec<_>>()));
        assert_eq!(refs.len(), MAX_IMAGES);
    }

    #[tokio::test]
    async fn no_images_or_no_bucket_leaves_the_prompt_unchanged() {
        let none = DraftImages::fetch(&email(&["[attachment: application/pdf a.pdf]"])).await;
        assert!(none.is_empty());
        assert_eq!(none.prompt_lines(), "");
    }
}
