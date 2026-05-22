//! Code Mode tool manifest (v1).
//!
//! Two artifacts:
//!
//! 1. [`ToolManifest::to_dts`] — a `.d.ts` block injected into the LLM system
//!    prompt so the model can write a typed TypeScript program against
//!    `globalThis.tools.*`.
//! 2. [`ToolManifest::to_runner_manifest`] — the flat allowlist of dotted
//!    tool names passed to the Deno runner sidecar as NDJSON. The runner
//!    builds a nested Proxy from this list and refuses any call whose name
//!    isn't in it.
//!
//! Both artifacts are derived from the same `Vec<ToolDef>`, so the dispatcher
//! (I4), the prompt (I5), and the runner (I1) all see the same surface.
//!
//! # Type signatures
//!
//! The TypeScript signatures here are the contract the model writes against.
//! They must stay aligned with the **Rust backings** that the dispatcher
//! ultimately calls. v1 backings (sketched here; wired up in I4):
//!
//! - `wiki.draftHint`        ← `augmentagent_wiki::WikiReader::draft_hint`
//! - `db.recentEmailsFrom`   ← **no direct backing yet**; see NOTE below.
//! - `db.threadHistory`      ← `augmentagent_store::Store::recent_emails_for_thread(thread_id, since_ms)`
//! - `db.isProcessed`        ← `augmentagent_store::Store::is_message_processed`
//! - `calendar.busy`         ← `augmentagent_channel_calendar::gcal::CalendarApi::list_events`
//! - `draft` (terminal)      ← `augmentagent_store::Store::log_action_code_mode`
//!
//! The TS-side shapes (`EmailContext`, `EmailSummary`, `CalEvent`, `Channel`)
//! are JSON-marshalled equivalents of the Rust types; the dispatcher is
//! responsible for the marshal.
//!
//! # NOTE — `db.recentEmailsFrom` is a follow-up dependency for I4
//!
//! `Store::recent_emails_since(since_ms, limit)` takes **no sender argument**
//! and returns `(from_email, subject, triage_result)` — it does **not**
//! include a `messageId` column. The TS surface `recentEmailsFrom(sender,
//! days): Promise<EmailSummary[]>` requires both sender filtering and
//! `messageId` in the result. Adapter logic in I4 cannot invent a column
//! that the query does not return.
//!
//! I4 will need a new query overload, e.g.:
//! ```text
//! Store::recent_emails_from_sender(sender: &str, since_ms: i64)
//!     -> StoreResult<Vec<(messageId, subject, timestampMs)>>
//! ```
//! This is a **known I4 blocker** — the adapter for `db.recentEmailsFrom`
//! cannot be completed until that query exists.

use std::collections::BTreeMap;

/// One tool exposed to the model.
///
/// `name` is the dotted JS path (`wiki.draftHint`). It is the **only**
/// identifier used by both the dispatcher and the runner allowlist; the
/// model invokes the tool as `await tools.wiki.draftHint(...)`.
///
/// `ts_signature` is the bare TS member signature **without** the leaf name
/// and **without** the trailing semicolon — e.g.
/// `"(email: EmailContext): Promise<string>"`. The leaf name comes from the
/// last dotted segment of `name`, so callers cannot accidentally desync the
/// runner allowlist from what the LLM sees.
///
/// `jsdoc` is the comment body (no leading `/**` / trailing `*/`). Multiple
/// lines are fine — each line gets a `*` prefix in the rendered `.d.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub ts_signature: String,
    pub jsdoc: String,
}

impl ToolDef {
    /// Convenience: build a `ToolDef` from string-like inputs.
    pub fn new(
        name: impl Into<String>,
        ts_signature: impl Into<String>,
        jsdoc: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            ts_signature: ts_signature.into(),
            jsdoc: jsdoc.into(),
        }
    }
}

/// Flat list of tools. Nesting is recovered from the dotted `name`s at
/// render time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolManifest {
    pub tools: Vec<ToolDef>,
}

impl ToolManifest {
    /// Construct from a flat vector. No validation here — `to_dts` will
    /// panic on duplicate dotted names (since that would render ambiguous
    /// `.d.ts`), but otherwise the input is taken as-is.
    pub fn new(tools: Vec<ToolDef>) -> Self {
        Self { tools }
    }

