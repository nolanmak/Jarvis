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
//! - **Draft by default; human merges.** PRs are opened `--draft`. The one
//!   exception (#630): with `AUGMENTAGENT_AUTOPR_AUTOMERGE=1`, issues
//!   authored by the OWNER (or an explicit login allowlist) merge
//!   automatically after the gate passes — everyone else always gets the
//!   draft + review flow. On the production box a merge deploys via the
//!   auto-updater, which is precisely the owner-only opt-in being made.
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

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

use augmentagent_channel_core::{build_reasoner, FallbackReasoner, Reasoner};

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

/// #816 — override for the single-flight lock file (tests).
const LOCK_FILE_ENV: &str = "AUGMENTAGENT_SELFIMPROVE_LOCK";

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
    /// #787 — filed by the daily `augmentagent research` pipeline rather
    /// than by a human. These are auto-filed with the owner's `gh` auth, so
    /// they LOOK owner-authored to the trust gate; they are picked last and
    /// never auto-merge.
    pub research_filed: bool,
}

/// #787 — does this body carry the research pipeline's filing stamp?
///
/// The pipeline reads papers and proposes speculative adoptions, filing 3/day
/// with the owner's `gh` credentials. Two consequences the picker must undo:
/// newest-first ordering hands them the entire daily budget forever (3 filed
/// per day vs. a cap of 3 runs), starving human bug reports outright; and the
/// owner-authored auto-merge test cannot tell them from issues the owner
/// actually wrote.
pub fn is_research_filed(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("auto-filed by the daily") || b.contains("`augmentagent research` pipeline")
}

/// One candidate issue as parsed from the REST `repos/*/issues` response
/// (#676). Only the fields the picker needs.
#[derive(Debug, PartialEq)]
struct RestIssue {
    number: u64,
    title: String,
    body: String,
    author: String,
    association: String,
    research_filed: bool,
}

/// Map the REST issues array into pick candidates, dropping what can never
/// be picked: pull requests (the REST issues endpoint interleaves them —
/// they carry a `pull_request` key), rows without a number, and issues
/// already labeled [`GAVE_UP_LABEL`]. Order is preserved (the query sorts
/// newest-first).
fn rest_issue_candidates(v: &serde_json::Value) -> Vec<RestIssue> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter(|iss| iss.get("pull_request").is_none())
        .filter_map(|iss| {
            let number = iss.get("number").and_then(serde_json::Value::as_u64)?;
            let gave_up = iss
                .get("labels")
                .and_then(serde_json::Value::as_array)
                .map(|ls| {
                    ls.iter()
                        .any(|l| l.get("name").and_then(|n| n.as_str()) == Some(GAVE_UP_LABEL))
                })
                .unwrap_or(false);
            if gave_up {
                return None;
            }
            let s = |k: &str| {
                iss.get(k)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let body = s("body");
            Some(RestIssue {
                number,
                title: s("title"),
                research_filed: is_research_filed(&body),
                body,
                author: iss
                    .pointer("/user/login")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                association: s("author_association"),
            })
        })
        .collect()
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
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !env_name_is_secret(k))
        .collect();
    // #692 — persist workspace build artifacts across gate runs. Without
    // this every per-issue worktree cold-builds the whole workspace
    // (~30G+ / tens of minutes on this box). Overridable; an explicit
    // CARGO_TARGET_DIR in the parent env wins.
    if !env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR") {
        env.push(("CARGO_TARGET_DIR".into(), gate_target_dir()));
    }
    // #780 — gate-run tests must NEVER file real GitHub issues. Channel
    // tests that build the production channel reach GhCliIssueRunner; the
    // per-crate test guards are the first line, this is the backstop.
    env.retain(|(k, _)| k != "AUGMENTAGENT_GH_DISABLE");
    env.push(("AUGMENTAGENT_GH_DISABLE".into(), "1".into()));
    env
}

/// Shared cargo target dir for gate + builder runs (#692).
/// `AUGMENTAGENT_GATE_TARGET_DIR` overrides; defaults under `~/.cache`.
fn gate_target_dir() -> String {
    std::env::var("AUGMENTAGENT_GATE_TARGET_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{home}/.cache/augmentagent-gate-target")
    })
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
    // #653 — no label gate: every open issue is considered (newest first, up
    // to 50) and the stage-1 scoping pass decides fixability itself. Issues
    // it judges not-fixable get the gave-up label + a comment, so each is
    // scoped at most once. `agent-fixable` is no longer required — it remains
    // only as documentation on old issues.
    //
    // #676 — fetched via the REST API, not `gh issue list --json`: the
    // installed gh build rejects `authorAssociation` as a list field
    // ("Unknown JSON field"), which killed every tick. `author_association`
    // is part of the REST issue schema on every gh version. The `{owner}/
    // {repo}` placeholders resolve from the cwd repo's origin, same as the
    // list command did.
    let (ok, stdout, stderr) = run(
        &gh,
        &[
            "api",
            "repos/{owner}/{repo}/issues?state=open&per_page=50&sort=created&direction=desc",
        ],
        repo_root,
    )
    .await?;
    if !ok {
        bail!("gh api issues failed: {stderr}");
    }
    let issues: serde_json::Value =
        serde_json::from_str(&stdout).context("parse gh api issues")?;

    // #787 — human-filed issues first, newest-first within each group. The
    // research pipeline files 3/day and the daily cap is 3, so strict
    // newest-first would hand it the entire budget forever and human bug
    // reports would never be reached.
    let (human, research): (Vec<_>, Vec<_>) = rest_issue_candidates(&issues)
        .into_iter()
        .partition(|i| !i.research_filed);
    for iss in human.into_iter().chain(research) {
        let RestIssue {
            number,
            title,
            body,
            author,
            association,
            research_filed,
        } = iss;
        if is_blast_radius(&format!("{title} {body}")) {
            info!(issue = number, "skip: blast-radius keyword in issue");
            continue;
        }
        if has_open_agent_pr(repo_root, number).await? {
            info!(issue = number, "skip: open agent PR exists (dedup)");
            continue;
        }
        // #300 — trust-gate on the author + its write-access association
        // before any host-side build/push.
        let author_trusted = author_is_trusted(&author, &association, &allowlist);
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
            research_filed,
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
/// Model tiers for the two-stage pipeline (#630). Defaults are pinned full
/// model IDs per the #448 rule — `model: None` silently inherits whatever the
/// owner last picked for their interactive Claude Code, which is a leak, not
/// a default (`fix_opts` shipped with exactly that bug until now). Env
/// overrides let the owner re-tier without a rebuild.
const AUTOPR_SCOPE_MODEL: &str = "claude-fable-5";
const AUTOPR_BUILD_MODEL: &str = "claude-opus-5";

fn scope_model() -> String {
    resolve_model(
        std::env::var("AUGMENTAGENT_AUTOPR_SCOPE_MODEL").ok().as_deref(),
        AUTOPR_SCOPE_MODEL,
    )
}

fn build_model() -> String {
    resolve_model(
        std::env::var("AUGMENTAGENT_AUTOPR_BUILD_MODEL").ok().as_deref(),
        AUTOPR_BUILD_MODEL,
    )
}

fn resolve_model(env_val: Option<&str>, default: &str) -> String {
    match env_val.map(str::trim) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => default.to_string(),
    }
}

