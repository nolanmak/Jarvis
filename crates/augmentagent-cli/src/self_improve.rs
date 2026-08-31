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

/// #814 — override for the persisted daily-run counter (tests).
const COUNTER_FILE_ENV: &str = "AUGMENTAGENT_AUTOPR_COUNTER_FILE";

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

/// #823 — paths whose change demands a human before it ships, mirroring
/// `scripts/agent-pr-verify-gate.sh`.
///
/// That script is a Claude Code **harness** hook: it fires on the harness's
/// `Bash` tool and blocks `gh pr create` unless a real end-to-end receipt
/// exists for the HEAD sha. This pipeline spawns `gh` itself, so the hook has
/// never applied to it — and this pipeline is the only actor that can merge
/// without review. The asymmetry runs the wrong way: the daemon has strictly
/// less ability to verify live behaviour than the human the gate was written
/// for. It cannot poll a real inbox or watch a card render, and its QA pass
/// reads the diff rather than exercising it.
///
/// Matching here does NOT refuse the work — the pipeline may still propose
/// these changes. It withholds auto-merge, exactly as `research_filed`
/// already does, so a human sees the diff before it reaches `main` and
/// deploys. Grading is advisory (`SCOPE_SYSTEM` asks for `hard`, nothing
/// checks it against a diff that doesn't exist yet); this is a check.
///
/// Kept in sync with the shell script by
/// `verify_gated_paths_match_the_pr_hook`.
const VERIFY_GATED_PATHS: &[&str] = &[
    "schema/*.md",
    "skills/*/SKILL.md",
    "skills/*/*.md",
    "skills/*.md",
    "crates/augmentagent-channel-core/src/reasoner.rs",
    "crates/augmentagent-channel-email/src/channel.rs",
    "crates/augmentagent-channel-email/src/outbound.rs",
    "crates/augmentagent-channel-email/src/trigger.rs",
    "crates/augmentagent-channel-core/src/trigger.rs",
    "crates/augmentagent-channel-core/src/governor/*.rs",
    "crates/augmentagent-channel-*/src/sigextract.rs",
    "crates/augmentagent-channel-*/src/tone.rs",
    "crates/augmentagent-approval-discord/src/event_handler.rs",
    "crates/augmentagent-approval-discord/src/loops.rs",
    "crates/augmentagent-approval-discord/src/process_loops.rs",
];

/// Match one path against one `VERIFY_GATED_PATHS` entry. The patterns come
/// from shell `case` globs, where `*` does not cross `/`; mirror that so
/// `crates/augmentagent-channel-*/src/tone.rs` stays one segment wide.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let (pat_segs, path_segs): (Vec<_>, Vec<_>) =
        (pattern.split('/').collect(), path.split('/').collect());
    if pat_segs.len() != path_segs.len() {
        return false;
    }
    pat_segs.iter().zip(path_segs).all(|(p, s)| match p.split_once('*') {
        None => *p == s,
        Some((pre, suf)) => s.len() >= pre.len() + suf.len()
            && s.starts_with(pre)
            && s.ends_with(suf),
    })
}

/// Does this staged file list touch anything the PR-verify hook guards?
fn touches_verify_gated_path(names: &str) -> Option<String> {
    names
        .lines()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .find(|f| VERIFY_GATED_PATHS.iter().any(|p| glob_matches(p, f)))
        .map(str::to_string)
}

