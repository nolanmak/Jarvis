//! `augmentagent gmail list-attachments` / `get-attachment` (#937).
//!
//! Composio's Gmail toolkit stages an attachment behind a presigned URL
//! (`GMAIL_GET_ATTACHMENT` → `data.file.s3url`); the id it needs comes from
//! the message's `attachmentList` (`GMAIL_FETCH_MESSAGE_BY_MESSAGE_ID`
//! format=full). Downloads land at `/tmp/aa-doc-<digits>-<idx>.<ext>` — the
//! same shape as Discord drops, which is the only `/tmp` shape the ask
//! agent's Read carve-out (scripts/aa-wiki-scope-guard.sh) admits — and
//! document kinds are extracted through the shared #939 pipeline so an
//! emailed scanned PDF gets the same OCR treatment as a Discord one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use augmentagent_channel_email::gmail::{pick_attachment, AttachmentMeta, ComposioClient};
use augmentagent_docs::{doc_kind_for, extract_text, OcrClient, OcrOutcome};
use augmentagent_store::Store;

/// Tempfile path for attachment `idx` of `message_id`. Gmail message ids are
/// 64-bit hex, so they fold to a decimal slot losslessly; anything else is
/// FNV-1a hashed so the slot stays digits-only and deterministic. The
/// extension is the (lower-cased, alphanumeric-only) filename extension, or
/// `bin` when there isn't a usable one.
pub fn tmp_doc_path(message_id: &str, idx: usize, filename: &str) -> PathBuf {
    let id = message_id.trim();
    let slot = u64::from_str_radix(id, 16).unwrap_or_else(|_| fnv1a64(id.as_bytes()));
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            e.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "bin".to_string());
    PathBuf::from(format!("/tmp/aa-doc-{slot}-{idx}.{ext}"))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `saved: <path> (<bytes> bytes, <mime>, "<original name>")`.
pub fn saved_line(path: &Path, bytes: usize, mime: &str, name: &str) -> String {
    format!(
        "saved: {} ({bytes} bytes, {mime}, {name:?})",
        path.display()
    )
}

/// Rows for `list-attachments --json`.
pub fn list_json_rows(metas: &[AttachmentMeta]) -> Vec<serde_json::Value> {
    metas
        .iter()
        .enumerate()
        .map(|(i, m)| {
            serde_json::json!({
                "index": i,
                "attachment_id": m.attachment_id,
                "filename": m.filename,
                "mime_type": m.mime_type,
            })
        })
        .collect()
}

/// Write `bytes` at `path` without ever following a symlink planted there
/// (CWE-59 — the `/tmp/aa-doc-*` path is predictable). `create_new` opens
/// with O_EXCL, which refuses to follow links; when something already sits at
/// the path (a previous download, or a planted link) it is unlinked AS A PATH
/// ENTRY — `remove_file` on a symlink removes the link, never its target — and
/// the exclusive create is retried once. A link re-planted in between makes
/// the retry fail loudly rather than write through it.
async fn write_no_follow(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut removed_once = false;
    loop {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(mut f) => {
                f.write_all(bytes).await?;
                f.flush().await?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && !removed_once => {
                tokio::fs::remove_file(path).await?;
                removed_once = true;
            }
            Err(e) => return Err(e),
        }
    }
}

fn composio_client(store: &Arc<Store>) -> Result<ComposioClient> {
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    Ok(ComposioClient::new(api_key).with_rate_limit_store(Arc::clone(store)))
}