/// Stage 1 of the two-stage pipeline: a read-only scoping pass on a stronger
/// model. Issues filed conversationally (e.g. by the Discord agent on the
/// owner's behalf) are often under-specified; the scoper reads the actual
/// code and turns the ask into a concrete implementation spec the builder
/// can follow, instead of letting the builder guess scope while editing.
fn scope_opts(worktree: PathBuf) -> augmentagent_channel_core::ReasonerOpts {
    augmentagent_channel_core::ReasonerOpts {
        system_prompt: SCOPE_SYSTEM.to_string(),
        model: Some(scope_model()),
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Bash(ls *)".into(),
            "Bash(git log*)".into(),
            "Bash(git diff*)".into(),
            "Bash(git status*)".into(),
        ],
        add_dirs: vec![worktree.clone()],
        permission_mode: "default".into(),
        cwd: Some(worktree),
        env: Vec::new(),
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

const SCOPE_SYSTEM: &str = "You are the scoping pass of a staged autonomous \
fix pipeline for this codebase. You are given a GitHub issue that may be \
vague, under-specified, or not actually fixable by a coding agent at all \
(research asks, epics, infrastructure/purchasing decisions). READ the \
relevant code first, then judge it and — when fixable — produce an \
implementation spec for a second, separate agent that will write the code.\n\
\n\
Your output MUST start with these two lines EXACTLY (then a blank line):\n\
VERDICT: fixable | not-fixable\n\
COMPLEXITY: simple | medium | hard\n\
\n\
Verdict guide: 'fixable' means a competent engineer could ship it from the \
issue text plus the code, as a focused diff under 600 changed lines, \
verifiable by the test suite. Research questions, multi-week epics, \
decisions needing the owner, and changes to deploy/auth/secret/CI paths are \
'not-fixable' — for those, follow the header with a 2-4 sentence reason and \
stop.\n\
Complexity guide: 'simple' = localized change, obvious approach, low regression \
risk. 'medium' = a few files or a subtle interaction, still well-understood. \
'hard' = cross-cutting, ambiguous, migration-shaped, or high blast radius if \
wrong. Grade honestly — 'hard' work is NOT auto-merged, it goes to human \
review.\n\
\n\
For fixable issues, after the header produce the spec:\n\
- Interpretation: what the issue is actually asking for, resolving any \
ambiguity with the most reasonable reading of the code and stating the \
assumption you made.\n\
- Files to touch: concrete paths, with what changes in each and which \
existing functions/patterns to reuse.\n\
- The smallest correct approach, and one sentence on why not the \
alternatives.\n\
- Edge cases and failure modes the builder must handle.\n\
- Acceptance criteria: which test to write FIRST (the builder follows TDD), \
what tests to add/update, and what observable behaviour proves the fix.\n\
Constraints: read-only — do NOT edit files. Stay inside the given working \
directory. Do NOT plan changes to deploy/auth/secret/CI paths (systemd, \
scripts/check-for-updates, .github/workflows, credentials/keyring/.env).\n\
\n\
Known repo facts — these are verified, and issue text often contradicts \
them. Trust these over the issue:\n\
- There is NO human-labelled triage corpus. Issues frequently cite \"the \
13,685 labelled decisions\" as an acceptance gate. What exists is the \
daemon's own past decisions in `data.db`, with no human ground truth, and \
that file is gitignored — it is NOT in your working directory. If an \
issue's acceptance criterion requires measuring accuracy against labelled \
data, it is NOT-FIXABLE; say so rather than inventing fixtures. Shipping \
the behaviour change while skipping its measurement gate inverts the \
issue's intent and is never acceptable.\n\
- `schema/*.md` prompts are compiled into the binary with `include_str!`, \
and the auto-updater only rebuilds when `crates/` or `Cargo.*` change. A \
schema-only change therefore merges, deploys, and does NOTHING. Never plan \
a schema-only fix.\n\
- `skills/**/*.md` are read from disk at runtime: editing one changes live \
behaviour for ~250 emails/day on the next pull, with no rebuild and no \
review. Grade ANY change to these files, or to triage/draft prompts, or to \
the send path, as `hard` — regardless of how few lines it is.\n\
- Changes to the self-improve pipeline's own gating logic are NOT-FIXABLE: \
this pipeline must not modify the gate that decides whether its own work \
merges.\n\
\n\
Grade complexity by BLAST RADIUS, not diff size. A 10-line prompt edit that \
alters every outbound email is `hard`; a 300-line self-contained validator \
with tests is `medium`.\n\
Output ONLY the header and spec/reason, no preamble.";

/// Complexity grade the scoping pass assigns (#653). Anything above
/// [`Complexity::Medium`] never auto-merges — it lands as a draft PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Complexity {
    Simple,
    Medium,
    Hard,
}

impl Complexity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Medium => "medium",
            Self::Hard => "hard",
        }
    }

    /// Auto-merge policy (#653): only simple/medium work qualifies.
    fn auto_mergeable(self) -> bool {
        !matches!(self, Self::Hard)
    }
}

/// Parsed stage-1 output (#653).
#[derive(Debug)]
struct ScopeOutcome {
    fixable: bool,
    complexity: Complexity,
    /// The spec (fixable) or the refusal reason (not-fixable) — the raw text
    /// with the header lines removed.
    body: String,
}

/// Parse the scoper's `VERDICT:` / `COMPLEXITY:` header, tolerantly: the
/// lines may appear anywhere in the first few lines, any case. Missing
/// verdict defaults to *fixable* (an unparsed run should still attempt the
/// fix); missing/unknown complexity defaults to *hard* (never auto-merge on
/// a formatting glitch — the conservative direction).
fn parse_scope_output(raw: &str) -> ScopeOutcome {
    let mut fixable = true;
    let mut complexity = Complexity::Hard;
    let mut body_lines: Vec<&str> = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let l = line.trim().to_ascii_lowercase();
        let is_header_zone = i < 10;
        if is_header_zone && l.starts_with("verdict:") {
            fixable = !l.contains("not-fixable") && !l.contains("not fixable");
            continue;
        }
        if is_header_zone && l.starts_with("complexity:") {
            complexity = if l.contains("simple") {
                Complexity::Simple
            } else if l.contains("medium") {
                Complexity::Medium
            } else {
                Complexity::Hard
            };
            continue;
        }
        body_lines.push(line);
    }
    ScopeOutcome {
        fixable,
        complexity,
        body: body_lines.join("\n").trim().to_string(),
    }
}

