//! Scope-pass eval harness for the auto-PR loop (#1011).
//!
//! Prompt tests prove a sentence is *in* a prompt, never that the model
//! *obeys* it, so every change to the loop's judgment shipped unmeasured.
//! This replays cached issue fixtures through the real stage-1 scoping pass
//! (same system prompt, model resolution and parser) and grades only the
//! fixable / not-fixable verdict. A ruler, not a rule: nothing feeds back into
//! a merge decision or loop state (pinned by `never_touches_production_state`).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::self_improve::{
    build_scope_prompt, parse_scope_output, scope_opts, truncate, Issue, ScopeOutcome,
};

pub const DEFAULT_CASES: &str = "eval/autopr-cases.json";
pub const DEFAULT_REPORT: &str = "eval/RESULTS.md";

/// Refusal reason / spec head quoted in the Misses section.
const MAX_NOTE_CHARS: usize = 500;
/// Same text in a table cell, where a paragraph would be unreadable.
const MAX_CELL_CHARS: usize = 160;

/// The scoper's verdict, in the fixture's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Expectation {
    Fixable,
    NotFixable,
}

impl Expectation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixable => "fixable",
            Self::NotFixable => "not-fixable",
        }
    }
}

/// One fixture case: a cached issue plus the verdict we expect for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalCase {
    pub id: String,
    pub issue: u64,
    pub expect: Expectation,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub author: String,
}

/// Parse the fixture. An unknown `expect`, a duplicate id, or an empty file
/// is a hard error: a fixture typo must never grade as a pass, and a suite
/// that ran nothing would render 0/0. Cases are decoded one at a time so the
/// error names the offending case: serde alone says `unknown variant
/// "maybe"`, which does not locate the typo in a long fixture.
pub fn parse_cases(json: &str) -> Result<Vec<EvalCase>> {
    let raw: Vec<serde_json::Value> = serde_json::from_str(json).context("parse eval cases")?;
    if raw.is_empty() {
        bail!("no eval cases in fixture");
    }
    let mut cases: Vec<EvalCase> = Vec::with_capacity(raw.len());
    for (i, v) in raw.into_iter().enumerate() {
        let id = v.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let c: EvalCase = serde_json::from_value(v)
            .with_context(|| format!("eval case {id:?} (index {i}) in fixture"))?;
        if id.trim().is_empty() {
            bail!("eval case for issue #{} has an empty id", c.issue);
        }
        if cases.iter().any(|p| p.id == id) {
            bail!("duplicate eval case id {id:?}");
        }
        cases.push(c);
    }
    Ok(cases)
}

/// Apply `--only id,id`, keeping fixture order. An unknown id is an error.
pub fn select_cases(cases: Vec<EvalCase>, only: Option<&str>) -> Result<Vec<EvalCase>> {
    let wanted: Vec<&str> = only.unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if wanted.is_empty() {
        return Ok(cases);
    }
    for id in &wanted {
        if !cases.iter().any(|c| c.id == *id) {
            bail!("--only names unknown eval case {id:?}");
        }
    }
    Ok(cases.into_iter().filter(|c| wanted.contains(&c.id.as_str())).collect())
}

