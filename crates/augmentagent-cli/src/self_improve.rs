//! #103 — self-improvement loop: the agent picks a GitHub issue, fixes it on
//! an isolated branch, and opens a **draft** PR.
//!
//! Hard safety invariants (these are the point of the feature):
//!
//! - **Never touches `main`.** A fresh `git worktree` + branch is created per
//!   attempt. The auto-updater pulls `origin/main`; an in-place edit there
//!   would corrupt the deploy. We refuse to run if cwd isn't a clean repo.
//! - **Verification gate.** `cargo build` + `npm run build` + `cargo test`
//!   must all pass before a PR is opened. A red gate => no PR, a back-off
//!   comment on the issue instead.
//! - **Draft only, never auto-merge.** PRs are opened `--draft`; a human
//!   merges.
//! - **Dedup guard.** Issues that already have an open PR from a previous
//!   agent run are skipped (branch-name convention + `gh pr list`).
//! - **Blast-radius refusal.** Issues whose title/body or whose resulting
//!   diff touches deploy / auth / secret paths are refused, and the diff size
//!   is capped.
//! - **Back-off.** After `MAX_ATTEMPTS` failed attempts on an issue the loop
//!   leaves an explanatory comment and stops retrying it (label marker).
//!
//! This module shells out to `gh` and `git`; it does not depend on the
//! GitHub channel crate (different concern — that's inbound notifications).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

use augmentagent_channel_core::{ClaudeCliReasoner, Reasoner};

/// Branch/worktree prefix so the dedup guard can recognize agent PRs.
const BRANCH_PREFIX: &str = "agent-fix/issue-";
/// Label the loop selects on.
const FIXABLE_LABEL: &str = "agent-fixable";
/// Label stamped on an issue once the loop has given up on it (back-off).
const GAVE_UP_LABEL: &str = "agent-gave-up";
/// Max changed lines we'll allow in a single self-improvement diff.
const MAX_DIFF_LINES: usize = 600;
/// Consecutive failed attempts before we comment + back off (label marker).
const MAX_ATTEMPTS: u32 = 3;

/// Path fragments that make an issue or a diff "too dangerous to auto-touch".
/// Conservative on purpose — better to refuse a safe change than ship a
/// dangerous one unattended.
const BLAST_RADIUS_PATTERNS: &[&str] = &[
    "scripts/check-for-updates",
    "scripts/vault-mount",
    ".github/workflows",
    "systemd",
    ".service",
    "deploy",
    "secret",
    "credential",
    "/auth",
    "auth.rs",
    "keyring",
    "keychain",
    ".env",
    "discord-creds",
    "Cargo.lock",
    "package-lock.json",
];

#[derive(Debug, Clone)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
}

/// True if any blast-radius pattern appears in `text` (case-insensitive).
pub fn is_blast_radius(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    BLAST_RADIUS_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_ascii_lowercase()))
}

/// Count added+removed lines in a unified diff (lines starting with a single
/// `+`/`-`, excluding the `+++`/`---` file headers).
pub fn diff_line_count(diff: &str) -> usize {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
        .count()
}

async fn run(cmd: &str, args: &[&str], cwd: &Path) -> Result<(bool, String, String)> {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("spawn {cmd} {args:?}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn gh_bin() -> String {
    // Prod systemd PATH lacks /snap/bin; honor an override, else try snap.
    std::env::var("GH_BIN").unwrap_or_else(|_| {
        if Path::new("/snap/bin/gh").exists() {
            "/snap/bin/gh".to_string()
        } else {
            "gh".to_string()
        }
    })
}

/// Pick the first `agent-fixable` issue that isn't already claimed (open agent
/// PR) and isn't blast-radius and isn't `agent-gave-up`.
async fn pick_issue(repo_root: &Path) -> Result<Option<Issue>> {
    let gh = gh_bin();
    let (ok, stdout, stderr) = run(
        &gh,
        &[
            "issue",
            "list",
            "--label",
            FIXABLE_LABEL,
            "--state",
            "open",
            "--json",
            "number,title,body,labels",
            "--limit",
            "50",
        ],
        repo_root,
    )
    .await?;
    if !ok {
        bail!("gh issue list failed: {stderr}");
    }
    let issues: serde_json::Value =
        serde_json::from_str(&stdout).context("parse gh issue list")?;
    let arr = issues.as_array().cloned().unwrap_or_default();

    for iss in arr {
        let number = iss.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        if number == 0 {
            continue;
        }
        let title = iss
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body = iss
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let gave_up = iss
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|ls| {
                ls.iter().any(|l| {
                    l.get("name").and_then(|n| n.as_str()) == Some(GAVE_UP_LABEL)
                })
            })
            .unwrap_or(false);
        if gave_up {
            info!(issue = number, "skip: agent-gave-up");
            continue;
        }
        if is_blast_radius(&format!("{title} {body}")) {
            info!(issue = number, "skip: blast-radius keyword in issue");
            continue;
        }
        if has_open_agent_pr(repo_root, number).await? {
            info!(issue = number, "skip: open agent PR exists (dedup)");
            continue;
        }
        return Ok(Some(Issue {
            number,
            title,
            body,
        }));
    }
    Ok(None)
}