pub async fn run_gmail_list_attachments(
    store: Arc<Store>,
    account: Option<String>,
    message_id: String,
    json: bool,
) -> Result<()> {
    let (entity_id, email) = crate::resolve_gmail_entity_id(&store, account)?;
    let gmail = composio_client(&store)?;
    let metas = gmail
        .list_attachments(&entity_id, &message_id)
        .await
        .with_context(|| format!("list attachments of {message_id} in {email}"))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "account": entity_id,
                "email": email,
                "message_id": message_id,
                "attachments": list_json_rows(&metas),
            }))?
        );
        return Ok(());
    }
    println!(
        "## account {entity_id} ({email}) — message {message_id} — {} attachment(s)",
        metas.len()
    );
    for (i, m) in metas.iter().enumerate() {
        println!(
            "[{i}] {}\n    attachmentId: {}",
            m.label(),
            m.attachment_id.as_deref().unwrap_or("-")
        );
    }
    if metas.is_empty() {
        println!("(no attachments on this message)");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_gmail_get_attachment(
    store: Arc<Store>,
    account: Option<String>,
    message_id: String,
    attachment_id: Option<String>,
    name: Option<String>,
    out: Option<PathBuf>,
    extract: bool,
    json: bool,
) -> Result<()> {
    let (entity_id, email) = crate::resolve_gmail_entity_id(&store, account)?;
    let gmail = composio_client(&store)?;
    let metas = gmail
        .list_attachments(&entity_id, &message_id)
        .await
        .with_context(|| format!("list attachments of {message_id} in {email}"))?;
    let (idx, meta) = pick_attachment(&metas, attachment_id.as_deref(), name.as_deref())?;
    let att_id = meta.attachment_id.as_deref().ok_or_else(|| {
        anyhow!(
            "attachment {} has no attachmentId in Composio's listing; cannot download",
            meta.label()
        )
    })?;
    let filename = meta
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("attachment-{idx}"));

    let file = gmail
        .get_attachment_file(&entity_id, &message_id, att_id, &filename)
        .await
        .with_context(|| format!("stage attachment {filename:?} for download"))?;
    let (bytes, content_type) = gmail
        .download_url(&file.s3url)
        .await
        .with_context(|| format!("download attachment {filename:?}"))?;
    let mime = [
        Some(file.mimetype.as_str()),
        meta.mime_type.as_deref(),
        content_type.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|s| !s.is_empty())
    .unwrap_or("application/octet-stream")
    .to_string();

    let path = out.unwrap_or_else(|| tmp_doc_path(&message_id, idx, &filename));
    write_no_follow(&path, &bytes)
        .await
        .with_context(|| format!("write {}", path.display()))?;

    // #939 — same pipeline as Discord drops. A failed extraction never undoes
    // a successful download; it's reported and the caller still has the file.
    let mut extracted: Option<(PathBuf, usize, OcrOutcome)> = None;
    let mut extract_error: Option<String> = None;
    if extract {
        if let Some(kind) = doc_kind_for(&filename, Some(&mime)) {
            let ocr = OcrClient::from_env();
            match extract_text(kind, &path, ocr.as_ref()).await {
                Ok(ex) => {
                    let txt = crate::doc_cmd::default_out_path(&path);
                    write_no_follow(&txt, ex.text.as_bytes())
                        .await
                        .with_context(|| format!("write {}", txt.display()))?;
                    extracted = Some((txt, ex.text.chars().count(), ex.ocr));
                }
                Err(e) => extract_error = Some(format!("{e:#}")),
            }
        }
    }

    if json {
        let mut v = serde_json::json!({
            "account": entity_id,
            "email": email,
            "message_id": message_id,
            "index": idx,
            "attachment_id": att_id,
            "filename": filename,
            "mime_type": mime,
            "bytes": bytes.len(),
            "path": path.display().to_string(),
            "extract_error": extract_error,
        });
        if let Some((txt, chars, outcome)) = &extracted {
            v["extracted"] = crate::doc_cmd::receipt_json(&path, Some(txt), *chars, outcome);
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("{}", saved_line(&path, bytes.len(), &mime, &filename));
    if let Some((txt, chars, outcome)) = &extracted {
        println!("{}", crate::doc_cmd::human_line(Some(txt), *chars, outcome));
    }
    if let Some(e) = extract_error {
        println!("extract failed: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The ask agent's Read carve-out (scripts/aa-wiki-scope-guard.sh) only
    /// admits `/tmp/aa-doc-<digits>-<digits>.<alnum>`. Gmail message ids are
    /// 64-bit hex, so we fold them to decimal to fit.
    fn guard_admits(p: &Path) -> bool {
        let s = p.to_string_lossy();
        let Some(rest) = s.strip_prefix("/tmp/aa-doc-") else {
            return false;
        };
        let mut parts = rest.splitn(3, ['-', '.']);
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        let ext = parts.next().unwrap_or("");
        !a.is_empty()
            && a.chars().all(|c| c.is_ascii_digit())
            && !b.is_empty()
            && b.chars().all(|c| c.is_ascii_digit())
            && !ext.is_empty()
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
    }

    #[test]
    fn tmp_doc_path_folds_the_hex_message_id_to_digits_and_keeps_the_extension() {
        let p = tmp_doc_path("1a05eed77b9f2074", 0, "PCT REPORTS .pdf");
        assert_eq!(p, Path::new("/tmp/aa-doc-1875167429129085044-0.pdf"));
        assert!(guard_admits(&p), "{}", p.display());
        // Index and case-folded extension.
        let p = tmp_doc_path("1a05eed77b9f2074", 2, "Notes.DOCX");
        assert_eq!(p, Path::new("/tmp/aa-doc-1875167429129085044-2.docx"));
        assert!(guard_admits(&p));
    }

    #[test]
    fn tmp_doc_path_hashes_non_hex_ids_and_falls_back_to_bin() {
        // A non-hex id (some Composio builds relay opaque ids) still yields a
        // digits-only slot, deterministically.
        let a = tmp_doc_path("msg_ABC-xyz", 0, "weird name.tar.gz");
        let b = tmp_doc_path("msg_ABC-xyz", 0, "weird name.tar.gz");
        assert_eq!(a, b);
        assert!(guard_admits(&a), "{}", a.display());
        assert!(a.to_string_lossy().ends_with("-0.gz"), "{}", a.display());
        // No usable extension → .bin; hostile extension chars are dropped.
        assert!(tmp_doc_path("ff", 0, "noext")
            .to_string_lossy()
            .ends_with("-0.bin"));
        assert!(tmp_doc_path("ff", 0, "x.p;d f")
            .to_string_lossy()
            .ends_with("-0.pdf"));
        assert!(tmp_doc_path("ff", 0, "x.")
            .to_string_lossy()
            .ends_with("-0.bin"));
    }

    // CodeRabbit on #946 (CWE-59): the /tmp/aa-doc-* path is predictable, so
    // a plain `fs::write` would follow a symlink planted there. The helper
    // must overwrite a regular file, refuse to follow a symlink (replacing
    // the link itself, never touching its target), and never leave the
    // download at a path it didn't create.
    #[tokio::test]
    async fn write_no_follow_overwrites_regular_files_but_never_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("aa-doc-1-0.pdf");

        // Fresh path → created.
        write_no_follow(&target, b"one").await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"one");
        // Existing regular file → overwritten (repeat downloads are normal).
        write_no_follow(&target, b"two").await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"two");

        // Symlink planted at the path → the victim must stay untouched.
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"precious").unwrap();
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&victim, &target).unwrap();
        write_no_follow(&target, b"payload").await.unwrap();
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"precious",
            "symlink was followed"
        );
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(
            meta.file_type().is_file(),
            "path must now be a regular file, not a link"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");
    }

    #[test]
    fn saved_line_reports_path_size_mime_and_original_name() {
        let l = saved_line(
            Path::new("/tmp/aa-doc-1-0.pdf"),
            637_812,
            "application/pdf",
            "PCT REPORTS .pdf",
        );
        assert_eq!(
            l,
            "saved: /tmp/aa-doc-1-0.pdf (637812 bytes, application/pdf, \"PCT REPORTS .pdf\")"
        );
    }

    #[test]
    fn list_json_rows_carry_index_id_name_mime() {
        let metas = vec![
            augmentagent_channel_email::gmail::AttachmentMeta {
                filename: Some("a.pdf".into()),
                mime_type: Some("application/pdf".into()),
                attachment_id: Some("ID1".into()),
            },
            augmentagent_channel_email::gmail::AttachmentMeta {
                filename: None,
                mime_type: None,
                attachment_id: None,
            },
        ];
        let rows = list_json_rows(&metas);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["index"], 0);
        assert_eq!(rows[0]["attachment_id"], "ID1");
        assert_eq!(rows[0]["filename"], "a.pdf");
        assert_eq!(rows[0]["mime_type"], "application/pdf");
        assert!(rows[1]["attachment_id"].is_null());
        assert!(rows[1]["filename"].is_null());
    }
}
