//! Read-only GitHub document sources. No ambient credential fallback or checkout.
use anyhow::{Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Subcommand)]
pub enum Command {
    /// List configured source aliases (no network access).
    Sources,
    /// Fetch the latest configured branch and list document paths.
    List {
        #[arg(long)]
        source: String,
        #[arg(long, default_value = "")]
        prefix: String,
    },
    /// Fetch an original document and stage it for ATTACH delivery.
    Get {
        #[arg(long)]
        source: String,
        #[arg(long)]
        path: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    sources: std::collections::BTreeMap<String, Source>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    repository: String,
    branch: String,
    key_path: PathBuf,
    known_hosts: PathBuf,
}
#[derive(Debug, Serialize)]
struct Entry {
    path: String,
    bytes: u64,
    blob: String,
}
fn config_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("HOME or XDG_CONFIG_HOME is required")?;
    Ok(base.join("augmentagent/repo-docs.json"))
}
fn private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let m = std::fs::symlink_metadata(path).context("read-only source file unavailable")?;
    anyhow::ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "source credentials/config must be regular files"
    );
    anyhow::ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.permissions().mode() & 0o077 == 0,
        "source credentials/config must be owned by this user with mode 0600"
    );
    Ok(())
}
fn load_config() -> Result<Config> {
    let path = config_path()?;
    private_file(&path).context("configure read-only sources in ~/.config/augmentagent/repo-docs.json; no ambient GitHub credential fallback")?;
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
fn validate_source(source: &Source) -> Result<()> {
    let parts: Vec<_> = source.repository.split('/').collect();
    anyhow::ensure!(
        parts.len() == 2
            && parts.iter().all(|p| !p.is_empty()
                && !p.starts_with('.')
                && !p.starts_with('-')
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))),
        "source must be a GitHub owner/repository"
    );
    validate_path(&source.branch, false)?;
    anyhow::ensure!(
        !source.branch.contains("..")
            && !source.branch.contains("@{")
            && !source.branch.ends_with('.'),
        "invalid source branch"
    );
    anyhow::ensure!(
        source.key_path.is_absolute() && source.known_hosts.is_absolute(),
        "credential paths must be absolute"
    );
    private_file(&source.key_path)?;
    anyhow::ensure!(
        source.known_hosts.is_file(),
        "SSH known_hosts must exist; verify GitHub's host key during setup"
    );
    Ok(())
}
fn validate_path(path: &str, empty_ok: bool) -> Result<()> {
    if empty_ok && path.is_empty() {
        return Ok(());
    }
    anyhow::ensure!(
        !path.is_empty()
            && !path.starts_with('-')
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "use a relative repository path without traversal or control characters"
    );
    Ok(())
}
fn supported(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "pdf" | "md" | "txt" | "doc" | "docx" | "csv" | "tsv" | "rtf" | "xlsx" | "pptx"
    )
}
fn parse_tree(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut result = Vec::new();
    for item in bytes.split(|b| *b == 0).filter(|x| !x.is_empty()) {
        let item = std::str::from_utf8(item).context("repository contains a non-UTF8 path")?;
        let (header, path) = item.split_once('\t').context("invalid git tree record")?;
        let fields: Vec<_> = header.split_whitespace().collect();
        anyhow::ensure!(fields.len() == 4, "invalid git tree fields");
        if fields[0] != "100644" || fields[1] != "blob" || !supported(path) {
            continue;
        }
        validate_path(path, false)?;
        anyhow::ensure!(
            fields[2].len() == 40 && fields[2].chars().all(|c| c.is_ascii_hexdigit()),
            "invalid blob id"
        );
        result.push(Entry {
            path: path.into(),
            bytes: fields[3].parse()?,
            blob: fields[2].into(),
        });
    }
    Ok(result)
}
fn quote_shell(path: &Path) -> Result<String> {
    let value = path.to_str().context("non-UTF8 credential path")?;
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "invalid credential path"
    );
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}
fn git_command(dir: &Path, source: &Source) -> Result<tokio::process::Command> {
    let mut cmd = tokio::process::Command::new("/usr/bin/git");
    // Deliberately discard GH_TOKEN, credential helpers, SSH agent and global Git config.
    cmd.env_clear().env("PATH","/usr/bin:/bin").env("HOME",dir)
        .env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
        .env("GIT_TERMINAL_PROMPT","0").env("GIT_SSH_VARIANT","ssh")
        .env("GIT_SSH_COMMAND",format!("/usr/bin/ssh -l git -F /dev/null -i {} -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile={}",quote_shell(&source.key_path)?,quote_shell(&source.known_hosts)?))
        .arg("-c").arg("core.hooksPath=/dev/null")
        .arg("-c").arg("protocol.file.allow=never")
        .arg("-c").arg("protocol.ext.allow=never")
        .current_dir(dir).kill_on_drop(true);
    Ok(cmd)
}
async fn git(dir: &Path, source: &Source, args: &[&str]) -> Result<Vec<u8>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        git_command(dir, source)?.args(args).output(),
    )
    .await
    .context("repository read timed out")??;
    anyhow::ensure!(output.status.success(),"repository read failed; check the configured branch, read-only deploy key and verified GitHub host key");
    Ok(output.stdout)
}
pub async fn run(op: &Command, wiki: Option<&Path>) -> Result<()> {
    let config = load_config()?;
    if matches!(op, Command::Sources) {
        println!(
            "{}",
            serde_json::to_string_pretty(&config.sources.keys().collect::<Vec<_>>())?
        );
        return Ok(());
    }
    let (alias, path) = match op {
        Command::List { source, prefix } => (source, prefix),
        Command::Get { source, path } => (source, path),
        Command::Sources => unreachable!(),
    };
    let source = config
        .sources
        .get(alias)
        .context("unknown source; use repo-docs sources")?;
    validate_source(source)?;
    validate_path(path, matches!(op, Command::List { .. }))?;
    let wiki = wiki
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("WIKI_ROOT").map(PathBuf::from));
    if matches!(op, Command::Get { .. }) {
        anyhow::ensure!(
            wiki.as_ref().is_some_and(|p| p.is_dir()),
            "--wiki-dir or WIKI_ROOT must identify an existing delivery root"
        );
    }
    let temp = tempfile::tempdir()?;
    git(temp.path(), source, &["init", "--bare", "--quiet"]).await?;
    let remote = format!("ssh://github.com/{}.git", source.repository);
    let branch = format!("refs/heads/{}", source.branch);
    git(
        temp.path(),
        source,
        &[
            "fetch",
            "--quiet",
            "--depth=1",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            &remote,
            &branch,
        ],
    )
    .await?;
    let revision =
        String::from_utf8(git(temp.path(), source, &["rev-parse", "FETCH_HEAD^{commit}"]).await?)?
            .trim()
            .to_string();
    let committed_at = String::from_utf8(
        git(
            temp.path(),
            source,
            &["show", "-s", "--format=%cI", &revision],
        )
        .await?,
    )?
    .trim()
    .to_string();
    let entries = parse_tree(
        &git(
            temp.path(),
            source,
            &["ls-tree", "-r", "-l", "-z", &revision],
        )
        .await?,
    )?;
    match op {
        Command::List { prefix, .. } => {
            let files: Vec<_> = entries
                .into_iter()
                .filter(|e| {
                    prefix.is_empty()
                        || e.path == *prefix
                        || e.path.starts_with(&format!("{prefix}/"))
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"source":alias,"revision":revision,"committed_at":committed_at,"files":files})
                )?
            );
        }
        Command::Get { path, .. } => {
            let entry=entries.iter().find(|e| e.path==*path).context("document not found or unsupported (symlinks, submodules and executable files are excluded)")?;
            anyhow::ensure!(
                entry.bytes <= augmentagent_docs::delivery::MAX_BYTES as u64,
                "document exceeds the 8 MiB delivery limit"
            );
            let bytes = git(temp.path(), source, &["cat-file", "blob", &entry.blob]).await?;
            anyhow::ensure!(
                bytes.len() as u64 == entry.bytes,
                "download size differs from Git object metadata"
            );
            let filename = Path::new(path)
                .file_name()
                .and_then(|x| x.to_str())
                .context("invalid filename")?;
            let staged =
                augmentagent_docs::delivery::stage(wiki.as_ref().unwrap(), filename, &bytes)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"source":alias,"revision":revision,"committed_at":committed_at,"source_path":path,"blob":entry.blob,"bytes":bytes.len(),"path":staged})
                )?
            );
            println!("ATTACH: {}", staged.display());
        }
        Command::Sources => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn revision_and_original_bytes_follow_the_current_git_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let source = Source {
            repository: "example/docs".into(),
            branch: "main".into(),
            key_path: tmp.path().join("unused-key"),
            known_hosts: tmp.path().join("unused-hosts"),
        };
        git(
            tmp.path(),
            &source,
            &["init", "--quiet", "--initial-branch=main"],
        )
        .await
        .unwrap();
        let mut previous = String::new();
        for bytes in [
            b"%PDF-1.7 first\x00".as_slice(),
            b"%PDF-1.7 second\xff".as_slice(),
        ] {
            std::fs::write(tmp.path().join("report.pdf"), bytes).unwrap();
            git(tmp.path(), &source, &["add", "--", "report.pdf"])
                .await
                .unwrap();
            git(
                tmp.path(),
                &source,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.com",
                    "commit",
                    "--quiet",
                    "-m",
                    "Update document",
                ],
            )
            .await
            .unwrap();
            let revision = String::from_utf8(
                git(tmp.path(), &source, &["rev-parse", "HEAD"])
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_ne!(revision, previous);
            let entries = parse_tree(
                &git(
                    tmp.path(),
                    &source,
                    &["ls-tree", "-r", "-l", "-z", revision.trim()],
                )
                .await
                .unwrap(),
            )
            .unwrap();
            assert_eq!(entries.len(), 1);
            let original = git(tmp.path(), &source, &["cat-file", "blob", &entries[0].blob])
                .await
                .unwrap();
            assert_eq!(original, bytes);
            previous = revision;
        }
    }

    #[test]
    fn source_credentials_are_private_and_git_is_isolated() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let key = tmp.path().join("read only key");
        std::fs::write(&key, b"synthetic").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hosts = tmp.path().join("known_hosts");
        std::fs::write(&hosts, b"synthetic").unwrap();
        let mut source = Source {
            repository: "example/docs".into(),
            branch: "main".into(),
            key_path: key.clone(),
            known_hosts: hosts,
        };
        assert!(validate_source(&source).is_ok());
        let command = git_command(tmp.path(), &source).unwrap();
        let env: std::collections::BTreeMap<_, _> = command
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(!env.contains_key("GH_TOKEN"));
        assert!(!env.contains_key("SSH_AUTH_SOCK"));
        assert_eq!(env["GIT_CONFIG_GLOBAL"].as_deref(), Some("/dev/null"));
        let ssh = env["GIT_SSH_COMMAND"].as_ref().unwrap();
        for guard in [
            "IdentityAgent=none",
            "IdentitiesOnly=yes",
            "StrictHostKeyChecking=yes",
            "-F /dev/null",
            "read only key",
        ] {
            assert!(ssh.contains(guard));
        }
        for invalid in [
            "https://example.invalid/docs",
            "-oProxyCommand=x/docs",
            "../docs",
            "example/docs/extra",
        ] {
            source.repository = invalid.into();
            assert!(validate_source(&source).is_err());
        }
        source.repository = "example/docs".into();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_source(&source).is_err());
        std::fs::remove_file(&key).unwrap();
        std::os::unix::fs::symlink(&source.known_hosts, &key).unwrap();
        assert!(validate_source(&source).is_err());
    }
    #[test]
    fn path_validation_rejects_options_traversal_and_controls() {
        for value in [
            "/etc/passwd",
            "../a",
            "a/../b",
            "a//b",
            "-config",
            "a\\b",
            "a\nb",
            "a/./b",
        ] {
            assert!(validate_path(value, false).is_err(), "{value}");
        }
        assert!(validate_path("reports/Example report.pdf", false).is_ok());
        assert!(validate_path("", true).is_ok());
        assert!(validate_path("", false).is_err());
    }
    #[test]
    fn tree_only_admits_regular_supported_documents() {
        let data = b"100644 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 12\treports/a.pdf\0\
120000 blob bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 8\tlink.pdf\0\
160000 commit cccccccccccccccccccccccccccccccccccccccc -\tsubmodule\0\
100644 blob dddddddddddddddddddddddddddddddddddddddd 7\ttool.sh\0";
        let entries = parse_tree(data).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "reports/a.pdf");
        assert_eq!(entries[0].bytes, 12);
        assert!(parse_tree(b"garbage\0").is_err());
    }
}
