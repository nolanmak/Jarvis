//! End-of-cycle self-page + memory nudge writer.
//!
//! Hermes (Nous Research) runs a "closed learning loop" — at idle/end-of-
//! cycle moments the agent surfaces "anything worth remembering?" and
//! persists the result so the next cycle starts with more context than the
//! previous one. Issue #112 is the closed-loop _surface_ for that pattern:
//! a deterministic, append-only writer that turns a per-cycle summary into
//! a durable cycle-log entry the next cycle can read.
//!
//! This module is intentionally narrow:
//!
//! - It writes — it does NOT invoke the reasoner. Constructing the summary
//!   (via `digest_opts`, `triage_opts`, or `ask_opts` end-of-session) and
//!   approving entries through Discord live in their respective channel
//!   crates. We just take a fully-formed [`CycleSummary`] and persist it.
//! - It's idempotent at the path level: writing the same `cycle_id` twice
//!   produces a single concatenated file (markdown append), not two
//!   competing files. That's important because the proactive runner may
//!   retry on transient failures.
//! - It's a thin layer on `std::fs`: no async, no SQLite, no Discord —
//!   those couplings are owned by the caller. Easy to unit-test, easy to
//!   swap for an FTS5-backed sink once #111's MCP memory server lands.
//!
//! The on-disk layout matches the issue spec:
//!
//! ```text
//! <root>/
//!   cycle-log.md           # rolling append of all cycle summaries
//!   cycles/
//!     2026-05-25T10:00.md  # one file per cycle (id = ISO-8601 minute)
//! ```
//!
//! A rolling `cycle-log.md` makes "give me last week's themes" cheap
//! (single `Read` on a single file), while the per-cycle files give the
//! UI a stable URL to link a single cycle into.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What kind of work the cycle did. Used as a section header so the writer
/// can mix multiple surfaces (email triage + Discord ask + digest) into one
/// `cycle-log.md` without losing the provenance of each entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleSurface {
    /// One email-triage batch (poll cycle).
    EmailTriage,
    /// One Discord `/ask` or DM Q&A session.
    DiscordAsk,
    /// The morning digest synthesis call.
    Digest,
    /// A `/loop` tick — recurring task pulse.
    LoopTick,
    /// Catch-all for surfaces we add later — caller supplies a label.
    Other(&'static str),
}

impl CycleSurface {
    /// Stable label used in the markdown header. Stable strings keep
    /// future cycle-log greps deterministic across daemon versions.
    pub fn label(&self) -> &'static str {
        match self {
            CycleSurface::EmailTriage => "email-triage",
            CycleSurface::DiscordAsk => "discord-ask",
            CycleSurface::Digest => "digest",
            CycleSurface::LoopTick => "loop-tick",
            CycleSurface::Other(s) => s,
        }
    }
}

/// One cycle's worth of "what just happened + what's worth remembering."
/// All fields except `id`/`surface` are optional so the caller can write
/// a stub entry on cycles that produced no new signal (e.g. the inbox was
/// empty) — those still get logged so we can compute coverage later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleSummary {
    /// Stable identifier — typically an ISO-8601 minute (`2026-05-25T10:00`).
    /// Used both as the per-cycle filename stem and the header anchor in
    /// the rolling log so a follow-up cycle can grep its predecessor.
    pub id: String,
    /// Which channel produced this cycle.
    pub surface: CycleSurface,
    /// One-line headline. Required because empty entries pollute the log.
    pub headline: String,
    /// Free-form prose body. Markdown allowed; rendered as-is.
    pub body: String,
    /// Bullet list of "remember this" entries the agent surfaced. These
    /// are the candidate memories #111's FTS5 server will eventually
    /// consume; until then they live alongside the prose so a human can
    /// triage them by reading the log.
    pub remember: Vec<String>,
}

impl CycleSummary {
    /// Construct a new summary. `id` and `headline` are trimmed; both must
    /// be non-empty after trimming (caller's responsibility — the writer
    /// rejects empty headlines so we don't accidentally produce a wall of
    /// `## ` rows).
    pub fn new(
        id: impl Into<String>,
        surface: CycleSurface,
        headline: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into().trim().to_string(),
            surface,
            headline: headline.into().trim().to_string(),
            body: String::new(),
            remember: Vec::new(),
        }
    }

    /// Builder-style body setter.
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    /// Builder-style remember-bullet appender. Trims each entry; skips
    /// empty results so callers can `extend` from optional sources without
    /// downstream emptiness handling.
    pub fn add_remember(mut self, entry: impl Into<String>) -> Self {
        let trimmed = entry.into().trim().to_string();
        if !trimmed.is_empty() {
            self.remember.push(trimmed);
        }
        self
    }

    /// Render the summary as a markdown block. The format is stable across
    /// versions so existing cycle-log files stay greppable:
    ///
    /// ```text
    /// <a id="<id>"></a>
    /// ## <id> · <surface-label> · <headline>
    ///
    /// <body>
    ///
    /// **Worth remembering:**
    /// - bullet 1
    /// - bullet 2
    /// ```
    ///
    /// Section is always terminated by a trailing newline so concatenation
    /// into `cycle-log.md` doesn't glue adjacent entries together.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        // Anchor first so the rolling-log table-of-contents can deep-link.
        out.push_str(&format!("<a id=\"{}\"></a>\n", self.id));
        out.push_str(&format!(
            "## {} · {} · {}\n\n",
            self.id,
            self.surface.label(),
            self.headline
        ));
        let body = self.body.trim();
        if !body.is_empty() {
            out.push_str(body);
            out.push_str("\n\n");
        }
        if !self.remember.is_empty() {
            out.push_str("**Worth remembering:**\n");
            for r in &self.remember {
                out.push_str(&format!("- {r}\n"));
            }
            out.push('\n');
        }
        out
    }
}

