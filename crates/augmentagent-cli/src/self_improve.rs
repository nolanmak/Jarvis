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
/// What [`run_once`] returns when no labeled issue is waiting. The #630
/// auto-PR loop matches on this to tell an idle tick (one cheap `gh issue
/// list`, no reasoner spend) from an engaged run (counts against the daily
/// cap because it spawned the claude CLI on the owner's subscription, #448).
const IDLE_MSG: &str = "no eligible agent-fixable issues";

/// #300 — Trust gate. Issue bodies are attacker-controllable now that the
/// repo is public, and the reasoner is granted Write/Edit/Bash(cargo/npm)
/// with the verification gate running `build.rs`/proc-macros/`npm postinstall`
/// on the host. To keep untrusted text out of that pipeline, only issues
/// authored by a trusted login auto-select into a run. Everyone else is
/// refused at selection time and routed through the explicit
/// `pending_approval` -> owner-OK flow (the multi-repo path already
/// implements that handshake).
///
/// Trusted authors come from `AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS`
/// (comma-separated GitHub logins, case-insensitive). If unset, the gate
/// falls back to the repo owner (`AUGMENTAGENT_GH_OWNER`, else the owner
/// segment parsed from the repo's `origin` remote). GitHub `authorAssociation`
/// of `OWNER` / `MEMBER` / `COLLABORATOR` (write-access roles) is also
/// honored so the owner doesn't have to enumerate every collaborator.
const TRUSTED_AUTHORS_ENV: &str = "AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS";
const GH_OWNER_ENV: &str = "AUGMENTAGENT_GH_OWNER";

/// Provider/API-key env vars stripped from the build/test gate's child
/// process (#300). The gate compiles + tests attacker-influenceable code
/// (`build.rs`, proc-macros, `npm postinstall`); none of those need a
/// provider key, so clearing them is behavior-preserving for honest fixes
/// while denying secret exfiltration to a hostile build script. Matched as
/// a prefix/suffix denylist over the inherited env (see [`gate_env`]).
const GATE_SECRET_ENV_SUBSTRINGS: &[&str] = &[
    "OPENAI",
    "ANTHROPIC",
    "CLAUDE",
    "COMPOSIO",
    "GROQ",
    "CEREBRAS",
    "SOCIALAPI",
    "DISCORD",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "AWS_",
    "GCP_",
    "GOOGLE_",
    "SECRET",
    "TOKEN",
    "API_KEY",
    "APIKEY",
    "PASSWORD",
    "CREDENTIAL",
];

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
    /// GitHub login of the issue author (#300). Empty when `gh` did not
    /// return an author (treated as untrusted).
    pub author: String,
    /// Whether [`author`](Self::author) passed the #300 trust gate at
    /// selection time. `false` ⇒ the body is attacker-controllable and the
    /// run must require explicit owner approval before any host-side build
    /// or branch push.
    pub author_trusted: bool,
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

/// #300 — Compute the sanitized env for the build/test verification gate.
///
/// The gate compiles + tests code that may have been influenced by an
/// attacker-controlled issue body (`build.rs`, proc-macros, `npm
/// postinstall` all execute during `cargo build`/`npm run build`). None of
/// those need provider/API secrets, so we strip every env var whose name
/// matches a [`GATE_SECRET_ENV_SUBSTRINGS`] token before spawning. This
/// returns the FULL allowed env (cleared of secrets) so the build still has
/// `PATH`, `HOME`, `CARGO_*`, etc. — behavior-preserving for honest fixes,
/// secret-denying for hostile ones.
fn gate_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| !env_name_is_secret(k))
        .collect()
}

/// True if an env-var name looks like a provider secret (case-insensitive
/// substring match against [`GATE_SECRET_ENV_SUBSTRINGS`]).
fn env_name_is_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    GATE_SECRET_ENV_SUBSTRINGS
        .iter()
        .any(|frag| upper.contains(frag))
}