/// #819 — the subset of [`BLAST_RADIUS_PATTERNS`] applied to issue *prose*.
///
/// The full list is written for diffs, where every entry is a path fragment
/// out of `git diff`. Matched against an issue's title and body it also hits
/// ordinary English: measured on 2026-08-27, bare `deploy` / `secret` /
/// `.service` / `Cargo.lock` made **20 of 52** eligible open issues invisible
/// to the picker — including plain bug reports, and including three reports
/// about this pipeline that merely quoted the auto-updater's `systemctl`
/// line. A guard that cannot tell "change the deploy path" from "explain what
/// happens after a deploy" is not reading intent, it is reading vocabulary.
///
/// So the prose prefilter keeps only path-shaped tokens — a body containing
/// one is naming a file, not using a word. The prose gate is a cost
/// optimisation (it saves one Fable call); the real barriers are unchanged
/// and all downstream: the scoper is told to return `not-fixable` for
/// deploy/auth/secret/CI work, [`is_blast_radius`] still refuses the produced
/// diff against the FULL list, and #300 means only trusted authors reach any
/// of it unattended.
const ISSUE_BLAST_RADIUS_PATTERNS: &[&str] = &[
    "scripts/check-for-updates",
    "scripts/vault-mount",
    ".github/workflows",
    "auth.rs",
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
/// Applied to DIFFS — use [`is_issue_blast_radius`] for issue prose.
pub fn is_blast_radius(text: &str) -> bool {
    blast_radius_hit(text).is_some()
}

/// The file paths a unified diff touches.
///
/// The guard asks one question — "does this change modify a deploy/auth/secret
/// PATH" — and every entry in [`BLAST_RADIUS_PATTERNS`] is a path fragment.
/// Matching them against diff *content* is a category error, and an expensive
/// one: it discards a completed agentic-Opus build.
///
/// Live evidence, #834 ("Triage should not draft replies to meeting/calendar
/// invites"), a change touching nothing deploy-shaped:
///
/// ```text
/// refused: diff hit the blast-radius guard pattern="deploy"
///   line=+            "Quick question on the deploy",
/// ```
///
/// That is a test fixture's fake email subject. Scanning context lines was the
/// same failure one step earlier — a fix refused for what the code *around* it
/// happened to say.
///
/// Dropping content scanning is not a weakening. A diff that creates a file
/// under a guarded path still shows that path in its `diff --git` header, and
/// secrets in content are the job of `scripts/check-no-personal-data.sh`,
/// which runs as the `pre-commit` hook — linked worktrees share
/// `.git/hooks`, so it covers the agent's commits too. Matching the literal
/// word "secret" was never secret detection; real credentials do not announce
/// themselves.
fn diff_touched_paths(diff: &str) -> String {
    diff.lines()
        .filter(|l| {
            l.starts_with("diff --git ")
                || l.starts_with("+++ ")
                || l.starts_with("--- ")
                || l.starts_with("rename ")
                || l.starts_with("copy ")
        })
        // The pattern list is substrings, so `.env` also matches
        // `.env.example` — a tracked, secret-free documentation file that
        // issues legitimately ask to update ("document every knob in
        // .env.example"). Two live builds on #658 were destroyed by exactly
        // that match. Exempt it by basename; the real `.env` still trips.
        .filter(|l| !l.trim_end().ends_with(".env.example"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Blast-radius check scoped to the PATHS a unified diff touches. Use this for
/// diffs; [`blast_radius_hit`] stays the generic text matcher (issue prose,
/// the multi-repo path).
pub fn blast_radius_hit_in_diff(diff: &str) -> Option<(&'static str, String)> {
    blast_radius_hit(&diff_touched_paths(diff))
}

/// Which pattern tripped, and the line it tripped on.
///
/// The refusal used to read "the produced diff touches a deploy/auth/secret
/// path" and name neither. That is a ~20-minute agentic Opus build discarded
/// with nothing a human can act on — seen live on #831, where the answer
/// ("Landlock code is security code") was only recoverable by re-deriving it
/// by hand. The guard is right to refuse; it should say what it saw.
pub fn blast_radius_hit(text: &str) -> Option<(&'static str, String)> {
    let lower = text.to_ascii_lowercase();
    let pattern = BLAST_RADIUS_PATTERNS
        .iter()
        .find(|p| lower.contains(&p.to_ascii_lowercase()))?;
    let needle = pattern.to_ascii_lowercase();
    let line = text
        .lines()
        .find(|l| l.to_ascii_lowercase().contains(&needle))
        .unwrap_or("")
        .trim();
    Some((pattern, truncate(line, 200)))
}

/// #819 — the prose variant, matching only [`ISSUE_BLAST_RADIUS_PATTERNS`].
pub fn is_issue_blast_radius(text: &str) -> bool {
    issue_blast_radius_hit(text).is_some()
}

/// Prose variant of [`blast_radius_hit`].
pub fn issue_blast_radius_hit(text: &str) -> Option<(&'static str, String)> {
    let lower = text.to_ascii_lowercase();
    let pattern = ISSUE_BLAST_RADIUS_PATTERNS
        .iter()
        .find(|p| lower.contains(&p.to_ascii_lowercase()))?;
    Some((pattern, String::new()))
}

fn contains_any(text: &str, patterns: &[&str]) -> bool {
    let lower = text.to_ascii_lowercase();
    patterns
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
    let ledger = AttemptLedger::load(&attempt_ledger_path());
    let today = utc_day_now();

    let (human, research): (Vec<_>, Vec<_>) = rest_issue_candidates(&issues)
        .into_iter()
        .partition(|i| !i.research_filed);
    for iss in human.into_iter().chain(research) {
        // #851 — an issue already attempted today lost a build to a refusal;
        // retrying it now would almost certainly lose another to the same
        // one. Spend today's remaining budget on DIFFERENT issues; attempts
        // still accumulate across days toward the gave-up label.
        if ledger.attempted_today(today, iss.number) {
            info!(issue = iss.number, "skip: already attempted today (#851 ledger)");
            continue;
        }
        let RestIssue {
            number,
            title,
            body,
            author,
            association,
            research_filed,
        } = iss;
        if let Some((pattern, _)) = issue_blast_radius_hit(&format!("{title} {body}")) {
            // #819 — every other refusal in this pipeline leaves a comment
            // and/or a label. This one used to `continue` silently, so a
            // matched issue stayed in the pool, was re-scanned on every tick
            // forever, and told nobody. Label it out and say why. Neither
            // call touches the reasoner, so this costs no daily budget.
            info!(issue = number, pattern, "refusing: blast-radius path named in issue");
            backoff_comment(
                repo_root,
                number,
                &format!(
                    "Auto-fix triage: this issue names a deploy/auth/secret \
                     path, so the unattended pipeline will not pick it up — \
                     that machinery only changes under human review. Remove \
                     the `{GAVE_UP_LABEL}` label to re-triage it (for \
                     example if the path is only mentioned as background)."
                ),
            )
            .await
            .ok();
            label_gave_up(repo_root, number).await.ok();
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
Your output MUST start with these four lines EXACTLY (then a blank line):\n\
VERDICT: fixable | not-fixable\n\
COMPLEXITY: simple | medium | hard\n\
EST-DIFF-LINES: <your honest estimate of added+removed lines>\n\
GUARDED-PATHS: yes | no\n\
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
EST-DIFF-LINES and GUARDED-PATHS are binding, not advisory. The pipeline \
refuses any diff over 600 changed lines and any diff touching \
deploy/auth/secret/CI paths (systemd units, scripts/check-for-updates, \
.github/workflows, Cargo.lock — i.e. new dependencies, credentials/keyring, \
.env) — AFTER the expensive build. Your two headers are the cheap check that \
runs BEFORE it: if your honest estimate exceeds ~600 lines, or the work \
cannot be done without touching a guarded path, say so and the pipeline \
stops here instead of burning a 20-minute build to learn the same thing. \
(.env.example is exempt — documenting knobs there is fine.) Estimating \
implementation + tests to an order of magnitude is enough.\n\
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
    /// #843 — the scoper's own size estimate. `None` when the header was
    /// missing or unparseable (older prompt, formatting glitch): absence must
    /// not refuse work, only an explicit over-cap estimate may.
    est_diff_lines: Option<usize>,
    /// #843 — the scoper's answer to "would the diff touch a guarded path?".
    /// Defaults to `false` for the same reason.
    guarded_paths: bool,
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
    let mut est_diff_lines: Option<usize> = None;
    let mut guarded_paths = false;
    let mut body_lines: Vec<&str> = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let l = line.trim().to_ascii_lowercase();
        let is_header_zone = i < 10;
        if is_header_zone && l.starts_with("verdict:") {
            fixable = !l.contains("not-fixable") && !l.contains("not fixable");
            continue;
        }
        if is_header_zone && l.starts_with("est-diff-lines:") {
            est_diff_lines = l["est-diff-lines:".len()..]
                .trim()
                .trim_start_matches('~')
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok());
            continue;
        }
        if is_header_zone && l.starts_with("guarded-paths:") {
            guarded_paths = l.contains("yes");
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
        est_diff_lines,
        guarded_paths,
        body: body_lines.join("\n").trim().to_string(),
    }
}

/// #843 — should this scoped issue be refused BEFORE the build?
///
/// Three of six live runs (2026-08-27/28) spent a ~20-minute agentic-Opus
/// build producing a diff a post-build guard then discarded: #831 and #658
/// hit the blast-radius guard, #667 came in at 2203 lines against the 600
/// cap. In every case the scoper had the information — its own prompt states
/// both constraints — but nothing made them binding. This does. The margin
/// (1.5x) tolerates honest underestimates; a scoper predicting ~900+ lines is
/// predicting a refusal, not a rounding error.
fn scope_predicts_refusal(s: &ScopeOutcome) -> Option<String> {
    if s.guarded_paths {
        return Some(
            "the scoping pass judged the fix cannot avoid a deploy/auth/secret \
             path, which the blast-radius guard would refuse after the build"
                .into(),
        );
    }
    match s.est_diff_lines {
        Some(n) if n > MAX_DIFF_LINES * 3 / 2 => Some(format!(
            "the scoping pass estimates a ~{n}-line diff against a \
             {MAX_DIFF_LINES}-line cap; the size guard would refuse it after \
             the build"
        )),
        _ => None,
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

/// Identifiers a diff introduces or changes, for caller lookup (#840).
///
/// Deliberately crude: scan ADDED lines for Rust item keywords and take the
/// name that follows. A missed symbol costs the reviewer one piece of
/// evidence; a wrong one costs a cheap `git grep` that finds nothing.
fn changed_symbols(diff: &str) -> Vec<String> {
    const KEYWORDS: &[&str] = &["fn ", "struct ", "enum ", "trait ", "const ", "static ", "type "];
    let mut out: Vec<String> = Vec::new();
    for line in diff.lines() {
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let body = line[1..].trim_start();
        for kw in KEYWORDS {
            let Some(rest) = body.split(kw).nth(1) else { continue };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            // Single letters are generics (`fn f<T>`), not call sites worth
            // grepping for.
            if name.len() > 2 && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Paths a unified diff touches, as repo-relative strings (#840).
fn changed_files(diff: &str) -> Vec<String> {
    diff.lines()
        .filter_map(|l| l.strip_prefix("+++ b/"))
        .map(str::to_string)
        .collect()
}

/// #840 — pre-computed system context for the independent reviewer.
///
/// Codex cannot run commands on this host: its `-s read-only` sandbox is
/// bubblewrap, and AppArmor blocks unprivileged user namespaces
/// (`apparmor_restrict_unprivileged_userns=1`), so every command dies with
/// `bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`. A reviewer
/// told to "find the callers" therefore finds nothing and, honestly, says so.
///
/// The daemon has no such restriction. So it does the lookup itself and hands
/// the reviewer the evidence: for every identifier the diff introduces, where
/// else in the repo that name appears, excluding the changed files themselves.
/// That is the part of "read the whole system" a text-only model can actually
/// use — it cannot chase an arbitrary hunch, and the prompt says so rather
/// than letting the model imply coverage it does not have.
async fn caller_evidence(worktree: &Path, diff: &str) -> String {
    const MAX_SYMBOLS: usize = 12;
    const MAX_HITS_PER_SYMBOL: usize = 8;

    let touched = changed_files(diff);
    let symbols = changed_symbols(diff);
    if symbols.is_empty() {
        return "No new named items in this diff, so there are no call sites to \
                look up."
            .into();
    }

    let mut sections: Vec<String> = Vec::new();
    for sym in symbols.iter().take(MAX_SYMBOLS) {
        let Ok((true, out, _)) = run(
            "git",
            &["grep", "-n", "--fixed-strings", "--", sym],
            worktree,
        )
        .await
        else {
            continue;
        };
        let hits: Vec<&str> = out
            .lines()
            .filter(|l| {
                // A definition inside the diff's own files is not a caller.
                !touched.iter().any(|f| l.starts_with(&format!("{f}:")))
            })
            .take(MAX_HITS_PER_SYMBOL)
            .collect();
        if hits.is_empty() {
            sections.push(format!(
                "`{sym}` — no references outside the changed files. Either it \
                 is genuinely new, or nothing calls it yet."
            ));
        } else {
            sections.push(format!("`{sym}` is referenced at:\n{}", hits.join("\n")));
        }
    }

    if sections.is_empty() {
        return "Caller lookup produced no results.".into();
    }
    format!(
        "Call sites for the identifiers this diff introduces, pre-computed \
         (up to {MAX_SYMBOLS} symbols, {MAX_HITS_PER_SYMBOL} hits each):\n\n{}",
        sections.join("\n\n")
    )
}

/// #828 — model for the independent codex reviews. Pinned per the #448 rule
/// (no preset may inherit an interactive default); the codex adapter derives
/// its own id from `model_for(Codex, tier_of(opts))`, and a non-`haiku` pin
/// maps to the Quality tier.
const AUTOPR_CODEX_MODEL: &str = "gpt-5.6-terra";

fn codex_model() -> String {
    resolve_model(
        std::env::var("AUGMENTAGENT_AUTOPR_CODEX_MODEL").ok().as_deref(),
        AUTOPR_CODEX_MODEL,
    )
}

/// Text-only preset for the independent reviewer (#840).
///
/// It carries no tools on purpose. Codex has no `Read`/`Grep`/`Glob` — those
/// are Claude Code tool names, and `allowed_tools` is never passed to the
/// codex adapter at all; codex's only tool is a shell governed by `-s`. On
/// this host that shell cannot start, so granting tools would be capability
/// theatre. The reviewer is given the diff and pre-computed call sites as
/// text instead, which is what it can actually act on.
///
/// `cwd` is still pinned to the worktree: it costs nothing and keeps the
/// spawn's working directory off the deploy checkout.
fn codex_review_opts(worktree: PathBuf, system_prompt: &str) -> augmentagent_channel_core::ReasonerOpts {
    augmentagent_channel_core::ReasonerOpts {
        system_prompt: system_prompt.to_string(),
        model: Some(codex_model()),
        // #840 — NO tools. Codex cannot execute anything on this host (its
        // read-only sandbox is bubblewrap; AppArmor blocks unprivileged user
        // namespaces), so tools would be surface with no capability behind
        // it. Everything the reviewer needs is supplied as text, including
        // pre-computed caller evidence. This keeps the preset TextOnly, which
        // codex is cleared for with no policy widening.
        allowed_tools: vec![],
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

const CODEX_DIFF_REVIEW_SYSTEM: &str = "You are an INDEPENDENT reviewer on a \
staged autonomous fix pipeline. A different model wrote the change you are \
about to read; its own QA pass already approved it. You are the second \
opinion, and you were chosen because you do not share that model's blind \
spots. Do not defer to it.\n\
\n\
This pass is the FOCUSED review: judge the diff itself, from what is in this \
message. You have no tools, so reason from the diff as given and say \
`changes-requested` if a decisive fact is missing rather than assuming it.\n\
- Correctness: walk at least one concrete input through the changed lines.\n\
- Best practices: error handling, naming, resource handling, edge cases, \
input validation, concurrency, and anything that will bite in six months. \
Flag it even when it merely matches bad surrounding code — say so if it does.\n\
- Tests: is there a test that would FAIL without this change? A bug fix \
without a regression test is `changes-requested` unless it is genuinely \
untestable.\n\
- Scope: no unrelated edits, no drive-by refactors, no leftover debug output, \
no scratch files.\n\
- Conventions: does it match the surrounding code's idiom and comment density?\n\
\n\
Your output MUST start with this line EXACTLY:\n\
CODEX-REVIEW: lgtm | changes-requested\n\
Then a blank line, then 3-8 sentences: for lgtm, what you verified and how; \
for changes-requested, the concrete defects as file:line. Read-only — do NOT \
edit anything. Output ONLY the verdict line and your notes.";

const CODEX_SYSTEM_REVIEW_SYSTEM: &str = "You are an INDEPENDENT reviewer on a \
staged autonomous fix pipeline, and this is the SYSTEM-INTERACTION pass. A \
separate review already judged the diff on its own terms. Your job is the \
question that one cannot answer from the hunks: what does this change do to \
the rest of the system?\n\
\n\
You have NO tools. Everything you get is in this message: the diff, and a \
pre-computed list of call sites for the identifiers it introduces. Reason from \
that evidence. Do not claim to have inspected anything you were not given, and \
if the decisive fact is genuinely absent, say `changes-requested` and name \
exactly what you would need. Concretely:\n\
- Work through the supplied CALL SITES: for each, does the change still hold \
there?\n\
- Invariants and contracts: does anything elsewhere assume what this change \
just altered — ordering, nullability, a field always being populated, an \
error being unreachable?\n\
- Persistence and schema: if a struct/table/serialized shape changed, what \
happens to rows or queued payloads written by the OLD code?\n\
- Runtime and deploy: this daemon runs unattended against a live inbox. Does \
this change behaviour for traffic nobody asked it to change? Is it hot-path?\n\
- Failure modes: what breaks if this runs when a dependency is down, slow, or \
returns something unexpected?\n\
- Duplication: does this reimplement something the repo already has?\n\
\n\
Your output MUST start with this line EXACTLY:\n\
CODEX-REVIEW: lgtm | changes-requested\n\
Then a blank line, then 3-8 sentences naming the specific callers/invariants \
you checked (file:line, from the supplied evidence) and what you concluded. \
\"I read the diff and it looks fine\" is not a system review. Output ONLY the \
verdict and notes.";

/// Verdict of one codex pass. Unparseable or missing ⇒ not approved, matching
/// `parse_review_output`'s default-reject: an approval here can flow into an
/// auto-merge.
fn parse_codex_review(raw: &str) -> (bool, String) {
    let mut approved = false;
    for (i, line) in raw.lines().enumerate() {
        if i >= 5 {
            break;
        }
        let l = line.trim().to_ascii_lowercase();
        if l.starts_with("codex-review:") {
            approved = l.contains("lgtm") && !l.contains("changes-requested");
            break;
        }
    }
    (approved, raw.trim().to_string())
}

/// Build the one-shot revision prompt: the reviewer's concrete findings,
/// handed back to the builder that wrote the diff.
///
/// This is the loop's missing last mile. Codex requests changes on most
/// first attempts — that is a reviewer doing its job — but until now the
/// pipeline just parked a draft PR and the findings went nowhere: seven
/// drafts accumulated over three days with nobody (human or model) acting
/// on a single finding. One bounded revision converts "codex found issues"
/// into "merged PR" without widening any gate.
fn build_revise_prompt(issue: &Issue, review_notes: &str, current_lines: usize) -> String {
    format!(
        "You previously implemented a fix for GitHub issue #{} ({}) in this \
         worktree. An independent reviewer examined your diff and requested \
         changes. Its findings:\n\n{}\n\nAddress each finding: fix what is \
         real (with a regression test where the finding is bug-shaped), and \
         where a finding is mistaken, add a brief code comment at the \
         relevant site explaining why the behaviour is correct.\n\
         IF THE FINDING IS ARCHITECTURAL — the reviewer shows your APPROACH \
         cannot handle the reported case, not just a detail of it — replace \
         the approach with the smallest correct alternative instead of \
         patching around it. Take a reviewer-proposed alternative seriously; \
         it has repeatedly been the fix. Patching a structurally wrong \
         approach for another round wastes the whole run. Otherwise keep the \
         diff focused; never refactor unrelated code.\n\
         Always add a regression test that reproduces the ORIGINAL reported \
         case verbatim (the exact inputs from the issue, placeholder \
         identities) — if that test cannot pass under your approach, the \
         approach is wrong.\n\
         HARD BUDGET: the total diff may not exceed {MAX_DIFF_LINES} changed \
         lines and is currently at {current_lines}. A revision that grows \
         past the cap is rejected wholesale, discarding all of this work — \
         if you are near the cap, trim (drop non-essential refactors, tighten \
         verbose tests) while keeping the fix and its regression tests. When \
         done, summarize what you changed per finding.",
        issue.number,
        issue.title,
        truncate(review_notes, 4000),
    )
}

/// Synthetic "findings" for a gate-repair round (#873): the last change went
/// RED — a compile error or failing test — and the next round's job is to
/// make it green again. This is the red→fix half of TDD; treating a red gate
/// as terminal discarded ~40-minute runs twice in one evening (#854's
/// calendar doctest, #855's revision that did not compile), when a compile
/// error is precisely the easiest failure to iterate on.
fn gate_findings(err: &str) -> String {
    format!(
        "Your last change FAILED the verification gate — it does not build or \
         its tests fail. Fix exactly this failure; change nothing unrelated. \
         Gate output:\n\n```\n{}\n```",
        truncate(err, 3000)
    )
}

/// Synthetic "findings" for a shrink round: the revision overgrew the cap,
/// and the next round's job is to cut it down, not to add more.
fn shrink_findings(lines: usize) -> String {
    format!(
        "Your last revision grew the diff to {lines} changed lines, over the \
         hard {MAX_DIFF_LINES}-line cap — it cannot ship at this size. Reduce \
         the diff below the cap without losing the fix or its regression \
         tests: drop non-essential refactors, collapse duplicated test \
         scaffolding, and prefer editing existing tests over adding parallel \
         ones."
    )
}

/// #851-follow-up — may this PR auto-merge despite touching a receipt-gated
/// path? Owner policy (2026-08-31): a DOUBLE codex LGTM overrides the
/// receipt gate when `AUGMENTAGENT_AUTOPR_LGTM_OVERRIDES_RECEIPT=1`.
///
/// The receipt gate exists because diffs that read clean can still break
/// live email behaviour (#209/#211/#213); codex reads diffs and cannot
/// exercise an inbox, so this trades that protection for throughput. The
/// owner made that call explicitly; the env flag keeps it one line to
/// revert, and OFF is the safe default for any other deployment.
fn automerge_receipt_ok(gated: Option<&str>, independent_approved: bool, flag: Option<&str>) -> bool {
    match gated {
        None => true,
        Some(_) => independent_approved && automerge_enabled_value(flag),
    }
}

/// Post a short notice to the owner's Discord webhook, best-effort (#851
/// visibility gap: draft PRs sat unseen for three days). A webhook failure
/// must never fail the run — the PR already exists.
async fn notify_discord(text: &str) {
    let Ok(url) = std::env::var("DISCORD_WEBHOOK_URL") else { return };
    if url.trim().is_empty() {
        return;
    }
    let body = serde_json::json!({ "content": truncate(text, 1800) });
    match reqwest::Client::new()
        .post(url.trim())
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => warn!(status = %r.status(), "draft-PR discord notice rejected"),
        Err(e) => warn!("draft-PR discord notice failed: {e}"),
    }
}

/// Issue number encoded in an agent branch name (`agent-fix/issue-845` → 845).
fn issue_from_branch(branch: &str) -> Option<u64> {
    branch.strip_prefix(BRANCH_PREFIX)?.parse().ok()
}

/// `complexity (scoping pass): <grade>` as written into every agent PR body.
/// Unparseable defaults to Hard — the conservative direction, exactly like
/// `parse_scope_output`.
fn complexity_from_pr_body(body: &str) -> Complexity {
    for line in body.lines() {
        let l = line.trim().to_ascii_lowercase();
        if let Some(rest) = l.strip_prefix("- complexity (scoping pass):") {
            return if rest.contains("simple") {
                Complexity::Simple
            } else if rest.contains("medium") {
                Complexity::Medium
            } else {
                Complexity::Hard
            };
        }
    }
    Complexity::Hard
}

/// A sitting draft the loop should pick up: the OLDEST open draft agent PR
/// whose issue has not already been attempted today.
///
/// Until now these were invisible: `has_open_agent_pr` deduplicates the
/// issue out of the pool, so a draft that nobody merged sat forever — seven
/// of them, three days, while the loop only ever started new work. Owner
/// directive 2026-08-31: sitting PRs are picked up too, and since they are
/// mostly-finished work they outrank new issues.
async fn find_resumable_draft(repo_root: &Path) -> Option<(u64, u64, String)> {
    let gh = gh_bin();
    let (ok, stdout, _) = run(
        &gh,
        &["pr", "list", "--state", "open", "--json", "number,isDraft,headRefName"],
        repo_root,
    )
    .await
    .ok()?;
    if !ok {
        return None;
    }
    let prs: serde_json::Value = serde_json::from_str(&stdout).ok()?;
    let ledger = AttemptLedger::load(&attempt_ledger_path());
    let today = utc_day_now();
    prs.as_array()?
        .iter()
        .filter_map(|pr| {
            let draft = pr.get("isDraft")?.as_bool()?;
            let number = pr.get("number")?.as_u64()?;
            let branch = pr.get("headRefName")?.as_str()?;
            let issue = issue_from_branch(branch)?;
            (draft && !ledger.attempted_today(today, issue))
                .then(|| (number, issue, branch.to_string()))
        })
        .min_by_key(|(number, _, _)| *number)
}

/// Outcome of the independent stage (#828).
struct IndependentReview {
    /// False when codex could not be reached at all — NOT the same as a
    /// rejection, and must never be treated as an approval.
    available: bool,
    diff_ok: bool,
    system_ok: bool,
    notes: String,
}

impl IndependentReview {
    fn approved(&self) -> bool {
        self.available && self.diff_ok && self.system_ok
    }

    /// One-line outcome for logs, the dry-run message, and the PR body.
    fn status(&self) -> String {
        if !self.available {
            return "unavailable".into();
        }
        match (self.diff_ok, self.system_ok) {
            (true, true) => "lgtm (diff + system)".into(),
            (true, false) => "changes requested on system interaction".into(),
            (false, true) => "changes requested on the diff".into(),
            (false, false) => "changes requested (diff + system)".into(),
        }
    }

    fn unavailable(reason: String) -> Self {
        Self {
            available: false,
            diff_ok: false,
            system_ok: false,
            notes: format!("Independent review unavailable: {reason}"),
        }
    }
}

/// #828 — two independent codex passes: one focused on the diff, one on how
/// the change lands in the rest of the system.
///
/// Pinned to codex via `build_pinned`, which returns `None` rather than
/// falling back. That is the whole point: `build_reasoner` would hand back
/// Claude, and an "independent" review served by the author's own model is
/// worse than none, because the PR would claim a second opinion it never got.
async fn independent_review(
    issue: &Issue,
    summary: &str,
    diff: &str,
    worktree: PathBuf,
) -> IndependentReview {
    let Some(reasoner) = augmentagent_channel_core::build_pinned(
        augmentagent_channel_core::ProviderKind::Codex,
    ) else {
        return IndependentReview::unavailable(
            "codex is not installed or not authenticated (`codex login`)".into(),
        );
    };

    let context = format!(
        "GitHub issue #{}: {}\n\n{}\n\nThe author model's own summary of its \
         change:\n{}\n\nThe complete staged diff follows. The full repository \
         is NOT browsable from here — work from what is quoted below.\n\n\
         ```diff\n{}\n```",
        issue.number,
        issue.title,
        truncate(&issue.body, 4000),
        truncate(summary, 2000),
        truncate(diff, 60_000),
    );

    let mut out = IndependentReview {
        available: true,
        diff_ok: false,
        system_ok: false,
        notes: String::new(),
    };

    let evidence = caller_evidence(&worktree, diff).await;
    let system_context = format!(
        "{context}\n\n## Pre-computed call sites\n{evidence}"
    );

    let passes = [
        ("focused diff review", CODEX_DIFF_REVIEW_SYSTEM, &context),
        ("system-interaction review", CODEX_SYSTEM_REVIEW_SYSTEM, &system_context),
    ];
    let mut sections: Vec<String> = Vec::new();
    for (label, system, prompt) in passes {
        let opts = codex_review_opts(worktree.clone(), system);
        match reasoner.call(&opts, prompt).await {
            Ok(raw) => {
                let (ok, notes) = parse_codex_review(&raw);
                info!(issue = issue.number, pass = label, approved = ok, "codex review");
                if label.starts_with("focused") {
                    out.diff_ok = ok;
                } else {
                    out.system_ok = ok;
                }
                sections.push(format!("### Codex — {label}\n{}", truncate(&notes, 1500)));
            }
            Err(e) => {
                // Provider-side failure is "no independent review", never an
                // approval and never a rejection of the diff.
                warn!(issue = issue.number, pass = label, "codex review failed: {e:#}");
                return IndependentReview::unavailable(format!("{label} failed: {e}"));
            }
        }
    }
    out.notes = sections.join("\n\n");
    out
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
/// #828 — does an independent LGTM also release the `hard` complexity band?
/// Off by default: turning it on is the decision that lets high-blast-radius
/// work merge with no human, on the strength of two independent reviews.
/// Max revision rounds per run (owner directive 2026-08-31: iterate until
/// LGTM). Default 3; `AUGMENTAGENT_AUTOPR_REVISE_ROUNDS` tunes it, clamped
/// to 5 — each round costs a build-tier call, a full gate run, and two codex
/// calls, and a reviewer/builder pair that has not converged in five rounds
/// is disagreeing, not iterating. `AUGMENTAGENT_AUTOPR_REVISE=0` still
/// disables outright. Exhausted rounds land as a draft carrying every
/// round's verdict, so the disagreement is legible to the human who inherits
/// it.
fn revise_rounds() -> u32 {
    if !revise_enabled() {
        return 0;
    }
    std::env::var("AUGMENTAGENT_AUTOPR_REVISE_ROUNDS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(3)
        .min(5)
}

/// Should sitting draft PRs be resumed before new issues? Default ON (#866).
/// `AUGMENTAGENT_AUTOPR_RESUME_FIRST=0` flips a run to new-issues-first — the
/// owner's "pick up new issues right now" lever, exported per-invocation for
/// a manual run without changing the daemon's default.
fn resume_first_enabled() -> bool {
    !matches!(
        std::env::var("AUGMENTAGENT_AUTOPR_RESUME_FIRST").ok().as_deref().map(str::trim),
        Some("0") | Some("false") | Some("FALSE")
    )
}

/// One revision pass against codex findings — default ON, `=0` disables.
fn revise_enabled() -> bool {
    !matches!(
        std::env::var("AUGMENTAGENT_AUTOPR_REVISE").ok().as_deref().map(str::trim),
        Some("0") | Some("false") | Some("FALSE")
    )
}

fn codex_unlocks_hard() -> bool {
    automerge_enabled_value(
        std::env::var("AUGMENTAGENT_AUTOPR_CODEX_UNLOCKS_HARD").ok().as_deref(),
    )
}

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
- Clean up after yourself: delete any scratch script, scratch note, or dump \
file you create. Everything still in the worktree when you finish is staged \
and lands in the PR.\n\
- Do NOT touch deploy/auth/secret/CI files (systemd units, scripts/check-for-updates, \
.github/workflows, anything with credentials/keyring/.env).\n\
- Keep the diff small and focused on the issue.\n\
- Test fixtures must use INVENTED placeholder emails, names, and handles \
(alice@example.com, not a real address quoted in the issue). A pre-commit \
hook rejects real-looking personal data and the commit will fail after all \
your work.\n\
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

/// Paths whose change makes the Node build load-bearing (#820). `src/` is the
/// TypeScript daemon + Express dashboard; the rest are what `npm run build`
/// (`tailwind:build && tsc`) actually consumes.
const NODE_GATE_PATHS: &[&str] = &[
    "src/",
    "views/",
    "test/",
    "sidecars/",
    "public/",
    "package.json",
    "package-lock.json",
    "tsconfig.json",
    "tailwind.config.js",
    "tailwind.input.css",
];

/// True if `path` is one the Node build would compile.
fn is_node_path(path: &str) -> bool {
    NODE_GATE_PATHS
        .iter()
        .any(|p| if p.ends_with('/') { path.starts_with(p) } else { path == *p })
}

/// Does the staged diff reach Node? Unknowable ⇒ `true`: skipping the gate is
/// the failure this replaces, so an unreadable diff must cost an install, not
/// a missed build.
async fn node_build_required(worktree: &Path) -> bool {
    match run("git", &["diff", "--cached", "--name-only"], worktree).await {
        Ok((true, out, _)) => out.lines().map(str::trim).any(is_node_path),
        _ => true,
    }
}

/// The workspace crates a change set touches, as `-p`-able package names —
/// or `None` when anything falls outside `crates/`, which means only the
/// full gate can vouch for it (Node sources, schema, scripts).
///
/// Crate directory names ARE the package names in this workspace
/// (`crates/augmentagent-cli` ⇒ package `augmentagent-cli`).
fn changed_crates(paths: &str) -> Option<Vec<String>> {
    let mut crates: Vec<String> = Vec::new();
    for path in paths.lines().map(str::trim).filter(|p| !p.is_empty()) {
        let mut segs = path.split('/');
        match (segs.next(), segs.next(), segs.next()) {
            (Some("crates"), Some(dir), Some(_)) => {
                if !crates.iter().any(|c| c == dir) {
                    crates.push(dir.to_string());
                }
            }
            // Workspace manifests change dependency resolution everywhere.
            _ => return None,
        }
    }
    (!crates.is_empty()).then_some(crates)
}

/// Build + test ONLY the given crates (#870). Used between revision rounds,
/// where the full single-threaded workspace suite (~5 min) was re-verifying
/// 30+ untouched crates after every round — ~20 of the 49 minutes of a
/// three-round run. The FULL gate still runs exactly once before anything is
/// pushed or merged; this trims the redundancy, not the bar.
async fn verification_gate_targeted(worktree: &Path, crates: &[String]) -> Result<()> {
    let env = gate_env();
    let pkgs: String = crates
        .iter()
        .map(|c| format!("-p {c}"))
        .collect::<Vec<_>>()
        .join(" ");
    info!(%pkgs, "verification gate (targeted): cargo build");
    let (ok, _o, e) = run_sandboxed(
        "bash",
        &["-lc", &gate_sh(&format!(". $HOME/.cargo/env && cargo build {pkgs} 2>&1 | tail -5"))],
        worktree,
        &env,
    )
    .await?;
    if !ok {
        bail!("targeted cargo build failed:\n{o}{e}", o = _o.trim());
    }
    info!(%pkgs, "verification gate (targeted): cargo test");
    let (ok, _o, e) = run_sandboxed(
        "bash",
        &["-lc", &gate_sh(&format!(
            ". $HOME/.cargo/env && cargo test {pkgs} -- --test-threads=1 2>&1 | tail -8"
        ))],
        worktree,
        &env,
    )
    .await?;
    if !ok {
        bail!("targeted cargo test failed:\n{o}{e}", o = _o.trim());
    }
    Ok(())
}

/// Gate for an intermediate revision round: targeted when the diff stays
/// inside `crates/`, full otherwise.
async fn gate_for_round(worktree: &Path, changed_paths: &str) -> Result<()> {
    match changed_crates(changed_paths) {
        Some(crates) => verification_gate_targeted(worktree, &crates).await,
        None => verification_gate(worktree).await,
    }
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
    // #820 — the Node half of the gate. It used to require `node_modules` to
    // already exist in the worktree, which `git worktree add` can never
    // produce (it is gitignored): `npm run build` had therefore run ZERO
    // times since #103, while `src/` stayed fully editable by the builder.
    // Install on demand instead, and only when the change actually reaches
    // Node — a Rust-only fix, which is nearly all of them, still skips it.
    if worktree.join("package.json").exists() && node_build_required(worktree).await {
        info!("verification gate: npm ci + npm run build (sandboxed env)");
        let (ok, _o, e) = run_sandboxed(
            "bash",
            &["-lc", &gate_sh("npm ci --no-audit --no-fund 2>&1 | tail -5")],
            worktree,
            &env,
        )
        .await?;
        if !ok {
            bail!("npm ci failed:\n{o}{e}", o = _o.trim());
        }
        let (ok, _o, e) =
            run_sandboxed("bash", &["-lc", &gate_sh("npm run build 2>&1 | tail -5")], worktree, &env)
                .await?;
        if !ok {
            bail!("npm run build failed:\n{o}{e}", o = _o.trim());
        }
    } else {
        info!("verification gate: npm build not required (diff touches no Node paths)");
    }
    Ok(())
}

/// #817 — delete untracked files the builder left at the worktree ROOT,
/// returning what was dropped.
///
/// Both `git add -A` calls in `run_once` sweep the whole worktree: whatever
/// the builder leaves behind is staged, measured by the size cap, scanned by
/// the blast-radius guard, and — if neither trips — committed, pushed, and
/// (owner-authored, graded ≤medium) auto-merged to `main` and deployed.
/// Observed live on the #811 run: the builder wrote `.aa811_check.txt` and
/// `.aa811_fix.py` into the worktree root. It happened to clean up; nothing
/// required it to, and `Write` + `Bash(npm *)` are both in its toolset.
///
/// Root depth is the discriminator, and a deliberately narrow one: scratch
/// lands in the process cwd, which is the worktree root, while this repo's
/// real source lives under `crates/`, `src/`, `scripts/`, `schema/`,
/// `skills/`, `views/`. Nested files are never touched, so a legitimately
/// created source file is safe. Every drop is logged — a fix that genuinely
/// needed a new root-level file will say so in `stderr.log` rather than
/// vanishing.
async fn drop_root_scratch(worktree: &Path) -> Vec<String> {
    let Ok((true, out, _)) = run(
        "git",
        &["ls-files", "--others", "--exclude-standard"],
        worktree,
    )
    .await
    else {
        return Vec::new();
    };
    let mut dropped = Vec::new();
    for path in out.lines().map(str::trim).filter(|p| !p.is_empty()) {
        // Nested paths are the builder's real work; only the root is scratch.
        if path.contains('/') {
            continue;
        }
        if tokio::fs::remove_file(worktree.join(path)).await.is_ok() {
            warn!(file = %path, "dropped builder scratch left at the worktree root");
            dropped.push(path.to_string());
        }
    }
    dropped
}

/// Resume a sitting draft PR (#866): re-review it against today's `main`,
/// revise against fresh findings, and merge on a double LGTM.
///
/// Deliberately re-reviews rather than trusting anything recorded on the PR:
/// `main` has moved since the draft opened, the guards have changed, and the
/// findings that held it may be stale. Round 0 costs two codex calls and no
/// build — a draft that is already good merges without spending the builder
/// at all.
async fn resume_draft_pr(
    repo_root: &Path,
    reasoner: &Arc<FallbackReasoner>,
    pr: u64,
    issue_no: u64,
    branch: &str,
    dry_run: bool,
) -> Result<RunReport> {
    let gh = gh_bin();
    info!(pr, issue = issue_no, %branch, "resuming sitting draft PR");

    // Whatever happens next counts as today's attempt on this issue, so a
    // failing resume moves on to other work instead of re-running every tick.
    AttemptLedger::mark_persist(&attempt_ledger_path(), utc_day_now(), issue_no);

    // The issue, for trust + prompts. Refuse untrusted authors exactly like
    // the fresh path (#300) — the PR was created from this issue's text.
    let (ok, ibody, _) = run(
        &gh,
        &["api", &format!("repos/{{owner}}/{{repo}}/issues/{issue_no}")],
        repo_root,
    )
    .await?;
    if !ok {
        bail!("resume: could not fetch issue #{issue_no}");
    }
    let iv: serde_json::Value = serde_json::from_str(&ibody).context("parse issue")?;
    let author = iv.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let association = iv.get("author_association").and_then(|v| v.as_str()).unwrap_or("");
    let allowlist = trusted_authors(repo_root).await;
    if !author_is_trusted(&author, association, &allowlist) {
        warn!(pr, issue = issue_no, %author, "resume refused: untrusted issue author");
        return Ok(RunReport::triage(format!(
            "PR #{pr}: resume refused — untrusted author '{author}'"
        )));
    }
    let issue = Issue {
        number: issue_no,
        title: iv.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        body: iv.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        author,
        author_trusted: true,
        research_filed: is_research_filed(
            iv.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        ),
    };
    let (_ok, prbody_raw, _) = run(
        &gh,
        &["pr", "view", &pr.to_string(), "--json", "body"],
        repo_root,
    )
    .await?;
    let complexity = serde_json::from_str::<serde_json::Value>(&prbody_raw)
        .ok()
        .and_then(|v| v.get("body").and_then(|b| b.as_str()).map(complexity_from_pr_body))
        .unwrap_or(Complexity::Hard);

    // Worktree from the PR's branch, brought up to date with main. A merge
    // conflict is a human's job — say so on the PR and move on.
    let worktree = repo_root
        .join(".self-improve-worktrees")
        .join(lane_from_env().worktree_name());
    reclaim_worktree(repo_root, &worktree, branch).await;
    let _ = run("git", &["fetch", "origin", branch], repo_root).await?;
    let (ok, _o, e) = run(
        "git",
        &[
            "worktree", "add", "-b", branch,
            &worktree.to_string_lossy(),
            &format!("origin/{branch}"),
        ],
        repo_root,
    )
    .await?;
    if !ok {
        bail!("resume: worktree add from origin/{branch} failed: {e}");
    }
    let cleanup = |wt: PathBuf, br: String, root: PathBuf| async move {
        let _ = run("git", &["worktree", "remove", "--force", &wt.to_string_lossy()], &root).await;
        let _ = run("git", &["branch", "-D", &br], &root).await;
    };
    let (ok, _o, _e) = run("git", &["merge", "--no-edit", "origin/main"], &worktree).await?;
    if !ok {
        let _ = run("git", &["merge", "--abort"], &worktree).await;
        cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
        let _ = run(
            &gh,
            &["pr", "comment", &pr.to_string(), "--body",
              "Auto-resume: this branch no longer merges cleanly with `main`; \
               it needs a human rebase before the loop can pick it up again."],
            repo_root,
        )
        .await;
        return Ok(RunReport::triage(format!(
            "PR #{pr}: resume skipped — merge conflict with main"
        )));
    }

    let git_name = std::env::var("AUGMENTAGENT_GIT_AUTHOR_NAME")
        .unwrap_or_else(|_| "AugmentAgent".to_string());
    let git_email = std::env::var("AUGMENTAGENT_GIT_AUTHOR_EMAIL")
        .unwrap_or_else(|_| "augmentagent@localhost".to_string());
    let name_arg = format!("user.name={git_name}");
    let email_arg = format!("user.email={git_email}");

    let mut rounds_done = 0u32;
    let mut notes_log: Vec<String> = Vec::new();
    let mut summary = format!("Resumed sitting draft PR #{pr}.");
    loop {
        // The PR's actual contribution, freshly computed each round.
        let (_ok, diff, _) =
            run("git", &["diff", "origin/main...HEAD"], &worktree).await?;
        // Guards re-run every round — the rules have changed since the draft
        // opened, and a revision can add lines or touch new paths.
        if let Some((pattern, line)) = blast_radius_hit_in_diff(&diff) {
            cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
            let _ = run(
                &gh,
                &["pr", "comment", &pr.to_string(), "--body",
                  &format!("Auto-resume refused: the diff touches `{pattern}`:\n```\n{line}\n```")],
                repo_root,
            )
            .await;
            return Ok(RunReport::built(format!(
                "PR #{pr}: resume refused — blast radius on `{pattern}`"
            )));
        }
        let lines_now = diff_line_count(&diff);
        if lines_now > MAX_DIFF_LINES {
            // A revision that grew past the cap gets a SHRINK round while
            // budget remains — refusing outright here discarded every round
            // of paid work on the live #839 resume. Codex is not consulted
            // on an oversized diff; the next round's only job is to cut.
            if rounds_done >= revise_rounds() {
                cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
                let _ = run(
                    &gh,
                    &["pr", "comment", &pr.to_string(), "--body",
                      &format!("Auto-resume refused: diff is {lines_now} lines \
                                (cap {MAX_DIFF_LINES}) and the revision budget \
                                is exhausted.")],
                    repo_root,
                )
                .await;
                record_attempt(repo_root, issue.number).await.ok();
                return Ok(RunReport::built(format!(
                    "PR #{pr}: resume refused — oversized after revisions"
                )));
            }
            rounds_done += 1;
            info!(pr, issue = issue.number, round = rounds_done, lines_now, "resume: shrink round");
            match reasoner
                .call(
                    &fix_opts(worktree.clone()),
                    &build_revise_prompt(&issue, &shrink_findings(lines_now), lines_now),
                )
                .await
            {
                Ok(rs) => {
                    let _ = drop_root_scratch(&worktree).await;
                    let _ = run("git", &["add", "-A"], &worktree).await?;
                    let msg = format!("review round {rounds_done}: reduce diff below the size cap");
                    let _ = run(
                        "git",
                        &["-c", &name_arg, "-c", &email_arg, "commit", "--allow-empty", "-m", &msg],
                        &worktree,
                    )
                    .await?;
                    summary = format!("{summary}\nRound {rounds_done} (shrink): {}", truncate(&rs, 200));
                    continue;
                }
                Err(e) => {
                    warn!(pr, "resume shrink round failed: {e:#}");
                    cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
                    return Ok(RunReport::built(format!(
                        "PR #{pr}: resume refused — oversized, shrink errored"
                    )));
                }
            }
        }
        // Round 0 earns the FULL suite (first verification of an old branch
        // against today's main); later rounds verify the changed crates
        // (#870) — the full gate runs once more before any merge.
        let gate_result = if rounds_done == 0 {
            verification_gate(&worktree).await
        } else {
            let (_ok, names, _) =
                run("git", &["diff", "--name-only", "origin/main...HEAD"], &worktree).await?;
            gate_for_round(&worktree, &names).await
        };
        if let Err(gate_err) = gate_result {
            // #873 — a red gate gets a repair round while budget remains.
            // Round 0 red means the draft rotted against today's main; a
            // later red means the revision broke it. Both are exactly the
            // failure a builder can iterate on (it sees the compiler/test
            // output verbatim), and terminal-refusing here discarded two
            // ~40-minute runs in one evening.
            if rounds_done < revise_rounds() {
                rounds_done += 1;
                info!(pr, issue = issue.number, round = rounds_done, "resume: gate-repair round");
                match reasoner
                    .call(
                        &fix_opts(worktree.clone()),
                        &build_revise_prompt(&issue, &gate_findings(&gate_err.to_string()), lines_now),
                    )
                    .await
                {
                    Ok(rs) => {
                        let _ = drop_root_scratch(&worktree).await;
                        let _ = run("git", &["add", "-A"], &worktree).await?;
                        let msg =
                            format!("review round {rounds_done}: repair the verification gate");
                        let _ = run(
                            "git",
                            &["-c", &name_arg, "-c", &email_arg, "commit", "--allow-empty", "-m", &msg],
                            &worktree,
                        )
                        .await?;
                        summary = format!(
                            "{summary}\nRound {rounds_done} (gate repair): {}",
                            truncate(&rs, 200)
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(pr, "gate-repair round failed: {e:#}");
                    }
                }
            }
            cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
            let _ = run(
                &gh,
                &["pr", "comment", &pr.to_string(), "--body",
                  &format!("Auto-resume: verification gate failed against current \
                            `main` and the repair budget is exhausted:\n```\n{}\n```",
                           truncate(&gate_err.to_string(), 1200))],
                repo_root,
            )
            .await;
            record_attempt(repo_root, issue.number).await.ok();
            return Ok(RunReport::built(format!("PR #{pr}: resume gate failed")));
        }

        let independent = independent_review(&issue, &summary, &diff, worktree.clone()).await;
        notes_log.push(format!(
            "round {rounds_done}: {}",
            independent.status()
        ));
        if independent.approved() {
            if rounds_done > 0 {
                // Revisions were verified crate-targeted; nothing merges
                // without the full suite passing once (#870).
                if let Err(gate_err) = verification_gate(&worktree).await {
                    cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
                    let _ = run(
                        &gh,
                        &["pr", "comment", &pr.to_string(), "--body",
                          &format!("Auto-resume: double LGTM, but the final \
                                    full-workspace gate failed:\n```\n{}\n```",
                                   truncate(&gate_err.to_string(), 1200))],
                        repo_root,
                    )
                    .await;
                    record_attempt(repo_root, issue.number).await.ok();
                    return Ok(RunReport::built(format!(
                        "PR #{pr}: LGTM but final full gate failed"
                    )));
                }
            }
            if dry_run {
                cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
                return Ok(RunReport::built(format!(
                    "PR #{pr}: DRY RUN — double LGTM after {rounds_done} revision round(s), would merge"
                )));
            }
            // Push whatever the rounds committed (the merge-with-main commit
            // included), surface the verdicts, and merge per policy.
            let (ok, _o, e) = run("git", &["push", "origin", branch], &worktree).await?;
            if !ok {
                cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
                bail!("resume: push failed: {e}");
            }
            let _ = run(
                &gh,
                &["pr", "comment", &pr.to_string(), "--body",
                  &format!("Auto-resume: double codex LGTM after {rounds_done} \
                            revision round(s) against current `main`.\n\n{}",
                           truncate(&independent.notes, 2500))],
                repo_root,
            )
            .await;
            let enabled = automerge_enabled_value(
                std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE").ok().as_deref(),
            );
            let complexity_ok = complexity.auto_mergeable() || codex_unlocks_hard();
            if !(enabled && complexity_ok && !issue.research_filed) {
                cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
                notify_discord(&format!(
                    "📝 resumed draft has double LGTM but needs a human merge: {} — PR #{pr}",
                    issue.title
                ))
                .await;
                return Ok(RunReport::built(format!(
                    "PR #{pr}: double LGTM, left for human merge (policy)"
                )));
            }
            let _ = run(&gh, &["pr", "ready", &pr.to_string()], repo_root).await;
            let (ok, _o, e) = run(
                &gh,
                &["pr", "merge", &pr.to_string(), "--squash", "--delete-branch"],
                repo_root,
            )
            .await?;
            cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
            if !ok {
                warn!(pr, "resume auto-merge failed; PR left ready: {e}");
                return Ok(RunReport::built(format!(
                    "PR #{pr}: double LGTM but merge FAILED (left open)"
                )));
            }
            notify_discord(&format!("✅ resumed draft merged: {} — PR #{pr}", issue.title)).await;
            return Ok(RunReport::built(format!("PR #{pr}: resumed and MERGED")));
        }

        if rounds_done >= revise_rounds() {
            // Out of rounds: publish the improved state + verdicts, stay draft.
            let _ = run("git", &["push", "origin", branch], &worktree).await;
            let _ = run(
                &gh,
                &["pr", "comment", &pr.to_string(), "--body",
                  &format!("Auto-resume: no double LGTM after {rounds_done} revision \
                            round(s) ({}). Revisions were verified against the \
                            changed crates' tests; the full workspace suite \
                            runs before any merge. Latest findings:\n\n{}",
                           notes_log.join("; "),
                           truncate(&independent.notes, 2500))],
                repo_root,
            )
            .await;
            cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
            record_attempt(repo_root, issue.number).await.ok();
            notify_discord(&format!(
                "📝 resumed draft still needs review after {rounds_done} rounds: {} — PR #{pr}",
                issue.title
            ))
            .await;
            return Ok(RunReport::built(format!(
                "PR #{pr}: resumed, no LGTM after {rounds_done} rounds; still draft"
            )));
        }

        rounds_done += 1;
        info!(pr, issue = issue.number, round = rounds_done, "resume: revising against findings");
        let rev_summary = match reasoner
            .call(
                &fix_opts(worktree.clone()),
                &build_revise_prompt(&issue, &independent.notes, lines_now),
            )
            .await
        {
            Ok(rs) => rs,
            Err(e) => {
                warn!(pr, "resume revision failed; leaving draft as-is: {e:#}");
                cleanup(worktree, branch.to_string(), repo_root.to_path_buf()).await;
                return Ok(RunReport::built(format!(
                    "PR #{pr}: resume revision errored; draft unchanged"
                )));
            }
        };
        let _ = drop_root_scratch(&worktree).await;
        let _ = run("git", &["add", "-A"], &worktree).await?;
        let msg = format!("review round {rounds_done}: address independent findings");
        let _ = run(
            "git",
            &["-c", &name_arg, "-c", &email_arg, "commit", "--allow-empty", "-m", &msg],
            &worktree,
        )
        .await?;
        summary = format!("{summary}\nRound {rounds_done}: {}", truncate(&rev_summary, 300));
    }
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

/// Which half of the pipeline this invocation runs (#871).
///
/// The owner has a PR backlog AND an issue backlog, and one serialized
/// pipeline forces them to queue behind each other. Lanes let a REVIEW
/// process (resume sitting drafts) and a BUILD process (new issues) run
/// concurrently: each lane has its own flock and its own worktree path, so
/// the #816 mutual-destruction hazard cannot occur between them, while two
/// runs of the SAME lane still exclude each other.
///
/// Cross-lane overlap is safe by construction: the resume lane only touches
/// issues that HAVE an open agent PR, the build lane's `pick_issue` skips
/// exactly those (dedup), and the attempt ledger writes are flocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// Default: resume a sitting draft if one exists, else build new work.
    Combined,
    /// Only resume sitting draft PRs; idle when none are eligible.
    Resume,
    /// Only build new issues; never resumes.
    Build,
}

/// `AUGMENTAGENT_AUTOPR_LANE` = `resume` | `build`; anything else (and the
/// daemon default) is [`Lane::Combined`].
fn lane_from_env() -> Lane {
    match std::env::var("AUGMENTAGENT_AUTOPR_LANE")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("resume") => Lane::Resume,
        Some("build") => Lane::Build,
        _ => Lane::Combined,
    }
}

impl Lane {
    /// Worktree directory name under `.self-improve-worktrees/`.
    fn worktree_name(self) -> &'static str {
        match self {
            // Combined and Build share a name deliberately: they also share
            // a lock, so they can never run concurrently.
            Lane::Combined | Lane::Build => "current",
            Lane::Resume => "resume",
        }
    }

    /// Suffix distinguishing this lane's lock file.
    fn lock_suffix(self) -> &'static str {
        match self {
            Lane::Combined | Lane::Build => "",
            Lane::Resume => "-resume",
        }
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
    let name = format!("self-improve{}.lock", lane_from_env().lock_suffix());
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/state/augmentagent").join(&name))
        .unwrap_or_else(|| PathBuf::from(format!(".{name}")))
}

/// What one `run_once` did, and whether it should cost daily budget.
///
/// The cap exists to bound spend against the owner's Claude subscription, and
/// the expensive part of a run is the BUILD call (Opus, agentic, ~20 minutes).
/// A run that stopped at triage — untrusted author, or a scoping pass that
/// returned `not-fixable` — spent at most one cheap Fable call and finished in
/// about thirty seconds. Billing those the same as a full build is what let a
/// single day of refusals consume the entire budget: on 2026-08-27 the loop
/// spent slot 2 of 3 labelling #828 out, a 30-second decision.
///
/// With 100 issues open and 50 already labelled `agent-gave-up`, a pool that
/// is mostly refusal-bound would otherwise take weeks to drain at three
/// refusals a day.
pub struct RunReport {
    pub message: String,
    /// True when the builder ran. Only these consume daily budget.
    pub billed: bool,
}

impl RunReport {
    fn idle() -> Self {
        Self { message: IDLE_MSG.to_string(), billed: false }
    }
    fn triage(message: String) -> Self {
        Self { message, billed: false }
    }
    fn built(message: String) -> Self {
        Self { message, billed: true }
    }
    fn is_idle(&self) -> bool {
        self.message == IDLE_MSG
    }
}

impl std::fmt::Display for RunReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Drive one self-improvement attempt. `dry_run` stops before opening the PR
/// (prints what it would do) so the loop can be exercised safely.
pub async fn run_once(repo_root: &Path, dry_run: bool) -> Result<RunReport> {
    // #816 — single-flight. Held for the whole run; dropped on every exit
    // path. A losing run reports idle so it consumes no daily budget.
    let _lock = match RunLock::try_acquire(&run_lock_path())? {
        Some(l) => l,
        None => {
            info!("self-improve: another run holds the lock; skipping this tick");
            return Ok(RunReport::idle());
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

    // #866 — sitting draft PRs outrank new work: they are mostly-finished
    // diffs that only lack an approving review, and until now the dedup guard
    // made them permanently invisible (seven drafts, three days, zero
    // merges). Resume the oldest one not yet attempted today.
    // #871 — lanes: the resume lane does ONLY this (idling when no draft is
    // eligible), the build lane skips it entirely, and the default combined
    // lane keeps resume-first (still flippable via the knob).
    let lane = lane_from_env();
    if lane != Lane::Build && resume_first_enabled() {
        if let Some((pr, issue_no, resume_branch)) = find_resumable_draft(repo_root).await {
            let reasoner = build_reasoner();
            return resume_draft_pr(repo_root, &reasoner, pr, issue_no, &resume_branch, dry_run)
                .await;
        }
    }
    if lane == Lane::Resume {
        return Ok(RunReport::idle());
    }

    let Some(issue) = pick_issue(repo_root).await? else {
        return Ok(RunReport::idle());
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
        return Ok(RunReport::triage(format!(
            "issue #{}: refused — untrusted author '{}' (requires owner approval)",
            issue.number, issue.author
        )));
    }

    let branch = format!("{BRANCH_PREFIX}{}", issue.number);
    // #692 — a FIXED path, force-recreated per issue (the branch stays
    // per-issue). Test binaries bake `env!("CARGO_MANIFEST_DIR")` at compile
    // time; with the shared gate target cache, binaries compiled under a
    // deleted per-issue path get reused from later runs and panic reading
    // fixtures. A stable path keeps every baked path resolvable.
    let worktree = repo_root
        .join(".self-improve-worktrees")
        .join(lane_from_env().worktree_name());

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
            return Ok(RunReport::triage(format!(
                "issue #{}: scoped as not agent-fixable — labeled out",
                issue.number
            )));
        }
    }
    // #843 — the scoper's own predictions, made binding. A refusal here has
    // spent one cheap Fable call and ~30 seconds; the same refusal after the
    // build costs a ~20-minute agentic-Opus run. Three of six live runs paid
    // the expensive version of this exact conclusion.
    if let Some(sc) = &scope {
        if let Some(reason) = scope_predicts_refusal(sc) {
            cleanup(worktree, branch, repo_root.to_path_buf()).await;
            backoff_comment(
                repo_root,
                issue.number,
                &format!(
                    "Auto-fix triage: refused before building — {reason}.\n\n\
                     The work itself may be sound; it does not fit the \
                     unattended pipeline's bounds (focused diff ≤ 600 lines, \
                     no deploy/auth/secret/CI paths). Splitting it into \
                     smaller issues usually resolves the size case. Remove \
                     the `{GAVE_UP_LABEL}` label to re-triage.\n\n\
                     Scoping spec, for whoever picks this up:\n\n{}",
                    truncate(&sc.body, 1500)
                ),
            )
            .await
            .ok();
            label_gave_up(repo_root, issue.number).await.ok();
            return Ok(RunReport::triage(format!(
                "issue #{}: refused pre-build ({reason}) — labeled out",
                issue.number
            )));
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
    let mut summary = match reasoner.call(&opts, &prompt).await {
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
    let mut dropped = drop_root_scratch(&worktree).await;
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
        return Ok(RunReport::built(format!(
            "issue #{}: reasoner made no changes; skipped (attempt {attempts})",
            issue.number
        )));
    }

    // Blast-radius + size guard on the actual diff. Each refusal burned a
    // full reasoner run, so it counts as an attempt (#630): a different
    // rollout MAY produce an acceptable diff, but after MAX_ATTEMPTS the
    // gave-up label pulls the issue from the pool — otherwise the unattended
    // loop would re-spend its whole daily cap on the same issue forever.
    let (_ok, full_diff, _) = run("git", &["diff", "--cached"], &worktree).await?;
    if let Some((pattern, line)) = blast_radius_hit_in_diff(&full_diff) {
        cleanup(worktree.clone(), branch.clone(), repo_root.to_path_buf()).await;
        let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
        warn!(
            issue = issue.number,
            pattern, %line, "refused: diff hit the blast-radius guard"
        );
        backoff_comment(
            repo_root,
            issue.number,
            &format!(
                "Self-improve refused: the produced diff touches a \
                 deploy/auth/secret path (blast-radius guard).\n\n\
                 Matched `{pattern}` on:\n```\n{line}\n```\n\n\
                 The guard runs on the DIFF, so this only surfaces after the \
                 build — the issue text itself named no such path. If the \
                 match is incidental, reword the fix to avoid that path; if \
                 the work is inherently deploy/auth/secret-shaped, it needs a \
                 human and should carry `{GAVE_UP_LABEL}` rather than burning \
                 the remaining attempts on the same refusal."
            ),
        )
        .await
        .ok();
        if attempts >= MAX_ATTEMPTS {
            label_gave_up(repo_root, issue.number).await.ok();
        }
        return Ok(RunReport::built(format!(
            "issue #{}: refused — diff hit blast-radius guard on `{pattern}` (attempt {attempts})",
            issue.number
        )));
    }
    // #823 — does this diff touch a path the PR-verify hook guards? Computed
    // next to the other diff-derived facts; it withholds auto-merge below
    // rather than refusing the work.
    let (_ok, staged_names, _) = run("git", &["diff", "--cached", "--name-only"], &worktree).await?;
    let mut gated = touches_verify_gated_path(&staged_names);

    let mut lines = diff_line_count(&full_diff);
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
        return Ok(RunReport::built(format!(
            "issue #{}: refused — diff too large ({lines} lines, attempt {attempts})",
            issue.number
        )));
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
        return Ok(RunReport::built(format!(
            "issue #{}: verification gate failed (attempt {attempts}); no PR opened",
            issue.number
        )));
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
        return Ok(RunReport::built(format!(
            "issue #{}: QA review rejected the diff (attempt {attempts}); no PR opened",
            issue.number
        )));
    }

    // Stage 4 (#828): INDEPENDENT review by a different provider. Two passes
    // — one focused on the diff, one on how the change lands in the rest of
    // the system — both with read-only access to the whole worktree. Runs
    // before the dry-run exit so dry runs exercise the full pipeline.
    //
    // A rejection here does NOT discard the work: the diff built, passed the
    // gate, and satisfied the author's own QA, so it opens as a draft PR
    // carrying both verdicts for a human. It does count as a failed attempt,
    // because a second opinion disagreeing is exactly what this stage is for.
    let mut independent = independent_review(&issue, &summary, &full_diff, worktree.clone()).await;
    let mut revision_note = String::new();
    if independent.available && !independent.approved() {
        warn!(
            issue = issue.number,
            diff_ok = independent.diff_ok,
            system_ok = independent.system_ok,
            "independent review requested changes"
        );
        // Not recorded as an attempt: the run continues either to a revision
        // or to a draft PR, and dedup keeps the issue out of the pool while
        // that PR is open (recording here double-counted — see #852).
    }

    // Iterate revisions until the reviewer approves (owner directive
    // 2026-08-31), bounded by `revise_rounds`. Seven drafts accumulated over
    // three days because a codex rejection just parked the work with nobody
    // acting on the findings; this loop is what turns findings into merges.
    // Every round re-runs everything downstream of the edit: scratch sweep,
    // blast/size guards, the full verification gate, and BOTH codex passes —
    // a revised diff earns its verdict, it does not inherit one.
    let max_rounds = revise_rounds();
    let mut round = 0u32;
    // Set when a round overgrew the size cap: the next round revises against
    // a shrink instruction instead of codex notes, and codex is not consulted
    // on a diff that cannot ship anyway.
    let mut findings_override: Option<String> = None;
    while independent.available && !independent.approved() && round < max_rounds {
        round += 1;
        let findings = findings_override
            .take()
            .unwrap_or_else(|| independent.notes.clone());
        info!(issue = issue.number, round, max_rounds, "revising against the independent findings");
        let round1 = independent.status();
        match reasoner
            .call(
                &fix_opts(worktree.clone()),
                &build_revise_prompt(&issue, &findings, lines),
            )
            .await
        {
            Err(e) => {
                // Provider trouble, not a verdict — keep the last outcome and
                // stop iterating rather than burning rounds on a flaky call.
                warn!(
                    issue = issue.number,
                    round, "revision pass failed; keeping the prior outcome: {e:#}"
                );
                break;
            }
            Ok(rev_summary) => {
                dropped.extend(drop_root_scratch(&worktree).await);
                let _ = run("git", &["add", "-A"], &worktree).await?;
                let (_ok, diff2, _) = run("git", &["diff", "--cached"], &worktree).await?;
                if let Some((pattern, line)) = blast_radius_hit_in_diff(&diff2) {
                    warn!(issue = issue.number, pattern, %line, "revision hit the blast-radius guard");
                    let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
                    backoff_comment(
                        repo_root,
                        issue.number,
                        &format!(
                            "Self-improve refused after revision: the revised diff \
                             touches `{pattern}`:\n```\n{line}\n```"
                        ),
                    )
                    .await
                    .ok();
                    if attempts >= MAX_ATTEMPTS {
                        label_gave_up(repo_root, issue.number).await.ok();
                    }
                    cleanup(worktree, branch, repo_root.to_path_buf()).await;
                    return Ok(RunReport::built(format!(
                        "issue #{}: revision hit blast-radius guard on `{pattern}` (attempt {attempts})",
                        issue.number
                    )));
                }
                let lines2 = diff_line_count(&diff2);
                if lines2 > MAX_DIFF_LINES {
                    // Shrink round while budget remains — a terminal refusal
                    // here discards every round of paid work (seen live on
                    // the #839 resume). Terminal only when rounds are spent.
                    if round < max_rounds {
                        warn!(
                            issue = issue.number,
                            round, lines2, "revision overgrew the cap; scheduling a shrink round"
                        );
                        findings_override = Some(shrink_findings(lines2));
                        lines = lines2;
                        revision_note.push_str(&format!(
                            "\n### Revision round {round}: overgrew the cap \
                             ({lines2} lines); next round shrinks\n"
                        ));
                        continue;
                    }
                    let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
                    backoff_comment(
                        repo_root,
                        issue.number,
                        &format!(
                            "Self-improve refused after revision: diff grew to \
                             {lines2} lines (cap {MAX_DIFF_LINES}) and the \
                             revision budget is exhausted."
                        ),
                    )
                    .await
                    .ok();
                    if attempts >= MAX_ATTEMPTS {
                        label_gave_up(repo_root, issue.number).await.ok();
                    }
                    cleanup(worktree, branch, repo_root.to_path_buf()).await;
                    return Ok(RunReport::built(format!(
                        "issue #{}: revision oversized ({lines2} lines, attempt {attempts})",
                        issue.number
                    )));
                }
                let (_ok, names2, _) =
                    run("git", &["diff", "--cached", "--name-only"], &worktree).await?;
                if let Err(gate_err) = gate_for_round(&worktree, &names2).await {
                    warn!(issue = issue.number, "post-revision gate failed: {gate_err:#}");
                    // #873 — a red gate gets a repair round while budget
                    // remains: red→fix is the other half of TDD, and a
                    // compile error is the easiest failure to iterate on.
                    // Codex is not consulted on a diff that does not build.
                    if round < max_rounds {
                        findings_override = Some(gate_findings(&gate_err.to_string()));
                        revision_note.push_str(&format!(
                            "\n### Revision round {round}: failed the gate; \
                             next round repairs\n"
                        ));
                        continue;
                    }
                    let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
                    if attempts >= MAX_ATTEMPTS {
                        backoff_comment(
                            repo_root,
                            issue.number,
                            &format!(
                                "Self-improve gave up after {attempts} attempts. \
                                 Post-revision gate failure:\n```\n{}\n```",
                                truncate(&gate_err.to_string(), 1500)
                            ),
                        )
                        .await
                        .ok();
                        label_gave_up(repo_root, issue.number).await.ok();
                    }
                    cleanup(worktree, branch, repo_root.to_path_buf()).await;
                    return Ok(RunReport::built(format!(
                        "issue #{}: post-revision gate failed (attempt {attempts}); no PR opened",
                        issue.number
                    )));
                }
                // The revised diff may touch different files.
                gated = touches_verify_gated_path(&names2);
                lines = lines2;
                independent = independent_review(&issue, &rev_summary, &diff2, worktree.clone()).await;
                revision_note.push_str(&format!(
                    "\n### Revision round {round} (prior verdict: {round1})\n{}\n\n\
                     Verdict after round {round}: {}\n",
                    truncate(&rev_summary, 1000),
                    independent.status()
                ));
                summary = format!("{summary}\n\nRevision {round}: {}", truncate(&rev_summary, 300));
            }
        }
    }
    if round > 0 {
        info!(
            issue = issue.number,
            rounds = round,
            approved = independent.approved(),
            "iterative review finished"
        );
        // Intermediate rounds ran the targeted gate (#870); everything that
        // ships — or even lands as a PR claiming workspace-pass — gets the
        // FULL suite exactly once here.
        if let Err(gate_err) = verification_gate(&worktree).await {
            warn!(issue = issue.number, "final full gate failed after revisions: {gate_err:#}");
            let attempts = record_attempt(repo_root, issue.number).await.unwrap_or(1);
            if attempts >= MAX_ATTEMPTS {
                backoff_comment(
                    repo_root,
                    issue.number,
                    &format!(
                        "Self-improve gave up after {attempts} attempts. Final \
                         full-workspace gate failure after revisions:\n```\n{}\n```",
                        truncate(&gate_err.to_string(), 1500)
                    ),
                )
                .await
                .ok();
                label_gave_up(repo_root, issue.number).await.ok();
            }
            cleanup(worktree, branch, repo_root.to_path_buf()).await;
            return Ok(RunReport::built(format!(
                "issue #{}: final full gate failed after {round} revision round(s)",
                issue.number
            )));
        }
    }

    if dry_run {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        return Ok(RunReport::built(format!(
            "issue #{}: DRY RUN — gate + QA review passed, {lines}-line diff \
             (complexity: {}), independent review: {}, would open PR",
            issue.number,
            complexity.as_str(),
            independent.status(),
        )));
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
        return Ok(RunReport::built(record_hard_failure(repo_root, issue.number, "git commit failed", &e).await));
    }
    // #815 — a run killed between the push and `gh pr create` leaves the
    // remote branch behind with no PR. `has_open_agent_pr` only looks at
    // PRs, so the issue is picked again, and this non-fast-forward push is
    // where it dies — after three reasoner calls and a full workspace gate,
    // every tick, forever. The `agent-fix/` namespace is pipeline-owned and
    // this branch has already been shown to carry no open PR, so a fresh
    // attempt is entitled to supersede the orphan. Force only in that
    // narrow case, and say so in the log.
    let (mut ok, _o, mut e) = run("git", &["push", "-u", "origin", &branch], &worktree).await?;
    if !ok && remote_branch_exists(repo_root, &branch).await {
        warn!(
            issue = issue.number,
            branch = %branch,
            "orphaned remote branch from an interrupted run; superseding it"
        );
        let forced = run(
            "git",
            &["push", "--force", "-u", "origin", &branch],
            &worktree,
        )
        .await?;
        ok = forced.0;
        e = forced.2;
    }
    if !ok {
        cleanup(worktree, branch, repo_root.to_path_buf()).await;
        return Ok(RunReport::built(record_hard_failure(repo_root, issue.number, "git push failed", &e).await));
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
        // #828 — an independent LGTM is REQUIRED for any auto-merge, and
        // when `AUGMENTAGENT_AUTOPR_CODEX_UNLOCKS_HARD` is set it also
        // releases the `hard` band: two independent reviewers is a real
        // answer to blast radius, where one model grading its own family's
        // work was not. Receipt-gated paths stay human-only either way —
        // those change live behaviour no reviewer can verify by reading.
        let complexity_ok = complexity.auto_mergeable() || codex_unlocks_hard();
        // Owner policy 2026-08-31: a double codex LGTM may override the
        // receipt gate (env-gated; see `automerge_receipt_ok`).
        let receipt_ok = automerge_receipt_ok(
            gated.as_deref(),
            independent.approved(),
            std::env::var("AUGMENTAGENT_AUTOPR_LGTM_OVERRIDES_RECEIPT").ok().as_deref(),
        );
        if enabled && complexity_ok && independent.approved() && !issue.research_filed
            && receipt_ok
        {
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
    let merge_note = match (automerge, gated.as_deref()) {
        (true, _) => "Auto-merged: owner-authored issue graded ≤medium, \
                      AUGMENTAGENT_AUTOPR_AUTOMERGE=1."
            .to_string(),
        // #823 — name the file, so the reviewer knows why this is a draft
        // even though the grade alone would have merged it.
        (false, Some(f)) => format!(
            "Draft — a human must review and merge. Auto-merge was withheld \
             because `{f}` is covered by the PR-verify receipt gate \
             (`scripts/agent-pr-verify-gate.sh`): changes there alter runtime \
             behaviour the test suite cannot model, so they need a real \
             exercise against the running daemon before they ship."
        ),
        (false, None) if !independent.approved() => format!(
            "Draft — a human must review and merge. The independent codex \
             review did not approve it ({}).",
            independent.status()
        ),
        (false, None) => "Draft — a human must review and merge.".to_string(),
    };
    // #817 — say so in the PR when the builder left scratch behind; a drop
    // that is never reported is indistinguishable from one that never
    // happened, and a fix that genuinely wanted a root-level file would
    // otherwise look complete while missing it.
    let scratch_note = if dropped.is_empty() {
        String::new()
    } else {
        format!(
            "\n- dropped builder scratch at the worktree root: {}",
            dropped.join(", ")
        )
    };
    let independent_section = format!(
        "\n\n## Independent review (codex)\n{}{revision_note}\n",
        truncate(&independent.notes, 3000)
    );
    let pr_body = format!(
        "Automated self-improvement for #{}.\n\n## Summary\n{}{plan_section}\n\n\
         ## QA review (approved)\n{}{independent_section}\n## Verification\n\
         - complexity (scoping pass): {}\n\
         - `cargo build --workspace`: pass\n- `cargo test --workspace`: pass\n\
         - diff size: {lines} lines (cap {MAX_DIFF_LINES}){scratch_note}\n\n\
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
        // #815 — the branch is pushed but has no PR. Left as an `Err` this
        // re-picks the same issue next tick and dies at the push above.
        return Ok(RunReport::built(record_hard_failure(repo_root, issue.number, "gh pr create failed", &e).await));
    }
    let pr_url = stdout.trim().to_string();
    if !automerge {
        // Visibility (#851): drafts sat unseen for three days. Tell the owner.
        notify_discord(&format!(
            "📝 auto-PR needs review: {} — {pr_url}\n{}",
            issue.title,
            truncate(&merge_note, 300)
        ))
        .await;
        return Ok(RunReport::built(format!(
            "issue #{}: draft PR opened — {pr_url}",
            issue.number
        )));
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
        return Ok(RunReport::built(format!(
            "issue #{}: PR opened but auto-merge FAILED (left open for review) — {pr_url}",
            issue.number
        )));
    }
    notify_discord(&format!("✅ auto-PR merged: {} — {pr_url}", issue.title)).await;
    Ok(RunReport::built(format!(
        "issue #{}: PR auto-merged (owner-authored) — {pr_url}",
        issue.number
    )))
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

/// Does `origin` already carry this branch? (#815)
///
/// Distinguishes "the push lost a race / auth broke" from "a previous run
/// pushed this branch and then died before opening its PR".
async fn remote_branch_exists(repo_root: &Path, branch: &str) -> bool {
    let refspec = format!("refs/heads/{branch}");
    match run(
        "git",
        &["ls-remote", "--heads", "origin", &refspec],
        repo_root,
    )
    .await
    {
        Ok((true, out, _)) => !out.trim().is_empty(),
        // A failed `ls-remote` says nothing; don't force-push on a guess.
        _ => false,
    }
}

/// #815 — record a post-gate failure as a *failed attempt* rather than an
/// error, and return the loop's message for it.
///
/// `git commit` / `git push` / `gh pr create` failures reach this point
/// having already spent three reasoner calls and a full workspace build+test.
/// Returned as `Err` they bypass both `record_attempt` (so `MAX_ATTEMPTS`
/// never accrues and the issue is never labeled out) and the loop's
/// `DailyCounter::record` (so the spend is never counted) — which turns a
/// sticky failure into an unbounded 30-minute retry of the most expensive
/// path in the daemon. Counting them makes the loop give up like it does on
/// a red gate or a rejected review.
async fn record_hard_failure(repo_root: &Path, issue: u64, what: &str, err: &str) -> String {
    let attempts = record_attempt(repo_root, issue).await.unwrap_or(1);
    warn!(issue, attempts, "self-improve: {what}: {err}");
    if attempts >= MAX_ATTEMPTS {
        backoff_comment(
            repo_root,
            issue,
            &format!(
                "Self-improve gave up after {attempts} attempts: the fix built \
                 and passed review, but publishing it kept failing \
                 (`{what}`). Last error:\n```\n{}\n```",
                truncate(err, 1000)
            ),
        )
        .await
        .ok();
        label_gave_up(repo_root, issue).await.ok();
    }
    format!("issue #{issue}: {what} (attempt {attempts}); no PR opened")
}

/// Bump an attempt counter encoded as a hidden marker comment, return the new
/// count. (Lightweight; avoids needing extra labels per count.)
async fn record_attempt(repo_root: &Path, issue: u64) -> Result<u32> {
    // #851 — remember the attempt for the rest of the UTC day so the picker
    // moves on instead of handing this issue straight back next tick.
    AttemptLedger::mark_persist(&attempt_ledger_path(), utc_day_now(), issue);
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
        attachments: Vec::new(),
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

/// Engaged-run counter with UTC-day rollover. Pure so it's testable;
/// [`load`](Self::load) / [`save`](Self::save) add the durability.
#[derive(Default, serde::Serialize, serde::Deserialize)]
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

    /// #814 — read the counter back from disk, defaulting to a fresh one.
    ///
    /// The cap exists to bound how much of the owner's Claude subscription
    /// the unattended loop spends per day, but it lived only in process
    /// memory — and the loop's *success* path is exactly what destroys that
    /// memory: an auto-merged PR touching `crates/` makes the auto-updater
    /// rebuild and `systemctl restart augmentagent.service`, so the counter
    /// resets to zero after every win. The cap therefore throttled failures
    /// and nothing else. (`stderr.log` shows 19 `auto-PR loop started` lines
    /// in the week to 2026-08-27 — 19 resets.)
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .unwrap_or_default()
    }

    /// Best-effort persist. A write failure must never abort a run — it only
    /// costs the cap its memory, which is where we already were.
    fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_vec(self) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    warn!(path = %path.display(), "could not persist auto-PR daily counter: {e}");
                }
            }
            Err(e) => warn!("could not serialize auto-PR daily counter: {e}"),
        }
    }
}

/// #851 — which issues were already attempted today, so the picker never
/// hands the same issue back after a refusal.
///
/// Without this, a post-build refusal was followed 30 minutes later by the
/// SAME issue: `pick_issue` only excludes `agent-gave-up` labels and open
/// agent PRs, and a refusal at attempt 1 or 2 is neither. When the refusal is
/// deterministic — a guard refusal usually is — every retry burns a full
/// agentic-Opus build reaching the identical conclusion. On 2026-08-28 that
/// spent 2 of 3 daily slots on #658 and starved ~40 eligible issues for 16
/// hours.
///
/// Keyed by UTC day, same rollover as [`DailyCounter`]: a flaky refusal gets
/// a fresh chance tomorrow, while `record_attempt`'s markers still accumulate
/// toward the permanent `agent-gave-up` label at [`MAX_ATTEMPTS`].
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct AttemptLedger {
    day: u64,
    issues: Vec<u64>,
}

impl AttemptLedger {
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .unwrap_or_default()
    }

    fn attempted_today(&self, day: u64, issue: u64) -> bool {
        self.day == day && self.issues.contains(&issue)
    }

    fn mark(&mut self, day: u64, issue: u64) {
        if self.day != day {
            self.day = day;
            self.issues.clear();
        }
        if !self.issues.contains(&issue) {
            self.issues.push(issue);
        }
    }

    /// Atomically read-modify-write a mark under an exclusive flock (#871).
    /// With the resume and build lanes running in parallel, two unlocked
    /// read-modify-write cycles could drop one lane's mark — which would
    /// re-expose an already-attempted issue to the picker the same day.
    fn mark_persist(path: &Path, day: u64, issue: u64) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                warn!(path = %path.display(), "attempt ledger open failed: {e}");
                return;
            }
        };
        // SAFETY: `file` owns the fd; flock neither reads nor writes it.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            warn!("attempt ledger flock failed; marking without it");
        }
        let mut ledger = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .unwrap_or_default();
        ledger.mark(day, issue);
        ledger.save(path);
        // Lock released when `file` drops.
    }

    /// Best-effort persist, mirroring [`DailyCounter::save`]: losing this
    /// file only costs the day's skip memory, which is where we already were.
    fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_vec(self) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    warn!(path = %path.display(), "could not persist attempt ledger: {e}");
                }
            }
            Err(e) => warn!("could not serialize attempt ledger: {e}"),
        }
    }
}

/// Ledger path: `AUGMENTAGENT_AUTOPR_ATTEMPTED_FILE` override (tests), else
/// the daemon state dir next to the daily counter.
fn attempt_ledger_path() -> PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_AUTOPR_ATTEMPTED_FILE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join(".local/state/augmentagent")
                .join("autopr-attempted.json")
        })
        .unwrap_or_else(|| PathBuf::from("autopr-attempted.json"))
}

