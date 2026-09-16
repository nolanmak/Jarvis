//! #1011 — scope-pass eval harness: measure the loop's JUDGMENT.
//!
//! The 408 tests in this crate pin what the code does. Eleven of them assert
//! on prompt text, and every one of those proves a sentence is *in* a prompt,
//! never that the model *obeys* it — `red_main_prompt_forbids_assertion_weakening`
//! greps the fix prompt for "Never weaken, loosen, or delete an assertion" and
//! stops there.
//!
//! So a prompt change that makes the scoper quietly more cautious breaks
//! nothing and is invisible: it just refuses issues it used to build, and the
//! only symptom is fewer PRs, which looks exactly like a quiet week. The build
//! model also resolves through a live pointer, so the judgment can move with no
//! commit of ours at all.
//!
//! This is the ruler. Tier one only: run the scoping pass over cached issue
//! text and grade its verdict. One reasoner call per case, no build, no gate,
//! no worktree, and — enforced by [`scratch_env`] — no production state.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use augmentagent_channel_core::Reasoner;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// What the fixture says the scoper SHOULD decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Fixable,
    NotFixable,
}

/// One graded case: an issue, cached so a run is reproducible and offline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCase {
    pub id: String,
    pub issue: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub expect: Expect,
    /// The commit to scope AGAINST, and the reason this harness measures
    /// anything at all.
    ///
    /// The scoping pass reads the working tree. Point it at today's `main` and
    /// every issue the loop already fixed is refused — correctly, because the
    /// fix is sitting right there. Grading that would only prove the scoper
    /// can read `git log`. The question worth asking is the one it faced at
    /// the time: at the commit where the decision was made, was this issue
    /// agent-fixable? `None` means today's checkout, which is right for a
    /// tracking epic that is not fixable at any commit.
    pub base: Option<String>,
}

/// What the scoper actually said, distilled from the real parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub fixable: bool,
    pub complexity: String,
    pub est_diff_lines: Option<usize>,
    /// The scoper's own words, so a miss is diagnosable rather than just red.
    pub reason: String,
    /// #1012 — the acceptance criteria the scoping pass emitted. Observed
    /// data, reported and never graded: whether criteria IMPROVE outcomes is
    /// a question about the score over time, not about one case. Reporting
    /// them is also the only way to see, from outside, that the scoper is
    /// emitting a block the parser actually accepts.
    pub criteria: Vec<String>,
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRow {
    pub id: String,
    pub issue: u64,
    pub base: Option<String>,
    pub expect: Expect,
    pub actual: String,
    pub pass: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub passed: usize,
    pub total: usize,
    pub misses: Vec<EvalRow>,
}

// ---------------------------------------------------------------------------
// Pure core. Everything gradeable lives here so the harness is testable
// without a network call, a reasoner, or a worktree (C1).
// ---------------------------------------------------------------------------

impl Expect {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(' ', "-").as_str() {
            "fixable" => Some(Expect::Fixable),
            "not-fixable" => Some(Expect::NotFixable),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Expect::Fixable => "fixable",
            Expect::NotFixable => "not-fixable",
        }
    }
}

/// Every state path the loop can be pointed at. The eval redirects all of
/// them; [`scratch_env`] is the single place that knows the list, and a test
/// rediscovers it from `self_improve.rs` so a new state file cannot be added
/// without the eval learning about it.
pub fn production_state_vars() -> &'static [(&'static str, &'static str)] {
    &[
        ("AUGMENTAGENT_AUTOPR_ATTEMPTED_FILE", "attempted.json"),
        ("AUGMENTAGENT_AUTOPR_BASELINE_FILE", "baseline.json"),
        ("AUGMENTAGENT_AUTOPR_COUNTER_FILE", "counter.json"),
        ("AUGMENTAGENT_AUTOPR_HISTORY_FILE", "history.json"),
        ("AUGMENTAGENT_SELFIMPROVE_LOCK", "self-improve.lock"),
    ]
}

/// Redirect every production state path into `dir`.
///
/// An eval run must not spend the daily cap, write the attempt ledger, poison
/// the baseline cache, or take the run lock the daemon uses. Scoping alone
/// should touch none of those, but "should" is how state gets written anyway:
/// the scope path is one refactor away from someone recording an attempt, and
/// a corrupted ledger would cost real shipped work. Redirect unconditionally.
pub fn scratch_env(dir: &Path) -> Vec<(String, String)> {
    production_state_vars()
        .iter()
        .map(|(var, file)| {
            (
                (*var).to_string(),
                dir.join(file).to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// Parse the fixture file.
///
/// Strict on purpose. An `expect` we do not understand, or a duplicated id,
/// stops the run and names the offender: the entire value of an eval is that a
/// miss is visible, and a tolerated typo becomes a permanently green row that
/// nobody looks at again.
pub fn parse_cases(json: &str) -> Result<Vec<EvalCase>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parse eval cases")?;
    let arr = v.as_array().context("eval cases must be a JSON array")?;
    let mut out: Vec<EvalCase> = Vec::with_capacity(arr.len());
    for (i, row) in arr.iter().enumerate() {
        let at = |k: &str| row.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
        let id = at("id").trim().to_string();
        if id.is_empty() {
            bail!("eval case #{i} has no id");
        }
        // The id becomes a directory name under the scratch dir, so keep it
        // to what a label needs and a path cannot escape.
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!(
                "eval case id {id:?} must be letters, digits, dash or underscore: \
                 it becomes a directory name"
            );
        }
        if out.iter().any(|c| c.id == id) {
            bail!("duplicate eval case id {id:?}; ids name rows in the report and must be unique");
        }
        let raw = at("expect");
        let expect = Expect::parse(raw).with_context(|| {
            format!("eval case {id:?} has unknown expect {raw:?}; use \"fixable\" or \"not-fixable\"")
        })?;
        out.push(EvalCase {
            id,
            issue: row.get("issue").and_then(serde_json::Value::as_u64).unwrap_or(0),
            title: at("title").to_string(),
            body: at("body").to_string(),
            author: at("author").to_string(),
            expect,
            base: Some(at("base").trim().to_string()).filter(|b| !b.is_empty()),
        });
    }
    Ok(out)
}

/// The cases named by `--only`, in fixture order so the report is stable.
/// An id that matches nothing is an error: silently evaluating zero cases and
/// reporting `0/0 passed` is the worst possible answer.
pub fn select<'a>(cases: &'a [EvalCase], only: Option<&str>) -> Result<Vec<&'a EvalCase>> {
    let Some(only) = only else {
        return Ok(cases.iter().collect());
    };
    let wanted: Vec<&str> = only.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if wanted.is_empty() {
        // `--only "$IDS"` with an unset variable lands here. Grading nothing
        // and reporting `0/0 passed` is the one answer a regression harness
        // must never give.
        bail!("--only named no case ids; pass at least one, or omit the flag to run them all");
    }
    for w in &wanted {
        if !cases.iter().any(|c| c.id.eq_ignore_ascii_case(w)) {
            bail!("--only names {w:?}, which is not an id in the fixture file");
        }
    }
    Ok(cases
        .iter()
        .filter(|c| wanted.iter().any(|w| c.id.eq_ignore_ascii_case(w)))
        .collect())
}

