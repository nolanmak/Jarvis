//! `augmentagent ops-archive sync` (#1414).
//!
//! The rule for data this project accumulates: **archive only what is small,
//! text and cannot be regenerated; prune everything else.** Build output,
//! temp files, old binaries and session transcripts are pruned. Rotated logs
//! and the audit sinks are the only record of past incidents and of what the
//! agent spent, and they compress to a few MB a month: those are archived.
//!
//! Off unless `AUGMENTAGENT_OPS_ARCHIVE_REMOTE` names a repo. Then, modelled
//! on `wiki sync` (#474):
//!
//! * only closed, compressed rotations from the state dir are taken — never
//!   a live file, the database, or `.env` — and a hard allowlist on the
//!   staged paths aborts the commit otherwise;
//! * the push is refused unless the remote is confirmed PRIVATE: these files
//!   contain message content;
//! * a file holding one of this process's own secret values, or a known
//!   credential prefix, is skipped and reported, never pushed;
//! * after a successful push, local rotations past retention are deleted.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::{Datelike, NaiveDate};

use crate::log_rotate;

/// GitHub rejects a single file at 100 MB.
const MAX_FILE_BYTES: u64 = 95 * 1024 * 1024;
/// A secret shorter than this is too likely to match ordinary text.
const MIN_SECRET_LEN: usize = 12;
/// Leading text of credentials that should never be in a log at all.
const CREDENTIAL_PREFIXES: &[&str] = &[
    "sk-ant-",
    "ghp_",
    "gho_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "-----BEGIN ",
];

#[derive(Debug, Clone)]
pub struct Config {
    /// `owner/repo` on GitHub.
    pub remote: String,
    /// The local clone the archive is committed to.
    pub repo_dir: PathBuf,
    pub state_dir: PathBuf,
    pub gh_bin: String,
    /// Values that must never be pushed.
    pub secrets: Vec<String>,
    pub today: NaiveDate,
    pub keep_months: u32,
}

/// The configured remote, if the archive is on.
pub fn remote_from_env() -> Option<String> {
    std::env::var("AUGMENTAGENT_OPS_ARCHIVE_REMOTE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn repo_dir_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AUGMENTAGENT_OPS_ARCHIVE_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("augmentagent"),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share/augmentagent"),
    };
    Some(data.join("ops-archive"))
}

/// This process's own secret values: every env var whose name looks like a
/// credential and whose value could be one.
pub fn secrets_from_env(is_secret_name: &dyn Fn(&str) -> bool) -> Vec<String> {
    std::env::vars()
        .filter(|(k, v)| is_secret_name(k) && could_be_credential(v))
        .map(|(_, v)| v)
        .collect()
}

/// A name like `DISCORD_CHANNEL_ID` or `CLAUDE_CONFIG_DIR` matches the
/// secret-name list, but its value is an identifier or a location, and those
/// are all over an ordinary log. Treating them as secrets skipped the main
/// daemon log on the first live run. A credential is long, opaque text.
pub fn could_be_credential(value: &str) -> bool {
    let v = value.trim();
    v.len() >= MIN_SECRET_LEN
        && !v.bytes().all(|b| b.is_ascii_digit())
        && !v.starts_with('/')
        && !v.starts_with("~/")
        && !v.starts_with("http://")
        && !v.starts_with("https://")
        && !v.contains(char::is_whitespace)
}

// ---------------------------------------------------------------------------
// Decisions (pure).
// ---------------------------------------------------------------------------

/// `owner/repo`, from a slug or a GitHub URL. Anything else is not a remote
/// whose visibility can be checked, and is refused.
pub fn github_slug(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let tail = r
        .strip_prefix("https://github.com/")
        .or_else(|| r.strip_prefix("git@github.com:"))
        .or_else(|| r.strip_prefix("ssh://git@github.com/"))
        .unwrap_or(r);
    let mut parts = tail.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    let ok = |s: &str| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (parts.next().is_none() && ok(owner) && ok(repo) && !tail.contains(':'))
        .then(|| format!("{owner}/{repo}"))
}

