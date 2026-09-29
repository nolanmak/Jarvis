//! #1300 — the executable Slack parity matrix.
//!
//! `docs/slack-parity-matrix.json` has one row per row of the epic's
//! feature-to-issue coverage table (#1281, copied into [`EPIC_COVERAGE`] so
//! the check runs offline). Each row names its owning issues, the Slack
//! capability-table entries it rests on, the named tests that prove it, its
//! status and one evidence slot per host. [`check`] fails when:
//!
//! * an epic row is missing, unknown, duplicated or owned by other issues;
//! * a row names no test, or a named test is not a `#[test]` /
//!   `#[tokio::test]` function in that package and target (an `#[ignore]`d
//!   test does not count: it never runs in CI);
//! * a row is `supported` (or `unverified-live`) while one of its
//!   capabilities is not `Supported` in [`crate::surface`], or `blocked`
//!   while all of them are — so the matrix and the capability tables can
//!   only change together;
//! * a capability in the tables is covered by no row, or a row names one
//!   the tables do not have;
//! * a `blocked` row has no reason and issue, an `unverified-live` row has
//!   no reason, or host evidence is incomplete or for an undeclared host;
//! * a shared behavior scenario has no Slack test, or names no tests and no
//!   gap for Discord or WhatsApp.
//!
//! Status meanings: `supported` — the behavior is implemented and its named
//! tests pass in CI on both hosts; `blocked` — it is not, with the issue
//! that owns the gap; `unverified-live` — it can only be proven on a real
//! workspace or host. No status, and no CI run, records real-host
//! acceptance: that lives in `host_evidence`, filled from the runbook's
//! owner acceptance script only.
//!
//! The matrix is read at run time, never `include_str!`'d, so editing it
//! does not rebuild the daemon.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::surface::{SupportStatus, SLACK_INTERACTIONS, SLACK_SHARED_CAPABILITIES};

/// The matrix, relative to the repository root.
pub const MATRIX_PATH: &str = "docs/slack-parity-matrix.json";

/// Every surface a shared scenario must account for.
pub const SCENARIO_SURFACES: [&str; 3] = ["slack", "discord", "whatsapp"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParityMatrix {
    pub schema: u32,
    pub epic: u32,
    pub gate_issue: u32,
    pub acceptance_procedure: String,
    pub hosts: Vec<Host>,
    pub rows: Vec<ParityRow>,
    pub shared_scenarios: Vec<SharedScenario>,
    pub regression_suites: Vec<RegressionSuite>,
}

/// A host type that needs its own real-host acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub id: String,
    pub label: String,
    /// Issues that stop acceptance on this host whatever the row.
    #[serde(default)]
    pub blockers: Vec<Blocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocker {
    pub issue: u32,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RowStatus {
    Supported,
    Blocked,
    UnverifiedLive,
}

impl RowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Blocked => "blocked",
            Self::UnverifiedLive => "unverified-live",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParityRow {
    pub id: String,
    /// The epic's "Required capability" text, verbatim.
    pub capability: String,
    pub issues: Vec<u32>,
    /// Keys from the Slack capability tables (`SupportStatus` rows).
    pub capabilities: Vec<String>,
    pub status: RowStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<Blocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_reason: Option<String>,
    pub tests: Vec<TestRef>,
    /// Steps of the runbook's owner acceptance script that exercise it.
    #[serde(default)]
    pub acceptance_steps: Vec<u32>,
    /// One slot per declared host; `null` until accepted on a real host.
    pub host_evidence: BTreeMap<String, Option<HostEvidence>>,
}

/// One named test: `package` is the crate directory name under `crates/`,
/// `target` is `lib`, `bin:<name>` or `test:<file stem>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRef {
    pub package: String,
    pub target: String,
    pub name: String,
}

impl fmt::Display for TestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.package, self.target, self.name)
    }
}

/// Real-host acceptance of one row, from the runbook script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEvidence {
    /// Full 40-character commit the host ran.
    pub commit: String,
    /// `sw_vers` / `/etc/os-release` summary.
    pub os_version: String,
    /// `uname -m`: `arm64`, `x86_64` or `aarch64`.
    pub architecture: String,
    pub commands: Vec<String>,
    pub artifact: String,
    /// Only `pass` is evidence.
    pub result: String,
    /// ISO date.
    pub date: String,
}