/// On-disk writer. Pinned to a root directory so tests can use a `TempDir`
/// and the daemon can use `<state-dir>/cycles/`. The writer does not own
/// the directory — it creates `cycles/` lazily and appends to
/// `cycle-log.md` in-place.
#[derive(Debug, Clone)]
pub struct CycleLogger {
    root: PathBuf,
}

impl CycleLogger {
    /// New logger anchored at `root`. The directory is created on first
    /// write so callers don't have to bootstrap state-dir ordering.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Path to the rolling log. Always the same regardless of cycle id.
    pub fn rolling_log_path(&self) -> PathBuf {
        self.root.join("cycle-log.md")
    }

    /// Path to the per-cycle file for `id`. Note: this is a method, not a
    /// constant, because callers may want to surface it (e.g. linkify in
    /// a Discord card) without doing a full write.
    pub fn cycle_path(&self, id: &str) -> PathBuf {
        // Cycle ids contain `:` (ISO-8601 minute). Linux fs accepts `:` in
        // filenames but Windows + Mac shells choke on it. We replace `:`
        // with `-` so the same code runs on any future port without
        // surprise. `T` is preserved so the ISO shape stays readable.
        let safe = id.replace(':', "-");
        self.root.join("cycles").join(format!("{safe}.md"))
    }

    /// Write a cycle summary. Performs both:
    /// - `<root>/cycle-log.md` — append.
    /// - `<root>/cycles/<id>.md` — overwrite (single source of truth for
    ///   this cycle; retries re-render the same content deterministically).
    ///
    /// Returns the path to the per-cycle file so callers can surface it.
    pub fn write(&self, summary: &CycleSummary) -> anyhow::Result<PathBuf> {
        if summary.id.is_empty() {
            anyhow::bail!("cycle id must not be empty");
        }
        if summary.headline.is_empty() {
            anyhow::bail!("cycle headline must not be empty (id={})", summary.id);
        }
        fs::create_dir_all(self.root.join("cycles")).map_err(|e| {
            anyhow::anyhow!(
                "failed to create cycles dir at {}: {e}",
                self.root.join("cycles").display()
            )
        })?;
        let rendered = summary.to_markdown();
        // Per-cycle: overwrite. Idempotent for retries; the input is the
        // sole source of truth.
        let cycle_path = self.cycle_path(&summary.id);
        fs::write(&cycle_path, &rendered).map_err(|e| {
            anyhow::anyhow!(
                "failed to write per-cycle file at {}: {e}",
                cycle_path.display()
            )
        })?;
        // Rolling log: append. We tolerate a missing file but also a
        // pre-existing one (e.g. the daemon was restarted mid-cycle).
        // Append, not overwrite, because the rolling log is the durable
        // history; deduplication is the reader's job (entries are anchor-
        // tagged by id).
        let mut log_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.rolling_log_path())
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to open rolling log at {}: {e}",
                    self.rolling_log_path().display()
                )
            })?;
        log_file
            .write_all(rendered.as_bytes())
            .map_err(|e| anyhow::anyhow!("failed to append rolling log: {e}"))?;
        Ok(cycle_path)
    }
}

