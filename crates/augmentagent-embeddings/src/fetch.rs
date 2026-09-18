//! Explicit model download with SHA-256 verification. The only code path
//! that ever fetches weights.

use std::path::Path;

use anyhow::{bail, Context};
use sha2::{Digest, Sha256};

use crate::model::{ModelFile, ModelSpec};

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Which pinned files are missing or have the wrong hash.
pub fn verify(spec: &ModelSpec, dir: &Path) -> anyhow::Result<Vec<&'static str>> {
    let mut bad = Vec::new();
    for f in spec.files {
        let p = dir.join(f.name);
        if !p.is_file() || sha256_file(&p)? != f.sha256 {
            bad.push(f.name);
        }
    }
    Ok(bad)
}

/// The error every caller shows when weights are absent.
pub fn missing_error(spec: &ModelSpec, dir: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "embedding model `{}` is not present at {} — run `augmentagent embeddings fetch-model` \
         (nothing downloads weights implicitly)",
        spec.name,
        dir.display()
    )
}

/// Download every pinned file that is missing or corrupt into `dir`, writing
/// to a temp name and renaming only after the hash matches. `base_url`
/// overrides the pinned URLs' origin (tests point it at a mock server).
pub async fn fetch(
    spec: &ModelSpec,
    dir: &Path,
    base_url: Option<&str>,
) -> anyhow::Result<Vec<&'static str>> {
    std::fs::create_dir_all(dir)?;
    let client = reqwest::Client::builder()
        .user_agent("augmentagent-embeddings")
        .build()?;
    let mut fetched = Vec::new();
    for f in spec.files {
        let target = dir.join(f.name);
        if target.is_file() && sha256_file(&target)? == f.sha256 {
            continue;
        }
        let url = match base_url {
            Some(base) => rewrite_origin(f.url, base),
            None => f.url.to_string(),
        };
        download_verified(&client, f, &url, &target).await?;
        fetched.push(f.name);
    }
    Ok(fetched)
}

fn rewrite_origin(url: &str, base: &str) -> String {
    // "https://host/path" → "<base>/path"
    let path = url.splitn(4, '/').nth(3).unwrap_or("");
    format!("{}/{}", base.trim_end_matches('/'), path)
}

async fn download_verified(
    client: &reqwest::Client,
    f: &ModelFile,
    url: &str,
    target: &Path,
) -> anyhow::Result<()> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    let bytes = resp.bytes().await?;
    let got = hex::encode(Sha256::digest(&bytes));
    if got != f.sha256 {
        bail!(
            "{}: SHA-256 mismatch (expected {}, got {}); refusing to install",
            f.name,
            f.sha256,
            got
        );
    }
    let tmp = target.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelSpec;

    const GOOD: &[u8] = b"good weights";
    // sha256("good weights")
    const GOOD_SHA: &str = "6d37d15b3cf5a5e9a1e7a2c32ea6ad2d53c2c4a2b8f7a0b0e5a9c9a1e3b8f5d1";

    fn spec(sha: &'static str) -> ModelSpec {
        ModelSpec {
            name: "test-model",
            dim: 4,
            max_tokens: 8,
            files: Box::leak(Box::new([ModelFile {
                name: "model.onnx",
                url: "https://example.invalid/repo/resolve/rev/model.onnx",
                sha256: sha,
            }])),
        }
    }

    fn real_sha(bytes: &[u8]) -> &'static str {
        Box::leak(hex::encode(Sha256::digest(bytes)).into_boxed_str())
    }

    #[test]
    fn missing_model_dir_errors_with_the_fetch_command() {
        let d = tempfile::tempdir().unwrap();
        let e = missing_error(&spec(GOOD_SHA), d.path()).to_string();
        assert!(e.contains("augmentagent embeddings fetch-model"), "{e}");
        assert!(e.contains("implicitly"));
    }

    #[test]
    fn corrupt_model_file_fails_the_sha_check() {
        let d = tempfile::tempdir().unwrap();
        let s = spec(real_sha(GOOD));
        assert_eq!(
            verify(&s, d.path()).unwrap(),
            ["model.onnx"],
            "absent counts as bad"
        );
        std::fs::write(d.path().join("model.onnx"), b"tampered").unwrap();
        assert_eq!(verify(&s, d.path()).unwrap(), ["model.onnx"]);
        std::fs::write(d.path().join("model.onnx"), GOOD).unwrap();
        assert!(verify(&s, d.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_installs_only_verified_bytes_and_skips_present_files() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/repo/resolve/rev/model.onnx")
            .with_body(GOOD)
            .expect(1)
            .create_async()
            .await;
        let d = tempfile::tempdir().unwrap();
        let s = spec(real_sha(GOOD));
        assert_eq!(
            fetch(&s, d.path(), Some(&server.url())).await.unwrap(),
            ["model.onnx"]
        );
        assert_eq!(std::fs::read(d.path().join("model.onnx")).unwrap(), GOOD);
        // Second call: present and valid → no request.
        assert!(fetch(&s, d.path(), Some(&server.url()))
            .await
            .unwrap()
            .is_empty());
        m.assert_async().await;
        assert!(!d.path().join("model.onnx.part").exists());
    }

    #[tokio::test]
    async fn fetch_refuses_bytes_with_the_wrong_hash() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/repo/resolve/rev/model.onnx")
            .with_body(b"not the pinned weights")
            .create_async()
            .await;
        let d = tempfile::tempdir().unwrap();
        let err = fetch(&spec(real_sha(GOOD)), d.path(), Some(&server.url()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SHA-256 mismatch"), "{err}");
        assert!(!d.path().join("model.onnx").exists(), "nothing installed");
    }

    #[test]
    fn origin_rewrite_keeps_the_path() {
        assert_eq!(
            rewrite_origin(
                "https://huggingface.co/a/b/resolve/r/x.onnx",
                "http://127.0.0.1:9/"
            ),
            "http://127.0.0.1:9/a/b/resolve/r/x.onnx"
        );
    }
}