/// One transport-neutral behavior scenario and who runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedScenario {
    pub id: String,
    pub title: String,
    /// Surface-independent tests of the shared contract itself.
    #[serde(default)]
    pub shared: Vec<TestRef>,
    pub surfaces: BTreeMap<String, SurfaceCoverage>,
}

/// A surface's side of a scenario: its tests, and the named gap when the
/// surface has no adapter (or only part of one) on main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceCoverage {
    #[serde(default)]
    pub tests: Vec<TestRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Blocker>,
}

/// A whole test target run unfiltered to show another surface did not
/// regress at the same commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegressionSuite {
    pub surface: String,
    pub package: String,
    pub target: String,
}

/// One row of the epic's feature-to-issue coverage table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpicRow {
    pub capability: &'static str,
    pub issues: &'static [u32],
}

const fn epic(capability: &'static str, issues: &'static [u32]) -> EpicRow {
    EpicRow { capability, issues }
}

/// #1281's "Feature-to-issue coverage" table, verbatim.
pub const EPIC_COVERAGE: [EpicRow; 20] = [
    epic(
        "Transport-neutral identity, capabilities and feature matrix",
        &[1282],
    ),
    epic("Real-time transport, verified limits and scopes", &[1283]),
    epic("App install, credentials and CLI lifecycle", &[1284, 1299]),
    epic(
        "Disconnect/restart/sleep recovery, durable delivery and deduplication",
        &[1285, 1299],
    ),
    epic(
        "Owner authority, DM and control channel, echo rejection",
        &[1286],
    ),
    epic(
        "Immediate interaction; any combination of surfaces",
        &[1287],
    ),
    epic(
        "Same tools, memory, skills, sessions, fallback, audit and permissions",
        &[1288],
    ),
    epic(
        "Follow-ups, threads, queueing, cancellation, clarification, handoffs",
        &[1288, 1285],
    ),
    epic(
        "Every approval kind; revise, presets, skip, recompose; cross-surface resolution",
        &[1289],
    ),
    epic(
        "Slack contact replies and new compose; actual send dispatch",
        &[1290, 1296],
    ),
    epic(
        "Schedule send, send now, cancel, back to queue, timezone handling",
        &[1291],
    ),
    epic(
        "Model selection, loops, reminders, journal, process controls and help",
        &[1292],
    ),
    epic(
        "Images, text/code files, PDF/DOCX and attachment-only input",
        &[1293],
    ),
    epic(
        "Full answers, generated files, progress, errors and delivery state",
        &[1294, 1285],
    ),
    epic(
        "Digests, reminders, nudges, audit, health, review and background notifications",
        &[1295],
    ),
    epic(
        "Subscriptions, identity, search and live/ingest reconciliation",
        &[1296],
    ),
    epic("Voice clips, transcription and spoken replies", &[1297]),
    epic("Live two-way voice", &[1298]),
    epic(
        "Installation, status/doctor, service management, upgrades and rollback on both hosts",
        &[1299],
    ),
    epic(
        "Shared parity suite, real-host acceptance on macOS and Linux, 24-hour soak",
        &[1300, 1282],
    ),
];

/// What a check found wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViolationKind {
    MissingEpicRow,
    UnknownRow,
    DuplicateRow,
    IssueMismatch,
    NoNamedTest,
    TestNotFound,
    TestIgnored,
    UnknownCapability,
    UncoveredCapability,
    /// The row claims more than the capability table supports.
    StatusAheadOfCapability,
    /// The capability table supports everything but the row says blocked.
    StatusBehindCapability,
    BlockerMismatch,
    LiveReasonMissing,
    HostMismatch,
    EvidenceIncomplete,
    ScenarioSurfaceMissing,
}

impl ViolationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingEpicRow => "missing-epic-row",
            Self::UnknownRow => "unknown-row",
            Self::DuplicateRow => "duplicate-row",
            Self::IssueMismatch => "issue-mismatch",
            Self::NoNamedTest => "no-named-test",
            Self::TestNotFound => "test-not-found",
            Self::TestIgnored => "test-ignored",
            Self::UnknownCapability => "unknown-capability",
            Self::UncoveredCapability => "uncovered-capability",
            Self::StatusAheadOfCapability => "status-ahead-of-capability",
            Self::StatusBehindCapability => "status-behind-capability",
            Self::BlockerMismatch => "blocker-mismatch",
            Self::LiveReasonMissing => "live-reason-missing",
            Self::HostMismatch => "host-mismatch",
            Self::EvidenceIncomplete => "evidence-incomplete",
            Self::ScenarioSurfaceMissing => "scenario-surface-missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub kind: ViolationKind,
    /// The row ID, capability, host or `scenario/surface` concerned.
    pub subject: String,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.kind.as_str(),
            self.subject,
            self.detail
        )
    }
}