/// Distil the scoper's raw output through the REAL parser (C7).
///
/// Deliberately not a reimplementation: the eval exists to grade the parser we
/// ship, and a copy could stay green while the shipped one broke.
pub fn observe(raw: &str) -> Observed {
    let parsed = crate::self_improve::parse_scope_output(raw);
    Observed {
        fixable: parsed.fixable,
        complexity: parsed.complexity.as_str().to_string(),
        est_diff_lines: parsed.est_diff_lines,
        reason: one_line(&parsed.body, 200),
        criteria: parsed.criteria.clone(),
    }
}

/// A table cell holds one line. Collapse whitespace and cap the WIDTH.
///
/// `max` counts characters and includes the ellipsis, because this is a
/// display width. Byte length is the wrong unit for a cell holding a model's
/// prose: one multi-byte character would blow a byte budget while occupying a
/// single column.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let keep = max.saturating_sub(1);
    let cut = flat.char_indices().nth(keep).map(|(i, _)| i).unwrap_or(flat.len());
    format!("{}…", &flat[..cut])
}

/// Grade one case. `None` means the scoper never answered (reasoner error,
/// timeout): a miss rather than a skip, or an outage would read as a perfect
/// score.
pub fn grade(case: &EvalCase, observed: Option<&Observed>) -> EvalRow {
    let Some(o) = observed else {
        return EvalRow {
            id: case.id.clone(),
            issue: case.issue,
            base: case.base.clone(),
            expect: case.expect,
            actual: "no verdict".into(),
            pass: false,
            note: "the scoping pass produced no parseable verdict".into(),
        };
    };
    let actual = if o.fixable { "fixable" } else { "not-fixable" };
    let pass = matches!(
        (case.expect, o.fixable),
        (Expect::Fixable, true) | (Expect::NotFixable, false)
    );
    let note = if pass {
        match case.expect {
            Expect::Fixable => format!(
                "complexity {}, est ~{} lines, {} criteria",
                o.complexity,
                o.est_diff_lines.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                o.criteria.len()
            ),
            Expect::NotFixable => format!("refused: {}", o.reason),
        }
    } else {
        match case.expect {
            // The expensive direction: work we know is buildable, refused.
            Expect::Fixable => format!("WRONGLY REFUSED: {}", o.reason),
            Expect::NotFixable => format!("should have refused, instead: {}", o.reason),
        }
    };
    EvalRow {
        id: case.id.clone(),
        issue: case.issue,
        base: case.base.clone(),
        expect: case.expect,
        actual: actual.into(),
        pass,
        note: one_line(&note, 200),
    }
}

pub fn summarize(rows: &[EvalRow]) -> Summary {
    Summary {
        passed: rows.iter().filter(|r| r.pass).count(),
        total: rows.len(),
        misses: rows.iter().filter(|r| !r.pass).cloned().collect(),
    }
}

/// `|` ends a cell in GitHub-flavoured markdown, and the scoper's reason is
/// free text written by a model. Unescaped, one backtick-quoted alternation
/// silently adds a column and shifts every cell after it.
fn cell(s: &str) -> String {
    s.replace('|', r"\|")
}

pub fn render_report(rows: &[EvalRow], cases_path: &str, started: &str) -> String {
    let s = summarize(rows);
    let mut out = String::new();
    out.push_str("# Auto-PR scope eval\n\n");
    out.push_str(&format!(
        "Cases: `{}` · run {} · **{}/{} passed**\n\n",
        cell(cases_path),
        cell(started),
        s.passed,
        s.total
    ));
    out.push_str("| # | Issue | Scoped at | Expected | Actual | Result | Notes |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    for r in rows {
        out.push_str(&format!(
            "| {} | #{} | `{}` | {} | `{}` | {} | {} |\n",
            cell(&r.id),
            r.issue,
            cell(r.base.as_deref().unwrap_or("HEAD")),
            r.expect.label(),
            cell(&r.actual),
            if r.pass { "pass" } else { "MISS" },
            cell(&r.note)
        ));
    }
    out.push_str("\n## What the grades mean\n");
    out.push_str("- **fixable**: the scoping pass judged the issue agent-fixable and the pipeline would build it.\n");
    out.push_str("- **not-fixable**: the scoping pass refused it, which for these fixtures is the correct call.\n");
    out.push_str("- **Scoped at**: the commit the working tree was placed at before scoping. `HEAD` means today's checkout.\n");
    out.push_str("- A miss on a `fixable` case is the expensive direction: shippable work the loop would quietly decline.\n");
    out.push_str("\n## Misses\n");
    if s.misses.is_empty() {
        out.push_str("- none\n");
    } else {
        for m in &s.misses {
            out.push_str(&format!("- **{}** (#{}) — {}\n", cell(&m.id), m.issue, cell(&m.note)));
        }
    }
    out
}

/// The saved rows for exactly the selected cases, in fixture order.
///
/// A case the saved run never covered becomes an explicit unrun row rather
/// than vanishing: dropping it would shrink the denominator and inflate the
/// score, which is the one thing a score must never do.
fn rows_for(saved: &[EvalRow], cases: &[&EvalCase]) -> Vec<EvalRow> {
    cases
        .iter()
        .map(|c| {
            saved
                .iter()
                // Matched on the whole question, not just the id. Editing a
                // fixture's expectation or its pinned commit asks a DIFFERENT
                // question, and inheriting the old verdict would report a
                // stale pass for a case that was never re-graded. A baseline
                // that reports a stale score is worse than none: it is
                // believed.
                .find(|r| r.id == c.id && r.expect == c.expect && r.base == c.base)
                .cloned()
                .unwrap_or_else(|| grade(c, None))
        })
        .collect()
}

/// Fold a partial run into the existing baseline.
///
/// A `--only` run grades a subset. Saving just that subset would discard the
/// rest of the baseline, so the next full `--report-only` would report every
/// other case as unrun — a silent loss of the very record the baseline exists
/// to keep. Rows present in both are replaced by the fresh result; rows the
/// run did not cover keep their previous verdict; a case the baseline has
/// never seen is appended.
fn merge_into_baseline(baseline: &[EvalRow], fresh: &[EvalRow]) -> Vec<EvalRow> {
    let mut out: Vec<EvalRow> = baseline
        .iter()
        .map(|b| {
            fresh
                .iter()
                .find(|f| f.id == b.id)
                .cloned()
                .unwrap_or_else(|| b.clone())
        })
        .collect();
    for f in fresh {
        if !baseline.iter().any(|b| b.id == f.id) {
            out.push(f.clone());
        }
    }
    out
}

