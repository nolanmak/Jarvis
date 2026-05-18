//! Media staging for the browser-posting path (#50/#76).
//!
//! The only "upload" primitive we use is CDP `setInputFiles` on Instagram's
//! hidden `<input type=file>` — the native OS file chooser never opens (that
//! would need a human). This module owns: validating the local media path(s),
//! finding the file input via the layered registry, and injecting the path.
//!
//! Surfaces:
//! - **Feed image** ([`stage_image`]): exactly one jpg/png.
//! - **Carousel** ([`stage_carousel`]): 2..=[`CAROUSEL_MAX`] images in one
//!   `set_input_files` call (#76 §4 — Meta's 20-item ceiling, enforced here
//!   before any UI is driven).
//! - **Reel** ([`stage_video`]): one mp4/mov into the video-accept input
//!   (#76 §3).
//! - **Story** reuses [`stage_image`] / [`stage_video`] — a single asset on
//!   its own composer route.

use std::path::{Path, PathBuf};

use augmentagent_browser_client::BrowserClient;
use thiserror::Error;

use crate::selectors::{Target, FILE_INPUT, VIDEO_FILE_INPUT};

/// Meta's current carousel ceiling (recently raised from 10 → 20, #76 §4).
/// Enforced in the channel *before* dispatch so we never drive the UI with
/// an over-cap set.
pub const CAROUSEL_MAX: usize = 20;
/// A carousel is ≥2 items by definition; one item is a plain feed post.
pub const CAROUSEL_MIN: usize = 2;

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("image not found: {0}")]
    NotFound(PathBuf),
    #[error("unsupported image extension: {0} (jpg/jpeg/png only)")]
    UnsupportedFormat(String),
    #[error("unsupported video extension: {0} (mp4/mov only)")]
    UnsupportedVideo(String),
    #[error(
        "carousel item count {n} out of range ({CAROUSEL_MIN}..={CAROUSEL_MAX})"
    )]
    CarouselCount { n: usize },
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

/// Reel / Story video. IG's web Reel composer `accept` is
/// `video/mp4,video/quicktime` (#76 §3) — restrict to those container exts.
pub fn validate_video(path: &Path) -> Result<(), UploadError> {
    if !path.is_file() {
        return Err(UploadError::NotFound(path.to_path_buf()));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "mp4" | "mov" | "qt") {
        return Err(UploadError::UnsupportedVideo(ext));
    }
    Ok(())
}