    /// Render a self-contained `.d.ts` block declaring the `tools` global
    /// and the JSON shape types it references.
    ///
    /// The output is structured as:
    ///
    /// ```ts
    /// // ----- supporting types (Channel, EmailContext, EmailSummary, CalEvent) -----
    /// // ----- declare const tools: { … nested by dotted name … }; -----
    /// ```
    ///
    /// Every leaf in the manifest appears as a `name(...): ReturnType;`
    /// line inside its namespace, with its `jsdoc` rendered as a `/** */`
    /// block immediately above it.
    pub fn to_dts(&self) -> String {
        let tree = TsNamespace::from_manifest(self);

        let mut out = String::new();
        out.push_str(SUPPORTING_TYPES);
        out.push('\n');
        out.push_str("declare const tools: ");
        tree.render(&mut out, 0);
        out.push_str(";\n");
        out
    }

    /// Flat allowlist of dotted tool names, in declaration order. This is
    /// what the Deno runner uses to build its `globalThis.tools` Proxy and
    /// to reject unknown calls.
    pub fn to_runner_manifest(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name.clone()).collect()
    }
}

/// A single tool invocation captured during a code-mode program run.
///
/// Used by:
/// - I2 (#48) — `Store::log_action_code_mode` receives a `Vec<ToolCallRecord>`
///   as the trace of calls the program made before emitting a draft.
/// - I4 (#50) — the `DefaultDispatcher` accumulates one `ToolCallRecord` per
///   dispatched call and passes the full vec to the action logger at program
///   completion.
///
/// Truncation of `args_summary` and `result_summary` is the caller's
/// responsibility; this type imposes no length constraints.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallRecord {
    /// Dotted tool name, e.g. `"wiki.draftHint"`.
    pub call: String,
    /// Truncated/summarized args (caller decides truncation policy).
    pub args_summary: serde_json::Value,
    /// Truncated result; `None` if the call errored.
    pub result_summary: Option<serde_json::Value>,
    /// Error message if the call failed; `None` on success.
    pub error: Option<String>,
    /// Unix timestamp (ms) when the call completed.
    pub timestamp_ms: i64,
}

/// Hard-coded v1 tool surface (see epic + issue #49).
///
/// Adding tools is intentionally a code change, not config: each new entry
/// also needs a dispatcher arm (I4) and, depending on side-effects, a
/// `RateGovernor::permit` call. Keep that wiring local to one place.
pub fn manifest_v1() -> ToolManifest {
    ToolManifest::new(vec![
        ToolDef::new(
            "wiki.draftHint",
            "(email: EmailContext): Promise<string>",
            "Hint string pointing at likely-relevant wiki pages for this email.\nEmpty string means \"no prior context — rely on the raw email only\".\nBacked by WikiReader::draft_hint.",
        ),
        ToolDef::new(
            "db.recentEmailsFrom",
            "(sender: string, days: number): Promise<EmailSummary[]>",
            "Most recent emails from `sender` within the last `days` days,\nnewest first. Returns at most 50 rows. Use this to check whether\nthe sender has recent context you should reference.",
        ),
        ToolDef::new(
            "db.threadHistory",
            "(threadId: string): Promise<EmailSummary[]>",
            "Every prior message in the given Gmail thread (oldest → newest).\nUse this when drafting a reply to make sure you don't repeat\nyourself or contradict an earlier message in the same thread.",
        ),
        ToolDef::new(
            "db.isProcessed",
            "(messageId: string): Promise<boolean>",
            "True iff an action row already exists for this messageId.\nUse this to avoid double-replying when a sibling channel may have\nalready handled the same message.",
        ),
        ToolDef::new(
            "calendar.busy",
            "(startIso: string, endIso: string): Promise<CalEvent[]>",
            "Busy intervals on the user's primary calendar between two ISO\n8601 timestamps. Use this before proposing a meeting time.\nBoth bounds are inclusive; recurring events are expanded to\nindividual instances.",
        ),
        ToolDef::new(
            "draft",
            "(channel: Channel, body: string, reason: string): Promise<void>",
            "TERMINAL. Submits a draft reply for human approval on Discord.\nMust be the last tool call in your program. `body` is the literal\nreply text; `reason` is a one-sentence justification logged with\nthe action. Calling this more than once in a single program is an\nerror.",
        ),
    ])
}