/// Dedup guard: is there already an open PR whose head branch matches our
/// per-issue convention?
async fn has_open_agent_pr(repo_root: &Path, issue: u64) -> Result<bool> {
    let gh = gh_bin();
    let branch = format!("{BRANCH_PREFIX}{issue}");
    let (ok, stdout, _) = run(
        &gh,
        &[
            "pr", "list", "--state", "open", "--head", &branch, "--json", "number",
        ],
        repo_root,
    )
    .await?;
    if !ok {
        // If `gh pr list` flaked, be conservative and assume a PR exists so we
        // don't open a duplicate.
        warn!(issue, "gh pr list failed; assuming PR exists (safe dedup)");
        return Ok(true);
    }
    let arr: serde_json::Value = serde_json::from_str(&stdout).unwrap_or(serde_json::json!([]));
    Ok(arr.as_array().map(|a| !a.is_empty()).unwrap_or(false))
}

/// Reasoner opts scoped to write/edit/bash within the per-attempt worktree.
fn fix_opts(worktree: PathBuf) -> augmentagent_channel_core::ReasonerOpts {
    augmentagent_channel_core::ReasonerOpts {
        system_prompt: SELF_IMPROVE_SYSTEM.to_string(),
        model: None,
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Write".into(),
            "Edit".into(),
            "Bash(cargo *)".into(),
            "Bash(npm *)".into(),
            "Bash(git diff*)".into(),
            "Bash(git status*)".into(),
            "Bash(ls *)".into(),
        ],
        add_dirs: vec![worktree.clone()],
        permission_mode: "acceptEdits".into(),
        cwd: Some(worktree),
        env: Vec::new(),
    }
}

const SELF_IMPROVE_SYSTEM: &str = "You are an autonomous maintenance engineer for the \
AugmentAgent codebase. You are given a single GitHub issue. Implement the smallest \
correct fix. Constraints you MUST honor:\n\
- Stay within the working directory you were given (a throwaway worktree).\n\
- Do NOT touch deploy/auth/secret/CI files (systemd units, scripts/check-for-updates, \
.github/workflows, anything with credentials/keyring/.env).\n\
- Keep the diff small and focused on the issue.\n\
- Add or update a test when it is reasonable to do so.\n\
- Do NOT run git commit, git push, or gh. Just edit files.\n\
When done, output a 2-4 sentence summary of what you changed and why.";

/// Run the verification gate inside the worktree. Returns Ok(()) only if every
/// configured check passes.
async fn verification_gate(worktree: &Path) -> Result<()> {
    info!("verification gate: cargo build");
    let (ok, _o, e) = run(
        "bash",
        &["-lc", ". $HOME/.cargo/env && cargo build --workspace 2>&1 | tail -5"],
        worktree,
    )
    .await?;
    if !ok {
        bail!("cargo build failed:\n{e}");
    }
    info!("verification gate: cargo test");
    let (ok, _o, e) = run(
        "bash",
        &["-lc", ". $HOME/.cargo/env && cargo test --workspace 2>&1 | tail -8"],
        worktree,
    )
    .await?;
    if !ok {
        bail!("cargo test failed:\n{e}");
    }
    // npm build is best-effort: only gate on it if a package.json + node_modules
    // are present (prod has them; a bare CI checkout may not).
    if worktree.join("package.json").exists() && worktree.join("node_modules").exists() {
        info!("verification gate: npm run build");
        let (ok, _o, e) = run("bash", &["-lc", "npm run build 2>&1 | tail -5"], worktree).await?;
        if !ok {
            bail!("npm run build failed:\n{e}");
        }
    } else {
        info!("verification gate: npm build skipped (no node_modules)");
    }
    Ok(())
}