/// Run a command with a sandboxed env: the child's environment is CLEARED
/// and repopulated from [`gate_env`] (full inherited env minus provider
/// secrets). Used for the build/test gate so a hostile `build.rs` /
/// `postinstall` cannot read provider API keys out of the daemon env.
//
// SECURITY: This "sandbox" is env-isolation only, NOT a full sandbox.
//   - What it DOES: clears the inherited environment and re-injects only
//     non-secret vars (`gate_env`), so a hostile build/test script
//     (`build.rs`, proc-macro, `npm` `postinstall`, a repo's `build_cmd`)
//     cannot read provider API keys / tokens out of the daemon's process env
//     and exfiltrate them.
//   - What it does NOT do: it does NOT block network egress. The child can
//     still open arbitrary outbound sockets in-process; a hostile script
//     could fetch a payload or POST data it scrapes from the checkout. There
//     is no in-process primitive that prevents this. Full isolation requires
//     running this gate inside a container or a network namespace (e.g. an
//     ephemeral, no-network sandbox) — deliberately out of scope here.
//   - PRIMARY defense is upstream, not here: untrusted-authored issues are
//     refused before reaching this gate (#300 trust gate), and the multi-repo
//     path operates on an ephemeral throwaway clone. Secret-stripping is the
//     defense-in-depth control for the honest-fix path, not the sole barrier
//     against a hostile author.
async fn run_sandboxed(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(String, String)],
) -> Result<(bool, String, String)> {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .await
        .with_context(|| format!("spawn (sandboxed) {cmd} {args:?}"))?;
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

/// #300 — Parse the configured trusted-author allowlist. Comma-separated
/// logins from `AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS`, normalized to
/// lowercase. Falls back to the repo owner (`AUGMENTAGENT_GH_OWNER`, else
/// the owner segment of the repo's `origin` remote) so a single-owner
/// install needs zero configuration.
async fn trusted_authors(repo_root: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::env::var(TRUSTED_AUTHORS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if let Ok(owner) = std::env::var(GH_OWNER_ENV) {
        let owner = owner.trim().to_ascii_lowercase();
        if !owner.is_empty() && !out.contains(&owner) {
            out.push(owner);
        }
    }
    if out.is_empty() {
        if let Some(owner) = repo_owner_from_remote(repo_root).await {
            out.push(owner);
        }
    }
    out
}

/// Best-effort: parse the `owner` segment of the repo's `origin` remote URL
/// (handles both `git@github.com:owner/repo.git` and
/// `https://github.com/owner/repo` forms). Used only as the trust-gate
/// fallback when no explicit trusted-author config is present.
async fn repo_owner_from_remote(repo_root: &Path) -> Option<String> {
    let (ok, url, _) = run(
        "git",
        &["config", "--get", "remote.origin.url"],
        repo_root,
    )
    .await
    .ok()?;
    if !ok {
        return None;
    }
    let url = url.trim().trim_end_matches(".git");
    // Strip protocol/host: keep everything after the last ':' or the
    // "host/" boundary, then take the leading path segment as the owner.
    let path = url
        .rsplit_once("github.com")
        .map(|(_, rest)| rest.trim_start_matches([':', '/']))
        .unwrap_or(url);
    let owner = path.split('/').next().unwrap_or("").trim();
    if owner.is_empty() {
        None
    } else {
        Some(owner.to_ascii_lowercase())
    }
}

/// #300 — Decide whether an issue's author is trusted for unattended
/// selection. An author is trusted when EITHER its login is on the
/// configured allowlist OR GitHub reports a write-access `authorAssociation`
/// (`OWNER` / `MEMBER` / `COLLABORATOR`). Default-deny: an empty login or an
/// unknown association is untrusted.
fn author_is_trusted(login: &str, association: &str, allowlist: &[String]) -> bool {
    let login = login.trim().to_ascii_lowercase();
    if login.is_empty() {
        return false;
    }
    if allowlist.iter().any(|a| a == &login) {
        return true;
    }
    matches!(
        association.trim().to_ascii_uppercase().as_str(),
        "OWNER" | "MEMBER" | "COLLABORATOR"
    )
}

/// Pick the first `agent-fixable` issue that isn't already claimed (open agent
/// PR) and isn't blast-radius and isn't `agent-gave-up`.
///
/// #300 trust gate: the returned [`Issue::author_trusted`] flag records
/// whether the author passed the trust check. Untrusted-authored issues are
/// still returned (so the loop can route them through owner approval rather
/// than silently swallowing them) but are NEVER auto-built or auto-pushed by
/// [`run_once`].
async fn pick_issue(repo_root: &Path) -> Result<Option<Issue>> {
    let allowlist = trusted_authors(repo_root).await;
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
            "number,title,body,labels,author,authorAssociation",
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
        // #300 — capture the author + its write-access association so the
        // caller can trust-gate before any host-side build/push.
        let author = iss
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let association = iss
            .get("authorAssociation")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let author_trusted = author_is_trusted(&author, association, &allowlist);
        if !author_trusted {
            info!(
                issue = number,
                author = %author,
                association = %association,
                "untrusted issue author — requires owner approval before any build/push"
            );
        }
        return Ok(Some(Issue {
            number,
            title,
            body,
            author,
            author_trusted,
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
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
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
    // #300 — Strip provider secrets from the gate's child env. `cargo
    // build`/`npm run build` execute `build.rs`/proc-macros/`npm
    // postinstall`, which the issue body could influence; none need
    // provider keys, so this is behavior-preserving for honest fixes.
    let env = gate_env();
    info!("verification gate: cargo build (sandboxed env)");
    let (ok, _o, e) = run_sandboxed(
        "bash",
        &["-lc", ". $HOME/.cargo/env && cargo build --workspace 2>&1 | tail -5"],
        worktree,
        &env,
    )
    .await?;
    if !ok {
        bail!("cargo build failed:\n{e}");
    }
    info!("verification gate: cargo test (sandboxed env)");
    let (ok, _o, e) = run_sandboxed(
        "bash",
        &["-lc", ". $HOME/.cargo/env && cargo test --workspace 2>&1 | tail -8"],
        worktree,
        &env,
    )
    .await?;
    if !ok {
        bail!("cargo test failed:\n{e}");
    }
    // npm build is best-effort: only gate on it if a package.json + node_modules
    // are present (prod has them; a bare CI checkout may not).
    if worktree.join("package.json").exists() && worktree.join("node_modules").exists() {
        info!("verification gate: npm run build (sandboxed env)");
        let (ok, _o, e) =
            run_sandboxed("bash", &["-lc", "npm run build 2>&1 | tail -5"], worktree, &env)
                .await?;
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
        return Ok(IDLE_MSG.to_string());
    };
    info!(issue = issue.number, title = %issue.title, "selected issue");

    // #300 — Trust gate. If the issue author is not trusted (not the owner /
    // a write-access collaborator / an allowlisted login), the body is
    // attacker-controllable. Refuse to run the unattended build/push pipeline
    // on it: we never create a worktree, never invoke the reasoner on the
    // untrusted text, never run the host-side build/test gate, and never push
    // a branch. The owner must explicitly opt the issue in (allowlist the
    // author via AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS, or drive it through
    // the multi-repo `pending_approval` -> owner-OK flow). This preserves the
    // owner's own-issue happy path unchanged while closing the public-issue
    // RCE/exfil path.
    if !issue.author_trusted {
        warn!(
            issue = issue.number,
            author = %issue.author,
            "refusing unattended self-improve on untrusted-authored issue (#300 trust gate)"
        );
        backoff_comment(
            repo_root,
            issue.number,
            "Self-improve refused: this issue was not authored by a trusted \
             maintainer (owner / write-access collaborator / allowlisted \
             login). Because issue bodies are attacker-controllable, the \
             unattended fix pipeline (which runs `cargo build`/`npm` and pushes \
             a branch) will not run on it automatically. A maintainer can opt \
             this in by adding the author to \
             `AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS`, or by reviewing and \
             driving it through the approval flow.",
        )
        .await
        .ok();
        return Ok(format!(
            "issue #{}: refused — untrusted author '{}' (requires owner approval)",
            issue.number, issue.author
        ));
    }

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

/// Resolve symlinks for the portion of `path` that exists, then re-append any
/// trailing components that do not exist yet.
///
/// `Path::canonicalize` requires every component to exist. For overlap checks we
/// must normalize symlinks (so `/tmp` and `/private/tmp` compare equal on macOS)
/// even when the leaf (e.g. a not-yet-created checkout dir) is absent. We walk up
/// to the deepest existing ancestor, canonicalize that, and rebuild the path by
/// appending the remaining components verbatim. No filesystem entries are
/// created.
fn canonicalize_lexically(path: &Path) -> std::io::Result<PathBuf> {
    // Fast path: the whole path exists.
    if let Ok(resolved) = path.canonicalize() {
        return Ok(resolved);
    }

    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    loop {
        match cursor.canonicalize() {
            Ok(mut base) => {
                for component in tail.iter().rev() {
                    base.push(component);
                }
                return Ok(base);
            }
            Err(_) => {
                let file_name = cursor.file_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no existing ancestor for {}", path.display()),
                    )
                })?;
                tail.push(file_name.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no existing ancestor for {}", path.display()),
                    )
                })?;
            }
        }
    }
}

/// Refuse a clone target that resolves inside the deploy checkout. This is
/// the multi-repo analogue of #103's "never touch main": a third-party
/// clone must never land on top of (or inside) our own running tree.
fn assert_outside_deploy(workspace: &Path, deploy_root: &Path) -> Result<()> {
    // Canonicalize symlinks before the overlap check. The checkout leaf may not
    // exist yet, so we canonicalize the deepest existing ancestor and re-append
    // the missing components (see `canonicalize_lexically`). This is required
    // for correctness on macOS, where `/tmp` -> `/private/tmp`: canonicalizing
    // only the parent (or falling back to the raw path when the leaf is absent)
    // would leave one side as `/tmp/...` and the other as `/private/tmp/...`,
    // defeating `starts_with` and silently ALLOWING an overlapping checkout. We
    // fall back to the raw path only if no ancestor resolves at all (extremely
    // unlikely for an absolute path); the conservative `starts_with` overlap
    // test below still runs in that case.
    let ws = canonicalize_lexically(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let dep = canonicalize_lexically(deploy_root).unwrap_or_else(|_| deploy_root.to_path_buf());
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
            "number,title,body,labels,author",
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
        let author = iss
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // #300 — the multi-repo path already requires explicit
        // `pending_approval` -> owner-OK before any PR is opened, so the
        // author-trust flag isn't load-bearing here; record it for
        // observability but the approval handshake is the real gate.
        return Ok(Some(Issue {
            number,
            title,
            body,
            author,
            author_trusted: false,
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
    info!(repo = %repo.full_name, cmd, "verification gate (sandboxed env)");
    let wrapped = format!(". $HOME/.cargo/env 2>/dev/null; {cmd}");
    // #300 — same secret-stripping as the single-repo gate: the per-repo
    // `build_cmd` runs attacker-influenceable build scripts and must not
    // inherit provider keys.
    let env = gate_env();
    let (ok, _o, e) = run_sandboxed("bash", &["-lc", &wrapped], workspace, &env).await?;
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

// ---------------------------------------------------------------------------
// #630 — auto-PR daemon loop
// ---------------------------------------------------------------------------

/// #630 — in-daemon listener that closes the loop from "issue filed" to
/// "draft PR up for review": on an interval, run the #103 [`run_once`]
/// pipeline, which picks the next open `agent-fixable` issue and — behind
/// its existing guardrails (trust gate #300, blast-radius refusal, diff cap,
/// verification gate, per-issue attempt back-off, dedup vs open agent PRs,
/// draft-only, isolated worktree) — ships a draft PR referencing it.
///
/// Guardrails this loop adds on top:
/// - **Opt-in.** Spawned only when `AUGMENTAGENT_AUTOPR=1|true`. Every
///   engaged run spawns the claude CLI on the owner's Max subscription
///   (#448), so merging this feature must not silently start billing.
/// - **Label gate.** Eligibility stays `agent-fixable` — filing an issue is
///   not consent to auto-build it; labeling it is.
/// - **Serial + rate-limited.** One pipeline run at a time (single loop),
///   first tick a full interval after boot (the auto-updater bounces the
///   daemon on every deploy; a boot tick would burn cap on each restart),
///   and at most `AUGMENTAGENT_AUTOPR_DAILY_CAP` engaged runs per UTC day.
///   The counter is in-memory — a restart resets it — so the cap is
///   belt-and-suspenders on top of the label gate and per-issue back-off,
///   not the primary control.
/// - **Never auto-merge.** Inherited from `run_once`: PRs are draft, a
///   human merges. The PR body's `Fixes #N` cross-links it on the issue.
pub struct AutoPrLoop {
    repo_root: PathBuf,
    dry_run: bool,
    interval: std::time::Duration,
    daily_cap: u32,
}

/// Engaged-run counter with UTC-day rollover. Pure so it's testable.
#[derive(Default)]
struct DailyCounter {
    day: u64,
    runs: u32,
}

impl DailyCounter {
    fn runs_today(&mut self, day: u64) -> u32 {
        if day != self.day {
            self.day = day;
            self.runs = 0;
        }
        self.runs
    }

    fn record(&mut self, day: u64) {
        let _ = self.runs_today(day);
        self.runs += 1;
    }
}

fn utc_day_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

impl AutoPrLoop {
    const DEFAULT_INTERVAL_SECS: u64 = 1_800;
    const DEFAULT_DAILY_CAP: u32 = 3;

    /// Env-gated constructor: `None` unless `AUGMENTAGENT_AUTOPR=1|true`.
    /// `AUGMENTAGENT_AUTOPR_INTERVAL_SECS` (default 1800, floor 300 — the
    /// tick does a real `gh issue list`) and `AUGMENTAGENT_AUTOPR_DAILY_CAP`
    /// (default 3) tune cadence and spend ceiling.
    pub fn from_env(repo_root: PathBuf, dry_run: bool) -> Option<Self> {
        Self::from_values(
            repo_root,
            dry_run,
            std::env::var("AUGMENTAGENT_AUTOPR").ok().as_deref(),
            std::env::var("AUGMENTAGENT_AUTOPR_INTERVAL_SECS")
                .ok()
                .as_deref(),
            std::env::var("AUGMENTAGENT_AUTOPR_DAILY_CAP").ok().as_deref(),
        )
    }

    fn from_values(
        repo_root: PathBuf,
        dry_run: bool,
        enabled: Option<&str>,
        interval_secs: Option<&str>,
        daily_cap: Option<&str>,
    ) -> Option<Self> {
        let on = matches!(
            enabled.map(str::trim),
            Some(v) if v == "1" || v.eq_ignore_ascii_case("true")
        );
        if !on {
            return None;
        }
        let interval = interval_secs
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(Self::DEFAULT_INTERVAL_SECS)
            .max(300);
        let daily_cap = daily_cap
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(Self::DEFAULT_DAILY_CAP)
            .max(1);
        Some(Self {
            repo_root,
            dry_run,
            interval: std::time::Duration::from_secs(interval),
            daily_cap,
        })
    }

    pub async fn run(self, shutdown: tokio_util::sync::CancellationToken) -> Result<()> {
        info!(
            interval_secs = self.interval.as_secs(),
            daily_cap = self.daily_cap,
            dry_run = self.dry_run,
            "auto-PR loop started (#630): polling for agent-fixable issues"
        );
        let mut counter = DailyCounter::default();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("auto-PR loop stopped");
                    return Ok(());
                }
                _ = tokio::time::sleep(self.interval) => {}
            }
            let today = utc_day_now();
            if counter.runs_today(today) >= self.daily_cap {
                info!(
                    daily_cap = self.daily_cap,
                    "auto-PR: daily cap reached; idling until the next UTC day"
                );
                continue;
            }
            match run_once(&self.repo_root, self.dry_run).await {
                Ok(msg) if msg == IDLE_MSG => {}
                Ok(msg) => {
                    counter.record(today);
                    info!(
                        runs_today = counter.runs_today(today),
                        daily_cap = self.daily_cap,
                        "auto-PR: {msg}"
                    );
                }
                // Transient refusals (e.g. dirty deploy tree while a sibling
                // session works, gh/network hiccup) — log and try next tick.
                Err(e) => warn!("auto-PR tick failed: {e:#}"),
            }
        }
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

    // --- #300 trust gate + gate-env sandbox -----------------------------

    #[test]
    fn author_trust_honors_allowlist_and_write_associations() {
        let allow = vec!["owner-login".to_string(), "trusted-bot".to_string()];
        // Allowlisted login (case-insensitive) is trusted regardless of role.
        assert!(author_is_trusted("Owner-Login", "NONE", &allow));
        assert!(author_is_trusted("trusted-bot", "FIRST_TIME_CONTRIBUTOR", &allow));
        // Write-access associations are trusted even when not allowlisted.
        assert!(author_is_trusted("someone", "OWNER", &allow));
        assert!(author_is_trusted("someone", "member", &allow));
        assert!(author_is_trusted("someone", "Collaborator", &allow));
    }

    #[test]
    fn author_trust_denies_strangers_and_empty() {
        let allow = vec!["owner-login".to_string()];
        // A random public author with a non-write association is untrusted.
        assert!(!author_is_trusted("random-attacker", "NONE", &allow));
        assert!(!author_is_trusted("drive-by", "CONTRIBUTOR", &allow));
        // Missing login is untrusted (default-deny).
        assert!(!author_is_trusted("", "OWNER", &allow));
        assert!(!author_is_trusted("   ", "MEMBER", &allow));
    }

    #[test]
    fn gate_env_strips_provider_secrets_by_name() {
        // Representative provider/secret names must be flagged...
        for name in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "COMPOSIO_API_KEY",
            "GROQ_API_KEY",
            "CEREBRAS_API_KEY",
            "DISCORD_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "SOME_PASSWORD",
            "MY_CREDENTIAL_FILE",
            "x_api_key", // case-insensitive
        ] {
            assert!(env_name_is_secret(name), "{name} should be stripped");
        }
        // ...while ordinary build env survives.
        for name in ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "LANG", "TERM"] {
            assert!(!env_name_is_secret(name), "{name} should survive the gate");
        }
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

    // ---- #630: auto-PR loop config + daily cap ----

    fn loop_values(
        enabled: Option<&str>,
        interval: Option<&str>,
        cap: Option<&str>,
    ) -> Option<AutoPrLoop> {
        AutoPrLoop::from_values(PathBuf::from("/tmp/repo"), true, enabled, interval, cap)
    }

    #[test]
    fn auto_pr_loop_requires_explicit_opt_in() {
        // Every engaged run spends the owner's subscription (#448) — absent,
        // empty, "0", or garbage must all leave the loop unspawned.
        assert!(loop_values(None, None, None).is_none());
        assert!(loop_values(Some(""), None, None).is_none());
        assert!(loop_values(Some("0"), None, None).is_none());
        assert!(loop_values(Some("yes"), None, None).is_none());
        assert!(loop_values(Some("1"), None, None).is_some());
        assert!(loop_values(Some("true"), None, None).is_some());
        assert!(loop_values(Some("TRUE"), None, None).is_some());
    }

    #[test]
    fn auto_pr_loop_parses_interval_and_cap_with_floors() {
        let l = loop_values(Some("1"), Some("600"), Some("5")).unwrap();
        assert_eq!(l.interval.as_secs(), 600);
        assert_eq!(l.daily_cap, 5);
        // Defaults when unset or unparsable.
        let l = loop_values(Some("1"), Some("nope"), None).unwrap();
        assert_eq!(l.interval.as_secs(), AutoPrLoop::DEFAULT_INTERVAL_SECS);
        assert_eq!(l.daily_cap, AutoPrLoop::DEFAULT_DAILY_CAP);
        // Floors: an interval under 5min would hammer `gh issue list`; a cap
        // of 0 would make the loop a silent no-op the owner enabled on purpose.
        let l = loop_values(Some("1"), Some("10"), Some("0")).unwrap();
        assert_eq!(l.interval.as_secs(), 300);
        assert_eq!(l.daily_cap, 1);
    }

    #[test]
    fn daily_counter_caps_within_a_day_and_resets_on_rollover() {
        let mut c = DailyCounter::default();
        assert_eq!(c.runs_today(100), 0);
        c.record(100);
        c.record(100);
        assert_eq!(c.runs_today(100), 2);
        // Same-day queries don't reset.
        assert_eq!(c.runs_today(100), 2);
        // New UTC day ⇒ fresh budget.
        assert_eq!(c.runs_today(101), 0);
        c.record(101);
        assert_eq!(c.runs_today(101), 1);
    }
}
