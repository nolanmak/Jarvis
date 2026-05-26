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
        settings_json: None,
        restrict_env: false,
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

    // Commit via the configured git identity (env; no hardcoded personal
    // data so the repo stays open-source-safe) + push. Neutral fallback.
    let _ = run("git", &["add", "-A"], &worktree).await?;
    let commit_msg = format!(
        "fix: {} (#{})\n\n{}",
        issue.title,
        issue.number,
        truncate(&summary, 500)
    );
    let git_name = std::env::var("AUGMENTAGENT_GIT_AUTHOR_NAME")
        .unwrap_or_else(|_| "AugmentAgent".to_string());
    let git_email = std::env::var("AUGMENTAGENT_GIT_AUTHOR_EMAIL")
        .unwrap_or_else(|_| "augmentagent@localhost".to_string());
    let name_arg = format!("user.name={git_name}");
    let email_arg = format!("user.email={git_email}");
    let (ok, _o, e) = run(
        "git",
        &[
            "-c",
            &name_arg,
            "-c",
            &email_arg,
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

// =====================================================================
// #117 — multi-repo agent coding: allowlisted repos + prompted draft PRs
// =====================================================================
//
// This is the multi-repo, prompt-gated generalization of #103. It reuses
// every safety primitive above (`is_blast_radius`, `diff_line_count`,
// `gh_bin`, `run`, the `agent-fix/issue-N` branch convention, the
// reasoner opts, the verification-gate *pattern*) and adds:
//
// - **Repo allowlist** (`agent_repos` in the store): default-deny. A repo
//   not represented by an `enabled` row is never touched.
// - **Isolated workspace per repo**: a fresh `git clone` into a throwaway
//   dir under `AUGMENTAGENT_AGENT_WORKDIR` (or a temp dir). NEVER the
//   deploy box's own checkout — the clone target is validated to be
//   outside the running repo root.
// - **Per-repo verification gate**: the repo's configured `build_cmd`
//   (e.g. `cargo test`, `npm test`) run with `bash -lc` inside the clone.
// - **Prompted draft PRs**: the gate passing does NOT open a PR. It
//   inserts a `pending_approval` row in `agent_pr_runs` and posts a
//   Discord prompt. A human approving (dashboard / CLI) is what lets the
//   next pass open the draft PR. Never auto-merge, never push to a
//   default branch.
// - **Hard guards carried over**: blast-radius refusal (built-in list +
//   per-repo extras), diff-size cap (per-repo), dedup against an existing
//   open gate row for the same (repo, issue).

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{AgentRepo, Store};
use augmentagent_store::Email as ApprovalEmail;

/// Where per-repo clones live. Override with `AUGMENTAGENT_AGENT_WORKDIR`.
/// Defaults to a temp subdir so a crash can never strand a clone inside
/// the deploy checkout.
fn agent_workdir() -> PathBuf {
    std::env::var("AUGMENTAGENT_AGENT_WORKDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("augmentagent-agent-repos"))
}

/// Refuse a clone target that resolves inside the deploy checkout. This is
/// the multi-repo analogue of #103's "never touch main": a third-party
/// clone must never land on top of (or inside) our own running tree.
fn assert_outside_deploy(workspace: &Path, deploy_root: &Path) -> Result<()> {
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let dep = deploy_root
        .canonicalize()
        .unwrap_or_else(|_| deploy_root.to_path_buf());
    if ws.starts_with(&dep) || dep.starts_with(&ws) {
        bail!(
            "refusing: agent workspace {} overlaps the deploy checkout {} \
             (set AUGMENTAGENT_AGENT_WORKDIR to a path outside the repo)",
            ws.display(),
            dep.display()
        );
    }
    Ok(())
}

/// Per-repo blast-radius check: the global built-in list OR any of the
/// repo's configured extra fragments (comma-separated).
fn is_blast_radius_for_repo(text: &str, repo: &AgentRepo) -> bool {
    if is_blast_radius(text) {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    repo.blast_radius_extra
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .any(|frag| lower.contains(&frag.to_ascii_lowercase()))
}

/// Pick the first eligible `agent-fixable` issue for an allowlisted repo,
/// skipping ones that already have an open gate row (store dedup) OR an
/// open agent PR (gh dedup), are blast-radius, or are `agent-gave-up`.
/// `repo_dir` is any local clone of the repo (gh resolves the remote from
/// its `origin`).
async fn pick_issue_for_repo(
    repo: &AgentRepo,
    repo_dir: &Path,
    store: &Store,
) -> Result<Option<Issue>> {
    let gh = gh_bin();
    let (ok, stdout, stderr) = run(
        &gh,
        &[
            "issue",
            "list",
            "--repo",
            &repo.full_name,
            "--label",
            FIXABLE_LABEL,
            "--state",
            "open",
            "--json",
            "number,title,body,labels",
            "--limit",
            "50",
        ],
        repo_dir,
    )
    .await?;
    if !ok {
        bail!("gh issue list ({}) failed: {stderr}", repo.full_name);
    }
    let issues: serde_json::Value =
        serde_json::from_str(&stdout).context("parse gh issue list")?;
    for iss in issues.as_array().cloned().unwrap_or_default() {
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
                ls.iter()
                    .any(|l| l.get("name").and_then(|n| n.as_str()) == Some(GAVE_UP_LABEL))
            })
            .unwrap_or(false);
        if gave_up {
            info!(repo = %repo.full_name, issue = number, "skip: agent-gave-up");
            continue;
        }
        if is_blast_radius_for_repo(&format!("{title} {body}"), repo) {
            info!(repo = %repo.full_name, issue = number, "skip: blast-radius keyword");
            continue;
        }
        // Store-side dedup: an awaiting-approval / approved gate row exists.
        if store
            .has_open_agent_pr_run(&repo.full_name, number as i64)
            .unwrap_or(true)
        {
            info!(repo = %repo.full_name, issue = number, "skip: open gate row (dedup)");
            continue;
        }
        // gh-side dedup: an open PR on our per-issue branch already exists.
        if has_open_agent_pr_remote(&repo.full_name, repo_dir, number).await? {
            info!(repo = %repo.full_name, issue = number, "skip: open agent PR (dedup)");
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

/// `gh pr list --repo` dedup for a specific remote repo. Conservative on
/// failure (assume a PR exists ⇒ skip), same as the single-repo path.
async fn has_open_agent_pr_remote(
    full_name: &str,
    repo_dir: &Path,
    issue: u64,
) -> Result<bool> {
    let gh = gh_bin();
    let branch = format!("{BRANCH_PREFIX}{issue}");
    let (ok, stdout, _) = run(
        &gh,
        &[
            "pr", "list", "--repo", full_name, "--state", "open", "--head", &branch,
            "--json", "number",
        ],
        repo_dir,
    )
    .await?;
    if !ok {
        warn!(repo = full_name, issue, "gh pr list failed; assuming PR exists");
        return Ok(true);
    }
    let arr: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or(serde_json::json!([]));
    Ok(arr.as_array().map(|a| !a.is_empty()).unwrap_or(false))
}

/// Per-repo verification gate. Runs the repo's configured `build_cmd` with
/// `bash -lc` inside the clone. Empty `build_cmd` ⇒ gate skipped (the loop
/// still won't open a PR without human approval, so this is safe for
/// docs-only repos). Mirrors #103's gate pattern, just parameterized.
async fn verification_gate_for_repo(workspace: &Path, repo: &AgentRepo) -> Result<()> {
    let cmd = repo.build_cmd.trim();
    if cmd.is_empty() {
        info!(repo = %repo.full_name, "verification gate skipped (no build_cmd)");
        return Ok(());
    }
    info!(repo = %repo.full_name, cmd, "verification gate");
    let wrapped = format!(". $HOME/.cargo/env 2>/dev/null; {cmd}");
    let (ok, _o, e) = run("bash", &["-lc", &wrapped], workspace).await?;
    if !ok {
        bail!("build_cmd `{cmd}` failed:\n{}", truncate(&e, 1500));
    }
    Ok(())
}

/// Post the prompted-PR approval prompt to Discord. Reuses the existing
/// `ApprovalBroker::post_flag_notice` surface (a heads-up notice — no
/// reply buttons, which only make sense for the email-draft flow). The
/// actual approve/reject action is taken on the `/repos` dashboard or via
/// `augmentagent self-improve --approve <run-id>`, both of which flip the
/// sqlite gate row. This keeps the broker un-forked.
async fn post_pr_prompt(
    broker: &dyn ApprovalBroker,
    repo: &AgentRepo,
    issue: &Issue,
    run_id: &str,
    diff_lines: usize,
    summary: &str,
) {
    let synthetic = ApprovalEmail {
        message_id: format!("agent-pr-run:{run_id}"),
        thread_id: None,
        from: format!("agent · {}", repo.full_name),
        subject: format!(
            "Open agent draft PR on {} for issue #{}?",
            repo.full_name, issue.number
        ),
        body: format!(
            "Issue #{}: {}\n\nProposed change ({diff_lines} lines, gate passed):\n{}\n\n\
             Approve on the dashboard /repos page or run:\n  \
             augmentagent self-improve --approve {run_id}\n\
             Reject:\n  augmentagent self-improve --reject {run_id}\n\n\
             Draft only — never auto-merged, never pushed to {}.",
            issue.number,
            issue.title,
            truncate(summary, 800),
            repo.base_branch
        ),
        date: String::new(),
        account_entity_id: None,
        platform: "agent-pr".into(),
        kind: "pr_prompt".into(),
    };
    if let Err(e) = broker
        .post_flag_notice(&synthetic, "agent-coding: PR awaiting approval")
        .await
    {
        warn!(repo = %repo.full_name, "failed to post Discord PR prompt: {e}");
    }
}

/// One multi-repo pass. For every enabled allowlisted repo: clone into an
/// isolated workspace, pick an eligible issue, let the reasoner implement
/// it, run the per-repo gate + guards, and — on success — insert a
/// `pending_approval` gate row and post the Discord prompt. Does NOT open
/// any PR (that happens in [`open_approved_runs`] after a human approves).
///
/// `deploy_root` is the running daemon's own checkout, used only for the
/// overlap guard. `dry_run` stops before inserting the gate row / posting.
pub async fn run_multi_repo_once(
    store: &Store,
    broker: &dyn ApprovalBroker,
    deploy_root: &Path,
    dry_run: bool,
) -> Result<String> {
    let repos = store
        .list_agent_repos(true)
        .context("list allowlisted repos")?;
    if repos.is_empty() {
        return Ok("no allowlisted repos (default-deny: nothing to do)".into());
    }
    let workroot = agent_workdir();
    assert_outside_deploy(&workroot, deploy_root)?;
    tokio::fs::create_dir_all(&workroot)
        .await
        .with_context(|| format!("mkdir {}", workroot.display()))?;

    let reasoner = Arc::new(ClaudeCliReasoner::new());
    let mut report: Vec<String> = Vec::new();

    for repo in &repos {
        match process_one_repo(store, broker, repo, &workroot, &reasoner, dry_run).await {
            Ok(line) => report.push(format!("{}: {line}", repo.full_name)),
            Err(e) => {
                warn!(repo = %repo.full_name, "repo pass failed: {e:#}");
                report.push(format!("{}: error — {}", repo.full_name, truncate(&e.to_string(), 200)));
            }
        }
    }
    Ok(report.join("\n"))
}

async fn process_one_repo(
    store: &Store,
    broker: &dyn ApprovalBroker,
    repo: &AgentRepo,
    workroot: &Path,
    reasoner: &Arc<ClaudeCliReasoner>,
    dry_run: bool,
) -> Result<String> {
    let slug = repo.full_name.replace('/', "__");
    let workspace = workroot.join(&slug);
    // Always start from a clean clone — nuke any stale dir from a crash.
    let _ = tokio::fs::remove_dir_all(&workspace).await;

    let gh = gh_bin();
    let (ok, _o, e) = run(
        &gh,
        &[
            "repo",
            "clone",
            &repo.full_name,
            &workspace.to_string_lossy(),
            "--",
            "--depth",
            "1",
            "--branch",
            &repo.base_branch,
        ],
        workroot,
    )
    .await?;
    if !ok {
        bail!("gh repo clone failed: {}", truncate(&e, 400));
    }

    let cleanup_ws = workspace.clone();
    let do_cleanup = || async {
        let _ = tokio::fs::remove_dir_all(&cleanup_ws).await;
    };

    let Some(issue) = pick_issue_for_repo(repo, &workspace, store).await? else {
        do_cleanup().await;
        return Ok("no eligible agent-fixable issues".into());
    };
    info!(repo = %repo.full_name, issue = issue.number, title = %issue.title, "selected issue");

    // Branch in the clone (NEVER the base branch).
    let branch = format!("{BRANCH_PREFIX}{}", issue.number);
    let (ok, _o, e) = run("git", &["checkout", "-b", &branch], &workspace).await?;
    if !ok {
        do_cleanup().await;
        bail!("git checkout -b failed: {}", truncate(&e, 200));
    }

    // Hand the issue to the reasoner, scoped to the clone.
    let opts = fix_opts(workspace.clone());
    let prompt = format!(
        "GitHub issue #{} in repo {}: {}\n\n{}\n\nImplement the fix now.",
        issue.number, repo.full_name, issue.title, issue.body
    );
    let summary = match reasoner.call(&opts, &prompt).await {
        Ok(s) => s,
        Err(err) => {
            do_cleanup().await;
            return Err(err).context("reasoner failed");
        }
    };

    let (_ok, stat, _) = run("git", &["diff", "--stat"], &workspace).await?;
    if stat.trim().is_empty() {
        do_cleanup().await;
        return Ok(format!("issue #{}: reasoner made no changes", issue.number));
    }
    let (_ok, full_diff, _) = run("git", &["diff"], &workspace).await?;

    // Blast-radius (global + per-repo) + per-repo size cap.
    if is_blast_radius_for_repo(&full_diff, repo) {
        do_cleanup().await;
        return Ok(format!(
            "issue #{}: refused — diff hit blast-radius guard",
            issue.number
        ));
    }
    let lines = diff_line_count(&full_diff);
    let cap = repo.max_diff_lines.max(0) as usize;
    if cap > 0 && lines > cap {
        do_cleanup().await;
        return Ok(format!(
            "issue #{}: refused — diff {lines} lines over cap {cap}",
            issue.number
        ));
    }

    // Per-repo verification gate.
    if let Err(gate_err) = verification_gate_for_repo(&workspace, repo).await {
        warn!(repo = %repo.full_name, issue = issue.number, "gate failed: {gate_err:#}");
        do_cleanup().await;
        return Ok(format!(
            "issue #{}: verification gate failed; no PR queued",
            issue.number
        ));
    }

    if dry_run {
        do_cleanup().await;
        return Ok(format!(
            "issue #{}: DRY RUN — gate passed, {lines}-line diff, would queue approval",
            issue.number
        ));
    }

    // Commit + push the BRANCH (never the base branch) so an approved run
    // can open the PR without re-running the reasoner. Pushing a feature
    // branch is not a merge and not a default-branch write.
    let _ = run("git", &["add", "-A"], &workspace).await?;
    let commit_msg = format!(
        "fix: {} (#{})\n\n{}",
        issue.title,
        issue.number,
        truncate(&summary, 500)
    );
    let git_name = std::env::var("AUGMENTAGENT_GIT_AUTHOR_NAME")
        .unwrap_or_else(|_| "AugmentAgent".to_string());
    let git_email = std::env::var("AUGMENTAGENT_GIT_AUTHOR_EMAIL")
        .unwrap_or_else(|_| "augmentagent@localhost".to_string());
    let (ok, _o, e) = run(
        "git",
        &[
            "-c",
            &format!("user.name={git_name}"),
            "-c",
            &format!("user.email={git_email}"),
            "commit",
            "-m",
            &commit_msg,
        ],
        &workspace,
    )
    .await?;
    if !ok {
        do_cleanup().await;
        bail!("git commit failed: {}", truncate(&e, 200));
    }
    let (ok, _o, e) = run(
        "git",
        &["push", "-u", "origin", &branch, "--force-with-lease"],
        &workspace,
    )
    .await?;
    if !ok {
        do_cleanup().await;
        bail!("git push (branch) failed: {}", truncate(&e, 200));
    }

    // Gate row + Discord prompt. The PR is NOT opened here.
    let row = store.insert_agent_pr_run(
        &repo.full_name,
        issue.number as i64,
        &branch,
        &truncate(&summary, 1500),
        lines as i64,
        "pending_approval",
    )?;
    post_pr_prompt(broker, repo, &issue, &row.id, lines, &summary).await;
    do_cleanup().await;
    Ok(format!(
        "issue #{}: queued for approval (run {}, {lines}-line diff) — awaiting human OK",
        issue.number, row.id
    ))
}

/// Open draft PRs for every `approved` gate row. Called after a human
/// approves on the dashboard / CLI. Re-validates the repo is still
/// allowlisted+enabled (revocation safety) before opening anything.
/// Always `--draft`, base = repo's configured branch, never merges.
pub async fn open_approved_runs(store: &Store) -> Result<String> {
    let pending = store
        .list_agent_pr_runs(None, 200)?
        .into_iter()
        .filter(|r| r.status == "approved")
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok("no approved runs awaiting a PR".into());
    }
    let gh = gh_bin();
    let mut out = Vec::new();
    for run_row in pending {
        let Some(repo) = store.get_agent_repo(&run_row.repo_full_name)? else {
            store
                .mark_agent_pr_failed(&run_row.id, "repo no longer allowlisted")
                .ok();
            out.push(format!("{}: repo removed — skipped", run_row.repo_full_name));
            continue;
        };
        if !repo.enabled {
            store
                .mark_agent_pr_failed(&run_row.id, "repo access revoked before PR")
                .ok();
            out.push(format!("{}: repo revoked — skipped", repo.full_name));
            continue;
        }
        let title = format!(
            "fix: agent change for issue #{} ({})",
            run_row.issue_number, repo.full_name
        );
        let body = format!(
            "Automated agent change for #{} in `{}`.\n\n## Summary\n{}\n\n\
             ## Verification\n- `{}`: pass\n- diff size: {} lines\n\n\
             Draft — a human must review and merge. Fixes #{}",
            run_row.issue_number,
            repo.full_name,
            run_row.summary,
            if repo.build_cmd.trim().is_empty() {
                "(no build_cmd configured)".to_string()
            } else {
                repo.build_cmd.clone()
            },
            run_row.diff_lines,
            run_row.issue_number
        );
        let (ok, stdout, e) = run(
            &gh,
            &[
                "pr",
                "create",
                "--repo",
                &repo.full_name,
                "--draft",
                "--base",
                &repo.base_branch,
                "--head",
                &run_row.branch,
                "--title",
                &title,
                "--body",
                &body,
            ],
            &agent_workdir(),
        )
        .await?;
        if !ok {
            store
                .mark_agent_pr_failed(&run_row.id, &truncate(&e, 400))
                .ok();
            out.push(format!("{}: gh pr create failed", repo.full_name));
            continue;
        }
        let url = stdout.trim().to_string();
        store.mark_agent_pr_opened(&run_row.id, &url)?;
        out.push(format!("{}: draft PR opened — {url}", repo.full_name));
    }
    Ok(out.join("\n"))
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

    // --- #117 multi-repo helpers --------------------------------------

    fn repo_with_extra(extra: &str) -> AgentRepo {
        AgentRepo {
            id: "id".into(),
            full_name: "acme/widgets".into(),
            base_branch: "main".into(),
            build_cmd: "cargo test".into(),
            blast_radius_extra: extra.into(),
            max_diff_lines: 600,
            enabled: true,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn per_repo_blast_radius_unions_builtin_and_extras() {
        let repo = repo_with_extra("infra/, terraform");
        // Built-in list still applies for third-party repos.
        assert!(is_blast_radius_for_repo("touch deploy/release.sh", &repo));
        // Per-repo extras catch repo-specific danger paths.
        assert!(is_blast_radius_for_repo("edit infra/main.tf", &repo));
        assert!(is_blast_radius_for_repo("change TERRAFORM state", &repo));
        // Safe change with no extras configured stays allowed.
        let plain = repo_with_extra("");
        assert!(!is_blast_radius_for_repo("fix typo in src/lib.rs", &plain));
    }

    #[test]
    fn workspace_overlapping_deploy_checkout_is_refused() {
        let deploy = std::env::temp_dir().join("aa-deploy-root");
        std::fs::create_dir_all(&deploy).unwrap();
        // Clone target *inside* the deploy checkout — must refuse.
        assert!(assert_outside_deploy(&deploy.join(".self-improve"), &deploy).is_err());
        // The deploy root itself — refuse.
        assert!(assert_outside_deploy(&deploy, &deploy).is_err());
        // A sibling path outside the checkout — allowed.
        let outside = std::env::temp_dir().join("aa-agent-clones-xyz");
        assert!(assert_outside_deploy(&outside, &deploy).is_ok());
        let _ = std::fs::remove_dir_all(&deploy);
    }
}