/// `--refresh`: re-fetch title, body and author of the selected cases from
/// the live issue (`gh issue view`, a read — never a write) and rewrite the
/// fixture so the cache tracks edits to the issue. The rewritten file is a
/// local cache; whether it is committed is the operator's call.
async fn refresh_cases(gh: &str, repo_root: &Path, path: &Path, all: &mut [EvalCase], only: Option<&str>) -> Result<()> {
    let ids: Vec<String> = select_cases(all.to_vec(), only)?.into_iter().map(|c| c.id).collect();
    for c in all.iter_mut().filter(|c| ids.contains(&c.id)) {
        let n = c.issue.to_string();
        let out = tokio::process::Command::new(gh)
            .args(["issue", "view", &n, "--json", "title,body,author"])
            .current_dir(repo_root)
            .output()
            .await
            .with_context(|| format!("spawn {gh} issue view {n}"))?;
        if !out.status.success() {
            bail!("gh issue view {n}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parse gh issue view")?;
        let field = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
        (c.title, c.body, c.author) = (field(&v["title"]), field(&v["body"]), field(&v["author"]["login"]));
    }
    std::fs::write(path, serde_json::to_string_pretty(&*all)? + "\n")
        .with_context(|| format!("write {}", path.display()))
}

/// What one case produced: the scoper's verdict plus ungraded observations,
/// a reasoner failure, or nothing (`--report-only`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Graded {
        actual: Expectation,
        complexity: &'static str,
        est_diff_lines: Option<usize>,
        /// Refusal reason or spec head: a miss is diagnosable from the scoper's own words.
        note: String,
    },
    /// The reasoner call failed. Neither a pass nor a miss; still non-zero.
    Error(String),
    Skipped,
}