/// Save a run: the rows AND when the scoring actually happened.
pub fn save_run(rows: &[EvalRow], started: &str) -> Result<String> {
    let v = serde_json::json!({
        "started": started,
        "rows": serde_json::from_str::<serde_json::Value>(&rows_to_json(rows)?)
            .context("re-read rows")?,
    });
    serde_json::to_string_pretty(&v).context("serialise saved run")
}

/// Read a saved run back, returning its rows and its ORIGINAL timestamp.
pub fn load_run(json: &str) -> Result<(Vec<EvalRow>, String)> {
    let v: serde_json::Value = serde_json::from_str(json).context("parse saved run")?;
    let started = v
        .get("started")
        .and_then(serde_json::Value::as_str)
        .context("saved run has no timestamp")?
        .to_string();
    let rows = v.get("rows").context("saved run has no rows")?;
    Ok((
        rows_from_json(&serde_json::to_string(rows).context("re-render rows")?)?,
        started,
    ))
}

/// Serialise graded rows so a report can be re-rendered without re-running.
pub fn rows_to_json(rows: &[EvalRow]) -> Result<String> {
    let v: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "issue": r.issue,
                "base": r.base,
                "expect": r.expect.label(),
                "actual": r.actual,
                "pass": r.pass,
                "note": r.note,
            })
        })
        .collect();
    serde_json::to_string_pretty(&v).context("serialise eval rows")
}