/// Validate a carousel set: count in `[CAROUSEL_MIN, CAROUSEL_MAX]` and
/// every item a valid image. Pure — callable before any browser work so an
/// over-cap or bad-format set never touches the UI.
pub fn validate_carousel(paths: &[PathBuf]) -> Result<(), UploadError> {
    let n = paths.len();
    if !(CAROUSEL_MIN..=CAROUSEL_MAX).contains(&n) {
        return Err(UploadError::CarouselCount { n });
    }
    for p in paths {
        validate_image(p)?;
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

/// Stage a full carousel set into the hidden file input in a single CDP
/// `setInputFiles` call (#76 §4). IG accepts a multi-file selection on the
/// first picker open; "Add more" appends are driven by the composer, not
/// here. Validates count + every item before touching the UI.
pub async fn stage_carousel(
    client: &BrowserClient,
    images: &[PathBuf],
) -> Result<(), UploadError> {
    validate_carousel(images)?;
    let selector = resolve_target(client, &FILE_INPUT, 4_000)
        .await
        .ok_or(UploadError::NoFileInput)?;
    client.set_input_files(selector, images).await?;
    tracing::info!(
        count = images.len(),
        "carousel ({} items) staged into IG file input",
        images.len()
    );
    Ok(())
}

/// Append more images to an open carousel via the *already-clicked*
/// "Add more" picker. Same primitive as [`stage_carousel`] but resolves the
/// (possibly fresh) input again and does not re-validate the running total —
/// the composer owns the cumulative cap check.
pub async fn append_carousel(
    client: &BrowserClient,
    images: &[PathBuf],
) -> Result<(), UploadError> {
    for p in images {
        validate_image(p)?;
    }
    let selector = resolve_target(client, &FILE_INPUT, 4_000)
        .await
        .ok_or(UploadError::NoFileInput)?;
    client.set_input_files(selector, images).await?;
    tracing::info!(count = images.len(), "appended carousel slides");
    Ok(())
}

/// Stage one video (Reel / Story) into the video-accept file input via CDP.
pub async fn stage_video(
    client: &BrowserClient,
    video: &Path,
) -> Result<(), UploadError> {
    validate_video(video)?;
    // Reel composer's input is video-accept; fall through to the generic
    // FILE_INPUT registry if the video-specific one doesn't resolve.
    let selector = match resolve_target(client, &VIDEO_FILE_INPUT, 4_000).await {
        Some(s) => s,
        None => resolve_target(client, &FILE_INPUT, 4_000)
            .await
            .ok_or(UploadError::NoFileInput)?,
    };
    client.set_input_files(selector, &[video]).await?;
    tracing::info!(video = %video.display(), "video staged into IG file input");
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

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().write_all(b"x").unwrap();
        p
    }

    #[test]
    fn validate_video_accepts_mp4_mov_only() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["clip.mp4", "clip.MOV", "clip.qt"] {
            assert!(
                validate_video(&touch(dir.path(), name)).is_ok(),
                "{name} should validate"
            );
        }
        let bad = touch(dir.path(), "clip.webm");
        assert!(matches!(
            validate_video(&bad).unwrap_err(),
            UploadError::UnsupportedVideo(e) if e == "webm"
        ));
        let img = touch(dir.path(), "still.jpg");
        assert!(matches!(
            validate_video(&img).unwrap_err(),
            UploadError::UnsupportedVideo(e) if e == "jpg"
        ));
    }

    #[test]
    fn validate_carousel_enforces_count_window() {
        let dir = tempfile::tempdir().unwrap();
        // 1 item → not a carousel.
        let one = vec![touch(dir.path(), "0.jpg")];
        assert!(matches!(
            validate_carousel(&one).unwrap_err(),
            UploadError::CarouselCount { n: 1 }
        ));
        // 2 items → ok (lower bound).
        let two: Vec<PathBuf> =
            (0..2).map(|i| touch(dir.path(), &format!("a{i}.jpg"))).collect();
        assert!(validate_carousel(&two).is_ok());
        // 20 items → ok (upper bound).
        let twenty: Vec<PathBuf> = (0..CAROUSEL_MAX)
            .map(|i| touch(dir.path(), &format!("b{i}.jpg")))
            .collect();
        assert!(validate_carousel(&twenty).is_ok());
        // 21 items → over Meta's ceiling, rejected before any UI work.
        let over: Vec<PathBuf> = (0..CAROUSEL_MAX + 1)
            .map(|i| touch(dir.path(), &format!("c{i}.jpg")))
            .collect();
        assert!(matches!(
            validate_carousel(&over).unwrap_err(),
            UploadError::CarouselCount { n } if n == CAROUSEL_MAX + 1
        ));
    }

    #[test]
    fn validate_carousel_rejects_bad_member_format() {
        let dir = tempfile::tempdir().unwrap();
        let mixed = vec![
            touch(dir.path(), "ok.jpg"),
            touch(dir.path(), "bad.gif"),
        ];
        assert!(matches!(
            validate_carousel(&mixed).unwrap_err(),
            UploadError::UnsupportedFormat(e) if e == "gif"
        ));
    }

    #[test]
    fn carousel_bounds_are_meta_ceiling() {
        assert_eq!(CAROUSEL_MAX, 20);
        assert_eq!(CAROUSEL_MIN, 2);
    }
}