/// The supporting type aliases the model needs to type its program. Kept
/// inline in the `.d.ts` so the runtime needs no `import`s (`--allow-none`
/// forbids them anyway).
const SUPPORTING_TYPES: &str = r#"// Code Mode tool surface — auto-generated from ToolManifest.
// All tools are async. The model writes:
//
//   async function main(): Promise<void> {
//     // ... call tools.* here ...
//     await tools.draft("gmail", "...", "...");
//   }
//
// Imports, fetch, and Deno.* are forbidden by the sandbox.

type Channel =
  | "gmail"
  | "linkedin"
  | "discord"
  | "twitter"
  | "instagram"
  | "whatsapp"
  | "slack";

interface EmailContext {
  from: string;
  subject: string;
  body: string;
  threadId?: string;
  messageId: string;
}

interface EmailSummary {
  messageId: string;
  from: string;
  subject: string;
  triageResult?: string;
}

interface CalEvent {
  startIso: string;
  endIso: string;
  summary?: string;
}
"#;

// ---------------------------------------------------------------------------
// .d.ts rendering — build a namespace tree, then walk it.
// ---------------------------------------------------------------------------

/// One node in the dotted-name tree we use to render nested namespaces.
///
/// `BTreeMap` preserves a deterministic alphabetical ordering per level,
/// which is what we want for reproducible `.d.ts` output regardless of the
/// declaration order in `manifest_v1`.
#[derive(Default)]
struct TsNamespace<'a> {
    /// Sub-namespaces, keyed by segment.
    children: BTreeMap<String, TsNamespace<'a>>,
    /// Leaves attached to *this* namespace, keyed by leaf name.
    leaves: BTreeMap<String, &'a ToolDef>,
}

impl<'a> TsNamespace<'a> {
    fn from_manifest(m: &'a ToolManifest) -> Self {
        let mut root = TsNamespace::default();
        for tool in &m.tools {
            let segments: Vec<&str> = tool.name.split('.').collect();
            assert!(
                !segments.is_empty() && segments.iter().all(|s| !s.is_empty()),
                "ToolDef.name must be a non-empty dotted path, got {:?}",
                tool.name
            );
            root.insert(&segments, tool);
        }
        root
    }

    fn insert(&mut self, segments: &[&str], tool: &'a ToolDef) {
        match segments {
            [] => unreachable!("checked by from_manifest"),
            [leaf] => {
                let inserted = self.leaves.insert((*leaf).to_string(), tool);
                assert!(
                    inserted.is_none(),
                    "duplicate tool name in manifest: {}",
                    tool.name
                );
            }
            [head, rest @ ..] => {
                let child = self.children.entry((*head).to_string()).or_default();
                child.insert(rest, tool);
            }
        }
    }

    fn render(&self, out: &mut String, depth: usize) {
        out.push_str("{\n");
        let inner = "  ".repeat(depth + 1);

        // Namespaces first (alphabetical), then leaves (alphabetical).
        for (name, child) in &self.children {
            out.push_str(&inner);
            out.push_str(name);
            out.push_str(": ");
            child.render(out, depth + 1);
            out.push_str(";\n");
        }

        for (leaf_name, tool) in &self.leaves {
            render_jsdoc(out, &tool.jsdoc, &inner);
            out.push_str(&inner);
            out.push_str(leaf_name);
            out.push_str(&tool.ts_signature);
            out.push_str(";\n");
        }

        out.push_str(&"  ".repeat(depth));
        out.push('}');
    }
}