/// Stage 3 (#653): read-only QA review of the builder's diff, on the same
/// stronger tier as the scoper. A rejected review means no PR — it counts as
/// a failed attempt, exactly like a red verification gate.
fn review_opts(worktree: PathBuf) -> augmentagent_channel_core::ReasonerOpts {
    augmentagent_channel_core::ReasonerOpts {
        system_prompt: REVIEW_SYSTEM.to_string(),
        model: Some(scope_model()),
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Bash(ls *)".into(),
            "Bash(git diff*)".into(),
            "Bash(git status*)".into(),
            "Bash(git log*)".into(),
        ],
        add_dirs: vec![worktree.clone()],
        permission_mode: "default".into(),
        cwd: Some(worktree),
        env: Vec::new(),
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

const REVIEW_SYSTEM: &str = "You are the QA review pass of a staged autonomous \
fix pipeline. A separate builder agent has just edited this worktree to fix a \
GitHub issue; the build and test suite already pass. Your job is to review the \
work like a skeptical senior engineer before it ships. Run `git diff` and read \
the changed files IN CONTEXT (open the surrounding code, not just the hunks). \
Check:\n\
- Correctness: does the diff actually fix what the issue describes? Walk at \
least one concrete input through the changed code path.\n\
- Tests: is there a test that would FAIL without this change (TDD)? Bug fixes \
without a regression test are a reject unless genuinely untestable.\n\
- Scope: no unrelated edits, no drive-by refactors, no touched \
deploy/auth/secret/CI paths.\n\
- Conventions: matches the surrounding code's style, error handling, and \
logging patterns; comments only where the code can't speak.\n\
Your output MUST start with this line EXACTLY:\n\
REVIEW: approve | reject\n\
Then a blank line, then 2-6 sentences: for approve, what you verified; for \
reject, the concrete defects (file:line) that must change. Read-only — do NOT \
edit files. Output ONLY the verdict line and notes.";

/// Parse the reviewer's verdict. Anything that is not an explicit approve —
/// including unparseable output — is a reject: the conservative direction,
/// since an approval here can flow straight into an auto-merge.
fn parse_review_output(raw: &str) -> (bool, String) {
    let mut approved = false;
    for (i, line) in raw.lines().enumerate() {
        if i >= 5 {
            break;
        }
        let l = line.trim().to_ascii_lowercase();
        if l.starts_with("review:") {
            approved = l.contains("approve") && !l.contains("reject");
            break;
        }
    }
    (approved, raw.trim().to_string())
}

/// Build the stage-2 prompt: the issue plus (when the scoping pass produced
/// one) the implementation spec.
fn build_fix_prompt(issue: &Issue, plan: Option<&str>) -> String {
    match plan {
        Some(p) => format!(
            "GitHub issue #{}: {}\n\n{}\n\n\
             ## Implementation spec (from a read-only scoping pass on a \
             stronger model — follow it unless the code contradicts it, and \
             say so in your summary if it does)\n{}\n\nImplement the fix now.",
            issue.number, issue.title, issue.body, p
        ),
        None => format!(
            "GitHub issue #{}: {}\n\n{}\n\nImplement the fix now.",
            issue.number, issue.title, issue.body
        ),
    }
}

/// #630 — auto-merge policy. Off unless `AUGMENTAGENT_AUTOPR_AUTOMERGE=1|true`,
/// and even then only for issues authored by the owner (or an explicit
/// `AUGMENTAGENT_AUTOPR_AUTOMERGE_AUTHORS` login list). Everyone else's
/// issues keep the draft-PR + human-review flow. NOTE the deploy coupling:
/// on the production box a merge to main is deployed to the live daemon by
/// the auto-updater within minutes — that is exactly the behaviour the owner
/// opted into for their own issues, and exactly why nobody else's issues
/// qualify.
fn automerge_enabled_value(v: Option<&str>) -> bool {
    matches!(v.map(str::trim), Some(x) if x == "1" || x.eq_ignore_ascii_case("true"))
}

fn automerge_eligible(author: &str, owner: Option<&str>, authors_csv: Option<&str>) -> bool {
    if author.trim().is_empty() {
        return false;
    }
    match authors_csv.map(str::trim).filter(|s| !s.is_empty()) {
        // Explicit allowlist set ⇒ it is the whole policy (owner must
        // list themselves too — no silent unions).
        Some(csv) => csv
            .split(',')
            .any(|a| a.trim().eq_ignore_ascii_case(author.trim())),
        None => matches!(owner, Some(o) if o.trim().eq_ignore_ascii_case(author.trim())),
    }
}

fn fix_opts(worktree: PathBuf) -> augmentagent_channel_core::ReasonerOpts {
    augmentagent_channel_core::ReasonerOpts {
        system_prompt: SELF_IMPROVE_SYSTEM.to_string(),
        model: Some(build_model()),
        // #692 — the builder's own `cargo check/test -p` self-verification
        // reuses the shared gate cache instead of cold-building per issue.
        env: vec![("CARGO_TARGET_DIR".into(), gate_target_dir())],
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
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

const SELF_IMPROVE_SYSTEM: &str = "You are an autonomous maintenance engineer for the \
AugmentAgent codebase. You are given a single GitHub issue (usually with an \
implementation spec from a scoping pass). Implement the smallest correct fix. \
Constraints you MUST honor:\n\
- Follow TDD: for bug-shaped issues, FIRST write a test that reproduces the \
issue and fails against the current code, then implement until it passes. For \
features, write the test alongside the change. A fix without a test that \
would fail without it will be rejected by the QA review that follows you, \
unless the behaviour is genuinely untestable (say so in your summary).\n\
- Run the targeted tests for the crates you touched (cargo test -p <crate>) \
before finishing — do NOT run workspace-wide test builds.\n\
- Follow the surrounding code's conventions: naming, error handling, logging, \
comment density. Comments only where the code can't speak for itself.\n\
- Review your own diff (git diff) before finishing: no unrelated edits, no \
drive-by refactors, no leftover debug output.\n\
- Stay within the working directory you were given (a throwaway worktree).\n\
- Do NOT touch deploy/auth/secret/CI files (systemd units, scripts/check-for-updates, \
.github/workflows, anything with credentials/keyring/.env).\n\
- Keep the diff small and focused on the issue.\n\
- Do NOT run git commit, git push, or gh. Just edit files.\n\
When done, output a 2-4 sentence summary of what you changed and why, noting \
the test that guards it.";

/// Run the verification gate inside the worktree. Returns Ok(()) only if every
/// configured check passes.
/// Wrap a gate shell command so a failure in ANY pipe stage fails the gate.
/// #681 — without `set -o pipefail`, `cargo build … | tail -5` reports
/// `tail`'s exit status (always 0): every gate command had passed
/// unconditionally since #103, letting a non-compiling diff reach the PR
/// stage. Caught live on the first real #653 pipeline run.
fn gate_sh(cmd: &str) -> String {
    format!("set -o pipefail; {cmd}")
}

async fn verification_gate(worktree: &Path) -> Result<()> {
    // #300 — Strip provider secrets from the gate's child env. `cargo
    // build`/`npm run build` execute `build.rs`/proc-macros/`npm
    // postinstall`, which the issue body could influence; none need
    // provider keys, so this is behavior-preserving for honest fixes.
    let env = gate_env();
    info!("verification gate: cargo build (sandboxed env)");
    let (ok, _o, e) = run_sandboxed(
        "bash",
        &["-lc", &gate_sh(". $HOME/.cargo/env && cargo build --workspace 2>&1 | tail -5")],
        worktree,
        &env,
    )
    .await?;
    if !ok {
        bail!("cargo build failed:\n{o}{e}", o = _o.trim());
    }
    info!("verification gate: cargo test (sandboxed env)");
    let (ok, _o, e) = run_sandboxed(
        "bash",
        &["-lc", &gate_sh(". $HOME/.cargo/env && cargo test --workspace -- --test-threads=1 2>&1 | tail -8")],
        worktree,
        &env,
    )
    .await?;
    if !ok {
        bail!("cargo test failed:\n{o}{e}", o = _o.trim());
    }
    // npm build is best-effort: only gate on it if a package.json + node_modules
    // are present (prod has them; a bare CI checkout may not).
    if worktree.join("package.json").exists() && worktree.join("node_modules").exists() {
        info!("verification gate: npm run build (sandboxed env)");
        let (ok, _o, e) =
            run_sandboxed("bash", &["-lc", &gate_sh("npm run build 2>&1 | tail -5")], worktree, &env)
                .await?;
        if !ok {
            bail!("npm run build failed:\n{o}{e}", o = _o.trim());
        }
    } else {
        info!("verification gate: npm build skipped (no node_modules)");
    }
    Ok(())
}

/// Force the pipeline's fixed worktree path back to a usable state before a
/// fresh `git worktree add`.
///
/// `git worktree remove` only acts on a *registered* worktree. A run killed
/// mid-flight (daemon restart, OOM, `kill`) can leave the directory behind
/// with a stale or absent `.git/worktrees` entry — `remove` then fails, and
/// the `add` that follows refuses an existing path, so every later tick dies
/// in the same place. Nothing here is recoverable state: the path is
/// pipeline-owned and the branch is re-created from `origin/main`, so the
/// unconditional `remove_dir_all` is safe and is what makes a crashed run
/// self-healing instead of permanently wedging the loop.
async fn reclaim_worktree(repo_root: &Path, worktree: &Path, branch: &str) {
    let _ = run(
        "git",
        &["worktree", "remove", "--force", &worktree.to_string_lossy()],
        repo_root,
    )
    .await;
    let _ = run("git", &["branch", "-D", branch], repo_root).await;
    if worktree.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(worktree).await {
            warn!(
                path = %worktree.display(),
                "could not reclaim stale self-improve worktree: {e}"
            );
        }
    }
    // Drop any registration left pointing at the path we just deleted;
    // without this `worktree add` reports it as already checked out.
    let _ = run("git", &["worktree", "prune"], repo_root).await;
}

/// #816 — process-wide single-flight guard for the pipeline.
///
/// The daemon's [`AutoPrLoop`] tick and a hand-run `augmentagent
/// self-improve` share one fixed worktree path, and every run force-removes
/// that path on entry. Concurrently, the second run deletes the first run's
/// checkout mid-gate and the first can go on to commit and push whatever the
/// second left there — with auto-merge on, straight to `main`. An advisory
/// `flock` held for the whole run makes the loser skip its tick instead,
/// which costs nothing: the next one is `AUGMENTAGENT_AUTOPR_INTERVAL_SECS`
/// away.
///
/// Until now the accidental serializer was the dirty-tree preflight — a run
/// in flight left `?? .self-improve-worktrees/` in `git status`, so the other
/// run refused. Teaching that preflight to ignore the pipeline's own worktree
/// is what makes a real lock necessary.
struct RunLock {
    /// Held only for its `Drop`: closing the fd releases the `flock`.
    _file: std::fs::File,
}

impl RunLock {
    /// Take the lock, or `Ok(None)` if another run already holds it.
    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create lock dir {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open self-improve lock {}", path.display()))?;
        // SAFETY: `file` owns the fd for the duration of the call, and
        // `flock` neither reads nor writes through it.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let err = std::io::Error::last_os_error();
        // EWOULDBLOCK == EAGAIN on Linux; match both so this reads correctly
        // wherever they differ.
        if matches!(
            err.raw_os_error(),
            Some(e) if e == libc::EWOULDBLOCK || e == libc::EAGAIN
        ) {
            return Ok(None);
        }
        Err(err).context("flock self-improve lock")
    }
}

/// Lock file path: `AUGMENTAGENT_SELFIMPROVE_LOCK` override (tests), else the
/// daemon state dir alongside `reasoner-cooldowns.json`, else a cwd-relative
/// fallback so a HOME-less environment still serializes.
fn run_lock_path() -> PathBuf {
    if let Ok(p) = std::env::var(LOCK_FILE_ENV) {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join(".local/state/augmentagent")
                .join("self-improve.lock")
        })
        .unwrap_or_else(|| PathBuf::from(".self-improve.lock"))
}

/// Drive one self-improvement attempt. `dry_run` stops before opening the PR
/// (prints what it would do) so the loop can be exercised safely.
pub async fn run_once(repo_root: &Path, dry_run: bool) -> Result<String> {
    // #816 — single-flight. Held for the whole run; dropped on every exit
    // path. A losing run reports idle so it consumes no daily budget.
    let _lock = match RunLock::try_acquire(&run_lock_path())? {
        Some(l) => l,
        None => {
            info!("self-improve: another run holds the lock; skipping this tick");
            return Ok(IDLE_MSG.to_string());
        }
    };

    // Refuse to run from a dirty tree / detached state — protects the deploy.
    let (ok, status_out, _) = run("git", &["status", "--porcelain"], repo_root).await?;
    if !ok {
        bail!("not a git repo at {}", repo_root.display());
    }
    let dirty_status = unmanaged_dirty_status(&status_out);
    if !dirty_status.is_empty() {
        bail!(
            "refusing to self-improve from a dirty working tree \
             (commit/stash first):\n{dirty_status}"
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
             `AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS` and removing the \
             `agent-gave-up` label, or by reviewing and driving it through \
             the approval flow.",
        )
        .await
        .ok();
        // #630 — the refusal is deterministic (the author won't change), so
        // label it out of the selection pool immediately. Without this, the
        // unattended loop re-picks the same issue every tick: it head-of-line
        // blocks every other labeled issue and re-comments daily, forever.
        label_gave_up(repo_root, issue.number).await.ok();
        return Ok(format!(
            "issue #{}: refused — untrusted author '{}' (requires owner approval)",
            issue.number, issue.author
        ));
    }

    let branch = format!("{BRANCH_PREFIX}{}", issue.number);
    // #692 — a FIXED path, force-recreated per issue (the branch stays
    // per-issue). Test binaries bake `env!("CARGO_MANIFEST_DIR")` at compile
    // time; with the shared gate target cache, binaries compiled under a
    // deleted per-issue path get reused from later runs and panic reading
    // fixtures. A stable path keeps every baked path resolvable.
    let worktree = repo_root.join(".self-improve-worktrees").join("current");

    // Clean any stale worktree from a crashed prior run.
    reclaim_worktree(repo_root, &worktree, &branch).await;

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

    let reasoner = build_reasoner();

    // Stage 1 (#630/#653): read-only scoping pass on a stronger model. It
    // decides fixability on its own (no label required), grades complexity
    // (which gates auto-merge), and expands the ask into an implementation
    // spec before the builder edits anything. Scoping failure degrades to
    // the single-stage behaviour with complexity defaulting to hard.
    let scope_prompt = format!(
        "GitHub issue #{}: {}\n\n{}\n\nProduce the verdict header and (if \
         fixable) the implementation spec now.",
        issue.number, issue.title, issue.body
    );
    let scope = match reasoner
        .call(&scope_opts(worktree.clone()), &scope_prompt)
        .await
    {
        Ok(p) => Some(parse_scope_output(&p)),
        Err(e) => {
            warn!(
                issue = issue.number,
                "scoping pass failed; building without a spec: {e:#}"
            );
            None
        }
    };
    if let Some(s) = &scope {
        if !s.fixable {
            // #653 — the scoper judged this not agent-fixable (research ask,
            // epic, owner decision, …). Label it out so it is scoped at most
            // once, and leave the reason on the issue.
            cleanup(worktree, branch, repo_root.to_path_buf()).await;
            backoff_comment(
                repo_root,
                issue.number,
                &format!(
                    "Auto-fix triage: the scoping pass judged this issue not \
                     agent-fixable, so the pipeline is leaving it for a \
                     human. Reason:\n\n{}\n\nRemove the `{GAVE_UP_LABEL}` \
                     label to have it re-triaged.",
                    truncate(&s.body, 1200)
                ),
            )
            .await
            .ok();
            label_gave_up(repo_root, issue.number).await.ok();
            return Ok(format!(
                "issue #{}: scoped as not agent-fixable — labeled out",
                issue.number
            ));
        }
    }
    let complexity = scope.as_ref().map(|s| s.complexity).unwrap_or(Complexity::Hard);
    let plan = scope
        .as_ref()
        .map(|s| s.body.clone())
        .filter(|b| !b.is_empty());

    // Stage 2: hand the issue (+ spec) to the builder inside the worktree.
    let opts = fix_opts(worktree.clone());
    let prompt = build_fix_prompt(&issue, plan.as_deref());
    let summary = match reasoner.call(&opts, &prompt).await {
        Ok(s) => s,
        Err(err) => {
            cleanup(worktree, branch, repo_root.to_path_buf()).await;
            return Err(err).context("reasoner failed during self-improve");
        }
    };

    // Did anything change? A no-op run still burned a reasoner call, so it
    // counts as an attempt (#630) — otherwise the unattended loop silently
    // re-spends its daily cap on an issue the model can't act on.
    // #793 — stage first: `git diff` is tracked-only, so every file the
    // builder CREATED is invisible to it. Both guards below (blast radius,
    // size cap) ran on that blind view, which meant a change of any size
    // made of new files passed the 600-line cap, and a newly-created
    // `.github/workflows/*.yml` or `scripts/deploy-*.sh` was never refused.
    // Staging here is idempotent with the commit step's own `git add -A`.
    let _ = run("git", &["add", "-A"], &worktree).await?;
    let (_ok, diff, _) = run("git", &["diff", "--cached", "--stat"], &worktree).await?;
    if diff.trim().is_empty() {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
        if attempts >= MAX_ATTEMPTS {
            backoff_comment(
                repo_root,
                issue.number,
                &format!(
                    "Self-improve gave up after {attempts} attempts: the \
                     reasoner produced no changes for this issue. Needs a \
                     human (or a more concrete issue body)."
                ),
            )
            .await
            .ok();
            label_gave_up(repo_root, issue.number).await.ok();
        }
        return Ok(format!(
            "issue #{}: reasoner made no changes; skipped (attempt {attempts})",
            issue.number
        ));
    }

    // Blast-radius + size guard on the actual diff. Each refusal burned a
    // full reasoner run, so it counts as an attempt (#630): a different
    // rollout MAY produce an acceptable diff, but after MAX_ATTEMPTS the
    // gave-up label pulls the issue from the pool — otherwise the unattended
    // loop would re-spend its whole daily cap on the same issue forever.
    let (_ok, full_diff, _) = run("git", &["diff", "--cached"], &worktree).await?;
    if is_blast_radius(&full_diff) {
        cleanup(worktree.clone(), branch.clone(), repo_root.to_path_buf()).await;
        let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
        backoff_comment(
            repo_root,
            issue.number,
            "Self-improve refused: the produced diff touches a \
             deploy/auth/secret path (blast-radius guard).",
        )
        .await
        .ok();
        if attempts >= MAX_ATTEMPTS {
            label_gave_up(repo_root, issue.number).await.ok();
        }
        return Ok(format!(
            "issue #{}: refused — diff hit blast-radius guard (attempt {attempts})",
            issue.number
        ));
    }
    let lines = diff_line_count(&full_diff);
    if lines > MAX_DIFF_LINES {
        cleanup(worktree.clone(), branch.clone(), repo_root.to_path_buf()).await;
        let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
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
        if attempts >= MAX_ATTEMPTS {
            label_gave_up(repo_root, issue.number).await.ok();
        }
        return Ok(format!(
            "issue #{}: refused — diff too large ({lines} lines, attempt {attempts})",
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

    // Stage 3 (#653): QA review of the diff by the stronger tier. A reject
    // is a failed attempt — same treatment as a red gate. Runs before the
    // dry-run exit so dry runs exercise the full pipeline.
    let review_prompt = format!(
        "GitHub issue #{}: {}\n\n{}\n\nBuilder's summary of its change:\n{}\n\n\
         Review the worktree's diff now and output your verdict.",
        issue.number,
        issue.title,
        truncate(&issue.body, 2000),
        truncate(&summary, 1000)
    );
    let (review_ok, review_notes) = match reasoner
        .call(&review_opts(worktree.clone()), &review_prompt)
        .await
    {
        Ok(r) => parse_review_output(&r),
        Err(e) => {
            // Transient reviewer failure (session limit etc.) — don't burn an
            // attempt on infrastructure noise; the next tick retries whole.
            cleanup(worktree, branch, repo_root.to_path_buf()).await;
            return Err(e).context("QA review pass failed during self-improve");
        }
    };
    if !review_ok {
        warn!(issue = issue.number, "QA review rejected the diff");
        let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
        backoff_comment(
            repo_root,
            issue.number,
            &format!(
                "Auto-fix QA review rejected the attempt:\n\n{}",
                truncate(&review_notes, 1200)
            ),
        )
        .await
        .ok();
        if attempts >= MAX_ATTEMPTS {
            label_gave_up(repo_root, issue.number).await.ok();
        }
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        return Ok(format!(
            "issue #{}: QA review rejected the diff (attempt {attempts}); no PR opened",
            issue.number
        ));
    }

    if dry_run {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        return Ok(format!(
            "issue #{}: DRY RUN — gate + QA review passed, {lines}-line diff \
             (complexity: {}), would open PR",
            issue.number,
            complexity.as_str()
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

    // Open the PR. Draft + human merge for everyone; owner-authored issues
    // auto-merge when the owner opted in AND the scoper graded the work
    // simple/medium (#653 — hard work always gets human eyes).
    let automerge = {
        let enabled = automerge_enabled_value(
            std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE").ok().as_deref(),
        );
        // #787 — research-filed issues never auto-merge: they are the
        // daemon's own speculative proposals, auto-filed with the owner's gh
        // auth (so they pass the owner-authored test), and they change core
        // behaviour. They land as draft PRs for human review.
        if enabled && complexity.auto_mergeable() && !issue.research_filed {
            let owner = std::env::var(GH_OWNER_ENV)
                .ok()
                .or(repo_owner_from_remote(repo_root).await);
            automerge_eligible(
                &issue.author,
                owner.as_deref(),
                std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE_AUTHORS")
                    .ok()
                    .as_deref(),
            )
        } else {
            false
        }
    };
    let gh = gh_bin();
    let plan_section = plan
        .as_deref()
        .map(|p| format!("\n\n## Implementation spec (scoping pass)\n{}", truncate(p, 1500)))
        .unwrap_or_default();
    let merge_note = if automerge {
        "Auto-merged: owner-authored issue graded ≤medium, AUGMENTAGENT_AUTOPR_AUTOMERGE=1."
    } else {
        "Draft — a human must review and merge."
    };
    let pr_body = format!(
        "Automated self-improvement for #{}.\n\n## Summary\n{}{plan_section}\n\n\
         ## QA review (approved)\n{}\n\n## Verification\n\
         - complexity (scoping pass): {}\n\
         - `cargo build --workspace`: pass\n- `cargo test --workspace`: pass\n\
         - diff size: {lines} lines (cap {MAX_DIFF_LINES})\n\n\
         {merge_note} Fixes #{}",
        issue.number,
        truncate(&summary, 1500),
        truncate(&review_notes, 1000),
        complexity.as_str(),
        issue.number
    );
    let title = format!("fix: {} (#{})", issue.title, issue.number);
    let mut args = vec!["pr", "create"];
    if !automerge {
        args.push("--draft");
    }
    args.extend_from_slice(&[
        "--base", "main", "--head", &branch, "--title", &title, "--body", &pr_body,
    ]);
    let (ok, stdout, e) = run(&gh, &args, &worktree).await?;
    cleanup(worktree, branch.clone(), repo_root.to_path_buf()).await;
    if !ok {
        bail!("gh pr create failed: {e}");
    }
    let pr_url = stdout.trim().to_string();
    if !automerge {
        return Ok(format!("issue #{}: draft PR opened — {pr_url}", issue.number));
    }
    // Merge immediately: the verification gate already passed, and `--auto`
    // needs branch protection this repo doesn't run. A merge failure (main
    // moved, protection added later) leaves the PR open for a human — never
    // retried blindly.
    let (ok, _o, e) = run(
        &gh,
        &["pr", "merge", &branch, "--squash", "--delete-branch"],
        repo_root,
    )
    .await?;
    if !ok {
        warn!(issue = issue.number, "auto-merge failed; PR left open: {e}");
        return Ok(format!(
            "issue #{}: PR opened but auto-merge FAILED (left open for review) — {pr_url}",
            issue.number
        ));
    }
    Ok(format!(
        "issue #{}: PR auto-merged (owner-authored) — {pr_url}",
        issue.number
    ))
}

/// The pipeline owns this untracked worktree. It must not make the deploy
/// appear dirty after a failed or interrupted attempt; every other porcelain
/// entry remains a hard safety stop.
fn unmanaged_dirty_status(status: &str) -> String {
    status
        .lines()
        .filter(|line| !line.starts_with("?? .self-improve-worktrees/"))
        .collect::<Vec<_>>()
        .join("\n")
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
            &format!(
                "<!-- self-improve-attempt --> attempt {n} did not produce an \
                 acceptable PR (gate failure, refused diff, or no changes)."
            ),
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

/// Clip `s` to a `max`-BYTE budget, appending an ellipsis.
///
/// #813 — `max` is a byte index, so the naive `&s[..max]` panics whenever the
/// cut lands inside a multi-byte character. Every caller here feeds it either
/// a GitHub issue body or raw model output, and the scoping/build/review
/// system prompts are themselves full of em dashes, so a mid-character cut is
/// a matter of when. The panic is not survivable where it happens: the loop
/// runs in a `tokio::spawn`ed task whose handle is joined behind other
/// never-terminating channel loops, so the task simply dies and the auto-PR
/// loop stays dead until the next process restart. Walk back to the nearest
/// char boundary instead.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
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
            research_filed: is_research_filed(&body),
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
    let wrapped = gate_sh(&format!(". $HOME/.cargo/env 2>/dev/null; {cmd}"));
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
        to: String::new(),
        cc: String::new(),
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

    let reasoner = build_reasoner();
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
    reasoner: &Arc<FallbackReasoner>,
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

    // #793 — same tracked-only blindness as the single-repo path.
    let _ = run("git", &["add", "-A"], &workspace).await?;
    let (_ok, stat, _) = run("git", &["diff", "--cached", "--stat"], &workspace).await?;
    if stat.trim().is_empty() {
        do_cleanup().await;
        return Ok(format!("issue #{}: reasoner made no changes", issue.number));
    }
    let (_ok, full_diff, _) = run("git", &["diff", "--cached"], &workspace).await?;

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
/// - **Self-triage, no label required (#653).** Every open issue is
///   considered; the stage-1 scoping pass judges fixability itself and
///   labels non-fixable issues out (`agent-gave-up` + reason comment) so
///   each is scoped at most once. The multi-repo path (other people's
///   repos) still requires the explicit `agent-fixable` label.
/// - **Serial + rate-limited.** One pipeline run at a time (single loop),
///   first tick a full interval after boot (the auto-updater bounces the
///   daemon on every deploy; a boot tick would burn cap on each restart),
///   and at most `AUGMENTAGENT_AUTOPR_DAILY_CAP` engaged runs per UTC day.
///   The counter is in-memory — a restart resets it — so the cap is
///   belt-and-suspenders on top of the label gate and per-issue back-off,
///   not the primary control.
/// - **Draft + human merge by default.** Inherited from `run_once`; the PR
///   body's `Fixes #N` cross-links it on the issue. Owner-authored issues
///   auto-merge only behind the separate `AUGMENTAGENT_AUTOPR_AUTOMERGE`
///   opt-in (see `automerge_eligible`).
///
/// Each engaged run is up to THREE reasoner calls (#630/#653): a read-only
/// scoping pass on `AUGMENTAGENT_AUTOPR_SCOPE_MODEL` (default Fable) that
/// judges fixability, grades complexity, and writes the implementation
/// spec; the build on `AUGMENTAGENT_AUTOPR_BUILD_MODEL` (default Opus,
/// TDD-instructed); and a read-only QA review back on the scope tier that
/// must approve before any PR opens. Not-fixable verdicts stop after one
/// call. Budget the daily cap accordingly.
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
            "auto-PR loop started (#630/#653): polling open issues (self-triage, no label gate)"
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

    #[test]
    fn truncate_never_splits_a_multibyte_char() {
        // The em dash occupies bytes 9..12, so a byte-index cut at 10 or 11
        // is exactly the panic the old implementation took.
        let s = format!("{}—tail", "a".repeat(9));
        assert_eq!(truncate(&s, 10), "aaaaaaaaa…");
        assert_eq!(truncate(&s, 11), "aaaaaaaaa…");
        // Landing exactly on a boundary keeps the whole character.
        assert_eq!(truncate(&s, 12), "aaaaaaaaa—…");
        // A cut before the first character yields just the ellipsis.
        assert_eq!(truncate("—————", 2), "…");
        // Multi-byte input that fits is returned untouched.
        assert_eq!(truncate("café — ok", 64), "café — ok");
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

    // ---- #630: auto-merge policy + two-stage model pins ----

    #[test]
    fn automerge_requires_explicit_opt_in_value() {
        assert!(!automerge_enabled_value(None));
        assert!(!automerge_enabled_value(Some("")));
        assert!(!automerge_enabled_value(Some("0")));
        assert!(!automerge_enabled_value(Some("yes")));
        assert!(automerge_enabled_value(Some("1")));
        assert!(automerge_enabled_value(Some("true")));
        assert!(automerge_enabled_value(Some(" TRUE ")));
    }

    #[test]
    fn automerge_eligibility_is_owner_only_by_default() {
        // Owner match, case-insensitive.
        assert!(automerge_eligible("nolanmak", Some("nolanmak"), None));
        assert!(automerge_eligible("NolanMak", Some("nolanmak"), None));
        // Anyone else — including trusted collaborators — stays draft+review.
        assert!(!automerge_eligible("collaborator", Some("nolanmak"), None));
        // No resolvable owner ⇒ never auto-merge.
        assert!(!automerge_eligible("nolanmak", None, None));
        // Empty author (gh returned none) ⇒ never.
        assert!(!automerge_eligible("", Some("nolanmak"), None));
    }

    #[test]
    fn automerge_allowlist_replaces_owner_default() {
        // An explicit allowlist IS the policy — no silent union with owner.
        assert!(automerge_eligible(
            "helper",
            Some("nolanmak"),
            Some("helper, other")
        ));
        assert!(!automerge_eligible(
            "nolanmak",
            Some("nolanmak"),
            Some("helper")
        ));
        // Blank allowlist falls back to the owner default.
        assert!(automerge_eligible("nolanmak", Some("nolanmak"), Some("  ")));
    }

    #[test]
    fn two_stage_models_are_pinned_with_env_override() {
        // #448 — model: None inherits the owner's interactive model; both
        // stages must pin. Defaults hold when the env is unset/blank.
        assert_eq!(resolve_model(None, AUTOPR_SCOPE_MODEL), "claude-fable-5");
        assert_eq!(resolve_model(Some("  "), AUTOPR_BUILD_MODEL), "claude-opus-5");
        assert_eq!(
            resolve_model(Some("claude-sonnet-5"), AUTOPR_BUILD_MODEL),
            "claude-sonnet-5"
        );
        // The opts constructors actually pin them.
        assert!(scope_opts(PathBuf::from("/tmp/wt")).model.is_some());
        assert!(fix_opts(PathBuf::from("/tmp/wt")).model.is_some());
    }

    #[test]
    fn scope_stage_is_read_only_and_fix_prompt_embeds_the_spec() {
        let opts = scope_opts(PathBuf::from("/tmp/wt"));
        for t in &opts.allowed_tools {
            assert!(
                !t.contains("Write") && !t.contains("Edit"),
                "scope stage must not be able to edit: {t}"
            );
        }
        let issue = Issue {
            number: 7,
            title: "t".into(),
            body: "b".into(),
            author: "a".into(),
            author_trusted: true,
            research_filed: false,
        };
        let with = build_fix_prompt(&issue, Some("the spec"));
        assert!(with.contains("the spec"));
        assert!(with.contains("Implementation spec"));
        let without = build_fix_prompt(&issue, None);
        assert!(!without.contains("Implementation spec"));
        assert!(without.contains("Implement the fix now."));
    }

    // ---- #692: shared gate target cache + stable worktree path ----

    #[test]
    fn gate_env_provides_a_shared_cargo_target_dir() {
        let env = gate_env();
        let target = env.iter().find(|(k, _)| k == "CARGO_TARGET_DIR");
        assert!(
            target.is_some(),
            "gate env must pin CARGO_TARGET_DIR or every issue cold-builds the workspace"
        );
        assert!(!target.unwrap().1.is_empty());
        // The builder stage shares the same cache.
        let opts = fix_opts(PathBuf::from("/tmp/wt"));
        assert!(opts.env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"));
    }

    // ---- #681: gate commands must fail when the piped stage fails ----

    #[test]
    fn gate_sh_prefixes_pipefail() {
        assert!(gate_sh("cargo build | tail -5").starts_with("set -o pipefail; "));
    }

    #[test]
    fn gate_sh_makes_a_failing_pipe_stage_fail_the_command() {
        // The exact failure that slipped through live (#681): first stage
        // fails, `| tail` succeeds. Run both shapes through real bash.
        let status = |cmd: &str| {
            std::process::Command::new("bash")
                .args(["-lc", cmd])
                .status()
                .expect("spawn bash")
                .success()
        };
        // Unwrapped: tail masks the failure (the bug).
        assert!(status("false 2>&1 | tail -1"));
        // Wrapped: the failure propagates.
        assert!(!status(&gate_sh("false 2>&1 | tail -1")));
        // Wrapped success still succeeds.
        assert!(status(&gate_sh("true 2>&1 | tail -1")));
    }

    #[test]
    fn scope_prompt_states_the_verified_repo_constraints() {
        // #787 — the scoper repeatedly graded speculative research issues as
        // buildable because their text asserts a labelled corpus exists and
        // presents prompt edits as one-file changes. These facts are verified
        // against the tree; losing any of them silently restores the bug.
        for needle in [
            "NO human-labelled triage corpus",
            "gitignored",
            "include_str!",
            "merges, deploys, and does NOTHING",
            "read from disk at runtime",
            "BLAST RADIUS, not diff size",
            "must not modify the gate",
        ] {
            assert!(
                SCOPE_SYSTEM.contains(needle),
                "scope prompt lost its {needle:?} constraint"
            );
        }
    }

    // ---- #793: guards must see newly-created files ----

    /// The guards run `git diff --cached` after staging. This asserts the
    /// distinction that broke them: plain `git diff` is blind to created
    /// files, so a 900-line new file and a new `.github/workflows/*.yml`
    /// both read as an empty diff (PR #792 self-reported 47 lines for a
    /// 545-line change and auto-merged).
    #[test]
    fn staged_diff_sees_created_files_that_plain_diff_misses() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("git")
        };
        git(&["init", "-q", "."]);
        git(&["-c", "user.email=t@e", "-c", "user.name=t", "commit", "-q",
              "--allow-empty", "-m", "init"]);
        std::fs::create_dir_all(repo.join(".github/workflows")).unwrap();
        std::fs::write(repo.join(".github/workflows/x.yml"), "name: x
").unwrap();
        std::fs::write(repo.join("big.rs"), "line
".repeat(900)).unwrap();

        let plain = String::from_utf8(git(&["diff"]).stdout).unwrap();
        assert_eq!(diff_line_count(&plain), 0, "precondition: plain diff is blind");
        assert!(!is_blast_radius(&plain), "precondition: guard sees nothing");

        git(&["add", "-A"]);
        let staged = String::from_utf8(git(&["diff", "--cached"]).stdout).unwrap();
        assert!(
            diff_line_count(&staged) > MAX_DIFF_LINES,
            "staged diff must expose the created lines so the cap applies"
        );
        assert!(
            is_blast_radius(&staged),
            "staged diff must expose a created .github/workflows path"
        );
    }

    // ---- #787: human-filed issues outrank research-filed ones ----

    fn rest(n: u64, body: &str) -> serde_json::Value {
        serde_json::json!({"number": n, "title": format!("t{n}"), "body": body,
                           "user": {"login": "nolanmak"}, "author_association": "OWNER"})
    }

    #[test]
    fn research_filing_stamp_is_detected() {
        assert!(is_research_filed(
            "Source: arXiv:1234\n\n_Auto-filed by the daily `augmentagent research` pipeline._"
        ));
        assert!(is_research_filed("AUTO-FILED BY THE DAILY pipeline"));
        // A human issue that merely mentions research must not be caught.
        assert!(!is_research_filed(
            "The research loop keeps filing dupes; add dedup before filing."
        ));
        assert!(!is_research_filed(""));
    }

    #[test]
    fn candidates_keep_order_and_carry_the_research_flag() {
        let stamp = "_Auto-filed by the daily `augmentagent research` pipeline._";
        let v = serde_json::json!([rest(9, stamp), rest(8, "real bug"), rest(7, stamp)]);
        let got = rest_issue_candidates(&v);
        assert_eq!(
            got.iter().map(|i| (i.number, i.research_filed)).collect::<Vec<_>>(),
            vec![(9, true), (8, false), (7, true)]
        );
        // The picker partitions this list human-first; verify the partition
        // the picker performs keeps newest-first inside each group.
        let (human, research): (Vec<_>, Vec<_>) =
            got.into_iter().partition(|i| !i.research_filed);
        let order: Vec<u64> = human.into_iter().chain(research).map(|i| i.number).collect();
        assert_eq!(
            order,
            vec![8, 9, 7],
            "human-filed #8 must outrank newer research-filed #9"
        );
    }

    // ---- #676: REST issue-candidate parsing ----

    #[test]
    fn rest_candidates_drop_prs_gave_up_and_body_nulls() {
        let v = serde_json::json!([
            // A PR interleaved by the REST issues endpoint — must be dropped.
            {"number": 10, "title": "a pr", "pull_request": {"url": "x"},
             "user": {"login": "nolanmak"}, "author_association": "OWNER"},
            // Already given up — dropped.
            {"number": 11, "title": "old", "body": "b",
             "labels": [{"name": "agent-gave-up"}],
             "user": {"login": "nolanmak"}, "author_association": "OWNER"},
            // Null body (GitHub sends null, not "") — must not panic.
            {"number": 12, "title": "good", "body": null,
             "labels": [{"name": "bug"}],
             "user": {"login": "nolanmak"}, "author_association": "OWNER"},
            {"number": 13, "title": "second", "body": "text",
             "user": {"login": "stranger"}, "author_association": "NONE"}
        ]);
        let out = rest_issue_candidates(&v);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].number, 12);
        assert_eq!(out[0].body, "");
        assert_eq!(out[0].author, "nolanmak");
        assert_eq!(out[0].association, "OWNER");
        assert_eq!(out[1].number, 13);
        assert_eq!(out[1].association, "NONE");
        // Non-array (error payload) ⇒ empty, not a panic.
        assert!(rest_issue_candidates(&serde_json::json!({"message": "rate limited"})).is_empty());
    }

    // ---- #653: self-triage verdict, complexity gate, QA review parsing ----

    #[test]
    fn scope_header_parses_verdict_and_complexity() {
        let out = parse_scope_output(
            "VERDICT: fixable\nCOMPLEXITY: medium\n\nInterpretation: do X.\nFiles: a.rs",
        );
        assert!(out.fixable);
        assert_eq!(out.complexity, Complexity::Medium);
        assert!(out.body.starts_with("Interpretation:"));
        assert!(!out.body.to_lowercase().contains("verdict:"));

        let out = parse_scope_output("verdict: NOT-FIXABLE\ncomplexity: hard\n\nresearch ask");
        assert!(!out.fixable);
        assert_eq!(out.complexity, Complexity::Hard);
        assert_eq!(out.body, "research ask");

        let out = parse_scope_output("VERDICT: fixable\nCOMPLEXITY: simple\n\nspec");
        assert_eq!(out.complexity, Complexity::Simple);
    }

    #[test]
    fn scope_header_defaults_are_fixable_but_never_automergeable() {
        // Missing/garbled header: still attempt the fix (fixable), but grade
        // hard so a formatting glitch can never unlock auto-merge.
        let out = parse_scope_output("just a spec with no header");
        assert!(out.fixable);
        assert_eq!(out.complexity, Complexity::Hard);
        let out = parse_scope_output("COMPLEXITY: banana\n\nspec");
        assert_eq!(out.complexity, Complexity::Hard);
        // Header lines buried late in the output are body, not directives.
        let late = format!("{}VERDICT: not-fixable", "line\n".repeat(15));
        assert!(parse_scope_output(&late).fixable);
    }

    #[test]
    fn complexity_gates_auto_merge_at_medium() {
        assert!(Complexity::Simple.auto_mergeable());
        assert!(Complexity::Medium.auto_mergeable());
        assert!(!Complexity::Hard.auto_mergeable());
    }

    #[test]
    fn review_verdict_defaults_to_reject() {
        assert!(parse_review_output("REVIEW: approve\n\nchecked X").0);
        assert!(parse_review_output("review: Approve").0);
        assert!(!parse_review_output("REVIEW: reject\n\nno test").0);
        // Unparseable / missing verdict ⇒ reject — an approval can flow
        // straight into an auto-merge, so the default must be the safe side.
        assert!(!parse_review_output("looks good to me").0);
        assert!(!parse_review_output("").0);
        let (_, notes) = parse_review_output("REVIEW: reject\n\nno regression test");
        assert!(notes.contains("no regression test"));
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

    // ---- #816: single-flight lock ----

    #[test]
    fn run_lock_is_exclusive_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/self-improve.lock");

        let first = RunLock::try_acquire(&path).unwrap();
        assert!(first.is_some(), "the first run must take the lock");
        assert!(
            RunLock::try_acquire(&path).unwrap().is_none(),
            "a second run must be turned away, not wedged into the same worktree"
        );

        drop(first);
        assert!(
            RunLock::try_acquire(&path).unwrap().is_some(),
            "the lock must be free again once the holding run finishes"
        );
    }

    // ---- #812: a crashed run must not wedge the loop forever ----

    #[tokio::test]
    async fn reclaim_clears_an_unregistered_leftover_worktree_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("git")
        };
        git(&["init", "-q", "-b", "main", "."]);
        git(&["-c", "user.email=t@e", "-c", "user.name=t", "commit", "-q",
              "--allow-empty", "-m", "init"]);

        // Simulate a run killed mid-flight: the directory survives with no
        // `.git/worktrees` registration at all, so `git worktree remove`
        // cannot clear it.
        let worktree = repo.join(".self-improve-worktrees").join("current");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("half-written.rs"), "fn main() {}\n").unwrap();
        let add = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("git")
        };
        assert!(
            !add(&["worktree", "add", "-b", "agent-fix/issue-1",
                   &worktree.to_string_lossy(), "main"])
                .status
                .success(),
            "precondition: worktree add refuses the existing path"
        );

        reclaim_worktree(repo, &worktree, "agent-fix/issue-1").await;

        assert!(!worktree.exists(), "the leftover directory must be gone");
        assert!(
            add(&["worktree", "add", "-b", "agent-fix/issue-1",
                  &worktree.to_string_lossy(), "main"])
                .status
                .success(),
            "after reclaim the pipeline must be able to create its worktree again"
        );
    }

    #[test]
    fn managed_worktree_is_not_treated_as_user_dirt() {
        assert_eq!(
            unmanaged_dirty_status("?? .self-improve-worktrees/\n"),
            ""
        );
        assert_eq!(
            unmanaged_dirty_status(
                "?? .self-improve-worktrees/\n M crates/augmentagent-cli/src/main.rs\n?? notes.txt\n"
            ),
            " M crates/augmentagent-cli/src/main.rs\n?? notes.txt"
        );
    }
}
