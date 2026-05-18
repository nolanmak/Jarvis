//! Image staging for the browser-posting path (#50).
//!
//! The only "upload" primitive we use is CDP `setInputFiles` on Instagram's
//! hidden `<input type=file>` — the native OS file chooser never opens (that
//! would need a human). This module owns: validating the local image path,
//! finding the file input via the layered registry, and injecting the path.
//!
//! Reel/carousel/story multi-asset staging is deferred (`Refs #76 —
//! deferred`); v1 stages exactly one image.

use std::path::{Path, PathBuf};

use augmentagent_browser_client::BrowserClient;
use thiserror::Error;

use crate::selectors::{Target, FILE_INPUT};

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("image not found: {0}")]
    NotFound(PathBuf),
    #[error("unsupported image extension: {0} (jpg/jpeg/png only)")]
    UnsupportedFormat(String),
    #[error("could not resolve the file input via any selector layer")]
    NoFileInput,
    #[error("browser: {0}")]
    Browser(#[from] augmentagent_browser_client::BrowserError),
}

/// Instagram feed accepts jpg/png for image posts. Reject anything else up
/// front so we fail before driving the UI rather than mid-flow.
pub fn validate_image(path: &Path) -> Result<(), UploadError> {
    if !path.is_file() {
        return Err(UploadError::NotFound(path.to_path_buf()));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
        return Err(UploadError::UnsupportedFormat(ext));
    }
    Ok(())
}

/// Walk a target's layered selectors and return the first query the sidecar
/// can resolve (via a short `wait_for`). Shared by composer + upload so
/// every UI touch uses the same resilience walk.
pub async fn resolve_target<'a>(
    client: &BrowserClient,
    target: &'a Target,
    per_layer_timeout_ms: u64,
) -> Option<&'a str> {
    for layer in target.layers {
        if client
            .wait_for(layer.query, per_layer_timeout_ms)
            .await
            .is_ok()
        {
            tracing::debug!(
                target = target.name,
                tier = ?layer.tier,
                query = layer.query,
                "selector layer resolved"
            );
            return Some(layer.query);
        }
    }
    tracing::warn!(target = target.name, "no selector layer resolved");
    None
}

/// Stage one image into Instagram's hidden file input via CDP. Assumes the
/// composer has already advanced the UI to the point where the input exists
/// in the DOM.
pub async fn stage_image(
    client: &BrowserClient,
    image: &Path,
) -> Result<(), UploadError> {
    validate_image(image)?;
    let selector = resolve_target(client, &FILE_INPUT, 4_000)
        .await
        .ok_or(UploadError::NoFileInput)?;
    client.set_input_files(selector, &[image]).await?;
    tracing::info!(image = %image.display(), "image staged into IG file input");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn validate_rejects_missing_file() {
        let err = validate_image(Path::new("/nope/missing.jpg")).unwrap_err();
        assert!(matches!(err, UploadError::NotFound(_)));
    }

    #[test]
    fn validate_rejects_bad_extension() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("clip.mp4");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"x").unwrap();
        let err = validate_image(&p).unwrap_err();
        assert!(matches!(err, UploadError::UnsupportedFormat(e) if e == "mp4"));
    }

    #[test]
    fn validate_accepts_jpg_and_png() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.jpg", "b.JPEG", "c.png"] {
            let p = dir.path().join(name);
            std::fs::File::create(&p).unwrap().write_all(b"x").unwrap();
            assert!(validate_image(&p).is_ok(), "{name} should validate");
        }
    }
}