/// Read back a saved run. Strict: a row missing its verdict is an error, never
/// a default, because a row that quietly renders as a pass is the one failure
/// mode an eval must not have.
pub fn rows_from_json(json: &str) -> Result<Vec<EvalRow>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parse saved run")?;
    let arr = v.as_array().context("saved run must be a JSON array")?;
    arr.iter()
        .enumerate()
        .map(|(i, r)| {
            let str_at = |k: &str| -> Result<String> {
                r.get(k)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .with_context(|| format!("saved row #{i} has no {k}"))
            };
            let expect_raw = str_at("expect")?;
            Ok(EvalRow {
                id: str_at("id")?,
                issue: r.get("issue").and_then(serde_json::Value::as_u64).unwrap_or(0),
                base: r
                    .get("base")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                expect: Expect::parse(&expect_raw)
                    .with_context(|| format!("saved row #{i} has unknown expect {expect_raw:?}"))?,
                actual: str_at("actual")?,
                pass: r
                    .get("pass")
                    .and_then(serde_json::Value::as_bool)
                    .with_context(|| format!("saved row #{i} has no pass verdict"))?,
                note: str_at("note")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Runner. The only impure part: one scoping call per case.
// ---------------------------------------------------------------------------

/// The prefix every eval scratch dir carries, under the system temp dir.
const SCRATCH_PREFIX: &str = "autopr-eval-";

/// Scratch dirs left by eval runs that are no longer alive.
///
/// A killed run — ctrl-c, an OOM, a `pkill` — never reaches its cleanup, so
/// its scratch dir survives. Since trees are materialised with `git archive`
/// there is no registration to leak — the leftovers are only a directory — but
/// they are a directory holding a full checkout per case, so the next run
/// reclaims them.
///
/// Conservative on every axis: only `<tmp>/autopr-eval-<pid>`, only when that
/// pid is gone, and never our own.
fn stale_scratch(
    dirs: impl Iterator<Item = PathBuf>,
    slug: &str,
    mine: u32,
    alive: impl Fn(u32) -> bool,
) -> Vec<PathBuf> {
    let tmp = std::env::temp_dir();
    let prefix = format!("{SCRATCH_PREFIX}{slug}-");
    dirs.filter(|d| {
        if d.parent() != Some(tmp.as_path()) {
            return false;
        }
        let Some(name) = d.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let Some(pid) = name.strip_prefix(&prefix).and_then(|p| p.parse::<u32>().ok()) else {
            return false;
        };
        pid != mine && !alive(pid)
    })
    .collect()
}

/// A short stable id for a repository, so scratch is owned per repo.
///
/// The temp dir is shared by every checkout on the box. Without this, an eval
/// in one repository would reclaim a dead eval's scratch from another: it
/// would delete a checkout belonging to another repository's run — one that
/// may still be in use if its pid was recycled.
fn repo_slug(repo_root: &Path) -> String {
    // FNV-1a over the path. A hash, not a name: it only has to be stable,
    // short, and safe in a path segment.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in repo_root.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

/// Is a process with this id still running?
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Remove what previous runs left behind, then drop any worktree
/// registrations that pointed into them.
async fn reclaim_stale_scratch(repo_root: &Path) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let dirs = entries.filter_map(|e| e.ok()).map(|e| e.path());
    let stale = stale_scratch(dirs, &repo_slug(repo_root), std::process::id(), pid_alive);
    if stale.is_empty() {
        return;
    }
    for d in &stale {
        // Nothing to deregister: `git archive` left no bookkeeping behind, so
        // a dead run's leftovers are just a directory.
        if let Err(e) = std::fs::remove_dir_all(d) {
            eprintln!("could not reclaim {}: {e}", d.display());
        }
    }
    println!("reclaimed {} scratch dir(s) from earlier runs", stale.len());
}

/// A fixture path as it should appear in a committed report: relative to the
/// repository when it lives inside it, so `RESULTS.md` does not record whose
/// checkout produced it.
fn display_path(path: &Path, repo_root: &Path) -> String {
    path.strip_prefix(repo_root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

pub const DEFAULT_CASES: &str = "eval/autopr-cases.json";
pub const DEFAULT_REPORT: &str = "eval/RESULTS.md";

/// Re-fetch the cached issue text from GitHub, keeping ids and expectations.
///
/// Fixtures cache the issue body so a run is reproducible and offline. That
/// cache goes stale when an issue is edited, and a stale fixture silently
/// grades the scoper on text it will never see in production, so refreshing is
/// explicit rather than automatic.
pub async fn refresh(repo_root: &Path, cases_path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(cases_path)
        .with_context(|| format!("read {}", cases_path.display()))?;
    let cases = parse_cases(&raw)?;
    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(cases.len());
    for c in &cases {
        let out = tokio::process::Command::new("gh")
            .args([
                "issue",
                "view",
                &c.issue.to_string(),
                "--json",
                "number,title,body,author",
            ])
            .current_dir(repo_root)
            .output()
            .await
            .context("spawn gh")?;
        if !out.status.success() {
            bail!(
                "gh issue view {} failed: {}",
                c.issue,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parse gh json")?;
        let get = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        rows.push(serde_json::json!({
            "id": c.id,
            "issue": c.issue,
            "title": get("title"),
            "body": get("body").trim(),
            "author": v.pointer("/author/login").and_then(serde_json::Value::as_str).unwrap_or(""),
            "expect": c.expect.label(),
            "base": c.base,
        }));
        println!("refreshed {} (#{})", c.id, c.issue);
    }
    std::fs::write(
        cases_path,
        serde_json::to_string_pretty(&rows).context("render cases")? + "\n",
    )
    .with_context(|| format!("write {}", cases_path.display()))?;
    Ok(())
}

/// Run the eval. Returns the process exit code: 0 when every case passed.
pub async fn run(
    repo_root: &Path,
    cases_path: Option<&Path>,
    only: Option<&str>,
    report_path: Option<&Path>,
    report_only: bool,
) -> Result<i32> {
    let cases_path = cases_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.join(DEFAULT_CASES));
    let report_path = report_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.join(DEFAULT_REPORT));

    let raw = std::fs::read_to_string(&cases_path)
        .with_context(|| format!("read {}", cases_path.display()))?;
    let all = parse_cases(&raw)?;
    let cases = select(&all, only)?;
    let started = now_utc();

    // Point every production state path at a scratch dir BEFORE anything can
    // read one. Scoping alone should touch none of them, but "should" is how
    // state gets written anyway, and a corrupted ledger costs real shipped
    // work. The dir is removed on the way out.
    reclaim_stale_scratch(repo_root).await;
    let scratch = std::env::temp_dir().join(format!(
        "{SCRATCH_PREFIX}{}-{}",
        repo_slug(repo_root),
        std::process::id()
    ));
    std::fs::create_dir_all(&scratch).context("create scratch dir")?;
    for (k, v) in scratch_env(&scratch) {
        std::env::set_var(k, v);
    }

    // Committed, not ignored: `--report-only` must render the committed eval
    // on a fresh clone without first spending a reasoner call per case, and a
    // machine-readable baseline can be diffed rather than eyeballed.
    let saved_path = cases_path.with_file_name("last-run.json");
    let mut started = started;
    let mut rows: Vec<EvalRow> = Vec::with_capacity(cases.len());
    if report_only {
        // Render from the last run rather than an empty table: a flag that can
        // only ever report 0/0 is a flag that lies.
        let raw = std::fs::read_to_string(&saved_path).with_context(|| {
            format!(
                "--report-only needs a previous run at {}; run the eval once first",
                saved_path.display()
            )
        })?;
        let (saved_rows, saved_started) = load_run(&raw)?;
        rows = rows_for(&saved_rows, &cases);
        started = saved_started;
    } else {
        let reasoner = augmentagent_channel_core::build_reasoner();
        for (i, c) in cases.iter().enumerate() {
            println!(
                "[{}/{}] {} — scoping issue #{} ({})",
                i + 1,
                cases.len(),
                c.id,
                c.issue,
                c.expect.label()
            );
            let issue = crate::self_improve::Issue {
                number: c.issue,
                title: c.title.clone(),
                body: c.body.clone(),
                author: c.author.clone(),
                author_trusted: true,
                research_filed: false,
            };
            // `None` for prior attempts: tier one measures the FIRST scoping
            // judgment on an issue. Retry judgment — how the scoper reacts to
            // an attempt digest — is real and unmeasured, but it needs a
            // different fixture shape (an issue plus a specific attempt
            // history) and belongs to its own tier rather than being faked
            // here with a digest no real run produced.
            let prompt = crate::self_improve::build_scope_prompt(&issue, None);

            // Place the tree at the commit the decision was actually made at.
            // Without this the scoper reads today's `main`, finds the fix
            // already merged, and refuses — correctly, which measures nothing.
            let (scope_dir, checkout) = match &c.base {
                None => (repo_root.to_path_buf(), None),
                Some(base) => {
                    let dir = scratch.join(format!("tree-{}", c.id));
                    match materialise_at(repo_root, base, &dir).await {
                        Ok(()) => (dir.clone(), Some(dir)),
                        Err(e) => {
                            eprintln!("    could not materialise {base}: {e:#}");
                            rows.push(grade(c, None));
                            continue;
                        }
                    }
                }
            };

            let observed = match reasoner
                .call(&crate::self_improve::scope_opts(scope_dir), &prompt)
                .await
            {
                Ok(out) => Some(observe(&out)),
                Err(e) => {
                    eprintln!("    scoping call failed: {e:#}");
                    None
                }
            };
            if let Some(dir) = checkout {
                let _ = std::fs::remove_dir_all(&dir);
            }
            let row = grade(c, observed.as_ref());
            println!("    {} — {}", if row.pass { "pass" } else { "MISS" }, row.note);
            for c in observed.iter().flat_map(|o| o.criteria.iter()) {
                println!("      criterion: {c}");
            }
            rows.push(row);
        }
    }

    let _ = std::fs::remove_dir_all(&scratch);

    if !report_only {
        // Fold into the baseline rather than replacing it, or a `--only` run
        // silently drops every case it did not grade.
        let to_save = match std::fs::read_to_string(&saved_path)
            .ok()
            .and_then(|raw| load_run(&raw).ok())
        {
            Some((prev, _)) => merge_into_baseline(&prev, &rows),
            None => rows.clone(),
        };
        std::fs::write(&saved_path, save_run(&to_save, &started)?)
            .with_context(|| format!("write {}", saved_path.display()))?;
    }

    let md = render_report(&rows, &display_path(&cases_path, repo_root), &started);
    if let Some(dir) = report_path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&report_path, &md)
        .with_context(|| format!("write {}", report_path.display()))?;
    println!("\n{md}");
    println!("report written to {}", report_path.display());

    let s = summarize(&rows);
    Ok(i32::from(s.passed != s.total))
}

/// Materialise the repository at `commit` into `dir`, registering NOTHING.
///
/// `git worktree add` would be the obvious tool and is the wrong one: it
/// records the checkout in the repository, so a run that dies before cleanup
/// leaves a registration behind in a repo this command does not own. `git
/// archive` writes the same tree with no bookkeeping at all, which means there
/// is no state to leak and nothing to reclaim but a directory.
///
/// Only committed content is written, which is exactly what should be scoped:
/// the tree as it was, not whatever is lying around untracked today.
async fn materialise_at(repo_root: &Path, commit: &str, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).context("create tree dir")?;
    let tar = dir.with_extension("tar");
    let out = tokio::process::Command::new("git")
        .args(["archive", "--format=tar", "-o", &tar.to_string_lossy(), commit])
        .current_dir(repo_root)
        .output()
        .await
        .context("spawn git archive")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tar);
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let out = tokio::process::Command::new("tar")
        .args(["-xf", &tar.to_string_lossy(), "-C", &dir.to_string_lossy()])
        .output()
        .await
        .context("spawn tar")?;
    let _ = std::fs::remove_file(&tar);
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn now_utc() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: &str, issue: u64, expect: Expect) -> EvalCase {
        EvalCase {
            id: id.into(),
            issue,
            title: "t".into(),
            body: "b".into(),
            author: "nolanmak".into(),
            expect,
            base: None,
        }
    }

    fn observed(fixable: bool, reason: &str) -> Observed {
        Observed {
            fixable,
            complexity: "simple".into(),
            est_diff_lines: Some(40),
            reason: reason.into(),
            criteria: Vec::new(),
        }
    }

    // ---- C2: an unknown expectation is a hard error, never a silent pass ----

    /// The whole value of an eval is that a miss is visible. An `expect` we do
    /// not understand must stop the run and name the case, because the
    /// alternative — defaulting it — turns a typo into a permanently green row
    /// that nobody ever looks at again.
    #[test]
    fn an_unknown_expectation_names_the_case_and_refuses_to_run() {
        let json = r#"[
          {"id":"E1","issue":1,"title":"t","body":"b","author":"a","expect":"fixable"},
          {"id":"E2","issue":2,"title":"t","body":"b","author":"a","expect":"probably?"}
        ]"#;
        let err = parse_cases(json).expect_err("unknown expect must not parse");
        let msg = format!("{err:#}");
        assert!(msg.contains("E2"), "the offending id must be named: {msg}");
        assert!(msg.contains("probably?"), "and the bad value: {msg}");
    }

    /// A case id becomes a directory name (`tree-<id>`) under the scratch
    /// dir. Codex flagged this non-blocking; it is five lines to make the
    /// invariant executable instead of implied, and I filed the same finding
    /// against someone else's repo this week. An id is a label, so restrict it
    /// to what a label needs and a path cannot escape.
    #[test]
    fn an_id_that_could_escape_the_scratch_directory_is_refused() {
        // No backslash case: JSON would eat it as an escape, so it tests the
        // decoder rather than the guard. The traversal shapes below cover it.
        for bad in ["../../etc", "a/b", ".", "..", "with space", "semi;colon", "~root"] {
            let json = format!(
                r#"[{{"id":"{bad}","issue":1,"title":"t","body":"b","author":"a","expect":"fixable"}}]"#
            );
            let err = parse_cases(&json).expect_err("must refuse id {bad:?}");
            assert!(format!("{err:#}").contains(bad), "the offending id must be named");
        }
        for ok in ["E1", "E1006", "case_1", "case-1"] {
            let json = format!(
                r#"[{{"id":"{ok}","issue":1,"title":"t","body":"b","author":"a","expect":"fixable"}}]"#
            );
            assert!(parse_cases(&json).is_ok(), "{ok:?} is a perfectly good id");
        }
    }

    #[test]
    fn duplicate_ids_are_refused_so_a_row_cannot_shadow_another() {
        let json = r#"[
          {"id":"E1","issue":1,"title":"t","body":"b","author":"a","expect":"fixable"},
          {"id":"E1","issue":2,"title":"t","body":"b","author":"a","expect":"fixable"}
        ]"#;
        let err = parse_cases(json).expect_err("duplicate id must not parse");
        assert!(format!("{err:#}").contains("E1"));
    }

    #[test]
    fn both_spellings_of_not_fixable_parse() {
        let json = r#"[
          {"id":"A","issue":1,"title":"t","body":"b","author":"a","expect":"not-fixable"},
          {"id":"B","issue":2,"title":"t","body":"b","author":"a","expect":"NOT FIXABLE"},
          {"id":"C","issue":3,"title":"t","body":"b","author":"a","expect":"Fixable"}
        ]"#;
        let cases = parse_cases(json).expect("parses");
        assert_eq!(
            cases.iter().map(|c| c.expect).collect::<Vec<_>>(),
            vec![Expect::NotFixable, Expect::NotFixable, Expect::Fixable]
        );
    }

    /// Found by running the harness: every `fixable` fixture was WRONGLY
    /// REFUSED, and the scoper was right each time — "this issue is already
    /// fixed at HEAD". A fixture set of issues the loop already shipped is
    /// worthless unless each one is scoped at the commit before its fix.
    #[test]
    fn a_case_can_pin_the_commit_it_is_scoped_against() {
        let json = r#"[
          {"id":"E1","issue":1,"title":"t","body":"b","author":"a",
           "expect":"fixable","base":"9084be0"},
          {"id":"E2","issue":2,"title":"t","body":"b","author":"a","expect":"not-fixable"}
        ]"#;
        let cases = parse_cases(json).expect("parses");
        assert_eq!(cases[0].base.as_deref(), Some("9084be0"));
        assert_eq!(cases[1].base, None, "an unpinned case uses today's checkout");
    }

    #[test]
    fn a_blank_base_is_the_same_as_no_base() {
        let json = r#"[{"id":"E1","issue":1,"title":"t","body":"b","author":"a",
                        "expect":"fixable","base":"   "}]"#;
        assert_eq!(parse_cases(json).expect("parses")[0].base, None);
    }

    /// The report must say which commit each verdict came from. Without it a
    /// score is not comparable to last week's, and comparing runs is the
    /// entire point of keeping one.
    #[test]
    fn the_report_names_the_commit_each_case_was_scoped_at() {
        let mut c = case("E1", 10, Expect::Fixable);
        c.base = Some("9084be0".into());
        let rows = vec![grade(&c, Some(&observed(true, "ok")))];
        let md = render_report(&rows, "c.json", "t");
        assert!(md.contains("9084be0"), "the base commit must appear:\n{md}");
        let unpinned = vec![grade(&case("E2", 11, Expect::Fixable), Some(&observed(true, "ok")))];
        assert!(
            render_report(&unpinned, "c.json", "t").contains("HEAD"),
            "an unpinned case must say it used the working checkout"
        );
    }

    // ---- C3: all four outcomes, and a miss carries the scoper's reason ----

    #[test]
    fn grading_covers_all_four_outcomes_and_quotes_the_reason_on_a_miss() {
        let want_fix = case("E1", 10, Expect::Fixable);
        let want_refuse = case("E2", 11, Expect::NotFixable);

        let hit = grade(&want_fix, Some(&observed(true, "clear one-file change")));
        assert!(hit.pass);
        assert_eq!(hit.actual, "fixable");

        let false_refusal = grade(&want_fix, Some(&observed(false, "ask is too vague")));
        assert!(!false_refusal.pass, "refusing work we know is buildable is a miss");
        assert!(
            false_refusal.note.contains("ask is too vague"),
            "a miss must carry the scoper's own reason: {}",
            false_refusal.note
        );

        let good_refusal = grade(&want_refuse, Some(&observed(false, "needs a product decision")));
        assert!(good_refusal.pass);

        let should_have_refused = grade(&want_refuse, Some(&observed(true, "looks mechanical")));
        assert!(!should_have_refused.pass);
        assert!(should_have_refused.note.contains("looks mechanical"));
    }

    /// A case the scoper never answered for (reasoner error, timeout) is a
    /// miss, not a silent skip — otherwise an outage reads as a perfect score.
    #[test]
    fn a_case_with_no_observation_is_a_miss_not_a_skip() {
        let row = grade(&case("E1", 10, Expect::Fixable), None);
        assert!(!row.pass);
        assert!(row.actual.contains("no verdict"), "{}", row.actual);
    }

    /// #1012 shipped a scope prompt that asks for acceptance criteria. Unit
    /// tests can prove the parser accepts a well-formed block; only a real
    /// scoping call can show the model actually EMITS one. The eval is the
    /// place that runs real scoping calls, so it reports what it saw.
    #[test]
    fn a_passing_case_reports_how_many_criteria_the_scoper_emitted() {
        let mut o = observed(true, "ok");
        o.criteria = vec!["C1: every lane guarded".into(), "C2: unknown fails closed".into()];
        let row = grade(&case("E1", 10, Expect::Fixable), Some(&o));
        assert!(row.pass);
        assert!(row.note.contains('2'), "the count must be visible: {}", row.note);
        assert!(
            row.note.to_lowercase().contains("criteria"),
            "and labelled, or the number means nothing: {}",
            row.note
        );

        // Zero is the interesting reading, so it must be stated rather than
        // omitted: it means the prompt asked and the model did not answer.
        let row = grade(&case("E2", 11, Expect::Fixable), Some(&observed(true, "ok")));
        assert!(row.note.contains("0 criteria"), "{}", row.note);
    }

    #[test]
    fn summarize_counts_and_lists_only_the_misses() {
        let rows = vec![
            grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "ok"))),
            grade(&case("E2", 2, Expect::Fixable), Some(&observed(false, "nope"))),
            grade(&case("E3", 3, Expect::NotFixable), Some(&observed(false, "ok"))),
        ];
        let s = summarize(&rows);
        assert_eq!((s.passed, s.total), (2, 3));
        assert_eq!(s.misses.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["E2"]);
    }

    // ---- C4: a reason containing a pipe must not break the table ----

    #[test]
    fn a_pipe_in_the_reason_cannot_break_the_markdown_table() {
        let rows = vec![grade(
            &case("E1", 10, Expect::Fixable),
            Some(&observed(false, "refused: `a | b` is ambiguous")),
        )];
        let md = render_report(&rows, "eval/autopr-cases.json", "2026-09-15T00:00:00Z");
        let row_line = md
            .lines()
            .find(|l| l.contains("E1") && l.starts_with('|'))
            .expect("a row for E1");
        assert!(row_line.contains(r"\|"), "the pipe must be escaped: {row_line}");
        // Counting every '|' would count the escaped one too. A cell
        // separator is a pipe NOT preceded by a backslash, and that is what
        // decides how many columns markdown actually renders.
        let separators = |l: &str| {
            let b = l.as_bytes();
            (0..b.len())
                .filter(|&i| b[i] == b'|' && (i == 0 || b[i - 1] != b'\\'))
                .count()
        };
        let header = md.lines().find(|l| l.contains("| # |")).expect("header");
        assert_eq!(
            separators(row_line),
            separators(header),
            "escaping failed, the row has extra cells:\n{header}\n{row_line}"
        );
    }

    /// RESULTS.md is committed as the baseline to compare later runs against,
    /// so it must not embed whoever's checkout produced it. The loop's own
    /// public-output rule says the same thing: never local paths.
    #[test]
    fn the_report_names_the_fixture_file_relatively_not_by_local_path() {
        let rows = vec![grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "ok")))];
        let md = render_report(
            &rows,
            &display_path(
                Path::new("/home/someone/checkout/.claude/worktrees/x/eval/autopr-cases.json"),
                Path::new("/home/someone/checkout/.claude/worktrees/x"),
            ),
            "t",
        );
        assert!(md.contains("eval/autopr-cases.json"));
        assert!(!md.contains("/home/someone"), "a local path leaked into the report:\n{md}");
    }

    #[test]
    fn a_fixture_outside_the_repo_keeps_its_full_path() {
        assert_eq!(
            display_path(Path::new("/elsewhere/c.json"), Path::new("/repo")),
            "/elsewhere/c.json",
            "only paths inside the repo can be shown relatively"
        );
    }

    #[test]
    fn the_report_states_the_score_and_lists_misses() {
        let rows = vec![
            grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "ok"))),
            grade(&case("E2", 2, Expect::Fixable), Some(&observed(false, "wrongly refused"))),
        ];
        let md = render_report(&rows, "eval/autopr-cases.json", "2026-09-15T00:00:00Z");
        assert!(md.contains("1/2"), "score must be stated: {md}");
        assert!(md.contains("## Misses"));
        assert!(md.contains("wrongly refused"));
    }

    #[test]
    fn a_clean_run_says_so_rather_than_leaving_an_empty_section() {
        let rows = vec![grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "ok")))];
        let md = render_report(&rows, "c.json", "t");
        assert!(md.contains("none"), "an empty Misses section must say none:\n{md}");
    }

    // ---- C5: never any production state. Pinned, not commented. ----

    /// The eval must not spend the daily cap, write the attempt ledger, poison
    /// the baseline cache, or take the run lock. This is the test that makes
    /// that a fact rather than an intention: it reads `self_improve.rs`, finds
    /// EVERY state path the loop can be pointed at, and fails if `scratch_env`
    /// does not redirect all of them. Adding a sixth state file breaks this
    /// test until the eval is taught about it.
    #[test]
    fn every_production_state_path_is_redirected_to_a_scratch_dir() {
        let src = include_str!("self_improve.rs");

        // const NAME: &str = "VALUE";
        let consts: std::collections::HashMap<&str, &str> = src
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                let rest = l.strip_prefix("const ")?;
                let (name, rest) = rest.split_once(':')?;
                let val = rest.split_once('"')?.1.split('"').next()?;
                Some((name.trim(), val))
            })
            .collect();

        let mut required: Vec<String> = Vec::new();
        let mut at = 0;
        while let Some(i) = src[at..].find("_path() -> PathBuf {") {
            let start = at + i;
            let end = start + src[start..].find("\n}\n").expect("fn end");
            let body = &src[start..end];
            if let Some(v) = body.split("std::env::var(").nth(1) {
                let tok = v.split(')').next().unwrap_or("").trim();
                let name = if let Some(lit) = tok.strip_prefix('"') {
                    lit.split('"').next().unwrap_or("").to_string()
                } else {
                    consts.get(tok).copied().unwrap_or("").to_string()
                };
                if name.starts_with("AUGMENTAGENT") {
                    required.push(name);
                }
            }
            at = end;
        }
        required.sort();
        required.dedup();
        assert!(
            required.len() >= 5,
            "expected to discover the loop's state paths, found {required:?}"
        );

        let dir = Path::new("/tmp/eval-scratch");
        let set: std::collections::HashSet<String> =
            scratch_env(dir).into_iter().map(|(k, _)| k).collect();
        for var in &required {
            assert!(
                set.contains(var),
                "{var} is a production state path the eval does not redirect; \
                 an eval run would write real loop state"
            );
        }
        for (_, v) in scratch_env(dir) {
            assert!(
                v.starts_with("/tmp/eval-scratch"),
                "every override must land in the scratch dir, got {v}"
            );
        }
    }

    /// Found in QA, by killing a run: the scratch dir and the git worktrees
    /// inside it survive, and a worktree stays REGISTERED in the user's repo.
    /// Litter in `/tmp` is untidy; a stale registration in someone's
    /// repository is the loop leaving state behind in a place that is not its
    /// own. A later run reclaims both.
    /// CodeRabbit, and codex before it, on the same thing from two angles:
    /// the eval registered git worktrees in the repository. Removing them by
    /// path narrowed the blast radius but kept the class of bug — a run that
    /// dies between `worktree add` and cleanup leaves a registration behind in
    /// a repository the eval does not own.
    ///
    /// `git archive` materialises the same tree and registers NOTHING, so
    /// there is no state to leak and nothing to reclaim but a directory. The
    /// issue promised an evaluator that runs with no worktree; this is what
    /// that actually takes.
    #[test]
    fn the_eval_registers_nothing_in_the_repository() {
        let src = include_str!("autopr_eval.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("test module")];
        // The literal git argument, not the word: the prose above explains
        // why worktrees are gone and should be allowed to say so.
        assert!(
            !code.contains(r#""worktree""#),
            "materialise the tree without registering it, or a killed run \
             leaves state in a repository this command does not own"
        );
        assert!(code.contains("\"archive\""), "git archive is how that is done");
    }

    /// CodeRabbit: `--only E963` graded one case and saved ONLY that case, so
    /// the committed baseline lost seven rows and a later full `--report-only`
    /// reported them as unrun misses. I hit this during QA and papered over it
    /// with `git checkout`, which is exactly the kind of manual step that
    /// stops being done.
    #[test]
    fn a_partial_run_updates_the_baseline_without_discarding_the_rest() {
        let baseline = [
            grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "old"))),
            grade(&case("E2", 2, Expect::Fixable), Some(&observed(true, "old"))),
            grade(&case("E3", 3, Expect::NotFixable), Some(&observed(false, "old"))),
        ];
        let fresh = [grade(&case("E2", 2, Expect::Fixable), Some(&observed(false, "new")))];

        let merged = merge_into_baseline(&baseline, &fresh);
        assert_eq!(
            merged.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["E1", "E2", "E3"],
            "baseline order and membership are preserved"
        );
        assert!(merged[1].note.contains("new"), "the re-run case is updated");
        // Untouched rows must survive byte for byte, which is the property
        // that matters and a stronger claim than inspecting their notes.
        assert_eq!(merged[0], baseline[0]);
        assert_eq!(merged[2], baseline[2]);

        let added = merge_into_baseline(&baseline, &[grade(&case("E9", 9, Expect::Fixable), None)]);
        assert_eq!(added.len(), 4, "a case not in the baseline is appended, not dropped");
        assert_eq!(added[3].id, "E9");
    }

    #[test]
    fn a_killed_run_leaves_scratch_that_the_next_run_reclaims() {
        let tmp = std::env::temp_dir();
        let me = repo_slug(Path::new("/repo/a"));
        let dirs = [
            tmp.join(format!("autopr-eval-{me}-111")),   // dead, ours: reclaim
            tmp.join(format!("autopr-eval-{me}-222")),   // alive: leave it
            tmp.join(format!("autopr-eval-{me}-333")),   // ours: leave it
            tmp.join(format!("autopr-eval-{me}-bogus")), // unparseable: leave it
            tmp.join("something-else"),
        ];
        let stale = stale_scratch(dirs.iter().cloned(), &me, 333, |pid| pid == 222);
        assert_eq!(stale, vec![tmp.join(format!("autopr-eval-{me}-111"))]);
    }

    /// Codex review: `/tmp/autopr-eval-<pid>` was a GLOBAL namespace, so an
    /// eval in one repository would reclaim a dead eval's scratch from
    /// another — deleting that repo's worktree directory while its own
    /// `git worktree remove` ran against the wrong repository, leaving behind
    /// precisely the stale registration this cleanup exists to prevent.
    #[test]
    fn scratch_from_a_different_repository_is_never_reclaimed() {
        let tmp = std::env::temp_dir();
        let mine = repo_slug(Path::new("/repo/a"));
        let theirs = repo_slug(Path::new("/repo/b"));
        assert_ne!(mine, theirs, "each repo must own its own scratch namespace");

        let dirs = vec![tmp.join(format!("autopr-eval-{theirs}-111"))];
        assert!(
            stale_scratch(dirs.into_iter(), &mine, 999, |_| false).is_empty(),
            "another repository's scratch is not ours to delete"
        );
    }

    #[test]
    fn a_repo_slug_is_stable_and_path_safe() {
        let a = repo_slug(Path::new("/repo/a"));
        assert_eq!(a, repo_slug(Path::new("/repo/a")), "stable across calls");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()) && !a.is_empty(),
            "must be safe in a path segment: {a:?}"
        );
    }

    #[test]
    fn reclaiming_never_touches_a_directory_outside_the_scratch_namespace() {
        let me = repo_slug(Path::new("/repo/a"));
        // Built from the real temp dir: with TMPDIR set, hardcoded /tmp paths
        // would be rejected for the wrong reason and the test would pass
        // vacuously.
        let tmp = std::env::temp_dir();
        let dirs = vec![
            tmp.join("eval-111"),
            tmp.join("autopr-eval"),
            PathBuf::from(format!("/home/someone/autopr-eval-{me}-111")),
        ];
        assert!(
            stale_scratch(dirs.into_iter(), &me, 1, |_| false).is_empty(),
            "only <tmp>/autopr-eval-<this repo>-<pid> is ours to delete"
        );
    }

    // ---- C6: --only selects a subset ----

    #[test]
    fn only_selects_the_named_cases_and_rejects_an_unknown_id() {
        let cases = vec![
            case("E1", 1, Expect::Fixable),
            case("E2", 2, Expect::Fixable),
            case("E3", 3, Expect::NotFixable),
        ];
        let picked = select(&cases, Some("E3, E1")).expect("selects");
        assert_eq!(picked.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["E1", "E3"],
            "selection keeps fixture order, so the report is stable");
        assert_eq!(select(&cases, None).expect("all").len(), 3);
        let err = select(&cases, Some("E9")).expect_err("unknown id must not silently select nothing");
        assert!(format!("{err:#}").contains("E9"));

        // Codex: `--only "$IDS"` with an unset variable is an ordinary way to
        // invoke this, and it used to grade nothing and exit 0 reporting
        // `0/0 passed`. A harness whose whole job is catching regressions must
        // never report success for having checked nothing.
        for empty in ["", "   ", ",", " , ,"] {
            assert!(
                select(&cases, Some(empty)).is_err(),
                "--only {empty:?} must not silently select zero cases"
            );
        }
    }

    /// Codex review: `--only E989 --report-only` selected E989 and then
    /// rendered all eight saved rows, because the saved run replaced the
    /// selection wholesale. A report that silently covers more than you asked
    /// for is a report you cannot trust to answer "did my change fix E989".
    #[test]
    fn report_only_honours_the_selection_too() {
        let all = vec![
            grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "ok"))),
            grade(&case("E2", 2, Expect::Fixable), Some(&observed(true, "ok"))),
            grade(&case("E3", 3, Expect::NotFixable), Some(&observed(false, "ok"))),
        ];
        let cases = [case("E1", 1, Expect::Fixable), case("E3", 3, Expect::NotFixable)];
        let picked = rows_for(&all, &cases.iter().collect::<Vec<_>>());
        assert_eq!(
            picked.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["E1", "E3"]
        );
    }

    /// A selected case the saved run never covered must not be silently
    /// dropped — that would quietly shrink the denominator and inflate the
    /// score.
    /// Codex, system pass: saved rows were matched by id alone, so editing a
    /// fixture's expectation or its pinned commit kept the old row — verdict,
    /// base and all — and `--report-only` reported a pass for a case that had
    /// materially changed and never been re-graded. A baseline that reports a
    /// stale score is worse than none, because it is believed.
    #[test]
    fn a_saved_row_is_ignored_once_its_case_has_materially_changed() {
        let mut pinned = case("E1", 1, Expect::Fixable);
        pinned.base = Some("aaaaaaa".into());
        let saved = [grade(&pinned, Some(&observed(true, "graded then")))];

        // Same id, different expectation.
        let mut flipped = pinned.clone();
        flipped.expect = Expect::NotFixable;
        let rows = rows_for(&saved, &[&flipped]);
        assert!(!rows[0].pass, "a changed expectation cannot inherit an old pass");
        assert!(rows[0].actual.contains("no verdict"), "{}", rows[0].actual);

        // Same id, different pinned commit.
        let mut moved = pinned.clone();
        moved.base = Some("bbbbbbb".into());
        let rows = rows_for(&saved, &[&moved]);
        assert!(!rows[0].pass, "a different commit is a different question");

        // Unchanged: the saved row is reused exactly, which is the property
        // that matters and a stronger claim than poking at its note.
        assert_eq!(rows_for(&saved, &[&pinned]), saved.to_vec());
    }

    #[test]
    fn a_selected_case_missing_from_the_saved_run_is_reported_as_unrun() {
        let all = vec![grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "ok")))];
        let cases = [case("E1", 1, Expect::Fixable), case("E9", 9, Expect::Fixable)];
        let picked = rows_for(&all, &cases.iter().collect::<Vec<_>>());
        assert_eq!(picked.len(), 2, "the denominator must not shrink");
        let missing = picked.iter().find(|r| r.id == "E9").expect("E9 present");
        assert!(!missing.pass);
        assert!(missing.actual.contains("no verdict"), "{}", missing.actual);
    }

    // ---- C7: grade the parser we actually ship ----

    /// If the eval reimplemented the header parsing it would grade a copy, and
    /// could stay green while the shipped parser broke. `observe` must go
    /// through `parse_scope_output` itself.
    #[test]
    fn observation_comes_from_the_real_scope_parser() {
        let raw = "VERDICT: not-fixable\nCOMPLEXITY: hard\nEST-DIFF-LINES: ~250\n\n\
                   This needs a product decision about which address wins.";
        let o = observe(raw);
        assert!(!o.fixable);
        assert_eq!(o.complexity, "hard");
        assert_eq!(o.est_diff_lines, Some(250));
        assert!(o.reason.contains("product decision"), "{}", o.reason);

        // The tolerances that matter are the parser's own, not a copy's: a
        // missing verdict means attempt the fix, a missing complexity means
        // hard. If someone changes those, this fails and they must decide.
        let bare = observe("Just a spec with no headers at all.");
        assert!(bare.fixable, "an unparsed scope output still attempts the fix");
        assert_eq!(bare.complexity, "hard");
    }

    #[test]
    fn a_long_reason_is_trimmed_to_one_readable_line() {
        let raw = format!("VERDICT: fixable\n\n{}", "x".repeat(2_000));
        let o = observe(&raw);
        assert!(
            o.reason.chars().count() <= 200,
            "reason width {}",
            o.reason.chars().count()
        );
        assert!(!o.reason.contains('\n'), "a table cell cannot hold newlines");
    }

    /// `--report-only` has to render from somewhere. Without a persisted run
    /// it can only ever produce an empty table, which is a flag that lies.
    /// Rows round-trip through JSON so the report can be re-rendered after a
    /// change to the renderer without spending a reasoner call per case.
    #[test]
    fn rows_round_trip_so_a_report_can_be_rerendered_without_spending_calls() {
        let rows = vec![
            grade(&case("E1", 10, Expect::Fixable), Some(&observed(true, "ok"))),
            grade(&case("E2", 11, Expect::NotFixable), Some(&observed(true, "a | b"))),
            grade(&case("E3", 12, Expect::Fixable), None),
        ];
        let json = rows_to_json(&rows).expect("serialise");
        assert_eq!(rows_from_json(&json).expect("parse"), rows);
    }

    /// A re-render must report when the SCORING happened, not when the
    /// markdown was regenerated. RESULTS.md is kept as a baseline to compare
    /// later runs against, and a baseline that misdates itself is worse than
    /// no baseline.
    #[test]
    fn a_saved_run_remembers_when_it_actually_ran() {
        let rows = vec![grade(&case("E1", 1, Expect::Fixable), Some(&observed(true, "ok")))];
        let json = save_run(&rows, "2026-09-16T00:58:24Z").expect("serialise");
        let (back, started) = load_run(&json).expect("parse");
        assert_eq!(back, rows);
        assert_eq!(started, "2026-09-16T00:58:24Z");
    }

    #[test]
    fn a_corrupt_saved_run_is_an_error_not_an_empty_report() {
        assert!(rows_from_json("not json").is_err());
        assert!(
            rows_from_json(r#"[{"id":"E1"}]"#).is_err(),
            "a row missing its verdict must not render as a silent pass"
        );
    }

    // ---- C8: the committed fixture file is real and loads ----

    /// Codex review: C8 requires `--report-only` to render the committed eval,
    /// and it could not on a fresh clone because the saved run was ignored.
    /// Committing it also gives the baseline a machine-readable twin, so a
    /// later run can be diffed against it rather than eyeballed.
    #[test]
    fn the_committed_baseline_run_loads_and_matches_the_fixtures() {
        let saved = include_str!("../../../eval/last-run.json");
        let (rows, started) = load_run(saved).expect("the committed baseline must load");
        assert!(!started.trim().is_empty(), "a baseline with no date cannot be compared");

        let cases = parse_cases(include_str!("../../../eval/autopr-cases.json")).expect("fixtures");
        assert_eq!(
            rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            cases.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            "the baseline and the fixtures must describe the same suite"
        );
        assert!(
            rows.iter().all(|r| r.pass),
            "a baseline is only useful if it is the state we want to hold: {:?}",
            rows.iter().filter(|r| !r.pass).map(|r| &r.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_committed_fixture_file_parses_and_covers_both_expectations() {
        let raw = include_str!("../../../eval/autopr-cases.json");
        let cases = parse_cases(raw).expect("the committed fixtures must parse");
        assert!(cases.len() >= 2, "need a real starting suite");
        assert!(cases.iter().any(|c| c.expect == Expect::Fixable));
        assert!(cases.iter().any(|c| c.expect == Expect::NotFixable));
        assert!(
            cases.iter().all(|c| !c.body.trim().is_empty()),
            "issue text must be cached, or the eval is not reproducible"
        );
    }
}