/// The status of one capability-table key (`query`, `buttons`, …).
pub fn capability_status(name: &str) -> Option<SupportStatus> {
    SLACK_SHARED_CAPABILITIES
        .iter()
        .find(|row| row.key.as_str() == name)
        .map(|row| row.status)
        .or_else(|| {
            SLACK_INTERACTIONS
                .iter()
                .find(|row| row.key.as_str() == name)
                .map(|row| row.status)
        })
}

/// Every key in both Slack capability tables.
pub fn capability_names() -> Vec<&'static str> {
    SLACK_SHARED_CAPABILITIES
        .iter()
        .map(|row| row.key.as_str())
        .chain(SLACK_INTERACTIONS.iter().map(|row| row.key.as_str()))
        .collect()
}

/// What [`check`] compares the matrix against.
pub struct CheckContext<'a> {
    /// Repository root: named tests are looked up under `crates/`.
    pub source_root: &'a Path,
    pub epic: &'a [EpicRow],
    /// Capability status lookup; the Slack tables unless a test swaps it.
    pub capabilities: &'a dyn Fn(&str) -> Option<SupportStatus>,
    pub capability_names: Vec<&'static str>,
}

impl<'a> CheckContext<'a> {
    /// The real epic and the real Slack capability tables.
    pub fn slack(source_root: &'a Path) -> Self {
        Self {
            source_root,
            epic: &EPIC_COVERAGE,
            capabilities: &capability_status,
            capability_names: capability_names(),
        }
    }
}

pub fn parse(json: &str) -> Result<ParityMatrix, serde_json::Error> {
    serde_json::from_str(json)
}

pub fn load(path: &Path) -> anyhow::Result<ParityMatrix> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    parse(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))
}