/// Where a rotated file lives in the archive: `logs/<year>/<name>`.
pub fn archive_path(rotated: &str) -> Option<String> {
    let (_, date) = log_rotate::parse_rotated(rotated)?;
    Some(format!("logs/{}/{rotated}", date.year()))
}

/// The hard guard: a staged path must be exactly an archived rotation.
/// Everything else — the database, `.env`, a live log, a stray file — is a
/// reason to abort, not to skip.
pub fn is_allowed_path(path: &str) -> bool {
    let mut parts = path.split('/');
    let (Some("logs"), Some(year), Some(name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    year.len() == 4
        && year.bytes().all(|b| b.is_ascii_digit())
        && archive_path(name).as_deref() == Some(path)
}

/// Why `text` must not be pushed, if it must not.
pub fn secret_reason(text: &[u8], secrets: &[String]) -> Option<&'static str> {
    let contains = |needle: &[u8]| !needle.is_empty() && text.windows(needle.len()).any(|w| w == needle);
    if secrets.iter().any(|s| s.len() >= MIN_SECRET_LEN && contains(s.as_bytes())) {
        return Some("holds one of this process's secret values");
    }
    CREDENTIAL_PREFIXES
        .iter()
        .any(|p| contains(p.as_bytes()))
        .then_some("holds what looks like a credential")
}

// ---------------------------------------------------------------------------
// Sync.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Outcome {
    pub file: String,
    pub action: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let (name, email) = git_identity();
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={email}")])
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Same identity knobs as `wiki sync`.
fn git_identity() -> (String, String) {
    (
        std::env::var("AUGMENTAGENT_GIT_AUTHOR_NAME").unwrap_or_else(|_| "AugmentAgent".into()),
        std::env::var("AUGMENTAGENT_GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "augmentagent@localhost".into()),
    )
}

/// `PRIVATE`, `PUBLIC`, … as GitHub reports it.
fn visibility(gh_bin: &str, slug: &str) -> Result<String> {
    let out = Command::new(gh_bin)
        .args(["repo", "view", slug, "--json", "visibility", "-q", ".visibility"])
        .output()
        .with_context(|| format!("run {gh_bin} repo view"))?;
    if !out.status.success() {
        bail!("could not read {slug}'s visibility: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn gunzip(path: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path)?).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Archive every rotation not yet in the repo, push, then expire local ones.
pub fn sync(cfg: &Config, dry_run: bool) -> Result<Vec<Outcome>> {
    let slug = github_slug(&cfg.remote).with_context(|| {
        format!("`{}` is not a GitHub repo (owner/repo): its visibility cannot be checked", cfg.remote)
    })?;
    if !cfg.repo_dir.join(".git").exists() {
        bail!(
            "{} is not a git repo yet; run scripts/ops-archive-bootstrap.sh once",
            cfg.repo_dir.display()
        );
    }
    // Before anything is even staged: the files hold message content.
    let vis = visibility(&cfg.gh_bin, &slug)?;
    if vis != "PRIVATE" {
        bail!("REFUSING: {slug} is {vis}, not PRIVATE. The ops archive must never be public.");
    }

    let mut rotated: Vec<String> = std::fs::read_dir(&cfg.state_dir)
        .with_context(|| format!("read {}", cfg.state_dir.display()))?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| log_rotate::parse_rotated(n).is_some())
        .collect();
    rotated.sort();

    let mut out = Vec::new();
    let mut staged: Vec<String> = Vec::new();
    for name in &rotated {
        let rel = archive_path(name).expect("parse_rotated accepted it");
        let dst = cfg.repo_dir.join(&rel);
        if dst.exists() {
            continue;
        }
        let src = cfg.state_dir.join(name);
        let size = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
        let skip = if size > MAX_FILE_BYTES {
            Some("larger than a git host accepts".to_string())
        } else {
            match gunzip(&src) {
                Ok(text) => secret_reason(&text, &cfg.secrets).map(String::from),
                Err(e) => Some(format!("unreadable: {e}")),
            }
        };
        if let Some(why) = skip {
            out.push(Outcome { file: name.clone(), action: format!("skipped: {why}") });
            continue;
        }
        if dry_run {
            out.push(Outcome { file: name.clone(), action: format!("would archive to {rel}") });
            continue;
        }
        std::fs::create_dir_all(dst.parent().expect("rel has a parent"))?;
        std::fs::copy(&src, &dst).with_context(|| format!("copy {name}"))?;
        staged.push(rel.clone());
        out.push(Outcome { file: name.clone(), action: format!("archived to {rel}") });
    }
    if dry_run {
        return Ok(out);
    }

    if !staged.is_empty() {
        let mut add = vec!["add", "--"];
        add.extend(staged.iter().map(String::as_str));
        git(&cfg.repo_dir, &add)?;
        // The hard guard, over what git actually staged.
        let listed = git(&cfg.repo_dir, &["diff", "--cached", "--name-only"])?;
        if let Some(bad) = listed.lines().find(|p| !is_allowed_path(p)) {
            let _ = git(&cfg.repo_dir, &["reset", "-q"]);
            bail!("REFUSING: `{bad}` is staged and is not an archived rotation; nothing was committed");
        }
        git(
            &cfg.repo_dir,
            &["commit", "-q", "-m", &format!("archive: {} rotated file(s), {}", staged.len(), cfg.today)],
        )?;
    }
    // Also pushes a commit an earlier, interrupted run left behind.
    git(&cfg.repo_dir, &["push", "-q", "origin", "HEAD"])?;

    // Past local retention and either safely archived or deliberately not
    // archivable: either way the local copy has served its time.
    for name in &rotated {
        let (_, date) = log_rotate::parse_rotated(name).expect("filtered above");
        if log_rotate::is_expired(date, cfg.today, cfg.keep_months)
            && std::fs::remove_file(cfg.state_dir.join(name)).is_ok()
        {
            out.push(Outcome { file: name.clone(), action: "deleted locally (past retention)".into() });
        }
    }
    Ok(out)
}

pub fn run_cli(dry_run: bool, json: bool, out: &mut dyn Write) -> Result<()> {
    let Some(remote) = remote_from_env() else {
        writeln!(out, "ops archive is off (AUGMENTAGENT_OPS_ARCHIVE_REMOTE is not set)")?;
        return Ok(());
    };
    let cfg = Config {
        remote,
        repo_dir: repo_dir_from_env().context("no HOME to locate the ops archive dir")?,
        state_dir: augmentagent_channel_core::state_dir::state_dir().context("no HOME to locate the state dir")?,
        gh_bin: std::env::var("AUGMENTAGENT_GH_BIN").unwrap_or_else(|_| "gh".into()),
        secrets: secrets_from_env(&crate::self_improve::env_name_is_secret),
        today: chrono::Utc::now().date_naive(),
        keep_months: log_rotate::Policy::from_env().keep_months,
    };
    let outcomes = sync(&cfg, dry_run)?;
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&outcomes)?)?;
        return Ok(());
    }
    for o in &outcomes {
        writeln!(out, "{}  {}", o.file, o.action)?;
    }
    if outcomes.is_empty() {
        writeln!(out, "nothing new to archive")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn a_remote_must_be_a_github_repo_whose_visibility_can_be_checked() {
        assert_eq!(github_slug("me/ops-archive").as_deref(), Some("me/ops-archive"));
        assert_eq!(github_slug("https://github.com/me/ops-archive.git").as_deref(), Some("me/ops-archive"));
        assert_eq!(github_slug("git@github.com:me/ops-archive.git").as_deref(), Some("me/ops-archive"));
        assert_eq!(github_slug("https://gitlab.com/me/ops"), None);
        assert_eq!(github_slug("/srv/git/ops.git"), None);
        assert_eq!(github_slug("me/ops; rm -rf ~"), None);
        assert_eq!(github_slug("me"), None);
    }

    #[test]
    fn only_an_archived_rotation_may_ever_be_staged() {
        assert_eq!(archive_path("stderr.log.20261006.gz").as_deref(), Some("logs/2026/stderr.log.20261006.gz"));
        assert!(is_allowed_path("logs/2026/stderr.log.20261006.gz"));
        assert!(is_allowed_path("logs/2026/token-usage.jsonl.20261006-2.gz"));
        for bad in [
            "data.db",
            ".env",
            "logs/2026/data.db",
            "logs/2026/stderr.log",
            "logs/2025/stderr.log.20261006.gz",
            "logs/2026/sub/stderr.log.20261006.gz",
            "stderr.log.20261006.gz",
            "logs/2026/../../.env",
        ] {
            assert!(!is_allowed_path(bad), "{bad} must be refused");
        }
    }

    #[test]
    fn identifiers_paths_and_urls_are_not_credentials() {
        // First live run: the daemon log was skipped because it contained the
        // Discord channel id, whose variable name contains DISCORD.
        assert!(!could_be_credential("1234567890123456789"), "a numeric id");
        assert!(!could_be_credential("/home/user/.config/app/creds.json"), "a path");
        assert!(!could_be_credential("~/.config/app/creds.json"));
        assert!(!could_be_credential("https://example.test/api/v1"), "an endpoint");
        assert!(!could_be_credential("claude,codex,gemini fallback chain"), "prose");
        assert!(!could_be_credential("short"));
        // Built at runtime: no credential-shaped literal in the source.
        assert!(could_be_credential(&"aB3x".repeat(6)), "long opaque text");
        assert!(could_be_credential(&format!("{}.{}", "Ab1".repeat(5), "cD2".repeat(4))));
    }

    #[test]
    fn a_file_with_a_secret_value_or_a_credential_prefix_is_not_pushed() {
        let secrets = vec!["s3cr3t-value-0123456789".to_string(), "short".to_string()];
        assert!(secret_reason(b"calling provider with s3cr3t-value-0123456789 ok", &secrets).is_some());
        // Built from the prefix table so no credential-shaped literal lives
        // in the source (the repo's own secret scan would rightly object).
        for prefix in CREDENTIAL_PREFIXES {
            let line = format!("Authorization: Bearer {prefix}EXAMPLE");
            assert!(secret_reason(line.as_bytes(), &[]).is_some(), "{prefix}");
        }
        assert!(secret_reason(b"a short word appears here", &secrets).is_none(), "too short to be a secret");
        assert!(secret_reason(b"poll complete channel=\"gmail\" fetched=3", &secrets).is_none());
    }

    // ---- end to end against a local bare remote and a stub `gh` ----

    struct Fixture {
        _tmp: tempfile::TempDir,
        cfg: Config,
        bare: PathBuf,
    }

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    }

    fn gz(path: &Path, text: &str) {
        let mut enc = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::fast());
        enc.write_all(text.as_bytes()).unwrap();
        enc.finish().unwrap();
    }

    fn fixture(visibility: &str) -> Fixture {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (bare, repo, state) = (root.join("remote.git"), root.join("archive"), root.join("state"));
        std::fs::create_dir_all(&state).unwrap();
        assert!(Command::new("git").args(["init", "-q", "--bare", "-b", "main"]).arg(&bare).status().unwrap().success());
        assert!(Command::new("git").args(["init", "-q", "-b", "main"]).arg(&repo).status().unwrap().success());
        sh(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        let gh = root.join("gh");
        std::fs::write(&gh, format!("#!/bin/sh\necho {visibility}\n")).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = Config {
            remote: "me/ops-archive".into(),
            repo_dir: repo,
            state_dir: state,
            gh_bin: gh.to_string_lossy().into_owned(),
            secrets: vec!["s3cr3t-value-0123456789".into()],
            today: d(2026, 10, 6),
            keep_months: 3,
        };
        Fixture { _tmp: tmp, cfg, bare }
    }

    fn pushed(f: &Fixture) -> Vec<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&f.bare)
            .args(["ls-tree", "-r", "--name-only", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect()
    }

    #[test]
    fn sync_pushes_clean_rotations_skips_secrets_and_never_takes_a_live_file() {
        let f = fixture("PRIVATE");
        let s = &f.cfg.state_dir;
        gz(&s.join("stderr.log.20260902.gz"), "poll complete\n");
        gz(&s.join("token-usage.jsonl.20260902.gz"), "{\"n\":1}\n");
        gz(&s.join("update.log.20260902.gz"), "token is s3cr3t-value-0123456789\n");
        gz(&s.join("stderr.log.20260502.gz"), "an old month\n");
        std::fs::write(s.join("stderr.log"), "the live log\n").unwrap();
        std::fs::write(s.join("data.db"), "not a log").unwrap();
        std::fs::write(s.join(".env"), "KEY=1").unwrap();

        let dry = sync(&f.cfg, true).unwrap();
        assert_eq!(dry.iter().filter(|o| o.action.starts_with("would archive")).count(), 3);
        assert!(pushed(&f).is_empty(), "a dry run pushes nothing");

        let done = sync(&f.cfg, false).unwrap();
        assert_eq!(
            pushed(&f),
            [
                "logs/2026/stderr.log.20260502.gz",
                "logs/2026/stderr.log.20260902.gz",
                "logs/2026/token-usage.jsonl.20260902.gz",
            ]
        );
        let skipped: Vec<_> = done.iter().filter(|o| o.action.starts_with("skipped")).collect();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].file, "update.log.20260902.gz");
        // Past retention: gone locally now that it is safely pushed.
        assert!(!s.join("stderr.log.20260502.gz").exists());
        assert!(s.join("stderr.log.20260902.gz").exists(), "inside retention: kept locally too");
        assert!(s.join("stderr.log").exists() && s.join("data.db").exists() && s.join(".env").exists());

        // Idempotent.
        let again = sync(&f.cfg, false).unwrap();
        assert!(again.iter().all(|o| o.action.starts_with("skipped")), "{again:?}");
        assert_eq!(pushed(&f).len(), 3);
    }

    #[test]
    fn a_public_remote_is_refused_before_anything_is_staged() {
        let f = fixture("PUBLIC");
        gz(&f.cfg.state_dir.join("stderr.log.20260902.gz"), "poll complete\n");
        let err = sync(&f.cfg, false).unwrap_err().to_string();
        assert!(err.contains("REFUSING") && err.contains("PUBLIC"), "{err}");
        assert!(pushed(&f).is_empty());
        assert!(!f.cfg.repo_dir.join("logs").exists(), "not even copied into the clone");
        // And so is a dry run: the answer to "would this push?" is no.
        assert!(sync(&f.cfg, true).is_err());
    }

    #[test]
    fn something_else_staged_in_the_clone_aborts_the_commit() {
        let f = fixture("PRIVATE");
        gz(&f.cfg.state_dir.join("stderr.log.20260902.gz"), "poll complete\n");
        std::fs::write(f.cfg.repo_dir.join("data.db"), "should never be here").unwrap();
        sh(&f.cfg.repo_dir, &["add", "data.db"]);
        let err = sync(&f.cfg, false).unwrap_err().to_string();
        assert!(err.contains("REFUSING") && err.contains("data.db"), "{err}");
        assert!(pushed(&f).is_empty(), "nothing was committed or pushed");
    }

    #[test]
    fn a_remote_that_is_not_github_or_a_clone_that_is_not_bootstrapped_is_refused() {
        let mut f = fixture("PRIVATE");
        f.cfg.remote = "/srv/git/ops.git".into();
        assert!(sync(&f.cfg, false).unwrap_err().to_string().contains("visibility cannot be checked"));
        let mut g = fixture("PRIVATE");
        g.cfg.repo_dir = g.cfg.state_dir.join("nope");
        assert!(sync(&g.cfg, false).unwrap_err().to_string().contains("ops-archive-bootstrap.sh"));
    }
}
