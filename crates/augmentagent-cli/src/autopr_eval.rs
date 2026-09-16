//! Scope-pass eval harness for the auto-PR loop (#1011).
//!
//! Every test of the loop's prompts proves a sentence is *in* a prompt, never
//! that the model *obeys* it, so each change to the loop's judgment (#955,
//! #973, #996, #1006) shipped unmeasured — and the model resolves through a
//! live pointer, so the judgment can drift with no commit at all. This
//! replays cached issue fixtures through the real stage-1 scoping pass (same
//! system prompt, model resolution and parser) and grades only the
//! fixable / not-fixable verdict. A ruler, not a rule: nothing feeds back
//! into a merge decision and nothing touches loop state (pinned by
//! `never_touches_production_state`).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

/// Parse the fixture. An unknown `expect` (serde names the value and line),
/// a duplicate id, or an empty file is a hard error: a fixture typo must
/// never grade as a pass, and a suite that ran nothing would render 0/0.
pub fn parse_cases(json: &str) -> Result<Vec<EvalCase>> {
    let cases: Vec<EvalCase> = serde_json::from_str(json).context("parse eval cases")?;
    if cases.is_empty() {
        bail!("no eval cases in fixture");
    }
    for (i, c) in cases.iter().enumerate() {
        if c.id.trim().is_empty() {
            bail!("eval case for issue #{} has an empty id", c.issue);
        }
        if cases[..i].iter().any(|p| p.id == c.id) {
            bail!("duplicate eval case id {:?}", c.id);
        }
    }
    Ok(cases)
}

/// Apply `--only id,id`, keeping fixture order. An unknown id is an error.
pub fn select_cases(cases: Vec<EvalCase>, only: Option<&str>) -> Result<Vec<EvalCase>> {
    let Some(only) = only.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(cases);
    };
    let wanted: Vec<&str> = only.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    for id in &wanted {
        if !cases.iter().any(|c| c.id == *id) {
            bail!("--only names unknown eval case {id:?}");
        }
    }
    Ok(cases
        .into_iter()
        .filter(|c| wanted.contains(&c.id.as_str()))
        .collect())
}

/// What one case produced: the scoper's verdict plus ungraded observations,
/// a reasoner failure, or nothing (`--report-only`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Graded {
        actual: Expectation,
        complexity: &'static str,
        est_diff_lines: Option<usize>,
        /// The refusal reason or the spec head, so a miss is diagnosable
        /// from the scoper's own words rather than just red.
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
            actual: if scope.fixable {
                Expectation::Fixable
            } else {
                Expectation::NotFixable
            },
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

/// Make a string safe inside one markdown table cell: a `|` would open a
/// new column and a newline a new row, and the scoper's reasons contain both.
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
        let cells: [String; 5] = match (&r.observed, r.pass()) {
            (
                Observed::Graded {
                    actual,
                    complexity,
                    est_diff_lines,
                    note,
                },
                pass,
            ) => [
                actual.as_str().into(),
                if pass == Some(true) { "pass" } else { "MISS" }.into(),
                (*complexity).into(),
                est_diff_lines.map_or("—".into(), |n| n.to_string()),
                if pass == Some(true) { String::new() } else { truncate(note, MAX_CELL_CHARS) },
            ],
            (Observed::Error(e), _) => ["error".into(), "error".into(), "—".into(), "—".into(), truncate(e, MAX_CELL_CHARS)],
            (Observed::Skipped, _) => ["—".into(), "—".into(), "—".into(), "—".into(), String::new()],
        };
        out.push_str(&format!(
            "| {} | #{} | {} |",
            escape_cell(&r.case.id),
            r.case.issue,
            r.case.expect.as_str()
        ));
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
        let Observed::Graded { actual, note, .. } = &r.observed else {
            unreachable!("a miss is always graded")
        };
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
    Issue {
        number: c.issue,
        title: c.title.clone(),
        body: c.body.clone(),
        author: c.author.clone(),
        author_trusted: true,
        research_filed: false,
    }
}