/// Where the persisted counter lives: `AUGMENTAGENT_AUTOPR_COUNTER_FILE`
/// override (tests), else the daemon state dir next to the reasoner
/// cooldown latch.
fn daily_counter_path() -> PathBuf {
    if let Ok(p) = std::env::var(COUNTER_FILE_ENV) {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join(".local/state/augmentagent")
                .join("autopr-daily-runs.json")
        })
        .unwrap_or_else(|| PathBuf::from("autopr-daily-runs.json"))
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
    /// How many consecutive triage-only refusals one tick may clear.
    const MAX_TRIAGE_PER_TICK: u32 = 5;

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
        let counter_path = daily_counter_path();
        let mut counter = DailyCounter::load(&counter_path);
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
            // Triage-only outcomes are cheap and permanently label the
            // issue out of the pool, so keep going within the SAME tick
            // instead of spending a 30-minute interval on each one. Bounded
            // so a pathological pool cannot spin; the pool shrinks with every
            // refusal, so this burst is self-limiting.
            let mut triaged = 0u32;
            loop {
                match run_once(&self.repo_root, self.dry_run).await {
                    Ok(r) if r.is_idle() => break,
                    Ok(r) if r.billed => {
                        counter.record(today);
                        counter.save(&counter_path);
                        info!(
                            runs_today = counter.runs_today(today),
                            daily_cap = self.daily_cap,
                            "auto-PR: {r}"
                        );
                        // #851 — with budget left, keep going in the SAME
                        // tick. The attempt ledger guarantees the next pick
                        // is a different issue, so remaining slots go to the
                        // rest of the pool instead of idling 30 minutes —
                        // or, worse, re-buying the refusal just recorded.
                        if counter.runs_today(today) >= self.daily_cap {
                            break;
                        }
                    }
                    Ok(r) => {
                        triaged += 1;
                        info!(triaged, "auto-PR (triage, unbilled): {r}");
                        if triaged >= Self::MAX_TRIAGE_PER_TICK {
                            info!(
                                triaged,
                                "auto-PR: triage burst limit reached; resuming next tick"
                            );
                            break;
                        }
                    }
                    // Transient refusals (dirty deploy tree while a sibling
                    // session works, gh/network hiccup) — try next tick.
                    Err(e) => {
                        warn!("auto-PR tick failed: {e:#}");
                        break;
                    }
                }
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

    // ---- throughput: cheap triage must not cost a build's budget ----

    #[test]
    fn only_builds_are_billed_against_the_daily_cap() {
        assert!(!RunReport::idle().billed);
        assert!(RunReport::idle().is_idle());

        // A scoping refusal is ~30s and one Fable call. Billing it the same
        // as a 20-minute Opus build is what let three refusals consume a
        // whole day's budget (observed 2026-08-27 on #828).
        let t = RunReport::triage("issue #828: scoped as not agent-fixable".into());
        assert!(!t.billed);
        assert!(!t.is_idle(), "a refusal is an outcome, not an idle tick");

        let b = RunReport::built("issue #799: PR auto-merged".into());
        assert!(b.billed);
        assert_eq!(b.to_string(), "issue #799: PR auto-merged");
    }

    #[test]
    fn triage_burst_is_bounded() {
        // The pool shrinks with every refusal (each one gets `agent-gave-up`),
        // so the burst self-limits — but it still needs a hard stop so a
        // pathological pool cannot spin a tick forever.
        assert!(AutoPrLoop::MAX_TRIAGE_PER_TICK >= 1);
        assert!(
            AutoPrLoop::MAX_TRIAGE_PER_TICK <= 10,
            "each triage still spends a scope call; keep the burst modest"
        );
    }

    // ---- #840: system context the reviewer can actually use ----

    #[test]
    fn changed_symbols_finds_new_items_and_ignores_noise() {
        let d = diff_of(&[
            "diff --git a/crates/x/src/a.rs b/crates/x/src/a.rs",
            "+++ b/crates/x/src/a.rs",
            "+pub fn is_calendar_invite(e: &Email) -> bool {",
            "+struct InviteShape {",
            "+    // fn mentioned in a comment should still be harmless",
            "+const MAX_TRIES: u32 = 3;",
            "-fn removed_thing() {}",
            "     fn context_thing() {}",
        ]);
        let got = changed_symbols(&d);
        assert!(got.contains(&"is_calendar_invite".to_string()));
        assert!(got.contains(&"InviteShape".to_string()));
        assert!(got.contains(&"MAX_TRIES".to_string()));
        // Removed and context lines are not what this diff introduces.
        assert!(!got.contains(&"removed_thing".to_string()));
        assert!(!got.contains(&"context_thing".to_string()));
    }

    #[test]
    fn changed_files_reads_the_b_side_paths() {
        let d = diff_of(&[
            "diff --git a/crates/x/src/a.rs b/crates/x/src/a.rs",
            "--- a/crates/x/src/a.rs",
            "+++ b/crates/x/src/a.rs",
            "+fn f() {}",
        ]);
        assert_eq!(changed_files(&d), vec!["crates/x/src/a.rs".to_string()]);
    }

    #[tokio::test]
    async fn caller_evidence_finds_call_sites_outside_the_changed_files() {
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
        std::fs::create_dir_all(repo.join("crates/x/src")).unwrap();
        // The definition lives in the changed file...
        std::fs::write(repo.join("crates/x/src/a.rs"), "pub fn is_invite() -> bool { true }\n")
            .unwrap();
        // ...and a caller lives elsewhere. That caller is the whole point.
        std::fs::write(
            repo.join("crates/x/src/caller.rs"),
            "fn go() { if is_invite() { return; } }\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["-c", "user.email=t@e", "-c", "user.name=t", "commit", "-q", "-m", "init"]);

        let d = diff_of(&[
            "diff --git a/crates/x/src/a.rs b/crates/x/src/a.rs",
            "+++ b/crates/x/src/a.rs",
            "+pub fn is_invite() -> bool { true }",
        ]);
        let ev = caller_evidence(repo, &d).await;
        assert!(
            ev.contains("crates/x/src/caller.rs"),
            "the caller outside the diff must be surfaced: {ev}"
        );
        assert!(
            !ev.contains("crates/x/src/a.rs:"),
            "the definition inside the changed file is not a caller: {ev}"
        );
    }

    #[tokio::test]
    async fn caller_evidence_says_so_when_there_is_nothing_to_look_up() {
        let dir = tempfile::tempdir().unwrap();
        let d = diff_of(&[
            "diff --git a/README.md b/README.md",
            "+++ b/README.md",
            "+just prose, no new items",
        ]);
        let ev = caller_evidence(dir.path(), &d).await;
        assert!(
            ev.to_lowercase().contains("no new named items"),
            "must state the absence rather than returning something that reads \
             like evidence: {ev}"
        );
    }

    // ---- #828: independent codex review ----

    #[test]
    fn codex_review_preset_stays_in_the_read_tools_class() {
        // Load-bearing: codex may serve ReadTools (behind the env flag)
        // precisely because that class has no shell. One `Bash(...)` entry
        // would reclassify this preset as FullAgentic, which codex may not
        // serve — the stage would silently start failing closed.
        let opts = codex_review_opts(PathBuf::from("/tmp/wt"), "sys");
        for t in &opts.allowed_tools {
            assert!(
                matches!(t.as_str(), "Read" | "Grep" | "Glob" | "LS" | "WebSearch"),
                "tool {t:?} would push the independent reviewer out of ReadTools"
            );
        }
        assert!(opts.model.is_some(), "presets pin their model (#448)");
        // It must see the whole worktree, not just the diff — a system-level
        // review is impossible otherwise.
        assert_eq!(opts.cwd, Some(PathBuf::from("/tmp/wt")));
        assert!(opts.add_dirs.contains(&PathBuf::from("/tmp/wt")));
    }

    #[test]
    fn codex_verdict_defaults_to_changes_requested() {
        assert!(parse_codex_review("CODEX-REVIEW: lgtm

checked the callers").0);
        assert!(parse_codex_review("codex-review:  LGTM ").0);
        assert!(!parse_codex_review("CODEX-REVIEW: changes-requested

no test").0);
        // Unparseable, empty, or a verdict buried past the header ⇒ reject.
        assert!(!parse_codex_review("I think it looks fine to me").0);
        assert!(!parse_codex_review("").0);
        assert!(!parse_codex_review("





CODEX-REVIEW: lgtm").0);
        // A line that says both must not read as approval.
        assert!(!parse_codex_review("CODEX-REVIEW: changes-requested (not lgtm)").0);
    }

    #[test]
    fn independent_approval_requires_availability_and_both_passes() {
        let mk = |available, diff_ok, system_ok| IndependentReview {
            available,
            diff_ok,
            system_ok,
            notes: String::new(),
        };
        assert!(mk(true, true, true).approved());
        assert!(!mk(true, true, false).approved(), "system pass must count");
        assert!(!mk(true, false, true).approved(), "diff pass must count");
        // The one that matters: codex unreachable is NOT an approval.
        assert!(
            !mk(false, true, true).approved(),
            "an unavailable reviewer must never read as an approval"
        );
        assert_eq!(mk(false, false, false).status(), "unavailable");
        assert_eq!(mk(true, true, false).status(), "changes requested on system interaction");
    }

    #[test]
    fn hard_band_stays_locked_unless_explicitly_opted_in() {
        assert!(!automerge_enabled_value(None));
        assert!(automerge_enabled_value(Some("1")));
        // `hard` never auto-merges on complexity alone.
        assert!(!Complexity::Hard.auto_mergeable());
    }

    /// Live probe (#828): actually spawns codex against the real review
    /// preset. `#[ignore]`d so the verification gate never spends provider
    /// quota — run it deliberately:
    ///
    /// ```text
    /// AUGMENTAGENT_CODEX_READ_TOOLS=1 cargo test -p augmentagent-cli \
    ///   --bins live_codex_review -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn live_codex_review_returns_a_parseable_verdict() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repo root");
        let reasoner = augmentagent_channel_core::build_pinned(
            augmentagent_channel_core::ProviderKind::Codex,
        )
        .expect("codex must be installed and authenticated for this probe");
        assert_eq!(reasoner.provider_names(), vec!["codex"]);

        let opts = codex_review_opts(repo, CODEX_DIFF_REVIEW_SYSTEM);
        let diff = "diff --git a/src/x.rs b/src/x.rs\n                    --- a/src/x.rs\n+++ b/src/x.rs\n                    @@ -1,3 +1,3 @@\n                    -fn total(v: &[u32]) -> u32 { v.iter().sum() }\n                    +fn total(v: &[u32]) -> u32 { v.iter().fold(0, |a, b| a + b) }\n";
        let raw = reasoner
            .call(&opts, &format!("Review this change.\n\n```diff\n{diff}\n```"))
            .await
            .expect("codex call");
        println!("--- codex said ---\n{raw}\n---");
        let (_ok, notes) = parse_codex_review(&raw);
        assert!(
            notes.to_ascii_lowercase().contains("codex-review:"),
            "codex must emit the verdict header; got: {notes}"
        );
    }

    /// Live probe (#840): the SYSTEM-interaction pass, end to end — real
    /// `git grep` evidence over this repo, then a real codex call. Guards the
    /// regression that motivated #840, where the pass reported
    /// "the read-only command environment failed before executing any
    /// command" and reviewed nothing.
    ///
    /// ```text
    /// cargo test -p augmentagent-cli --bins live_codex_system_pass -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn live_codex_system_pass_reasons_from_supplied_evidence() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repo root");

        // A diff over a symbol this repo really calls from more than one file.
        let diff = diff_of(&[
            "diff --git a/crates/augmentagent-cli/src/self_improve.rs b/crates/augmentagent-cli/src/self_improve.rs",
            "+++ b/crates/augmentagent-cli/src/self_improve.rs",
            "+pub fn is_blast_radius(text: &str) -> bool { false }",
        ]);

        let evidence = caller_evidence(&repo, &diff).await;
        println!("--- evidence ---\n{evidence}\n");
        assert!(
            evidence.contains("is_blast_radius"),
            "the daemon must find real call sites: {evidence}"
        );

        let reasoner = augmentagent_channel_core::build_pinned(
            augmentagent_channel_core::ProviderKind::Codex,
        )
        .expect("codex must be installed and authenticated");
        let opts = codex_review_opts(repo, CODEX_SYSTEM_REVIEW_SYSTEM);
        let raw = reasoner
            .call(
                &opts,
                &format!("```diff\n{diff}\n```\n\n## Pre-computed call sites\n{evidence}"),
            )
            .await
            .expect("codex call");
        println!("--- codex system pass ---\n{raw}\n---");

        assert!(
            raw.to_ascii_lowercase().contains("codex-review:"),
            "must emit the verdict header: {raw}"
        );
        assert!(
            !raw.to_ascii_lowercase().contains("failed before executing any command"),
            "the #840 regression is back — the pass is trying to run commands: {raw}"
        );
    }

    // ---- #823: the receipt gate must bind the daemon too ----

    #[test]
    fn verify_gated_paths_match_the_pr_hook() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/agent-pr-verify-gate.sh");
        let src = std::fs::read_to_string(&script)
            .unwrap_or_else(|e| panic!("read {}: {e}", script.display()));

        let mut from_hook: Vec<&str> = Vec::new();
        for line in src.lines() {
            let line = line.trim();
            if !line.contains("MATCHED+=") {
                continue;
            }
            let Some((pats, _)) = line.split_once(')') else { continue };
            from_hook.extend(pats.split('|').map(str::trim).filter(|p| !p.is_empty()));
        }
        assert!(
            !from_hook.is_empty(),
            "parsed no case patterns out of {} — this test's parser has drifted \
             and is silently asserting nothing",
            script.display()
        );
        for pat in from_hook {
            assert!(
                VERIFY_GATED_PATHS.contains(&pat),
                "`{pat}` needs a verification receipt from a human, but the \
                 auto-PR loop would still auto-merge it — add it to \
                 VERIFY_GATED_PATHS"
            );
        }
    }

    #[test]
    fn receipt_gated_paths_are_detected_in_a_staged_diff() {
        // The real 2026-08-27T19:03Z unattended run on #800.
        assert_eq!(
            touches_verify_gated_path(
                "crates/augmentagent-channel-core/src/reasoner.rs\nschema/wiki-ask.md\n"
            )
            .as_deref(),
            Some("crates/augmentagent-channel-core/src/reasoner.rs")
        );
        assert_eq!(
            touches_verify_gated_path("crates/augmentagent-channel-instagram/src/tone.rs")
                .as_deref(),
            Some("crates/augmentagent-channel-instagram/src/tone.rs")
        );
        // An ordinary Rust fix elsewhere still auto-merges as before.
        assert!(touches_verify_gated_path(
            "crates/augmentagent-cli/src/self_improve.rs\ncrates/augmentagent-store/src/store.rs"
        )
        .is_none());
    }

    #[test]
    fn glob_matching_mirrors_shell_case_semantics() {
        // `*` does not cross '/' in a shell `case` glob, so the Rust matcher
        // must not widen the hook's list when mirroring it.
        assert!(glob_matches("schema/*.md", "schema/wiki-ask.md"));
        assert!(!glob_matches("schema/*.md", "schema/nested/x.md"));
        assert!(!glob_matches("schema/*.md", "schema/notes.txt"));
        assert!(glob_matches(
            "crates/augmentagent-channel-*/src/tone.rs",
            "crates/augmentagent-channel-slack/src/tone.rs"
        ));
        assert!(!glob_matches(
            "crates/augmentagent-channel-*/src/tone.rs",
            "crates/a/b/src/tone.rs"
        ));
        assert!(glob_matches("skills/*/SKILL.md", "skills/invoice/SKILL.md"));
    }

    // ---- #820: the Node half of the gate must be reachable ----

    #[test]
    fn node_gate_fires_on_node_paths_and_not_on_rust_only_diffs() {
        assert!(is_node_path("src/index.ts"));
        assert!(is_node_path("views/dashboard.ejs"));
        assert!(is_node_path("package.json"));
        assert!(is_node_path("tailwind.input.css"));

        assert!(!is_node_path("crates/augmentagent-cli/src/self_improve.rs"));
        assert!(!is_node_path("schema/triage.md"));
        assert!(!is_node_path("Cargo.toml"));
        // Prefix matching must be path-segment honest, not substring.
        assert!(!is_node_path("crates/x/src/lib.rs"));
        assert!(!is_node_path("package.json.bak"));
    }

    #[tokio::test]
    async fn node_build_is_required_only_when_the_staged_diff_reaches_node() {
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

        // A Rust-only change: the common case, and it must stay fast.
        std::fs::create_dir_all(repo.join("crates/x/src")).unwrap();
        std::fs::write(repo.join("crates/x/src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(&["add", "-A"]);
        assert!(!node_build_required(repo).await);

        // Touch the TypeScript daemon and the gate becomes load-bearing.
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/index.ts"), "export const x = 1;\n").unwrap();
        git(&["add", "-A"]);
        assert!(node_build_required(repo).await);
    }

    // ---- #817: builder scratch must not reach the commit ----

    #[tokio::test]
    async fn root_scratch_is_dropped_but_nested_source_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(wt)
                .output()
                .expect("git")
        };
        git(&["init", "-q", "-b", "main", "."]);
        git(&["-c", "user.email=t@e", "-c", "user.name=t", "commit", "-q",
              "--allow-empty", "-m", "init"]);

        // What the builder left at the root on the live #811 run...
        std::fs::write(wt.join(".aa811_fix.py"), "print(1)\n").unwrap();
        std::fs::write(wt.join(".aa811_check.txt"), "notes\n").unwrap();
        // ...next to a real new source file, which must survive.
        std::fs::create_dir_all(wt.join("crates/x/src")).unwrap();
        std::fs::write(wt.join("crates/x/src/new.rs"), "pub fn f() {}\n").unwrap();

        let mut dropped = drop_root_scratch(wt).await;
        dropped.sort();
        assert_eq!(dropped, vec![".aa811_check.txt", ".aa811_fix.py"]);
        assert!(!wt.join(".aa811_fix.py").exists());
        assert!(
            wt.join("crates/x/src/new.rs").exists(),
            "a created source file is the builder's work, not scratch"
        );

        // And the staged set the guards + commit see is now only the source.
        git(&["add", "-A"]);
        let staged = String::from_utf8(git(&["diff", "--cached", "--name-only"]).stdout).unwrap();
        assert_eq!(staged.trim(), "crates/x/src/new.rs");
    }

    // ---- #866: sitting drafts are picked up, not orphaned ----

    #[test]
    fn issue_number_parses_from_agent_branch_names() {
        assert_eq!(issue_from_branch("agent-fix/issue-845"), Some(845));
        assert_eq!(issue_from_branch("agent-fix/issue-0"), Some(0));
        // Non-agent branches must never be resumed.
        assert_eq!(issue_from_branch("fix/845-something"), None);
        assert_eq!(issue_from_branch("agent-fix/issue-"), None);
        assert_eq!(issue_from_branch("agent-fix/issue-12x"), None);
    }

    #[test]
    fn complexity_recovers_from_a_pr_body_and_defaults_hard() {
        assert_eq!(
            complexity_from_pr_body("## Verification\n- complexity (scoping pass): medium\n- diff"),
            Complexity::Medium
        );
        assert_eq!(
            complexity_from_pr_body("- complexity (scoping pass): simple"),
            Complexity::Simple
        );
        // Old or hand-written bodies without the line stay conservative.
        assert_eq!(complexity_from_pr_body("no such line"), Complexity::Hard);
        assert_eq!(
            complexity_from_pr_body("- complexity (scoping pass): who knows"),
            Complexity::Hard
        );
    }

    // ---- #870: targeted gate between rounds ----

    #[test]
    fn changed_crates_targets_inside_crates_and_bails_outside() {
        assert_eq!(
            changed_crates("crates/augmentagent-cli/src/self_improve.rs\ncrates/augmentagent-store/src/store.rs"),
            Some(vec!["augmentagent-cli".into(), "augmentagent-store".into()])
        );
        // Duplicates collapse.
        assert_eq!(
            changed_crates("crates/x/src/a.rs\ncrates/x/src/b.rs"),
            Some(vec!["x".into()])
        );
        // Anything outside crates/ means only the full gate can vouch for it.
        assert_eq!(changed_crates("src/index.ts"), None);
        assert_eq!(changed_crates("crates/x/src/a.rs\nCargo.toml"), None);
        assert_eq!(changed_crates("schema/triage.md"), None);
        // A bare `crates/<dir>` line (no file) is not a crate change we can
        // name — full gate.
        assert_eq!(changed_crates("crates/x"), None);
        // Empty set: nothing to target.
        assert_eq!(changed_crates(""), None);
        assert_eq!(changed_crates("\n\n"), None);
    }

    // ---- revise loop + receipt override ----

    #[test]
    fn revise_prompt_carries_the_findings_and_forbids_a_rewrite() {
        let issue = Issue {
            number: 845,
            title: "To/CC bug".into(),
            body: "b".into(),
            author: "a".into(),
            author_trusted: true,
            research_filed: false,
        };
        let p = build_revise_prompt(&issue, "rfind(',') splits quoted display names", 373);
        assert!(p.contains("rfind"), "the reviewer's findings must reach the builder");
        assert!(p.contains("#845"));
        // The live #853 resume burned four rounds because "do not start
        // over" entrenched a structurally wrong approach: the guard matched
        // greeting names against address tokens ("Gary" vs "glozoff"), codex
        // showed the reported case could never pass, and the builder kept
        // patching details around the hole. Revisions must be allowed to
        // pivot when the finding is architectural.
        assert!(
            p.contains("IF THE FINDING IS ARCHITECTURAL"),
            "an architectural finding must permit replacing the approach"
        );
        assert!(
            p.contains("reviewer-proposed alternative"),
            "the reviewer's prescription is signal, not noise"
        );
        assert!(
            p.contains("ORIGINAL reported case verbatim"),
            "the issue's own repro is the acceptance test that catches a non-fix"
        );
        assert!(
            p.contains("mistaken"),
            "the builder must be allowed to rebut a wrong finding rather than \
             blindly comply with it"
        );
        // The budget must be stated every round — the live #839 resume blew
        // the cap on round 2 because the builder was never told it existed.
        assert!(p.contains("373"), "current size must be in the prompt");
        assert!(p.contains("600"), "the cap must be in the prompt");
    }

    #[test]
    fn gate_findings_carry_the_error_and_forbid_unrelated_changes() {
        let f = gate_findings("error[E0308]: mismatched types\n --> src/x.rs:9");
        assert!(f.contains("E0308"), "the builder must see the compiler's own words");
        assert!(f.contains("FAILED the verification gate"));
        assert!(
            f.contains("change nothing unrelated"),
            "a repair round repairs; it must not become a rewrite"
        );
    }

    #[test]
    fn shrink_findings_name_the_size_and_forbid_losing_the_fix() {
        let f = shrink_findings(812);
        assert!(f.contains("812"));
        assert!(f.contains("600"));
        assert!(
            f.contains("without losing the fix"),
            "a shrink round must cut scaffolding, not the fix or its tests"
        );
    }

    #[test]
    fn receipt_override_requires_double_lgtm_and_the_explicit_flag() {
        // Ungated work merges as before, reviewer or not.
        assert!(automerge_receipt_ok(None, true, None));
        assert!(automerge_receipt_ok(None, false, None));

        let gated = Some("crates/augmentagent-channel-email/src/channel.rs");
        // Gated + no flag: held, even with a double LGTM — safe default for
        // any deployment that has not made the owner's call.
        assert!(!automerge_receipt_ok(gated, true, None));
        // Gated + flag + double LGTM: the owner's 2026-08-31 policy.
        assert!(automerge_receipt_ok(gated, true, Some("1")));
        // Gated + flag but codex did NOT approve: still held. The override
        // is "LGTM overrides the receipt", never "the flag overrides codex".
        assert!(!automerge_receipt_ok(gated, false, Some("1")));
    }

    #[test]
    fn revise_rounds_defaults_bounded_and_respects_the_kill_switch() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_r = std::env::var("AUGMENTAGENT_AUTOPR_REVISE_ROUNDS").ok();
        let prev_e = std::env::var("AUGMENTAGENT_AUTOPR_REVISE").ok();

        std::env::remove_var("AUGMENTAGENT_AUTOPR_REVISE_ROUNDS");
        std::env::remove_var("AUGMENTAGENT_AUTOPR_REVISE");
        assert_eq!(revise_rounds(), 3, "iterate-until-LGTM defaults to 3 rounds");

        std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE_ROUNDS", "10");
        assert_eq!(revise_rounds(), 5, "each round is paid work; the ceiling holds");

        std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE_ROUNDS", "1");
        assert_eq!(revise_rounds(), 1);

        // The kill switch beats the round count.
        std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE", "0");
        assert_eq!(revise_rounds(), 0);

        match prev_r {
            Some(v) => std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE_ROUNDS", v),
            None => std::env::remove_var("AUGMENTAGENT_AUTOPR_REVISE_ROUNDS"),
        }
        match prev_e {
            Some(v) => std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE", v),
            None => std::env::remove_var("AUGMENTAGENT_AUTOPR_REVISE"),
        }
    }

    #[test]
    fn lanes_have_disjoint_locks_and_worktrees() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AUGMENTAGENT_AUTOPR_LANE").ok();

        std::env::remove_var("AUGMENTAGENT_AUTOPR_LANE");
        assert_eq!(lane_from_env(), Lane::Combined);
        std::env::set_var("AUGMENTAGENT_AUTOPR_LANE", "resume");
        assert_eq!(lane_from_env(), Lane::Resume);
        std::env::set_var("AUGMENTAGENT_AUTOPR_LANE", "build");
        assert_eq!(lane_from_env(), Lane::Build);
        // Unknown values must not invent a third lane.
        std::env::set_var("AUGMENTAGENT_AUTOPR_LANE", "yolo");
        assert_eq!(lane_from_env(), Lane::Combined);

        // The whole point: resume and build can run CONCURRENTLY, so they
        // must never share a lock or a worktree...
        assert_ne!(Lane::Resume.lock_suffix(), Lane::Build.lock_suffix());
        assert_ne!(Lane::Resume.worktree_name(), Lane::Build.worktree_name());
        // ...while combined and build must EXCLUDE each other (the daemon
        // runs combined), so they share both.
        assert_eq!(Lane::Combined.lock_suffix(), Lane::Build.lock_suffix());
        assert_eq!(Lane::Combined.worktree_name(), Lane::Build.worktree_name());

        std::env::set_var("AUGMENTAGENT_AUTOPR_LANE", "resume");
        assert!(run_lock_path().to_string_lossy().contains("self-improve-resume.lock"));
        std::env::remove_var("AUGMENTAGENT_AUTOPR_LANE");
        assert!(run_lock_path().to_string_lossy().ends_with("self-improve.lock"));

        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_AUTOPR_LANE", v),
            None => std::env::remove_var("AUGMENTAGENT_AUTOPR_LANE"),
        }
    }

    #[test]
    fn ledger_mark_persist_survives_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attempted.json");
        // Two lanes marking different issues at once must both land — an
        // unlocked read-modify-write drops one.
        let p1 = path.clone();
        let p2 = path.clone();
        let t1 = std::thread::spawn(move || {
            for i in 0..50u64 {
                AttemptLedger::mark_persist(&p1, 100, 1000 + i);
            }
        });
        let t2 = std::thread::spawn(move || {
            for i in 0..50u64 {
                AttemptLedger::mark_persist(&p2, 100, 2000 + i);
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let l = AttemptLedger::load(&path);
        for i in 0..50u64 {
            assert!(l.attempted_today(100, 1000 + i), "lane-1 mark {} lost", 1000 + i);
            assert!(l.attempted_today(100, 2000 + i), "lane-2 mark {} lost", 2000 + i);
        }
    }

    #[test]
    fn resume_first_is_default_and_flippable_per_run() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AUGMENTAGENT_AUTOPR_RESUME_FIRST").ok();

        std::env::remove_var("AUGMENTAGENT_AUTOPR_RESUME_FIRST");
        assert!(resume_first_enabled(), "draining sitting drafts stays the default");
        std::env::set_var("AUGMENTAGENT_AUTOPR_RESUME_FIRST", "0");
        assert!(!resume_first_enabled(), "the owner's new-issues-now lever");
        std::env::set_var("AUGMENTAGENT_AUTOPR_RESUME_FIRST", "1");
        assert!(resume_first_enabled());

        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_AUTOPR_RESUME_FIRST", v),
            None => std::env::remove_var("AUGMENTAGENT_AUTOPR_RESUME_FIRST"),
        }
    }

    #[test]
    fn revise_is_on_by_default_and_disableable() {
        // Pin the env either way (#838's lesson: never read ambient state).
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AUGMENTAGENT_AUTOPR_REVISE").ok();

        std::env::remove_var("AUGMENTAGENT_AUTOPR_REVISE");
        assert!(revise_enabled(), "revision is the default");
        std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE", "0");
        assert!(!revise_enabled());
        std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE", "1");
        assert!(revise_enabled());

        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_AUTOPR_REVISE", v),
            None => std::env::remove_var("AUGMENTAGENT_AUTOPR_REVISE"),
        }
    }

    // ---- #851: never hand a refused issue straight back ----

    #[test]
    fn attempt_ledger_skips_within_the_day_and_resets_on_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/attempted.json");

        let mut l = AttemptLedger::load(&path);
        assert!(!l.attempted_today(100, 658));
        l.mark(100, 658);
        l.save(&path);

        // Reload (daemon restart mid-day) — the memory must survive, or one
        // deploy re-exposes the whole pool to the same refusal.
        let l2 = AttemptLedger::load(&path);
        assert!(l2.attempted_today(100, 658));
        assert!(!l2.attempted_today(100, 667), "other issues stay eligible");
        // Tomorrow the issue gets a fresh chance; gave-up handles permanence.
        assert!(!l2.attempted_today(101, 658));

        // Marking on a new day clears yesterday's entries.
        let mut l3 = l2;
        l3.mark(101, 700);
        assert!(!l3.attempted_today(101, 658));
        assert!(l3.attempted_today(101, 700));
    }

    #[test]
    fn attempt_ledger_survives_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, b"{ nope").unwrap();
        assert!(!AttemptLedger::load(&junk).attempted_today(1, 1));
    }

    // ---- #843: the scoper's predictions are binding, pre-build ----

    #[test]
    fn scope_headers_carry_size_and_guarded_path_predictions() {
        let o = parse_scope_output(
            "VERDICT: fixable\nCOMPLEXITY: medium\nEST-DIFF-LINES: ~2200\nGUARDED-PATHS: no\n\nspec",
        );
        assert!(o.fixable);
        assert_eq!(o.est_diff_lines, Some(2200));
        assert!(!o.guarded_paths);

        let o = parse_scope_output(
            "VERDICT: fixable\nCOMPLEXITY: simple\nEST-DIFF-LINES: 120\nGUARDED-PATHS: yes\n\nspec",
        );
        assert_eq!(o.est_diff_lines, Some(120));
        assert!(o.guarded_paths);

        // Missing headers (older prompt, glitch) must not invent predictions.
        let o = parse_scope_output("VERDICT: fixable\nCOMPLEXITY: simple\n\nspec");
        assert_eq!(o.est_diff_lines, None);
        assert!(!o.guarded_paths);
    }

    #[test]
    fn scope_predictions_refuse_before_the_build_only_when_explicit() {
        let base = parse_scope_output("VERDICT: fixable\nCOMPLEXITY: simple\n\nspec");
        // No headers -> no refusal: absence is not evidence.
        assert!(scope_predicts_refusal(&base).is_none());

        // #667's shape: ~2200 lines against a 600 cap.
        let big = parse_scope_output(
            "VERDICT: fixable\nCOMPLEXITY: medium\nEST-DIFF-LINES: 2200\nGUARDED-PATHS: no\n\nspec",
        );
        let reason = scope_predicts_refusal(&big).expect("must refuse pre-build");
        assert!(reason.contains("2200"), "{reason}");

        // An honest near-cap estimate proceeds — the margin absorbs it.
        let near = parse_scope_output(
            "VERDICT: fixable\nCOMPLEXITY: medium\nEST-DIFF-LINES: 700\nGUARDED-PATHS: no\n\nspec",
        );
        assert!(scope_predicts_refusal(&near).is_none());

        // #831's shape: the work cannot avoid guarded paths.
        let guarded = parse_scope_output(
            "VERDICT: fixable\nCOMPLEXITY: simple\nEST-DIFF-LINES: 50\nGUARDED-PATHS: yes\n\nspec",
        );
        assert!(scope_predicts_refusal(&guarded).is_some());
    }

    #[test]
    fn env_example_is_exempt_from_the_path_guard() {
        // #658 burned two builds on this: `.env` substring-matched
        // `.env.example`, a tracked documentation file the issue itself asked
        // to update.
        let docs = diff_of(&[
            "diff --git a/.env.example b/.env.example",
            "+++ b/.env.example",
            "+AUGMENTAGENT_MODEL_CODEX_QUALITY=gpt-5.6-terra",
        ]);
        assert!(
            blast_radius_hit_in_diff(&docs).is_none(),
            "documenting a knob in .env.example must not cost a build"
        );
        // The real .env is still refused.
        let real = diff_of(&[
            "diff --git a/.env b/.env",
            "+++ b/.env",
            "+SECRET=x",
        ]);
        assert!(blast_radius_hit_in_diff(&real).is_some());
    }

    // ---- the diff guard judges PATHS, not prose ----

    /// Build a unified diff with no leading indentation. Written as joined
    /// lines rather than a `\`-continued literal on purpose: continuations
    /// keep the source's indentation inside the string, so every line after
    /// the first starts with spaces instead of `+`/`-`, and a test built that
    /// way passes vacuously no matter what the guard does.
    fn diff_of(lines: &[&str]) -> String {
        format!("{}\n", lines.join("\n"))
    }

    #[test]
    fn diff_content_does_not_trip_the_guard() {
        // The real #834 refusal: a test fixture's fake email subject.
        let fixture = diff_of(&[
            "diff --git a/crates/x/src/triage.rs b/crates/x/src/triage.rs",
            "+++ b/crates/x/src/triage.rs",
            "+            \"Quick question on the deploy\",",
        ]);
        assert!(
            blast_radius_hit(&fixture).is_some(),
            "precondition: the unscoped matcher DOES trip on that fixture line"
        );
        assert!(
            blast_radius_hit_in_diff(&fixture).is_none(),
            "a fake email subject in a test must not discard a completed build"
        );

        // Context lines — the same failure one step earlier.
        let context = diff_of(&[
            "diff --git a/crates/x/src/triage.rs b/crates/x/src/triage.rs",
            "+++ b/crates/x/src/triage.rs",
            "@@ -10,7 +10,7 @@",
            "     // NOTE: the deploy path rebuilds this on every push.",
            "-    if is_invite(e) { draft(e) }",
            "+    if is_invite(e) { skip(e) }",
        ]);
        assert!(blast_radius_hit_in_diff(&context).is_none());
    }

    #[test]
    fn diff_guard_still_refuses_guarded_paths() {
        for (lines, want) in [
            (
                vec![
                    "diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml",
                    "+++ b/.github/workflows/ci.yml",
                    "+  run: echo hi",
                ],
                ".github/workflows",
            ),
            (
                vec![
                    "diff --git a/scripts/check-for-updates.sh b/scripts/check-for-updates.sh",
                    "+++ b/scripts/check-for-updates.sh",
                    "+echo hi",
                ],
                "scripts/check-for-updates",
            ),
            (
                vec![
                    "diff --git a/crates/augmentagent-auth/src/auth.rs b/crates/augmentagent-auth/src/auth.rs",
                    "+++ b/crates/augmentagent-auth/src/auth.rs",
                    "+fn f() {}",
                ],
                // `/auth` matches before `auth.rs` — both are in the list and
                // either is a correct refusal; pin the one the order yields.
                "/auth",
            ),
        ] {
            let d = diff_of(&lines);
            let (pat, _) = blast_radius_hit_in_diff(&d)
                .unwrap_or_else(|| panic!("must refuse: {}", lines[0]));
            assert_eq!(pat, want);
        }

        // A newly CREATED file under a guarded path shows in the header, so
        // dropping content scanning does not lose the #793 property.
        let created = diff_of(&[
            "diff --git a/systemd/augmentagent.service b/systemd/augmentagent.service",
            "new file mode 100644",
            "+++ b/systemd/augmentagent.service",
            "+ExecStart=/usr/bin/augmentagent",
        ]);
        assert!(blast_radius_hit_in_diff(&created).is_some());
    }

    #[test]
    fn generic_text_matcher_is_unchanged() {
        // The multi-repo path and the issue-prose path still use free text.
        assert!(is_blast_radius("rotate the DISCORD secret"));
        assert!(is_blast_radius("edit scripts/check-for-updates.sh"));
        assert!(!is_blast_radius("fix a typo in the README"));
    }

    // ---- a refusal must say what it saw ----

    #[test]
    fn blast_radius_refusal_names_the_pattern_and_the_line() {
        let diff = "diff --git a/src/ok.rs b/src/ok.rs\n                    +fn fine() {}\n                    diff --git a/systemd/augmentagent.service b/systemd/augmentagent.service\n                    +ExecStart=/usr/bin/augmentagent serve\n";
        let (pattern, line) = blast_radius_hit(diff).expect("must trip");
        assert!(
            BLAST_RADIUS_PATTERNS.contains(&pattern),
            "the reported pattern must be one the guard actually holds"
        );
        assert!(
            line.contains("systemd"),
            "the reported line must be the offending one, not the first line: {line}"
        );
        // Clean diffs stay clean.
        assert!(blast_radius_hit("diff --git a/src/x.rs b/src/x.rs\n+fn f() {}").is_none());
        // The bool wrapper keeps its old meaning for every existing caller.
        assert!(is_blast_radius(diff));
        assert!(!is_blast_radius("+fn f() {}"));
    }

    #[test]
    fn issue_prose_refusal_names_its_pattern() {
        let (pattern, _) =
            issue_blast_radius_hit("please patch scripts/check-for-updates.sh").expect("trips");
        assert_eq!(pattern, "scripts/check-for-updates");
        assert!(issue_blast_radius_hit("an ordinary bug in the digest").is_none());
    }

    // ---- #819: the prose prefilter must not eat the pool ----

    #[test]
    fn issue_prose_filter_keeps_paths_and_drops_english() {
        // Real bodies from the open pool that the full list was blocking.
        for prose in [
            "an auto-merged PR makes the updater rebuild and deploy",
            "the updater runs systemctl --user restart augmentagent.service",
            "grep finds only Cargo.lock in the list",
        ] {
            assert!(
                is_blast_radius(prose),
                "precondition: the diff-level list matches this prose: {prose}"
            );
        }
        // The first two are description, not intent, and must now be pickable.
        assert!(!is_issue_blast_radius(
            "an auto-merged PR makes the updater rebuild and deploy"
        ));
        assert!(!is_issue_blast_radius(
            "the updater runs systemctl --user restart augmentagent.service"
        ));

        // Path-shaped tokens still refuse.
        assert!(is_issue_blast_radius("patch scripts/check-for-updates.sh"));
        assert!(is_issue_blast_radius("add a .github/workflows/ci.yml"));
        assert!(is_issue_blast_radius("edit crates/augmentagent-auth/src/auth.rs"));
        // `keyring` / `secret` / `credential` are prose, not paths: an issue
        // that merely mentions where a token lives must still be pickable.
        // The scoper is the component equipped to judge intent, and the
        // diff-level guard still refuses whatever it produces.
        assert!(!is_issue_blast_radius("the token is read from the keyring slot"));
        assert!(is_blast_radius("the token is read from the keyring slot"));

        // The diff-level guard is untouched — it still sees everything.
        assert!(is_blast_radius("+++ b/deploy/release.sh"));
        assert!(is_blast_radius("+++ b/crates/augmentagent-auth/src/auth.rs"));
    }

    // ---- #814: the daily cap must outlive the deploy restart ----

    #[test]
    fn daily_counter_survives_a_restart_within_the_same_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/autopr-daily-runs.json");

        let mut c = DailyCounter::load(&path);
        c.record(100);
        c.record(100);
        c.save(&path);

        // An auto-merged PR makes the updater restart the daemon. The cap
        // must not start over, or success buys the loop a fresh budget.
        let mut reloaded = DailyCounter::load(&path);
        assert_eq!(reloaded.runs_today(100), 2);
        // Rollover still zeroes it.
        assert_eq!(reloaded.runs_today(101), 0);
    }

    #[test]
    fn daily_counter_load_falls_back_to_zero_on_missing_or_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        assert_eq!(DailyCounter::load(&missing).runs_today(1), 0);

        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, b"{ not json").unwrap();
        assert_eq!(DailyCounter::load(&junk).runs_today(1), 0);
    }

    // ---- #815: an orphaned remote branch must be detectable ----

    #[tokio::test]
    async fn remote_branch_exists_sees_a_branch_pushed_by_an_interrupted_run() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("origin.git");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr));
        };
        git(dir.path(), &["init", "-q", "--bare", "origin.git"]);
        git(&work, &["init", "-q", "-b", "main", "."]);
        git(&work, &["-c", "user.email=t@e", "-c", "user.name=t", "commit", "-q",
                     "--allow-empty", "-m", "init"]);
        git(&work, &["remote", "add", "origin", &bare.to_string_lossy()]);
        git(&work, &["push", "-q", "origin", "main"]);

        let branch = format!("{BRANCH_PREFIX}1");
        assert!(
            !remote_branch_exists(&work, &branch).await,
            "a branch that was never pushed must not read as orphaned"
        );

        // Simulate a run that pushed and then died before `gh pr create`.
        git(&work, &["branch", &branch]);
        git(&work, &["push", "-q", "origin", &branch]);
        assert!(
            remote_branch_exists(&work, &branch).await,
            "the orphaned branch must be detected, or the next attempt dies \
             at a non-fast-forward push every tick forever"
        );
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
