//! Host-selected raw attachment capabilities for one explicit owner DM.
//! Task-local binding keeps paths out of model arguments and other turns.
use anyhow::{Context, Result};
use serenity::all::Attachment;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::io::AsyncWriteExt;

pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
tokio::task_local! {
    pub static SELECTED: BTreeMap<String, PathBuf>;
}
pub fn selected() -> BTreeMap<String, PathBuf> {
    SELECTED.try_with(Clone::clone).unwrap_or_default()
}

pub fn eligible(filename: &str, content_type: Option<&str>, bytes: u64) -> bool {
    if bytes > MAX_BYTES {
        return false;
    }
    let extension = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    extension == "xlsx" || augmentagent_docs::inbound::classify(filename, content_type, 0).is_ok()
}

pub struct DownloadedInputs {
    _directory: tempfile::TempDir,
    pub files: BTreeMap<String, PathBuf>,
    pub labels: BTreeMap<String, String>,
}

pub async fn download(attachments: &[Attachment]) -> Result<Option<DownloadedInputs>> {
    if attachments.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        attachments.len() <= 32,
        "compute attachment count exceeds 32"
    );
    let directory = tempfile::Builder::new().prefix("jc-in-").tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut total = 0u64;
    let mut files = BTreeMap::new();
    let mut labels = BTreeMap::new();
    for (index, attachment) in attachments.iter().enumerate() {
        anyhow::ensure!(
            eligible(
                &attachment.filename,
                attachment.content_type.as_deref(),
                u64::from(attachment.size)
            ),
            "unsupported compute attachment"
        );
        let url = reqwest::Url::parse(&attachment.url)?;
        anyhow::ensure!(
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && matches!(
                    url.host_str(),
                    Some("cdn.discordapp.com" | "media.discordapp.net")
                ),
            "invalid attachment origin"
        );
        let mut response = client.get(url).send().await?.error_for_status()?;
        anyhow::ensure!(response.status().is_success(), "attachment redirect denied");
        anyhow::ensure!(
            response.content_length().unwrap_or(0) <= MAX_BYTES - total,
            "compute input byte limit exceeded"
        );
        let extension = attachment
            .filename
            .rsplit('.')
            .next()
            .filter(|ext| {
                !ext.is_empty() && ext.len() <= 10 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let alias = format!("input-{}.{}", index + 1, extension);
        let path = directory.path().join(&alias);
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).await?;
        let before = total;
        while let Some(bytes) = response
            .chunk()
            .await
            .context("attachment download failed")?
        {
            total += bytes.len() as u64;
            anyhow::ensure!(total <= MAX_BYTES, "compute input byte limit exceeded");
            file.write_all(&bytes).await?;
        }
        file.flush().await?;
        anyhow::ensure!(
            total - before == u64::from(attachment.size),
            "attachment size changed during import"
        );
        labels.insert(alias.clone(), attachment.filename.clone());
        files.insert(alias, path);
    }
    Ok(Some(DownloadedInputs {
        _directory: directory,
        files,
        labels,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spreadsheet_input_has_explicit_size_boundary() {
        assert!(eligible("numbers.xlsx", None, MAX_BYTES));
        assert!(!eligible("numbers.xlsx", None, MAX_BYTES + 1));
        assert!(!eligible("program.exe", None, 1));
    }
    #[tokio::test]
    async fn concurrent_turns_cannot_observe_other_input_paths() {
        let first = BTreeMap::from([("first".into(), PathBuf::from("/fixture/first"))]);
        let second = BTreeMap::from([("second".into(), PathBuf::from("/fixture/second"))]);
        let (a, b) = tokio::join!(
            SELECTED.scope(first.clone(), async {
                tokio::task::yield_now().await;
                selected()
            }),
            SELECTED.scope(second.clone(), async {
                tokio::task::yield_now().await;
                selected()
            })
        );
        assert_eq!(a, first);
        assert_eq!(b, second);
        assert!(selected().is_empty());
    }
}