/// Entry point for the `autopr-eval` subcommand. Exit 0 when every graded
/// case passes (or nothing was graded), 1 on any miss or reasoner error.
pub async fn run(
    repo_root: &Path,
    cases: Option<&Path>,
    only: Option<&str>,
    report_only: bool,
) -> Result<i32> {
    let cases_path = cases.map_or_else(|| repo_root.join(DEFAULT_CASES), Path::to_path_buf);
    let raw = std::fs::read_to_string(&cases_path)
        .with_context(|| format!("read {}", cases_path.display()))?;
    let cases = select_cases(parse_cases(&raw)?, only)?;

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
    let report_path = repo_root.join(DEFAULT_REPORT);
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
        EvalCase {
            id: id.to_string(),
            issue,
            expect,
            title: format!("title {id}"),
            body: format!("body {id}"),
            author: "owner".to_string(),
        }
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
        assert!(msg.contains("maybe"), "error must quote the bad value: {msg}");

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
        let Observed::Graded { complexity, est_diff_lines, note, .. } =
            Observed::graded(&parse_scope_output(&long))
        else {
            panic!()
        };
        assert_eq!((complexity, est_diff_lines), ("hard", None));
        assert!(note.chars().count() <= MAX_NOTE_CHARS + 1 && note.ends_with('…'));
        let scope = parse_scope_output("VERDICT: fixable\nCOMPLEXITY: medium\nEST-DIFF-LINES: ~120\n\nspec");
        let Observed::Graded { complexity, est_diff_lines, .. } = Observed::graded(&scope) else { panic!() };
        assert_eq!((complexity, est_diff_lines), ("medium", Some(120)));
        // Errors and skips grade as nothing.
        assert_eq!(errored("E", 1, Fixable, "boom").pass(), None);
        assert_eq!(Row { case: case("E", 1, Fixable), observed: Observed::Skipped }.pass(), None);
    }

    #[test]
    fn counts_pass_over_graded_cases_only_and_an_error_is_never_green() {
        let clean = [graded("E1", 1, Fixable, "VERDICT: fixable\n\nspec")];
        assert_eq!(counts(&clean), (1, 1, 0));
        assert!(!needs_attention(&clean));
        assert!(needs_attention(&[errored("E1", 1, Fixable, "boom")]));
        assert!(needs_attention(&[graded("E1", 1, NotFixable, "VERDICT: fixable\n\nspec")]));
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
        assert!(needs_attention(&rows));
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
        assert!(report.contains("Pass: 2/4 (1 error)\n"), "{report}");
        assert!(report.contains("scope model: claude-test · cases: 5\n"));
        // Both miss directions are diagnosable from the scoper's own words.
        assert!(report.contains("## Misses\n\n### E2 — #2: expected fixable, scoper said not-fixable\n\n> needs a \\| decision\n> from the owner \\| and budget\n"), "{report}");
        assert!(report.contains("### E4 — #4: expected not-fixable, scoper said fixable\n\n> big spec\n"), "{report}");
        assert_eq!(escape_cell("a | b\nc\r\nd"), "a \\| b c  d");
    }

    #[test]
    fn report_only_and_only_subset_render_dashes_without_a_pass_count() {
        let all = vec![case("E1", 1, Fixable), case("E2", 2, NotFixable), case("E3", 3, Fixable)];
        let rows: Vec<Row> = select_cases(all, Some("E2,E3"))
            .unwrap()
            .into_iter()
            .map(|case| Row { case, observed: Observed::Skipped })
            .collect();
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
        assert!(cases.iter().any(|c| c.expect == Fixable));
        assert!(cases.iter().any(|c| c.expect == NotFixable));
        for c in &cases {
            assert!(!c.title.is_empty() && !c.body.is_empty(), "case {} is not cached", c.id);
        }
    }

    /// Structural pins on the module source: the eval must grade through
    /// the shipped parser and never reach loop state or write to GitHub.
    #[test]
    fn never_touches_production_state() {
        let src = include_str!("autopr_eval.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("test marker")];
        for forbidden in [
            "attempt_ledger_path", "attempt_history_path", "daily_counter_path",
            "baseline_cache_path", "AttemptLedger", "AttemptHistory", "run_once(",
            "gh_bin", "\"comment\"", "\"create\"", "\"edit\"", "\"label\"", "\"merge\"",
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