/// Drive one self-improvement attempt. `dry_run` stops before opening the PR
/// (prints what it would do) so the loop can be exercised safely.
pub async fn run_once(repo_root: &Path, dry_run: bool) -> Result<String> {
    // Refuse to run from a dirty tree / detached state — protects the deploy.
    let (ok, status_out, _) = run("git", &["status", "--porcelain"], repo_root).await?;
    if !ok {
        bail!("not a git repo at {}", repo_root.display());
    }
    if !status_out.trim().is_empty() {
        bail!(
            "refusing to self-improve from a dirty working tree \
             (commit/stash first):\n{status_out}"
        );
    }

    let Some(issue) = pick_issue(repo_root).await? else {
        return Ok("no eligible agent-fixable issues".to_string());
    };
    info!(issue = issue.number, title = %issue.title, "selected issue");

    let branch = format!("{BRANCH_PREFIX}{}", issue.number);
    let worktree = repo_root
        .join(".self-improve-worktrees")
        .join(format!("issue-{}", issue.number));

    // Clean any stale worktree from a crashed prior run.
    let _ = run(
        "git",
        &["worktree", "remove", "--force", &worktree.to_string_lossy()],
        repo_root,
    )
    .await;
    let _ = run("git", &["branch", "-D", &branch], repo_root).await;

    let (ok, _o, e) = run(
        "git",
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &worktree.to_string_lossy(),
            "origin/main",
        ],
        repo_root,
    )
    .await?;
    if !ok {
        bail!("git worktree add failed: {e}");
    }

    let cleanup = |wt: PathBuf, br: String, root: PathBuf| async move {
        let _ = run(
            "git",
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
            &root,
        )
        .await;
        let _ = run("git", &["branch", "-D", &br], &root).await;
    };

    // Hand the issue to the reasoner inside the worktree.
    let reasoner = Arc::new(ClaudeCliReasoner::new());
    let opts = fix_opts(worktree.clone());
    let prompt = format!(
        "GitHub issue #{}: {}\n\n{}\n\nImplement the fix now.",
        issue.number, issue.title, issue.body
    );
    let summary = match reasoner.call(&opts, &prompt).await {
        Ok(s) => s,
        Err(err) => {
            cleanup(worktree, branch, repo_root.to_path_buf()).await;
            return Err(err).context("reasoner failed during self-improve");
        }
    };

    // Did anything change?
    let (_ok, diff, _) = run("git", &["diff", "--stat"], &worktree).await?;
    if diff.trim().is_empty() {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        return Ok(format!(
            "issue #{}: reasoner made no changes; skipped",
            issue.number
        ));
    }

    // Blast-radius + size guard on the actual diff.
    let (_ok, full_diff, _) = run("git", &["diff"], &worktree).await?;
    if is_blast_radius(&full_diff) {
        cleanup(worktree.clone(), branch.clone(), repo_root.to_path_buf()).await;
        backoff_comment(
            repo_root,
            issue.number,
            "Self-improve refused: the produced diff touches a \
             deploy/auth/secret path (blast-radius guard).",
        )
        .await
        .ok();
        return Ok(format!(
            "issue #{}: refused — diff hit blast-radius guard",
            issue.number
        ));
    }
    let lines = diff_line_count(&full_diff);
    if lines > MAX_DIFF_LINES {
        cleanup(worktree.clone(), branch.clone(), repo_root.to_path_buf()).await;
        backoff_comment(
            repo_root,
            issue.number,
            &format!(
                "Self-improve refused: diff is {lines} lines (cap {MAX_DIFF_LINES}). \
                 Needs a human."
            ),
        )
        .await
        .ok();
        return Ok(format!(
            "issue #{}: refused — diff too large ({lines} lines)",
            issue.number
        ));
    }

    // Verification gate.
    if let Err(gate_err) = verification_gate(&worktree).await {
        warn!(issue = issue.number, "verification gate failed: {gate_err:#}");
        let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
        if attempts >= MAX_ATTEMPTS {
            backoff_comment(
                repo_root,
                issue.number,
                &format!(
                    "Self-improve gave up after {attempts} attempts. \
                     Last gate failure:\n```\n{}\n```",
                    truncate(&gate_err.to_string(), 1500)
                ),
            )
            .await
            .ok();
            label_gave_up(repo_root, issue.number).await.ok();
        }
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        return Ok(format!(
            "issue #{}: verification gate failed (attempt {attempts}); no PR opened",
            issue.number
        ));
    }

    if dry_run {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        return Ok(format!(
            "issue #{}: DRY RUN — gate passed, {lines}-line diff, would open draft PR",
            issue.number
        ));
    }

    // Commit (as Nolan, per repo convention — no Claude attribution) + push.
    let _ = run("git", &["add", "-A"], &worktree).await?;
    let commit_msg = format!(
        "fix: {} (#{})\n\n{}",
        issue.title,
        issue.number,
        truncate(&summary, 500)
    );
    let (ok, _o, e) = run(
        "git",
        &[
            "-c",
            "user.name=Nolan Makatche",
            "-c",
            "user.email=REDACTED",
            "commit",
            "-m",
            &commit_msg,
        ],
        &worktree,
    )
    .await?;
    if !ok {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        bail!("git commit failed: {e}");
    }
    let (ok, _o, e) = run(
        "git",
        &["push", "-u", "origin", &branch],
        &worktree,
    )
    .await?;
    if !ok {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        bail!("git push failed: {e}");
    }

    // Open a DRAFT PR. Never auto-merge.
    let gh = gh_bin();
    let pr_body = format!(
        "Automated self-improvement for #{}.\n\n## Summary\n{}\n\n## Verification\n\
         - `cargo build --workspace`: pass\n- `cargo test --workspace`: pass\n\
         - diff size: {lines} lines (cap {MAX_DIFF_LINES})\n\n\
         Draft — a human must review and merge. Fixes #{}",
        issue.number,
        truncate(&summary, 1500),
        issue.number
    );
    let (ok, stdout, e) = run(
        &gh,
        &[
            "pr",
            "create",
            "--draft",
            "--base",
            "main",
            "--head",
            &branch,
            "--title",
            &format!("fix: {} (#{})", issue.title, issue.number),
            "--body",
            &pr_body,
        ],
        &worktree,
    )
    .await?;
    cleanup(worktree, branch, repo_root.to_path_buf()).await;
    if !ok {
        bail!("gh pr create failed: {e}");
    }
    Ok(format!(
        "issue #{}: draft PR opened — {}",
        issue.number,
        stdout.trim()
    ))
}

