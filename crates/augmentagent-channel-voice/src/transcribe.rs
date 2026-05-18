//! Speech-to-text via a local whisper.cpp binary, run as a subprocess with a
//! hard timeout. Decoupled behind a trait so the backend can be swapped per
//! deploy (hosted STT, a different local model) without touching the channel.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, warn};

/// Hard wallclock cap on one transcription. A stuck whisper process must
/// never wedge the listener loop.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum TranscribeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcription timed out after {0:?}")]
    Timeout(Duration),
    #[error("whisper exited {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("empty transcript")]
    Empty,
}

#[async_trait]
pub trait Transcriber: Send + Sync {
    /// Transcribe an audio file already on disk. Returns plain text.
    async fn transcribe(&self, audio_path: &Path) -> Result<String, TranscribeError>;
}

/// whisper.cpp subprocess transcriber.
///
/// Invokes `<bin> -m <model> -f <audio> -otxt -of <stem>` and reads the
/// `<stem>.txt` whisper.cpp writes. `scripts/build-whisper.sh` vendors both
/// the binary and the `ggml-medium.en` model into `vendor/whisper/`.
pub struct WhisperCppTranscriber {
    pub bin: PathBuf,
    pub model: PathBuf,
    pub timeout: Duration,
}

impl WhisperCppTranscriber {
    pub fn new(bin: impl Into<PathBuf>, model: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            model: model.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Default on-disk layout produced by `scripts/build-whisper.sh`,
    /// rooted at the repo. `<repo>/vendor/whisper/{main,models/ggml-medium.en.bin}`.
    pub fn from_repo_root(repo_root: &Path) -> Self {
        let base = repo_root.join("vendor/whisper");
        Self::new(
            base.join("main"),
            base.join("models/ggml-medium.en.bin"),
        )
    }
}

#[async_trait]
impl Transcriber for WhisperCppTranscriber {
    async fn transcribe(&self, audio_path: &Path) -> Result<String, TranscribeError> {
        let stem = audio_path.with_extension("");
        let out_txt = audio_path.with_extension("txt");

        let mut cmd = Command::new(&self.bin);
        cmd.arg("-m")
            .arg(&self.model)
            .arg("-f")
            .arg(audio_path)
            .arg("-otxt")
            .arg("-of")
            .arg(&stem)
            .arg("-nt") // no timestamps in the txt output
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let child = cmd.spawn()?;
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(r) => r?,
            Err(_) => {
                warn!(?self.timeout, "whisper transcription timed out");
                return Err(TranscribeError::Timeout(self.timeout));
            }
        };
        if !output.status.success() {
            return Err(TranscribeError::Exit {
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let text = tokio::fs::read_to_string(&out_txt).await.unwrap_or_default();
        let _ = tokio::fs::remove_file(&out_txt).await;
        let trimmed = text.trim().to_string();
        debug!(chars = trimmed.len(), "whisper transcript ready");
        if trimmed.is_empty() {
            return Err(TranscribeError::Empty);
        }
        Ok(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted transcriber used by the channel tests so they don't need
    /// a real whisper build.
    pub struct StubTranscriber(pub String);

    #[async_trait]
    impl Transcriber for StubTranscriber {
        async fn transcribe(&self, _p: &Path) -> Result<String, TranscribeError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn missing_binary_is_io_error() {
        let t = WhisperCppTranscriber::new("/nonexistent/whisper-bin", "/dev/null");
        let err = t
            .transcribe(Path::new("/tmp/does-not-matter.ogg"))
            .await
            .unwrap_err();
        assert!(matches!(err, TranscribeError::Io(_)));
    }

    #[tokio::test]
    async fn from_repo_root_builds_expected_paths() {
        let t = WhisperCppTranscriber::from_repo_root(Path::new("/repo"));
        assert!(t.bin.ends_with("vendor/whisper/main"));
        assert!(t.model.ends_with("vendor/whisper/models/ggml-medium.en.bin"));
    }

    #[tokio::test]
    async fn stub_returns_scripted_text() {
        let s = StubTranscriber("hello world".into());
        assert_eq!(
            s.transcribe(Path::new("/x")).await.unwrap(),
            "hello world"
        );
    }
}