/// Resolve the default cycles root: `$XDG_STATE_HOME/augmentagent/` if set,
/// else `$HOME/.local/state/augmentagent/`, matching the existing daemon
/// state-dir layout (see systemd unit's `StateDirectory=`).
///
/// Returns just the parent dir; the [`CycleLogger`] will create `cycles/`
/// underneath. The path is not guaranteed to exist — callers should pass
/// it to [`CycleLogger::new`] which creates the subtree on first write.
pub fn default_cycles_root() -> PathBuf {
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(state).join("augmentagent");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/state/augmentagent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("read")
    }

    #[test]
    fn summary_renders_minimal_markdown() {
        let s = CycleSummary::new("2026-05-25T10:00", CycleSurface::Digest, "morning brief");
        let md = s.to_markdown();
        assert!(md.contains("<a id=\"2026-05-25T10:00\"></a>"));
        assert!(md.contains("## 2026-05-25T10:00 · digest · morning brief"));
        // No body, no remember — must not produce trailing "Worth remembering:".
        assert!(!md.contains("Worth remembering"));
        // Always ends with newline so appending stays clean.
        assert!(md.ends_with('\n'));
    }

    #[test]
    fn summary_renders_body_and_remember() {
        let s = CycleSummary::new("2026-05-25T11:30", CycleSurface::EmailTriage, "12 emails")
            .with_body("Skipped 8 newsletters, drafted 4 replies.\n")
            .add_remember("VC X follows up monthly — escalate")
            .add_remember("Domain provider sent renewal notice");
        let md = s.to_markdown();
        assert!(md.contains("Skipped 8 newsletters"));
        assert!(md.contains("**Worth remembering:**"));
        assert!(md.contains("- VC X follows up monthly — escalate"));
        assert!(md.contains("- Domain provider sent renewal notice"));
    }

    #[test]
    fn add_remember_trims_and_skips_empty() {
        let s = CycleSummary::new("x", CycleSurface::Digest, "h")
            .add_remember("  hi  ")
            .add_remember("")
            .add_remember("   ");
        // Two empty entries dropped — they'd render as blank bullets.
        assert_eq!(s.remember, vec!["hi".to_string()]);
    }

    #[test]
    fn writer_produces_both_files() {
        let tmp = TempDir::new().unwrap();
        let logger = CycleLogger::new(tmp.path());
        let s = CycleSummary::new("2026-05-25T10:00", CycleSurface::Digest, "h")
            .with_body("b");
        let cycle_path = logger.write(&s).unwrap();
        assert!(cycle_path.exists(), "per-cycle file must exist");
        assert!(logger.rolling_log_path().exists(), "rolling log must exist");
        let per_cycle = read(&cycle_path);
        let rolling = read(&logger.rolling_log_path());
        // First write: per-cycle == rolling log slice.
        assert_eq!(per_cycle, rolling);
    }

    #[test]
    fn rolling_log_appends_across_writes() {
        let tmp = TempDir::new().unwrap();
        let logger = CycleLogger::new(tmp.path());
        logger
            .write(&CycleSummary::new("2026-05-25T10:00", CycleSurface::Digest, "first"))
            .unwrap();
        logger
            .write(&CycleSummary::new("2026-05-25T11:00", CycleSurface::EmailTriage, "second"))
            .unwrap();
        let rolling = read(&logger.rolling_log_path());
        // Both anchors land in the rolling file in chronological order.
        let first = rolling
            .find("2026-05-25T10:00")
            .expect("first anchor present");
        let second = rolling
            .find("2026-05-25T11:00")
            .expect("second anchor present");
        assert!(first < second, "writes append in call order");
    }

    #[test]
    fn per_cycle_overwrite_is_idempotent_on_retry() {
        let tmp = TempDir::new().unwrap();
        let logger = CycleLogger::new(tmp.path());
        let s = CycleSummary::new("2026-05-25T10:00", CycleSurface::Digest, "h");
        let p = logger.write(&s).unwrap();
        let first = read(&p);
        // Retry: same input → same per-cycle bytes. Rolling log will grow
        // (that's the append contract) but per-cycle stays one source-of-
        // truth.
        logger.write(&s).unwrap();
        let second = read(&p);
        assert_eq!(first, second, "per-cycle must be idempotent on retry");
    }

    #[test]
    fn colon_in_id_is_sanitized_in_filename() {
        let tmp = TempDir::new().unwrap();
        let logger = CycleLogger::new(tmp.path());
        let p = logger.cycle_path("2026-05-25T10:00");
        // `:` replaced with `-` for cross-platform safety. Anchor inside
        // the rendered markdown keeps the raw id so deep-links work.
        assert!(
            p.file_name().unwrap().to_string_lossy().contains("10-00"),
            "got: {}",
            p.display()
        );
        assert!(
            !p.file_name().unwrap().to_string_lossy().contains("10:00"),
            "raw colon must not leak into filename"
        );
    }

    #[test]
    fn writer_rejects_empty_id_or_headline() {
        let tmp = TempDir::new().unwrap();
        let logger = CycleLogger::new(tmp.path());
        // Empty id.
        let s = CycleSummary {
            id: String::new(),
            surface: CycleSurface::Digest,
            headline: "h".into(),
            body: String::new(),
            remember: Vec::new(),
        };
        let err = logger.write(&s).unwrap_err();
        assert!(format!("{err}").contains("id must not be empty"));
        // Empty headline.
        let s2 = CycleSummary {
            id: "x".into(),
            surface: CycleSurface::Digest,
            headline: String::new(),
            body: String::new(),
            remember: Vec::new(),
        };
        let err = logger.write(&s2).unwrap_err();
        assert!(format!("{err}").contains("headline must not be empty"));
    }

    #[test]
    fn default_cycles_root_honors_xdg() {
        let prev = std::env::var("XDG_STATE_HOME").ok();
        std::env::set_var("XDG_STATE_HOME", "/tmp/state-test");
        assert_eq!(
            default_cycles_root(),
            PathBuf::from("/tmp/state-test/augmentagent")
        );
        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }

    #[test]
    fn other_surface_carries_custom_label() {
        let s = CycleSummary::new("x", CycleSurface::Other("custom-thing"), "h");
        assert!(s.to_markdown().contains("custom-thing"));
    }
}
