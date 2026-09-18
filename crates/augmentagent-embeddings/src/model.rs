//! Pinned model specifications and where their weights live on disk.

use std::path::{Path, PathBuf};

/// One downloadable file of a model, pinned by SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Short name used in `ModelId::model` and in the cache directory.
    pub name: &'static str,
    pub dim: usize,
    /// Maximum tokens per input; longer inputs are truncated.
    pub max_tokens: usize,
    pub files: &'static [ModelFile],
}

/// bge-small-en-v1.5, pinned to one upstream revision. MIT licensed.
pub const DEFAULT_MODEL: ModelSpec = ModelSpec {
    name: "bge-small-en-v1.5",
    dim: 384,
    max_tokens: 512,
    files: &[
        ModelFile {
            name: "model.onnx",
            url: "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/5c38ec7c405ec4b44b94cc5a9bb96e735b38267a/onnx/model.onnx",
            sha256: "828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35",
        },
        ModelFile {
            name: "tokenizer.json",
            url: "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/5c38ec7c405ec4b44b94cc5a9bb96e735b38267a/tokenizer.json",
            sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        },
    ],
};

pub const ENV_MODEL_DIR: &str = "AUGMENTAGENT_EMBEDDINGS_MODEL_DIR";
pub const ENV_THREADS: &str = "AUGMENTAGENT_EMBEDDINGS_THREADS";

impl ModelSpec {
    /// `$AUGMENTAGENT_EMBEDDINGS_MODEL_DIR/<name>` or
    /// `~/.local/share/augmentagent/models/<name>`.
    pub fn dir(&self) -> PathBuf {
        self.dir_under(std::env::var_os(ENV_MODEL_DIR).map(PathBuf::from))
    }

    pub fn dir_under(&self, root: Option<PathBuf>) -> PathBuf {
        let root = root.unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local/share/augmentagent/models")
        });
        root.join(self.name)
    }

    pub fn file_path(&self, dir: &Path, name: &str) -> PathBuf {
        dir.join(name)
    }

    /// Every pinned file present. Does not verify hashes (that is `fetch::verify`).
    pub fn is_present(&self, dir: &Path) -> bool {
        self.files.iter().all(|f| dir.join(f.name).is_file())
    }
}

/// Intra-op thread count: `AUGMENTAGENT_EMBEDDINGS_THREADS`, else half the
/// cores (min 1). Measured on the deployment class of machine: more threads
/// than physical cores is slower, and a backfill must leave room for the
/// daemon and its reasoner subprocesses.
pub fn thread_count() -> usize {
    thread_count_from(
        std::env::var(ENV_THREADS).ok().as_deref(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2),
    )
}

pub fn thread_count_from(env: Option<&str>, available: usize) -> usize {
    match env.and_then(|s| s.trim().parse::<usize>().ok()) {
        Some(n) if n >= 1 => n,
        _ => (available / 2).max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_count_defaults_to_half_the_cores_and_respects_the_env() {
        assert_eq!(thread_count_from(None, 12), 6);
        assert_eq!(thread_count_from(None, 1), 1);
        assert_eq!(thread_count_from(Some("3"), 12), 3);
        assert_eq!(thread_count_from(Some("0"), 12), 6);
        assert_eq!(thread_count_from(Some("lots"), 8), 4);
    }

    #[test]
    fn model_dir_honours_env_root_and_defaults_under_home() {
        let spec = &DEFAULT_MODEL;
        assert_eq!(
            spec.dir_under(Some(PathBuf::from("/models"))),
            PathBuf::from("/models/bge-small-en-v1.5")
        );
        let d = tempfile::tempdir().unwrap();
        assert!(!spec.is_present(d.path()));
        for f in spec.files {
            std::fs::write(d.path().join(f.name), b"x").unwrap();
        }
        assert!(spec.is_present(d.path()));
    }

    #[test]
    fn default_model_is_fully_pinned() {
        for f in DEFAULT_MODEL.files {
            assert!(
                f.url
                    .contains("/resolve/5c38ec7c405ec4b44b94cc5a9bb96e735b38267a/"),
                "{}",
                f.url
            );
            assert_eq!(f.sha256.len(), 64, "{} must carry a real SHA-256", f.name);
            assert!(
                f.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}",
                f.name
            );
        }
    }
}