/// The repository root for a matrix at `<root>/docs/slack-parity-matrix.json`.
pub fn source_root_for(matrix_path: &Path) -> PathBuf {
    matrix_path
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Every violation, in a stable order.
pub fn check(matrix: &ParityMatrix, ctx: &CheckContext) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut push = |kind, subject: &str, detail: String| {
        out.push(Violation {
            kind,
            subject: subject.to_string(),
            detail,
        })
    };
    let mut finder = TestFinder::new(ctx.source_root);

    // Epic coverage.
    let mut seen_ids = BTreeSet::new();
    for row in &matrix.rows {
        if !seen_ids.insert(row.id.as_str()) {
            push(
                ViolationKind::DuplicateRow,
                &row.id,
                "row ID used twice".into(),
            );
        }
    }
    for epic in ctx.epic {
        let rows: Vec<&ParityRow> = matrix
            .rows
            .iter()
            .filter(|row| row.capability == epic.capability)
            .collect();
        match rows.as_slice() {
            [] => push(
                ViolationKind::MissingEpicRow,
                epic.capability,
                format!(
                    "epic row owned by {} has no matrix row",
                    issues(epic.issues)
                ),
            ),
            [row] => {
                if row.issues != epic.issues {
                    push(
                        ViolationKind::IssueMismatch,
                        &row.id,
                        format!(
                            "owned by {} in the epic, {} here",
                            issues(epic.issues),
                            issues(&row.issues)
                        ),
                    );
                }
            }
            [first, ..] => push(
                ViolationKind::DuplicateRow,
                &first.id,
                format!("{} rows cover `{}`", rows.len(), epic.capability),
            ),
        }
    }
    for row in &matrix.rows {
        if !ctx
            .epic
            .iter()
            .any(|epic| epic.capability == row.capability)
        {
            push(
                ViolationKind::UnknownRow,
                &row.id,
                format!(
                    "`{}` is not a row of the epic's coverage table",
                    row.capability
                ),
            );
        }
    }

    // Hosts.
    let hosts: BTreeSet<&str> = matrix.hosts.iter().map(|h| h.id.as_str()).collect();

    for row in &matrix.rows {
        // Named tests.
        if row.tests.is_empty() {
            push(
                ViolationKind::NoNamedTest,
                &row.id,
                "no named test proves this row".into(),
            );
        }
        for test in &row.tests {
            if let Err((kind, detail)) = finder.find(test) {
                push(kind, &row.id, detail);
            }
        }

        // Capabilities agree with the tables.
        let mut not_supported = Vec::new();
        for name in &row.capabilities {
            match (ctx.capabilities)(name) {
                None => push(
                    ViolationKind::UnknownCapability,
                    &row.id,
                    format!("`{name}` is not in the Slack capability tables"),
                ),
                Some(SupportStatus::Supported) => {}
                Some(status) => not_supported.push(format!("{name} is {status:?}")),
            }
        }
        let known = row
            .capabilities
            .iter()
            .any(|name| (ctx.capabilities)(name).is_some());
        match row.status {
            RowStatus::Supported | RowStatus::UnverifiedLive if !not_supported.is_empty() => push(
                ViolationKind::StatusAheadOfCapability,
                &row.id,
                format!(
                    "row is {} but the capability table says {}",
                    row.status.as_str(),
                    not_supported.join(", ")
                ),
            ),
            RowStatus::Blocked if known && not_supported.is_empty() => push(
                ViolationKind::StatusBehindCapability,
                &row.id,
                format!(
                    "row is blocked but every capability it names ({}) is Supported",
                    row.capabilities.join(", ")
                ),
            ),
            _ => {}
        }

        // Status fields.
        let blocker_ok = row
            .blocker
            .as_ref()
            .is_some_and(|b| b.issue > 0 && !b.reason.trim().is_empty());
        match (row.status, &row.blocker) {
            (RowStatus::Blocked, _) if !blocker_ok => push(
                ViolationKind::BlockerMismatch,
                &row.id,
                "a blocked row needs a blocker with a reason and an owning issue".into(),
            ),
            (RowStatus::Supported | RowStatus::UnverifiedLive, Some(_)) => push(
                ViolationKind::BlockerMismatch,
                &row.id,
                format!("a {} row cannot carry a blocker", row.status.as_str()),
            ),
            _ => {}
        }
        if row.status == RowStatus::UnverifiedLive
            && row
                .live_reason
                .as_deref()
                .is_none_or(|r| r.trim().is_empty())
        {
            push(
                ViolationKind::LiveReasonMissing,
                &row.id,
                "an unverified-live row must say what needs a real workspace or host".into(),
            );
        }

        // Host evidence.
        let keys: BTreeSet<&str> = row.host_evidence.keys().map(String::as_str).collect();
        if keys != hosts {
            push(
                ViolationKind::HostMismatch,
                &row.id,
                format!(
                    "host_evidence has {:?}, the declared hosts are {:?}",
                    keys, hosts
                ),
            );
        }
        for (host, evidence) in &row.host_evidence {
            if let Some(evidence) = evidence {
                if let Err(why) = evidence_complete(evidence) {
                    push(
                        ViolationKind::EvidenceIncomplete,
                        &row.id,
                        format!("{host}: {why}"),
                    );
                }
            }
        }
    }

    // Every capability-table key is covered by a row.
    for name in &ctx.capability_names {
        if !matrix
            .rows
            .iter()
            .any(|row| row.capabilities.iter().any(|c| c == name))
        {
            push(
                ViolationKind::UncoveredCapability,
                name,
                "no matrix row names this capability-table entry".into(),
            );
        }
    }

    // Shared scenarios.
    for scenario in &matrix.shared_scenarios {
        for test in &scenario.shared {
            if let Err((kind, detail)) = finder.find(test) {
                push(kind, &scenario.id, detail);
            }
        }
        for surface in SCENARIO_SURFACES {
            let subject = format!("{}/{surface}", scenario.id);
            let Some(coverage) = scenario.surfaces.get(surface) else {
                push(
                    ViolationKind::ScenarioSurfaceMissing,
                    &subject,
                    "the scenario does not account for this surface".into(),
                );
                continue;
            };
            if surface == "slack" && coverage.tests.is_empty() {
                push(
                    ViolationKind::ScenarioSurfaceMissing,
                    &subject,
                    "every shared scenario must run against Slack; a gap does not count".into(),
                );
            }
            let gap_ok = coverage
                .gap
                .as_ref()
                .is_some_and(|g| g.issue > 0 && !g.reason.trim().is_empty());
            if coverage.tests.is_empty() && !gap_ok {
                push(
                    ViolationKind::ScenarioSurfaceMissing,
                    &subject,
                    "no tests and no named gap with an owning issue".into(),
                );
            }
            for test in &coverage.tests {
                if let Err((kind, detail)) = finder.find(test) {
                    push(kind, &subject, detail);
                }
            }
        }
    }
    for suite in &matrix.regression_suites {
        if finder.files(&suite.package, &suite.target).is_err() {
            push(
                ViolationKind::TestNotFound,
                &format!("regression/{}", suite.surface),
                format!("no target {} in {}", suite.target, suite.package),
            );
        }
    }
    out
}

