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
use augmentagent_channel_core::images::{PromptImages, IMAGE_EXT_ALLOWLIST};
use augmentagent_channel_journal::s3::AttachmentSource;
use augmentagent_store::Email;
use tracing::debug;

/// Most images taken from one message.
pub const MAX_IMAGES: usize = PromptImages::MAX;

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

/// Download the message's images. Never fails.
pub async fn fetch(email: &Email) -> PromptImages {
    let mut images = PromptImages::default();
    let refs = image_refs(email);
    if refs.is_empty() {
        return images;
    }
    let source = match AttachmentSource::from_env() {
        Ok(source) => source,
        Err(error) => {
            debug!("image attachments not fetched: {error}");
            return images;
        }
    };
    for image in refs {
        let file = match PromptImages::new_file(&image.ext) {
            Ok(file) => file,
            Err(error) => {
                debug!("image attachment temp file failed: {error}");
                continue;
            }
        };
        match source.fetch(&image.uri, file.path()).await {
            Ok(bytes) if bytes > 0 && bytes <= PromptImages::MAX_BYTES => images.push(file),
            Ok(bytes) => debug!("image attachment skipped: {bytes} bytes"),
            Err(error) => debug!("image attachment download failed: {error}"),
        }
    }
    images
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
        let none = fetch(&email(&["[attachment: application/pdf a.pdf]"])).await;
        assert!(none.is_empty());
        assert_eq!(none.prompt_lines(), "");
    }
}