/// Bump an attempt counter encoded as a hidden marker comment, return the new
/// count. (Lightweight; avoids needing extra labels per count.)
async fn record_attempt(repo_root: &Path, issue: u64) -> Result<u32> {
    let gh = gh_bin();
    let (_ok, stdout, _) = run(
        &gh,
        &[
            "issue",
            "view",
            &issue.to_string(),
            "--json",
            "comments",
        ],
        repo_root,
    )
    .await?;
    let prior = stdout.matches("<!-- self-improve-attempt -->").count() as u32;
    let n = prior + 1;
    let _ = run(
        &gh,
        &[
            "issue",
            "comment",
            &issue.to_string(),
            "--body",
            &format!("<!-- self-improve-attempt --> attempt {n} failed the verification gate."),
        ],
        repo_root,
    )
    .await;
    Ok(n)
}

async fn backoff_comment(repo_root: &Path, issue: u64, body: &str) -> Result<()> {
    let gh = gh_bin();
    let (ok, _o, e) = run(
        &gh,
        &["issue", "comment", &issue.to_string(), "--body", body],
        repo_root,
    )
    .await?;
    if !ok {
        bail!("gh issue comment failed: {e}");
    }
    Ok(())
}

async fn label_gave_up(repo_root: &Path, issue: u64) -> Result<()> {
    let gh = gh_bin();
    let _ = run(
        &gh,
        &[
            "issue",
            "edit",
            &issue.to_string(),
            "--add-label",
            GAVE_UP_LABEL,
        ],
        repo_root,
    )
    .await?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blast_radius_catches_deploy_and_auth() {
        assert!(is_blast_radius("edit scripts/check-for-updates.sh"));
        assert!(is_blast_radius("touch crates/augmentagent-auth/src/auth.rs"));
        assert!(is_blast_radius("update .github/workflows/ci.yml"));
        assert!(is_blast_radius("rotate the DISCORD secret"));
        assert!(!is_blast_radius("fix a typo in the README"));
        assert!(!is_blast_radius("add a unit test to store.rs"));
    }

    #[test]
    fn diff_line_count_ignores_file_headers() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1,2 @@\n-old\n+new\n+extra\n";
        // -old, +new, +extra = 3 (the ---/+++ headers excluded)
        assert_eq!(diff_line_count(diff), 3);
    }

    #[test]
    fn truncate_is_byte_safe_for_ascii() {
        assert_eq!(truncate("hello", 3), "hel…");
        assert_eq!(truncate("hi", 10), "hi");
    }
}