fn issues(list: &[u32]) -> String {
    list.iter()
        .map(|i| format!("#{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn evidence_complete(e: &HostEvidence) -> Result<(), String> {
    let placeholder = |s: &str| {
        matches!(
            s.trim().to_lowercase().as_str(),
            "" | "todo" | "tbd" | "..."
        )
    };
    if e.commit.len() != 40
        || !e
            .commit
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("commit must be the full 40-character hash".into());
    }
    if placeholder(&e.os_version) || placeholder(&e.artifact) || placeholder(&e.date) {
        return Err("os_version, artifact and date are required".into());
    }
    if !matches!(e.architecture.as_str(), "arm64" | "x86_64" | "aarch64") {
        return Err(format!("unknown architecture `{}`", e.architecture));
    }
    if e.commands.is_empty() || e.commands.iter().any(|c| placeholder(c)) {
        return Err("the commands that were run are required".into());
    }
    if e.result != "pass" {
        return Err(format!("result is `{}`, only `pass` is evidence", e.result));
    }
    Ok(())
}

/// Finds `#[test]` functions in the source tree, caching file contents.
struct TestFinder<'a> {
    root: &'a Path,
    cache: BTreeMap<PathBuf, Option<String>>,
}

impl<'a> TestFinder<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            cache: BTreeMap::new(),
        }
    }

    /// The source files a target's tests can live in.
    fn files(&self, package: &str, target: &str) -> Result<Vec<PathBuf>, String> {
        let krate = self.root.join("crates").join(package);
        if package.is_empty()
            || package.contains(['/', '\\', '.'])
            || !krate.join("Cargo.toml").is_file()
        {
            return Err(format!("no package {package} under crates/"));
        }
        let mut files = Vec::new();
        if target == "lib" || target.starts_with("bin:") {
            collect_rs(&krate.join("src"), &mut files);
        } else if let Some(stem) = target.strip_prefix("test:") {
            if stem.is_empty() || stem.contains(['/', '\\', '.']) {
                return Err(format!("bad test target `{target}`"));
            }
            let file = krate.join("tests").join(format!("{stem}.rs"));
            if file.is_file() {
                files.push(file);
            }
            collect_rs(&krate.join("tests").join(stem), &mut files);
        } else {
            return Err(format!(
                "unknown target `{target}` (lib, bin:<name> or test:<file>)"
            ));
        }
        if files.is_empty() {
            return Err(format!("no source for {package} {target}"));
        }
        Ok(files)
    }

    fn find(&mut self, test: &TestRef) -> Result<(), (ViolationKind, String)> {
        let files = self
            .files(&test.package, &test.target)
            .map_err(|why| (ViolationKind::TestNotFound, format!("{test}: {why}")))?;
        let mut ignored = false;
        for file in files {
            let text = self
                .cache
                .entry(file.clone())
                .or_insert_with(|| std::fs::read_to_string(&file).ok());
            let Some(text) = text else { continue };
            match test_attr(text, &test.name) {
                Some(false) => return Ok(()),
                Some(true) => ignored = true,
                None => {}
            }
        }
        if ignored {
            Err((
                ViolationKind::TestIgnored,
                format!("{test}: marked #[ignore], so it never runs in CI"),
            ))
        } else {
            Err((
                ViolationKind::TestNotFound,
                format!("{test}: no #[test] fn with this name in that target"),
            ))
        }
    }
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `Some(ignored)` when `name` is a test function in `text`.
fn test_attr(text: &str, name: &str) -> Option<bool> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = None;
    for (i, line) in lines.iter().enumerate() {
        let mut rest = line.trim_start();
        for prefix in ["pub(crate) ", "pub ", "async "] {
            rest = rest.strip_prefix(prefix).unwrap_or(rest);
        }
        let Some(after) = rest.strip_prefix("fn ").and_then(|r| r.strip_prefix(name)) else {
            continue;
        };
        if !after.starts_with(['(', '<']) {
            continue;
        }
        let mut is_test = false;
        let mut ignored = false;
        for above in lines[..i].iter().rev() {
            let above = above.trim();
            if above.starts_with("#[") {
                is_test |= above == "#[test]" || above.starts_with("#[tokio::test");
                ignored |= above.starts_with("#[ignore");
            } else if !(above.is_empty() || above.starts_with("//")) {
                break;
            }
        }
        if is_test {
            if !ignored {
                return Some(false);
            }
            found = Some(true);
        }
    }
    found
}