fn render_jsdoc(out: &mut String, jsdoc: &str, indent: &str) {
    if jsdoc.is_empty() {
        return;
    }
    out.push_str(indent);
    out.push_str("/**\n");
    for line in jsdoc.lines() {
        out.push_str(indent);
        if line.is_empty() {
            out.push_str(" *\n");
        } else {
            out.push_str(" * ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(indent);
    out.push_str(" */\n");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: collect leaf names from `to_dts()` by looking for lines that
    /// match `name(...):` after stripping indentation. JSDoc lines start
    /// with `*`, namespace lines end with `: {`, and the closing `};` is
    /// also filtered out — so a `name(...):` pattern is unambiguous.
    fn leaf_names_in_dts(dts: &str) -> Vec<String> {
        let mut out = Vec::new();
        for raw in dts.lines() {
            let line = raw.trim_start();
            // Skip JSDoc / type-aliases / namespace openers / closers.
            if line.starts_with("/**")
                || line.starts_with("*")
                || line.starts_with("//")
                || line.starts_with("type ")
                || line.starts_with("interface ")
                || line.starts_with("declare const")
                || line.starts_with('|')
                || line.is_empty()
                || line.starts_with('}')
            {
                continue;
            }
            // Namespace lines end with `: {` (e.g. `wiki: {`).
            if line.ends_with(": {") {
                continue;
            }
            // Leaf signature line: `name(...): ReturnType;`. Strip everything
            // from the first `(` onward to recover the bare name.
            if let Some(paren) = line.find('(') {
                let name = &line[..paren];
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    out.push(name.to_string());
                }
            }
        }
        out
    }

    #[test]
    fn manifest_v1_has_expected_tools() {
        let m = manifest_v1();
        let names = m.to_runner_manifest();
        assert_eq!(
            names,
            vec![
                "wiki.draftHint",
                "db.recentEmailsFrom",
                "db.threadHistory",
                "db.isProcessed",
                "calendar.busy",
                "draft",
            ]
        );
    }

    #[test]
    fn runner_manifest_length_matches_leaf_count_in_dts() {
        let m = manifest_v1();
        let manifest = m.to_runner_manifest();
        let dts = m.to_dts();
        let leaves = leaf_names_in_dts(&dts);
        assert_eq!(
            manifest.len(),
            leaves.len(),
            "runner manifest length ({}) != leaf count in .d.ts ({}). \
             manifest={:?} leaves={:?}",
            manifest.len(),
            leaves.len(),
            manifest,
            leaves
        );
    }

    #[test]
    fn every_leaf_name_in_runner_manifest_appears_in_dts() {
        let m = manifest_v1();
        let dts = m.to_dts();
        for full in m.to_runner_manifest() {
            // Extract the last dotted segment — that's the JS leaf name as
            // it appears in `to_dts()`. Namespaces (`wiki:` `db:`) appear
            // separately on their own lines as `name: {`.
            let leaf = full.rsplit('.').next().unwrap();
            let needle = format!("{}(", leaf);
            assert!(
                dts.contains(&needle),
                "leaf {} ({}) not found in .d.ts output:\n{}",
                full,
                needle,
                dts
            );
        }
    }

    #[test]
    fn dts_declares_tools_global() {
        let dts = manifest_v1().to_dts();
        assert!(dts.contains("declare const tools: {"));
        assert!(dts.trim_end().ends_with("};"));
    }

    #[test]
    fn dts_nests_namespaces_correctly() {
        let dts = manifest_v1().to_dts();
        // `wiki:` opens a namespace and contains `draftHint(`.
        let wiki_idx = dts.find("wiki: {").expect("wiki namespace");
        let draft_hint_idx = dts.find("draftHint(").expect("draftHint leaf");
        assert!(
            wiki_idx < draft_hint_idx,
            "draftHint should appear after wiki: {{"
        );
        // Top-level `draft(` (the terminal) is *not* inside the wiki namespace.
        // It should appear at the same depth as `wiki:` / `db:` / `calendar:`.
        let draft_idx = dts
            .find("\n  draft(")
            .expect("top-level draft at 2-space indent");
        assert!(draft_idx > 0);
    }

    #[test]
    fn dts_includes_jsdoc_above_each_leaf() {
        let dts = manifest_v1().to_dts();
        // Every leaf has a multi-line jsdoc, so we expect at least one ` * `
        // line per leaf. Cheap check: `/**` count >= leaf count.
        let m = manifest_v1();
        let jsdoc_opens = dts.matches("/**").count();
        assert!(
            jsdoc_opens >= m.tools.len(),
            "expected >={} JSDoc blocks, found {}",
            m.tools.len(),
            jsdoc_opens
        );
    }

    #[test]
    fn dts_includes_supporting_types() {
        let dts = manifest_v1().to_dts();
        for ty in [
            "type Channel",
            "interface EmailContext",
            "interface EmailSummary",
            "interface CalEvent",
        ] {
            assert!(dts.contains(ty), "expected supporting type {} in dts", ty);
        }
    }

    #[test]
    fn dts_output_is_deterministic() {
        let a = manifest_v1().to_dts();
        let b = manifest_v1().to_dts();
        assert_eq!(a, b);
    }

    #[test]
    #[should_panic(expected = "duplicate tool name")]
    fn duplicate_tool_name_panics() {
        let m = ToolManifest::new(vec![
            ToolDef::new("dup", "(): Promise<void>", ""),
            ToolDef::new("dup", "(): Promise<void>", ""),
        ]);
        // Render forces tree construction, which detects the dupe.
        let _ = m.to_dts();
    }

    #[test]
    #[should_panic(expected = "non-empty dotted path")]
    fn empty_name_panics() {
        let m = ToolManifest::new(vec![ToolDef::new("", "(): Promise<void>", "")]);
        let _ = m.to_dts();
    }

    /// Validate the generated `.d.ts` against `deno check` when Deno is
    /// installed locally. This isn't gated by a Cargo feature because the
    /// test silently skips when Deno is missing, which keeps CI green on
    /// hosts without Deno while still catching regressions on a dev box
    /// (and on hosts where I1's Deno runner sidecar is installed).
    #[test]
    fn dts_passes_deno_check_when_available() {
        use std::io::Write;
        use std::process::Command;

        // Skip silently if Deno isn't on PATH.
        let probe = Command::new("deno").arg("--version").output();
        let Ok(probe) = probe else {
            eprintln!("[skip] deno not installed — `dts_passes_deno_check_when_available`");
            return;
        };
        if !probe.status.success() {
            eprintln!("[skip] `deno --version` failed — skipping deno check");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let dts_path = dir.path().join("tools.d.ts");
        let harness_path = dir.path().join("harness.ts");

        let dts = manifest_v1().to_dts();
        std::fs::write(&dts_path, &dts).expect("write d.ts");

        // Minimal harness: triple-slash-reference the .d.ts so the global
        // declaration is in scope, then write a program that exercises a
        // few signatures. If the harness type-checks, the .d.ts is valid.
        let harness = r#"/// <reference path="./tools.d.ts" />

async function main(): Promise<void> {
  const email: EmailContext = {
    from: "alice@example.com",
    subject: "hi",
    body: "hello",
    messageId: "m-1",
  };
  const hint: string = await tools.wiki.draftHint(email);
  const recents: EmailSummary[] = await tools.db.recentEmailsFrom("alice@example.com", 7);
  const thread: EmailSummary[] = await tools.db.threadHistory("t-1");
  const seen: boolean = await tools.db.isProcessed("m-1");
  const busy: CalEvent[] = await tools.calendar.busy("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z");
  void hint; void recents; void thread; void seen; void busy;
  await tools.draft("gmail", "draft body", "reason");
}

void main;
"#;
        let mut f = std::fs::File::create(&harness_path).expect("create harness");
        f.write_all(harness.as_bytes()).expect("write harness");
        drop(f);

        let out = Command::new("deno")
            .arg("check")
            .arg(&harness_path)
            .output()
            .expect("spawn deno check");
        assert!(
            out.status.success(),
            "`deno check` failed for generated .d.ts:\nstdout:\n{}\nstderr:\n{}\n--- dts ---\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            dts
        );
    }

    /// Pin the exact rendered `.d.ts` string so that any change to indentation,
    /// namespace ordering, JSDoc formatting, or tool declarations trips a test
    /// failure in CI rather than silently passing the idempotence check.
    #[test]
    fn dts_output_matches_snapshot() {
        const EXPECTED: &str = r#"// Code Mode tool surface — auto-generated from ToolManifest.
// All tools are async. The model writes:
//
//   async function main(): Promise<void> {
//     // ... call tools.* here ...
//     await tools.draft("gmail", "...", "...");
//   }
//
// Imports, fetch, and Deno.* are forbidden by the sandbox.

type Channel =
  | "gmail"
  | "linkedin"
  | "discord"
  | "twitter"
  | "instagram"
  | "whatsapp"
  | "slack";

interface EmailContext {
  from: string;
  subject: string;
  body: string;
  threadId?: string;
  messageId: string;
}

interface EmailSummary {
  messageId: string;
  from: string;
  subject: string;
  triageResult?: string;
}

interface CalEvent {
  startIso: string;
  endIso: string;
  summary?: string;
}

declare const tools: {
  calendar: {
    /**
     * Busy intervals on the user's primary calendar between two ISO
     * 8601 timestamps. Use this before proposing a meeting time.
     * Both bounds are inclusive; recurring events are expanded to
     * individual instances.
     */
    busy(startIso: string, endIso: string): Promise<CalEvent[]>;
  };
  db: {
    /**
     * True iff an action row already exists for this messageId.
     * Use this to avoid double-replying when a sibling channel may have
     * already handled the same message.
     */
    isProcessed(messageId: string): Promise<boolean>;
    /**
     * Most recent emails from `sender` within the last `days` days,
     * newest first. Returns at most 50 rows. Use this to check whether
     * the sender has recent context you should reference.
     */
    recentEmailsFrom(sender: string, days: number): Promise<EmailSummary[]>;
    /**
     * Every prior message in the given Gmail thread (oldest → newest).
     * Use this when drafting a reply to make sure you don't repeat
     * yourself or contradict an earlier message in the same thread.
     */
    threadHistory(threadId: string): Promise<EmailSummary[]>;
  };
  wiki: {
    /**
     * Hint string pointing at likely-relevant wiki pages for this email.
     * Empty string means "no prior context — rely on the raw email only".
     * Backed by WikiReader::draft_hint.
     */
    draftHint(email: EmailContext): Promise<string>;
  };
  /**
   * TERMINAL. Submits a draft reply for human approval on Discord.
   * Must be the last tool call in your program. `body` is the literal
   * reply text; `reason` is a one-sentence justification logged with
   * the action. Calling this more than once in a single program is an
   * error.
   */
  draft(channel: Channel, body: string, reason: string): Promise<void>;
};
"#;
        let actual = manifest_v1().to_dts();
        assert_eq!(
            actual, EXPECTED,
            "manifest_v1().to_dts() output changed — update the snapshot in \
             `dts_output_matches_snapshot` if this was intentional"
        );
    }

    /// Verify ToolCallRecord round-trips through JSON without data loss.
    #[test]
    fn tool_call_record_json_round_trip_success() {
        let record = ToolCallRecord {
            call: "wiki.draftHint".to_string(),
            args_summary: serde_json::json!({"email": "alice@example.com"}),
            result_summary: Some(serde_json::json!("hint text here")),
            error: None,
            timestamp_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let back: ToolCallRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.call, record.call);
        assert_eq!(back.args_summary, record.args_summary);
        assert_eq!(back.result_summary, record.result_summary);
        assert_eq!(back.error, record.error);
        assert_eq!(back.timestamp_ms, record.timestamp_ms);
    }

    /// Verify ToolCallRecord round-trips correctly when the call errored
    /// (result_summary is None, error is set).
    #[test]
    fn tool_call_record_json_round_trip_error() {
        let record = ToolCallRecord {
            call: "db.recentEmailsFrom".to_string(),
            args_summary: serde_json::json!({"sender": "bob@example.com", "days": 7}),
            result_summary: None,
            error: Some("db connection timeout".to_string()),
            timestamp_ms: 1_700_000_001_234,
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let back: ToolCallRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.call, record.call);
        assert!(back.result_summary.is_none());
        assert_eq!(back.error.as_deref(), Some("db connection timeout"));
        assert_eq!(back.timestamp_ms, record.timestamp_ms);
    }
}