impl Observed {
    /// Grade what the real parser made of the scoper's text.
    pub fn graded(scope: &ScopeOutcome) -> Self {
        Self::Graded {
            actual: if scope.fixable { Expectation::Fixable } else { Expectation::NotFixable },
            complexity: scope.complexity.as_str(),
            est_diff_lines: scope.est_diff_lines,
            note: truncate(&scope.body, MAX_NOTE_CHARS),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub case: EvalCase,
    pub observed: Observed,
}

impl Row {
    /// `Some(true)` pass, `Some(false)` miss, `None` when nothing was graded.
    pub fn pass(&self) -> Option<bool> {
        match &self.observed {
            Observed::Graded { actual, .. } => Some(*actual == self.case.expect),
            Observed::Error(_) | Observed::Skipped => None,
        }
    }
}

/// `(graded, passed, errors)`: graded counts only cases whose reasoner call
/// succeeded, so a pass rate never hides an error.
pub fn counts(rows: &[Row]) -> (usize, usize, usize) {
    let graded = rows.iter().filter(|r| r.pass().is_some()).count();
    let passed = rows.iter().filter(|r| r.pass() == Some(true)).count();
    let errors = rows.iter().filter(|r| matches!(r.observed, Observed::Error(_))).count();
    (graded, passed, errors)
}

/// Anything a human must look at: a miss or an error.
pub fn needs_attention(rows: &[Row]) -> bool {
    let (graded, passed, errors) = counts(rows);
    passed < graded || errors > 0
}

/// One markdown cell: a `|` would open a column and a newline a row; reasons contain both.
pub fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Render `eval/RESULTS.md`: the expected-vs-actual table, the pass count,
/// and a Misses section quoting the scoper's own words.
pub fn render_report(rows: &[Row], model: &str, run_at: &str) -> String {
    let mut out = format!(
        "# Scope-pass eval\n\nRun: {run_at} · scope model: {} · cases: {}\n\n\
         | id | issue | expected | actual | result | complexity | est-lines | note |\n\
         |----|-------|----------|--------|--------|------------|-----------|------|\n",
        escape_cell(model),
        rows.len()
    );
    for r in rows {
        let pass = r.pass() == Some(true);
        let cells: [String; 5] = match &r.observed {
            Observed::Graded { actual, complexity, est_diff_lines, note } => [
                actual.as_str().into(),
                if pass { "pass" } else { "MISS" }.into(),
                (*complexity).into(),
                est_diff_lines.map_or("—".into(), |n| n.to_string()),
                if pass { String::new() } else { truncate(note, MAX_CELL_CHARS) },
            ],
            Observed::Error(e) => ["error".into(), "error".into(), "—".into(), "—".into(), truncate(e, MAX_CELL_CHARS)],
            Observed::Skipped => ["—".into(), "—".into(), "—".into(), "—".into(), String::new()],
        };
        out.push_str(&format!("| {} | #{} | {} |", escape_cell(&r.case.id), r.case.issue, r.case.expect.as_str()));
        for c in &cells {
            out.push_str(&format!(" {} |", escape_cell(c)));
        }
        out.push('\n');
    }
    let (graded, passed, errors) = counts(rows);
    out.push_str(&match (graded, errors) {
        (0, 0) => "\nPass: n/a (no reasoner calls made)\n".to_string(),
        (_, 0) => format!("\nPass: {passed}/{graded}\n"),
        (_, 1) => format!("\nPass: {passed}/{graded} (1 error)\n"),
        _ => format!("\nPass: {passed}/{graded} ({errors} errors)\n"),
    });
    for (i, r) in rows.iter().filter(|r| r.pass() == Some(false)).enumerate() {
        let Observed::Graded { actual, note, .. } = &r.observed else { unreachable!("a miss is always graded") };
        if i == 0 {
            out.push_str("\n## Misses\n");
        }
        out.push_str(&format!(
            "\n### {} — #{}: expected {}, scoper said {}\n\n",
            escape_cell(&r.case.id),
            r.case.issue,
            r.case.expect.as_str(),
            actual.as_str()
        ));
        for line in note.lines() {
            out.push_str(&format!("> {}\n", escape_cell(line)));
        }
    }
    out
}

fn issue_for(c: &EvalCase) -> Issue {
    let (title, body, author) = (c.title.clone(), c.body.clone(), c.author.clone());
    Issue { number: c.issue, title, body, author, author_trusted: true, research_filed: false }
}

/// Entry point for the `autopr-eval` subcommand. Exit 0 when every graded
/// case passes (or nothing was graded), 1 on any miss or reasoner error.
pub async fn run(
    repo_root: &Path,
    cases: Option<&Path>,
    only: Option<&str>,
    report: Option<&Path>,
    refresh: bool,
    report_only: bool,
) -> Result<i32> {
    let cases_path = cases.map_or_else(|| repo_root.join(DEFAULT_CASES), Path::to_path_buf);
    let report_path = report.map_or_else(|| repo_root.join(DEFAULT_REPORT), Path::to_path_buf);
    let raw = std::fs::read_to_string(&cases_path)
        .with_context(|| format!("read {}", cases_path.display()))?;
    let mut cases = parse_cases(&raw)?;
    if refresh {
        refresh_cases(&crate::self_improve::gh_bin(), repo_root, &cases_path, &mut cases, only).await?;
    }
    let cases = select_cases(cases, only)?;

    let opts = scope_opts(repo_root.to_path_buf());
    let model = opts.model.clone().unwrap_or_else(|| "(inherited)".to_string());
    let reasoner = (!report_only).then(augmentagent_channel_core::build_reasoner);

    let mut rows = Vec::with_capacity(cases.len());
    for c in cases {
        let observed = match &reasoner {
            None => Observed::Skipped,
            Some(r) => {
                use augmentagent_channel_core::Reasoner;
                eprintln!("eval {}: scoping #{} …", c.id, c.issue);
                let prompt = build_scope_prompt(&issue_for(&c), None);
                match r.call(&opts, &prompt).await {
                    Ok(text) => Observed::graded(&parse_scope_output(&text)),
                    Err(e) => Observed::Error(format!("{e:#}")),
                }
            }
        };
        rows.push(Row { case: c, observed });
    }

    let report = render_report(&rows, &model, &chrono::Utc::now().to_rfc3339());
    if let Some(dir) = report_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&report_path, &report)
        .with_context(|| format!("write {}", report_path.display()))?;
    print!("{report}");
    println!("(written to {})", report_path.display());
    Ok(i32::from(needs_attention(&rows)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Expectation::{Fixable, NotFixable};

    fn case(id: &str, issue: u64, expect: Expectation) -> EvalCase {
        let (title, body, author) = (format!("title {id}"), format!("body {id}"), "owner".into());
        EvalCase { id: id.into(), issue, expect, title, body, author }
    }

    fn graded(id: &str, issue: u64, expect: Expectation, scoper_text: &str) -> Row {
        let observed = Observed::graded(&parse_scope_output(scoper_text));
        Row { case: case(id, issue, expect), observed }
    }

    fn errored(id: &str, issue: u64, expect: Expectation, msg: &str) -> Row {
        Row { case: case(id, issue, expect), observed: Observed::Error(msg.to_string()) }
    }

    fn table_rows(report: &str) -> Vec<&str> {
        report.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| id ")).collect()
    }

    #[test]
    fn parse_rejects_bad_expect_duplicate_ids_and_empty_fixture() {
        let ok = r#"[{"id":"E1","issue":10,"expect":"fixable","title":"t1","body":"b1","author":"a1"},
                     {"id":"E2","issue":11,"expect":"not-fixable"}]"#;
        let cases = parse_cases(ok).unwrap();
        assert_eq!((cases[0].expect, cases[0].issue, cases[0].title.as_str()), (Fixable, 10, "t1"));
        assert_eq!((cases[1].expect, cases[1].body.as_str()), (NotFixable, ""));

        let bad = r#"[{"id":"E1","issue":10,"expect":"fixable"},{"id":"E2","issue":11,"expect":"maybe"}]"#;
        let msg = format!("{:#}", parse_cases(bad).expect_err("unknown expect must not parse"));
        assert!(msg.contains("maybe") && msg.contains("\"E2\""), "must name value and case: {msg}");

        let dup = r#"[{"id":"E1","issue":10,"expect":"fixable"},{"id":"E1","issue":11,"expect":"fixable"}]"#;
        assert!(format!("{:#}", parse_cases(dup).unwrap_err()).contains("E1"));
        assert!(parse_cases("[]").is_err(), "empty fixture would render a vacuous 0/0");
    }

    #[test]
    fn only_selects_named_cases_in_fixture_order_and_rejects_unknown_ids() {
        let all = vec![case("E1", 1, Fixable), case("E2", 2, NotFixable), case("E3", 3, Fixable)];
        let picked = select_cases(all.clone(), Some("E3, E1")).unwrap();
        let ids: Vec<&str> = picked.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["E1", "E3"]);
        assert_eq!(select_cases(all.clone(), Some("  ")).unwrap().len(), 3);
        let err = select_cases(all, Some("E9")).expect_err("unknown id");
        assert!(format!("{err:#}").contains("E9"));
    }

    #[test]
    fn grades_all_four_cells_through_the_real_parser() {
        // (expected, scoper text, pass?, actual, note)
        let table = [
            (Fixable, "VERDICT: fixable\nCOMPLEXITY: simple\nEST-DIFF-LINES: 40\n\nthe spec", true, Fixable),
            (Fixable, "VERDICT: not-fixable\nCOMPLEXITY: hard\n\nNeeds an owner decision.", false, NotFixable),
            (NotFixable, "verdict: NOT-FIXABLE\n\nresearch ask", true, NotFixable),
            (NotFixable, "VERDICT: fixable\nCOMPLEXITY: medium\n\n## Interpretation\nAdd a crate.", false, Fixable),
        ];
        for (expect, text, pass, actual) in table {
            let row = graded("E", 1, expect, text);
            assert_eq!(row.pass(), Some(pass), "{text:?}");
            let Observed::Graded { actual: got, note, .. } = &row.observed else { panic!() };
            assert_eq!(*got, actual, "{text:?}");
            assert_eq!(note.as_str(), text.split("\n\n").nth(1).unwrap(), "note is the body");
        }
        // Ungraded observations ride along; the note is bounded.
        let long = format!("VERDICT: not-fixable\n\n{}", "x".repeat(2_000));
        let Observed::Graded { complexity, est_diff_lines, note, .. } = Observed::graded(&parse_scope_output(&long)) else { panic!() };
        assert_eq!((complexity, est_diff_lines), ("hard", None));
        assert!(note.chars().count() <= MAX_NOTE_CHARS + 1 && note.ends_with('…'));
        let scope = parse_scope_output("VERDICT: fixable\nCOMPLEXITY: medium\nEST-DIFF-LINES: ~120\n\nspec");
        let Observed::Graded { complexity, est_diff_lines, .. } = Observed::graded(&scope) else { panic!() };
        assert_eq!((complexity, est_diff_lines), ("medium", Some(120)));
        // Errors and skips grade as nothing; an error is still never green.
        assert_eq!(errored("E", 1, Fixable, "boom").pass(), None);
        assert!(needs_attention(&[errored("E", 1, Fixable, "boom")]));
        assert_eq!(Row { case: case("E", 1, Fixable), observed: Observed::Skipped }.pass(), None);
    }

    #[test]
    fn report_shows_every_outcome_escapes_cells_and_quotes_misses() {
        let rows = vec![
            graded("E1", 1, Fixable, "VERDICT: fixable\nCOMPLEXITY: simple\nEST-DIFF-LINES: 30\n\nspec"),
            graded("E2", 2, Fixable, "VERDICT: not-fixable\n\nneeds a | decision\nfrom the owner | and budget"),
            graded("E3", 3, NotFixable, "VERDICT: not-fixable\n\nepic"),
            graded("E4", 4, NotFixable, "VERDICT: fixable\nCOMPLEXITY: hard\n\nbig spec"),
            errored("E5", 5, Fixable, "reasoner: quota"),
        ];
        assert_eq!(counts(&rows), (4, 2, 1));
        assert!(needs_attention(&rows) && !needs_attention(&rows[..1]));
        let report = render_report(&rows, "claude-test", "2026-09-15T00:00:00Z");
        let rows = table_rows(&report);
        assert_eq!(rows.len(), 5);
        assert!(rows[0].contains("| E1 | #1 | fixable | fixable | pass | simple | 30 |  |"), "{}", rows[0]);
        assert!(rows[1].contains("| E2 | #2 | fixable | not-fixable | MISS | hard | — | needs a \\| decision from the owner \\| and budget |"), "{}", rows[1]);
        assert!(rows[2].contains("| E3 | #3 | not-fixable | not-fixable | pass |"), "{}", rows[2]);
        assert!(rows[3].contains("| E4 | #4 | not-fixable | fixable | MISS | hard | — | big spec |"), "{}", rows[3]);
        assert!(rows[4].contains("| E5 | #5 | fixable | error | error | — | — | reasoner: quota |"), "{}", rows[4]);
        // A reason full of pipes and newlines cannot add columns or rows.
        let header_cols = report.lines().find(|l| l.starts_with("| id ")).unwrap().matches('|').count();
        for row in &rows {
            assert_eq!(row.replace("\\|", "").matches('|').count(), header_cols, "{row}");
        }
        assert!(report.contains("Pass: 2/4 (1 error)\n") && report.contains("scope model: claude-test · cases: 5\n"), "{report}");
        // Both miss directions are diagnosable from the scoper's own words.
        assert!(report.contains("## Misses\n\n### E2 — #2: expected fixable, scoper said not-fixable\n\n> needs a \\| decision\n> from the owner \\| and budget\n"), "{report}");
        assert!(report.contains("### E4 — #4: expected not-fixable, scoper said fixable\n\n> big spec\n"), "{report}");
    }

    #[test]
    fn report_only_and_only_subset_render_dashes_without_a_pass_count() {
        let all = vec![case("E1", 1, Fixable), case("E2", 2, NotFixable), case("E3", 3, Fixable)];
        let picked = select_cases(all, Some("E2,E3")).unwrap();
        let rows: Vec<Row> = picked.into_iter().map(|case| Row { case, observed: Observed::Skipped }).collect();
        assert!(!needs_attention(&rows));
        let report = render_report(&rows, "m", "now");
        let rows = table_rows(&report);
        assert_eq!(rows.len(), 2, "{report}");
        assert!(rows[0].contains("| E2 | #2 | not-fixable | — | — | — | — |  |"), "{}", rows[0]);
        assert!(rows[1].starts_with("| E3 |"));
        assert!(!report.contains("| E1 |"));
        assert!(report.contains("cases: 2\n") && report.contains("Pass: n/a"), "{report}");
        assert!(!report.contains("## Misses"));
    }

    #[test]
    fn committed_fixture_parses_and_covers_both_expectations() {
        let cases = parse_cases(include_str!("../../../eval/autopr-cases.json"))
            .expect("eval/autopr-cases.json must parse");
        assert!(cases.iter().any(|c| c.expect == Fixable) && cases.iter().any(|c| c.expect == NotFixable));
        assert!(cases.iter().all(|c| !c.title.is_empty() && !c.body.is_empty()), "every case is cached");
    }

    #[tokio::test]
    async fn report_goes_where_asked_and_refresh_rewrites_only_the_selected_cases() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("cases.json");
        std::fs::write(&fixture, r#"[{"id":"E1","issue":7,"expect":"fixable","title":"stale","body":"stale","author":"x"},
                                    {"id":"E2","issue":8,"expect":"not-fixable","title":"keep","body":"keep","author":"x"}]"#).unwrap();
        // A stand-in `gh issue view` returning the live issue as gh would.
        let gh = dir.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\necho '{\"title\":\"fresh\",\"body\":\"fresh | body\",\"author\":{\"login\":\"owner\"}}'\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut all = parse_cases(&std::fs::read_to_string(&fixture).unwrap()).unwrap();
        refresh_cases(gh.to_str().unwrap(), dir.path(), &fixture, &mut all, Some("E1")).await.unwrap();
        let again = parse_cases(&std::fs::read_to_string(&fixture).unwrap()).unwrap();
        assert_eq!(again, all, "the fixture is rewritten with the refreshed cache");
        assert_eq!((again[0].title.as_str(), again[0].body.as_str(), again[0].author.as_str()), ("fresh", "fresh | body", "owner"));
        assert_eq!((again[1].title.as_str(), again[1].expect), ("keep", NotFixable), "unselected case untouched");
        assert!(refresh_cases("/nonexistent/gh", dir.path(), &fixture, &mut all, None).await.is_err());
        // `--report` picks the destination; the default path is not written.
        let report = dir.path().join("out/custom.md");
        assert_eq!(run(dir.path(), Some(&fixture), Some("E2"), Some(&report), false, true).await.unwrap(), 0);
        assert!(std::fs::read_to_string(&report).unwrap().contains("| E2 | #8 | not-fixable | — |"));
        assert!(!dir.path().join(DEFAULT_REPORT).exists());
    }

    /// Structural pins on the module source: the eval must grade through
    /// the shipped parser and never reach loop state or write to GitHub
    /// (`--refresh` reads via `gh issue view`; every write subcommand stays
    /// forbidden here).
    #[test]
    fn never_touches_production_state() {
        let src = include_str!("autopr_eval.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("test marker")];
        for forbidden in [
            "attempt_ledger_path", "attempt_history_path", "daily_counter_path",
            "baseline_cache_path", "AttemptLedger", "AttemptHistory", "run_once(",
            "\"comment\"", "\"create\"", "\"edit\"", "\"label\"", "\"merge\"", "\"close\"",
            "\"worktree\"", "fn parse_scope_output", "\"verdict:\"",
        ] {
            assert!(!code.contains(forbidden), "eval module must not reference {forbidden}");
        }
        // Grading goes through the real parser, prompt and opts.
        let run = &code[code.find("pub async fn run(").expect("run")..];
        assert!(run.contains("Observed::graded(&parse_scope_output(&text))"));
        assert!(run.contains("build_scope_prompt(&issue_for(&c), None)"), "prior: None");
        assert!(run.contains("scope_opts(repo_root.to_path_buf())"), "the real scope opts");
    }
}