// --- report ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub ok: bool,
    pub violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub rows: usize,
    pub supported: usize,
    pub blocked: usize,
    pub unverified_live: usize,
    pub named_tests: usize,
    pub shared_scenarios: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportRow {
    pub id: String,
    pub capability: String,
    pub issues: Vec<u32>,
    pub status: RowStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<Blocker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_reason: Option<String>,
    pub tests: usize,
    pub acceptance_steps: Vec<u32>,
    /// Host ID → `pending` or `pass <date> <short commit>`.
    pub hosts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenBlocker {
    /// `blocked`, `unverified-live`, `host`, `host-acceptance` or
    /// `shared-suite-gap`.
    pub kind: &'static str,
    pub subject: String,
    pub issues: Vec<u32>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParityReport {
    pub epic: u32,
    pub gate_issue: u32,
    pub acceptance_procedure: String,
    pub check: CheckResult,
    pub summary: Summary,
    pub hosts: Vec<String>,
    pub rows: Vec<ReportRow>,
    pub open_blockers: Vec<OpenBlocker>,
}

pub fn report(matrix: &ParityMatrix, violations: &[Violation]) -> ParityReport {
    let count = |status| matrix.rows.iter().filter(|r| r.status == status).count();
    let hosts: Vec<String> = matrix.hosts.iter().map(|h| h.id.clone()).collect();
    let rows = matrix
        .rows
        .iter()
        .map(|row| ReportRow {
            id: row.id.clone(),
            capability: row.capability.clone(),
            issues: row.issues.clone(),
            status: row.status,
            blocker: row.blocker.clone(),
            live_reason: row.live_reason.clone(),
            tests: row.tests.len(),
            acceptance_steps: row.acceptance_steps.clone(),
            hosts: hosts
                .iter()
                .map(|h| {
                    let state = match row.host_evidence.get(h) {
                        Some(Some(e)) => {
                            format!("pass {} {}", e.date, &e.commit[..e.commit.len().min(7)])
                        }
                        _ => "pending".to_string(),
                    };
                    (h.clone(), state)
                })
                .collect(),
        })
        .collect();

    let mut open = Vec::new();
    for row in &matrix.rows {
        match (row.status, &row.blocker, &row.live_reason) {
            (RowStatus::Blocked, Some(b), _) => open.push(OpenBlocker {
                kind: "blocked",
                subject: row.id.clone(),
                issues: vec![b.issue],
                reason: b.reason.clone(),
            }),
            (RowStatus::UnverifiedLive, _, reason) => open.push(OpenBlocker {
                kind: "unverified-live",
                subject: row.id.clone(),
                issues: row.issues.clone(),
                reason: reason.clone().unwrap_or_default(),
            }),
            _ => {}
        }
    }
    for host in &matrix.hosts {
        for b in &host.blockers {
            open.push(OpenBlocker {
                kind: "host",
                subject: host.id.clone(),
                issues: vec![b.issue],
                reason: b.reason.clone(),
            });
        }
    }
    for host in &matrix.hosts {
        let pending = matrix
            .rows
            .iter()
            .filter(|row| !matches!(row.host_evidence.get(&host.id), Some(Some(_))))
            .count();
        if pending > 0 {
            open.push(OpenBlocker {
                kind: "host-acceptance",
                subject: host.id.clone(),
                issues: vec![matrix.gate_issue],
                reason: format!(
                    "real-host acceptance not recorded for {pending} of {} rows on {}; run {}",
                    matrix.rows.len(),
                    host.label,
                    matrix.acceptance_procedure
                ),
            });
        }
    }
    for scenario in &matrix.shared_scenarios {
        for (surface, coverage) in &scenario.surfaces {
            if let Some(gap) = &coverage.gap {
                open.push(OpenBlocker {
                    kind: "shared-suite-gap",
                    subject: format!("{}/{surface}", scenario.id),
                    issues: vec![gap.issue],
                    reason: gap.reason.clone(),
                });
            }
        }
    }

    ParityReport {
        epic: matrix.epic,
        gate_issue: matrix.gate_issue,
        acceptance_procedure: matrix.acceptance_procedure.clone(),
        check: CheckResult {
            ok: violations.is_empty(),
            violations: violations.iter().map(ToString::to_string).collect(),
        },
        summary: Summary {
            rows: matrix.rows.len(),
            supported: count(RowStatus::Supported),
            blocked: count(RowStatus::Blocked),
            unverified_live: count(RowStatus::UnverifiedLive),
            named_tests: matrix.rows.iter().map(|r| r.tests.len()).sum(),
            shared_scenarios: matrix.shared_scenarios.len(),
        },
        hosts,
        rows,
        open_blockers: open,
    }
}

/// The human report: check result, one line per row, then open blockers.
pub fn render_text(report: &ParityReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Slack parity matrix (epic #{}, gate #{})",
        report.epic, report.gate_issue
    );
    if report.check.ok {
        let _ = writeln!(out, "Matrix check: ok");
    } else {
        let n = report.check.violations.len();
        let _ = writeln!(
            out,
            "Matrix check: FAILED ({n} problem{})",
            if n == 1 { "" } else { "s" }
        );
        for v in &report.check.violations {
            let _ = writeln!(out, "  - {v}");
        }
    }
    let s = &report.summary;
    let _ = writeln!(
        out,
        "Rows: {} ({} supported, {} blocked, {} unverified-live); {} named tests; {} shared scenarios",
        s.rows, s.supported, s.blocked, s.unverified_live, s.named_tests, s.shared_scenarios
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{:<16} {:<23} {:<13} {:>5}  HOSTS ({})",
        "STATUS",
        "ROW",
        "ISSUES",
        "TESTS",
        report.hosts.join(" / ")
    );
    for row in &report.rows {
        let hosts: Vec<&str> = report
            .hosts
            .iter()
            .map(|h| row.hosts.get(h).map(String::as_str).unwrap_or("pending"))
            .collect();
        let _ = writeln!(
            out,
            "{:<16} {:<23} {:<13} {:>5}  {}",
            row.status.as_str(),
            row.id,
            issues(&row.issues),
            row.tests,
            hosts.join(" / ")
        );
        let _ = writeln!(out, "{:<16} {}", "", row.capability);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Open blockers ({})", report.open_blockers.len());
    for b in &report.open_blockers {
        let _ = writeln!(
            out,
            "  - {} {} ({}): {}",
            b.kind,
            b.subject,
            issues(&b.issues),
            b.reason
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "No row is verified on a real host until its evidence is recorded from {}.",
        report.acceptance_procedure
    );
    out
}

// --- CI commands -------------------------------------------------------------

fn selector(target: &str) -> String {
    match target {
        "lib" => "--lib".to_string(),
        t if t.starts_with("bin:") => format!("--bin {}", &t[4..]),
        t => format!("--test {}", t.trim_start_matches("test:")),
    }
}

/// One `cargo test` line per (package, target) that runs every named test
/// in the matrix (rows and shared scenarios), then each regression suite
/// unfiltered. Sorted, so the output is stable.
pub fn cargo_commands(matrix: &ParityMatrix) -> Vec<String> {
    let mut grouped: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let tests = matrix.rows.iter().flat_map(|r| r.tests.iter()).chain(
        matrix.shared_scenarios.iter().flat_map(|s| {
            s.shared
                .iter()
                .chain(s.surfaces.values().flat_map(|c| c.tests.iter()))
        }),
    );
    for test in tests {
        grouped
            .entry((test.package.clone(), test.target.clone()))
            .or_default()
            .insert(test.name.clone());
    }
    let mut out: Vec<String> = grouped
        .into_iter()
        .map(|((package, target), names)| {
            let names: Vec<String> = names.into_iter().collect();
            format!(
                "cargo test -p {package} {} -- {}",
                selector(&target),
                names.join(" ")
            )
        })
        .collect();
    let mut regression: Vec<String> = matrix
        .regression_suites
        .iter()
        .map(|s| format!("cargo test -p {} {}", s.package, selector(&s.target)))
        .collect();
    regression.sort();
    regression.dedup();
    out.extend(regression);
    out
}
