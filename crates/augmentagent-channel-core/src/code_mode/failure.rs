//! Code-Mode failure handling: self-repair retry + classic-fallback signal +
//! postmortem reporting (gh issue + Discord notice).
//!
//! ## Flow
//!
//! [`handle_code_mode_failure`] is the entry point the channel calls when
//! the first code-mode attempt errors. It:
//!
//! 1. Asks the reasoner for ONE repair pass
//!    ([`crate::reasoner::Reasoner::call_code_mode_with_repair`]).
//! 2. If the repair returns a fenced TS block, executes it through the
//!    real Deno sandbox (`run_program`). On success the dispatcher's
//!    terminal `tools.draft` has already landed an `actions` row with
//!    `mode='code'`; we patch that row's `toolCallTrace` tail entry to
//!    flag `repair_used: true` and return [`DraftOutcome::CodeMode`].
//! 3. If either the repair call OR its program execution fails, we return
//!    [`DraftOutcome::ClassicNeeded`] carrying the [`FailureRecord`]. The
//!    channel does its own classic-draft path (which is deeply intertwined
//!    with per-channel context — tone, thread history, archetype, ask
//!    resolution — and would be the wrong thing to lift into channel-core),
//!    writes the row via `log_action`, then calls
//!    [`report_classic_fallback`] with the resulting `action_id` to file
//!    the gh issue + Discord notice and patch `generatedSource` onto the
//!    classic row for audit.
//!
//! Splitting the flow into "decide if classic is needed" (this module) +
//! "run classic" (channel) + "report" (this module) is intentional: it keeps
//! per-channel context out of channel-core while still owning the postmortem
//! envelope here. Successful self-repair never files an issue — repair
//! working is the system behaving as intended.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{Email, Store};

use crate::code_mode::{
    self, manifest::ToolManifest, CodeModeError, DefaultDispatcher, MessageContext, RunnerError,
};
use crate::reasoner::{Reasoner, ReasonerOpts};

/// What `handle_code_mode_failure` decided.
#[derive(Debug)]
pub enum DraftOutcome {
    /// Self-repair succeeded. The repaired program executed cleanly and the
    /// dispatcher's terminal `tools.draft` already wrote the `actions` row.
    /// `action_id` is the row id; `repair_used` is always `true` on this
    /// variant (kept as a field so callers don't have to track context).
    CodeMode {
        action_id: String,
        repair_used: bool,
    },
    /// Self-repair could not produce a working program. The channel should
    /// run its own classic-draft path against the carried context, write a
    /// classic action row via `Store::log_action`, then call
    /// [`report_classic_fallback`] with the row id so the postmortem
    /// envelope (gh issue + Discord notice + `generatedSource` patch on
    /// the classic row) fires consistently.
    ClassicNeeded(FailureRecord),
}

/// Snapshot of a code-mode failure carried from
/// [`handle_code_mode_failure`] to [`report_classic_fallback`]. The channel
/// passes it through unchanged — it only exists to keep the postmortem
/// fields in one place between the two calls.
#[derive(Debug, Clone)]
pub struct FailureRecord {
    /// Original (failed) program source. Persisted on the classic action
    /// row as `generatedSource` for audit.
    pub original_source: String,
    /// Human-readable error message for the original attempt.
    pub original_error: String,
    /// Pipeline stage that produced `original_error` — populates the
    /// `Failure stage` field of the postmortem template.
    pub original_stage: FailureStage,
    /// Repair-attempt source, if the reasoner returned one (None when the
    /// repair call itself failed).
    pub repair_source: Option<String>,
    /// Repair-attempt error, if a repair was attempted (None when no
    /// repair was tried — should not happen in v1; included for symmetry).
    pub repair_error: Option<String>,
    /// Pipeline stage that produced `repair_error`. None when no repair
    /// was attempted.
    pub repair_stage: Option<FailureStage>,
}

/// Where in the code-mode pipeline a failure surfaced. Maps 1:1 to the
/// `Failure stage` field of the postmortem template in #53.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStage {
    /// `Reasoner::call_code_mode` (initial program) failed — claude spawn,
    /// IO, stream parse, OR `NoCodeBlock`.
    CallCodeMode,
    /// `run_program` (initial program) errored — sandbox timeout, runtime
    /// throw, dispatcher returned `Internal`, etc.
    RunProgram,
    /// `Reasoner::call_code_mode_with_repair` failed before producing a
    /// fenced block.
    RepairCallCodeMode,
    /// Repaired program executed but the sandbox rejected it.
    RepairRunProgram,
}

impl FailureStage {
    fn as_str(self) -> &'static str {
        match self {
            FailureStage::CallCodeMode => "call_code_mode",
            FailureStage::RunProgram => "run_program",
            FailureStage::RepairCallCodeMode => "repair_call_code_mode",
            FailureStage::RepairRunProgram => "repair_run_program",
        }
    }
}

/// Abstraction over the `gh issue create` invocation so tests don't actually
/// create issues on the real repo. Production wires
/// [`GhCliIssueRunner`]; tests inject a `Mutex<Vec<...>>` recorder.
///
/// The returned `u64` is the new issue number (parsed from the URL `gh`
/// prints on success). On failure (binary missing, auth, API error) callers
/// should `log+warn`, not abort — the Discord notice is best-effort by design.
#[async_trait]
pub trait GhIssueRunner: Send + Sync {
    async fn create_issue(
        &self,
        title: &str,
        body: &str,
        labels: &[&str],
    ) -> anyhow::Result<u64>;
}

/// Production implementation: shells out to `gh issue create`. The binary
/// is resolved via PATH; if it's missing, every call errors and the
/// caller's `warn!`+continue path kicks in.
pub struct GhCliIssueRunner {
    bin: String,
}

impl Default for GhCliIssueRunner {
    fn default() -> Self {
        Self {
            // `AUGMENTAGENT_GH_BIN` lets dev boxes override the binary
            // path without recompiling. Mirrors `AUGMENTAGENT_DENO_BIN` /
            // `CLAUDE_CLI`.
            bin: std::env::var("AUGMENTAGENT_GH_BIN").unwrap_or_else(|_| "gh".into()),
        }
    }
}

impl GhCliIssueRunner {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GhIssueRunner for GhCliIssueRunner {
    async fn create_issue(
        &self,
        title: &str,
        body: &str,
        labels: &[&str],
    ) -> anyhow::Result<u64> {
        // Escape hatch for tests / CI runs that should never touch the real
        // repo. Setting `AUGMENTAGENT_GH_DISABLE=1` makes every invocation
        // a no-op-that-errors-immediately so the reporter logs+continues
        // without spawning `gh`. Tests in the workspace set this once via
        // a `Once` at the top of their cfg(test) module.
        if std::env::var("AUGMENTAGENT_GH_DISABLE").as_deref() == Ok("1") {
            anyhow::bail!("gh disabled via AUGMENTAGENT_GH_DISABLE=1 (test mode)");
        }
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("issue").arg("create");
        cmd.arg("--title").arg(title);
        cmd.arg("--body").arg(body);
        for l in labels {
            cmd.arg("--label").arg(l);
        }
        let out = cmd.output().await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            anyhow::bail!("gh issue create failed: {stderr}");
        }
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        // `gh` prints a URL like `https://github.com/owner/repo/issues/123`
        // on success. Parse the trailing path segment for the issue number.
        let url = stdout.trim();
        let n = url
            .rsplit('/')
            .find_map(|s| s.parse::<u64>().ok())
            .ok_or_else(|| anyhow::anyhow!("gh stdout has no issue number: {url:?}"))?;
        Ok(n)
    }
}

/// Per-call context for [`handle_code_mode_failure`] and
/// [`report_classic_fallback`]. Built once per code-mode attempt by the
/// channel; refs only — nothing here is owned.
pub struct FailureCtx<'a> {
    /// Reasoner for the repair retry.
    pub reasoner: &'a dyn Reasoner,
    /// Opts mirroring the first `call_code_mode` (same system prompt etc).
    pub opts: ReasonerOpts,
    /// User message — the repair tail is appended via
    /// `call_code_mode_with_repair`.
    pub user_msg: String,
    /// Manifest used for the original attempt (same allowlist on retry).
    pub manifest: ToolManifest,
    /// Per-message context for the repair-program dispatcher.
    pub message_ctx: MessageContext,
    /// Precomputed wiki hint (channel runs `WikiReader::draft_hint` once
    /// before code-mode kicks off). Forwarded to the repair dispatcher.
    pub wiki_hint: String,
    /// Store the channel uses — failure.rs needs it for the
    /// `generatedSource` patch on the classic-fallback action row.
    pub store: Arc<Store>,
    /// Broker for the failure Discord notice. Uses `post_flag_notice` with
    /// a synthetic Email envelope so the message lands in the approval
    /// channel with the action id + issue number.
    pub broker: Arc<dyn ApprovalBroker>,
    /// gh CLI runner for filing `code-mode-failure` issues. Behind a trait
    /// so tests can swap in a recorder.
    pub gh: Arc<dyn GhIssueRunner>,
    /// Email the failure is about — copied into the postmortem and the
    /// Discord notice envelope.
    pub email: Email,
    /// Canonical channel name (`"gmail"`, etc.) for the postmortem.
    pub channel: String,
    /// Optional model id for the postmortem (`opts.model` typically). The
    /// channel may want to override (e.g. "Sonnet 4.5" instead of the
    /// internal alias). `None` falls back to `opts.model` or `"default"`.
    pub model: Option<String>,
    /// Manifest version label for the postmortem. v1 hard-codes `"v1"`.
    pub manifest_version: &'static str,
}

/// Attempt one self-repair pass on a failed code-mode draft. Returns either
/// a successful code-mode action id (repair worked) or a [`FailureRecord`]
/// the caller threads back into [`report_classic_fallback`] after running
/// its own classic-draft path.
///
/// `original_source` is the failed program (may be empty if the original
/// failure was `CallCodeMode` — no source ever materialised). `original_err`
/// is the rendered error message that goes into the repair prompt.
///
/// Note: `original_source` being empty when the original error was a reasoner
/// failure is fine — `call_code_mode_with_repair` will format an empty
/// `<prior_program>` block. The model has been observed to recover from this.
pub async fn handle_code_mode_failure(
    ctx: &FailureCtx<'_>,
    original_source: &str,
    original_err: &CodeModeError,
    original_stage: FailureStage,
) -> DraftOutcome {
    let original_error = original_err.to_string();

    // #660 — self-repair exists for "the model wrote a bad program", not for
    // "the provider is gone". A quota refusal / outage / timeout would fail
    // the repair call identically (and, pre-#655, one rate-limited email
    // burned three back-to-back spawns: code-mode → repair → classic — the
    // exact #448 amplification shape). Skip straight to the classic signal;
    // the classic attempt fails fast against the cooldown latch.
    if let CodeModeError::ReasonerFailed(inner) = original_err {
        if crate::reasoner::ReasonerError::find_in(inner).is_some_and(|re| re.is_provider_side()) {
            warn!(
                message_id = %ctx.email.message_id,
                channel = %ctx.channel,
                "code-mode failed provider-side ({original_error}); skipping \
                 self-repair — retrying a rate-limited/unavailable provider \
                 only amplifies (#448/#660)"
            );
            return DraftOutcome::ClassicNeeded(FailureRecord {
                original_source: original_source.to_string(),
                original_error,
                original_stage,
                repair_source: None,
                repair_error: Some(
                    "skipped: provider-side failure (rate limit / outage / timeout) — \
                     self-repair would amplify, see #660"
                        .into(),
                ),
                repair_stage: Some(FailureStage::RepairCallCodeMode),
            });
        }
    }

    info!(
        message_id = %ctx.email.message_id,
        channel = %ctx.channel,
        stage = original_stage.as_str(),
        "code-mode failure — attempting self-repair"
    );

    // Step 1: ask the reasoner for a corrected program.
    let repaired_source = match ctx
        .reasoner
        .call_code_mode_with_repair(&ctx.opts, &ctx.user_msg, original_source, &original_error)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(
                message_id = %ctx.email.message_id,
                "self-repair call_code_mode failed: {e:#}"
            );
            return DraftOutcome::ClassicNeeded(FailureRecord {
                original_source: original_source.to_string(),
                original_error,
                original_stage,
                repair_source: None,
                repair_error: Some(e.to_string()),
                repair_stage: Some(FailureStage::RepairCallCodeMode),
            });
        }
    };

    // Step 2: execute the repair through the real sandbox using a fresh
    // dispatcher (one DefaultDispatcher is single-use — its trace buffer
    // and last_action_id are owned).
    let dispatcher = DefaultDispatcher::new(
        ctx.store.as_ref(),
        ctx.message_ctx.clone(),
        repaired_source.clone(),
    )
    .with_wiki_hint(ctx.wiki_hint.clone());

    if let Err(e) = code_mode::run_program(&repaired_source, &ctx.manifest, &dispatcher).await {
        warn!(
            message_id = %ctx.email.message_id,
            "self-repair run_program failed: {e}"
        );
        return DraftOutcome::ClassicNeeded(FailureRecord {
            original_source: original_source.to_string(),
            original_error,
            original_stage,
            repair_source: Some(repaired_source),
            repair_error: Some(render_runner_error(&e)),
            repair_stage: Some(FailureStage::RepairRunProgram),
        });
    }

    let action_id = match dispatcher.last_action_id() {
        Some(id) => id,
        None => {
            // Program completed but never called tools.draft — same as
            // primary path: treat as a soft failure and fall back to
            // classic.
            warn!(
                message_id = %ctx.email.message_id,
                "self-repair program completed without calling tools.draft"
            );
            return DraftOutcome::ClassicNeeded(FailureRecord {
                original_source: original_source.to_string(),
                original_error,
                original_stage,
                repair_source: Some(repaired_source),
                repair_error: Some("repair program did not call tools.draft".into()),
                repair_stage: Some(FailureStage::RepairRunProgram),
            });
        }
    };

    // Patch the persisted trace tail entry to flag `repair_used: true`. We
    // do this with a small UPDATE rather than threading a "repair_used"
    // flag through the dispatcher — it keeps the dispatch path
    // ignorant of repair bookkeeping and avoids a store-API change.
    if let Err(e) = mark_trace_repair_used(&ctx.store, &action_id) {
        warn!(
            message_id = %ctx.email.message_id,
            action_id = %action_id,
            "could not mark toolCallTrace repair_used: {e}"
        );
        // Non-fatal — the row is still a valid code-mode draft, just
        // missing the audit flag.
    }

    info!(
        message_id = %ctx.email.message_id,
        action_id = %action_id,
        "self-repair succeeded; classic fallback skipped, no gh issue filed"
    );

    DraftOutcome::CodeMode {
        action_id,
        repair_used: true,
    }
}

/// File the gh issue + post the Discord notice + patch `generatedSource`
/// onto the classic action row. Called by the channel AFTER it has run its
/// own classic-draft path and persisted the row via `Store::log_action`.
///
/// Best-effort throughout: a failed gh invocation falls back to a Discord
/// notice without an issue number; a failed Discord post is logged but
/// doesn't propagate. The classic action row is already live — neither
/// side effect failing here is allowed to break the user's reply.
pub async fn report_classic_fallback(
    ctx: &FailureCtx<'_>,
    record: &FailureRecord,
    classic_action_id: &str,
) {
    // 1. Patch generatedSource onto the classic row (audit trail). The
    // `log_action` helper doesn't write this column; we do it here so the
    // store crate stays untouched.
    if let Err(e) = persist_failed_source(&ctx.store, classic_action_id, &record.original_source) {
        warn!(
            action_id = %classic_action_id,
            "could not persist failed code-mode source on classic action row: {e}"
        );
    }

    // 2. File the gh issue. Failure here is logged but doesn't abort —
    // the Discord notice will fire with `issue=None`.
    let title = build_issue_title(&record.original_error);
    let body = build_postmortem(
        ctx,
        record,
        classic_action_id,
        FinalDraftMode::Classic,
    );
    let issue_number = match ctx.gh.create_issue(&title, &body, &["code-mode-failure"]).await {
        Ok(n) => Some(n),
        Err(e) => {
            error!(
                action_id = %classic_action_id,
                "gh issue create failed: {e:#}"
            );
            None
        }
    };

    // 3. Post the Discord notice via the approval broker's `post_flag_notice`
    // path. We synthesise an Email envelope so the existing card renderer
    // surfaces the message in the approvals channel; the body carries the
    // structured failure summary.
    let issue_str = match issue_number {
        Some(n) => format!("#{n}"),
        None => "<gh issue not created>".to_string(),
    };
    let notice = format!(
        "code-mode failure on action {classic_action_id}; filed issue {issue_str}; fell back to classic."
    );
    let notice_email = Email {
        to: String::new(),
        cc: String::new(),
        message_id: format!("code-mode-failure:{classic_action_id}"),
        thread_id: None,
        from: format!("code-mode/{}", ctx.channel),
        subject: "[code-mode] failure → classic fallback".to_string(),
        body: notice.clone(),
        date: String::new(),
        account_entity_id: ctx.email.account_entity_id.clone(),
        platform: ctx.channel.clone(),
        kind: "code_mode_failure_notice".into(),
    };
    if let Err(e) = ctx.broker.post_flag_notice(&notice_email, &notice).await {
        warn!(
            action_id = %classic_action_id,
            "post_flag_notice (code-mode failure) failed: {e}"
        );
    }
}

/// What the postmortem reports as the final draft mode. Currently only
/// `Classic` is exercised (a successful repair never reaches the reporter),
/// but kept as an enum so a future "repair attempted + succeeded" telemetry
/// path can reuse the same template.
#[derive(Debug, Clone, Copy)]
enum FinalDraftMode {
    Classic,
}

impl FinalDraftMode {
    fn as_str(self) -> &'static str {
        match self {
            FinalDraftMode::Classic => "classic",
        }
    }
}

fn build_issue_title(err: &str) -> String {
    // Squash newlines + clip to ~60 chars so the GitHub title stays
    // grep-friendly. The full error lives in the body.
    let one_line: String = err
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = one_line.trim();
    const MAX: usize = 60;
    let summary: String = trimmed.chars().take(MAX).collect();
    let dots = if trimmed.chars().count() > MAX { "…" } else { "" };
    format!("[code-mode] {summary}{dots}")
}

fn build_postmortem(
    ctx: &FailureCtx<'_>,
    record: &FailureRecord,
    action_id: &str,
    final_mode: FinalDraftMode,
) -> String {
    let model = ctx
        .model
        .clone()
        .or_else(|| ctx.opts.model.clone())
        .unwrap_or_else(|| "default".to_string());
    let repair_attempted = if record.repair_stage.is_some() {
        "yes"
    } else {
        "no"
    };
    let final_stage = record
        .repair_stage
        .unwrap_or(record.original_stage)
        .as_str();
    let repair_source = record.repair_source.as_deref().unwrap_or("N/A");
    let combined_error = match (&record.repair_error, record.repair_stage) {
        (Some(rep_err), Some(stage)) => format!(
            "{}\n\n[repair: {}]\n{}",
            record.original_error,
            stage.as_str(),
            rep_err
        ),
        _ => record.original_error.clone(),
    };
    format!(
        "## Postmortem\n\n\
         **Action id:** {action_id}\n\
         **Message id:** {msg_id}\n\
         **Channel:** {channel}\n\
         **Model:** {model}\n\
         **Manifest version:** {manifest_version}\n\
         **Failure stage:** {final_stage}\n\
         **Error:**\n\
         ```\n{combined_error}\n```\n\
         **Original program:**\n\
         ```ts\n{original_source}\n```\n\
         **Repair attempted:** {repair_attempted}\n\
         **Repair program:**\n\
         ```ts\n{repair_source}\n```\n\
         **Final draft mode:** {final_mode}\n",
        msg_id = ctx.email.message_id,
        channel = ctx.channel,
        manifest_version = ctx.manifest_version,
        original_source = record.original_source,
        final_mode = final_mode.as_str(),
    )
}

fn render_runner_error(e: &RunnerError) -> String {
    // RunnerError's Display is concise; embed the kind/stack where present.
    match e {
        RunnerError::RuntimeError { message, stack, kind } => {
            let kind = kind.as_deref().unwrap_or("runtime");
            format!("[{kind}] {message}\n{stack}")
        }
        other => other.to_string(),
    }
}

fn persist_failed_source(store: &Store, action_id: &str, source: &str) -> anyhow::Result<()> {
    use augmentagent_store::rusqlite::params;
    store.with_conn(|c| {
        c.execute(
            "UPDATE actions SET generatedSource = ?1 WHERE id = ?2",
            params![source, action_id],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// Read `actions.toolCallTrace`, parse, append a `repair_used: true` marker
/// to the tail entry, write back. No-op when the row has no trace (defensive
/// — successful code-mode rows always have a trace, but we guard).
fn mark_trace_repair_used(store: &Store, action_id: &str) -> anyhow::Result<()> {
    use augmentagent_store::rusqlite::params;
    let trace_str: Option<String> = store.with_conn(|c| {
        c.query_row(
            "SELECT toolCallTrace FROM actions WHERE id = ?1",
            params![action_id],
            |r| r.get::<_, Option<String>>(0),
        )
    })?;
    let Some(trace_str) = trace_str else {
        return Ok(());
    };
    let mut arr: Vec<Value> = serde_json::from_str(&trace_str).unwrap_or_default();
    if let Some(last) = arr.last_mut() {
        if let Some(obj) = last.as_object_mut() {
            obj.insert("repair_used".to_string(), Value::Bool(true));
        }
    } else {
        // Empty array — push a sentinel so the audit flag is at least
        // visible. This branch is defensive; in practice the dispatcher
        // always pushes the terminal `draft` entry before writing the row.
        arr.push(serde_json::json!({"repair_used": true}));
    }
    let updated = serde_json::to_string(&arr)?;
    store.with_conn(|c| {
        c.execute(
            "UPDATE actions SET toolCallTrace = ?1 WHERE id = ?2",
            params![updated, action_id],
        )?;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    use augmentagent_approval_discord::{ApprovalBroker, ApprovalError, NoopBroker};
    use augmentagent_store::Email;

    use crate::code_mode::manifest_v1;
    use crate::reasoner::ReasonerOpts;

    /// Records every gh issue create + returns canned issue numbers.
    #[derive(Default)]
    struct RecordingGh {
        calls: Mutex<Vec<(String, String, Vec<String>)>>,
        next_number: Mutex<u64>,
    }
    impl RecordingGh {
        fn new(start: u64) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                next_number: Mutex::new(start),
            }
        }
    }
    #[async_trait]
    impl GhIssueRunner for RecordingGh {
        async fn create_issue(
            &self,
            title: &str,
            body: &str,
            labels: &[&str],
        ) -> anyhow::Result<u64> {
            self.calls.lock().unwrap().push((
                title.to_string(),
                body.to_string(),
                labels.iter().map(|s| s.to_string()).collect(),
            ));
            let mut n = self.next_number.lock().unwrap();
            let out = *n;
            *n += 1;
            Ok(out)
        }
    }

    /// Records every Discord notice. `post_approval` is unreachable in this
    /// path; `post_flag_notice` is what the failure reporter calls.
    #[derive(Default)]
    struct RecordingBroker {
        notices: Mutex<Vec<(Email, String)>>,
    }
    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(
            &self,
            _: &str,
            _: &Email,
            _: &str,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            email: &Email,
            reason: &str,
        ) -> Result<(), ApprovalError> {
            self.notices
                .lock()
                .unwrap()
                .push((email.clone(), reason.to_string()));
            Ok(())
        }
    }

    /// Scripted reasoner — pop the next canned response for every `call`.
    /// `call_code_mode_with_repair` flows through the trait's default impl,
    /// which calls `call_code_mode`, which calls `call`. We test the unwrap
    /// path end-to-end this way without faking the fenced-block layer.
    struct ScriptedReasoner {
        responses: Mutex<std::collections::VecDeque<String>>,
    }
    impl ScriptedReasoner {
        fn new<I: IntoIterator<Item = &'static str>>(resps: I) -> Self {
            Self {
                responses: Mutex::new(resps.into_iter().map(String::from).collect()),
            }
        }
    }
    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
            let mut q = self.responses.lock().unwrap();
            q.pop_front()
                .ok_or_else(|| anyhow::anyhow!("ScriptedReasoner: queue empty"))
        }
    }

    fn tmp_store() -> Arc<Store> {
        // Mirror the governor tests' `fresh_store` shape: seed the
        // Node-owned tables (`actions`, `emails`, `gmail_accounts`) before
        // Store::open so the additive migrations have something to ALTER.
        // We can't reuse that helper directly (it's `pub(crate)` on the
        // governor module) so the equivalent SQL is inlined here.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failure-test.db");
        let conn = augmentagent_store::rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS actions (
                id TEXT PRIMARY KEY,
                messageId TEXT NOT NULL,
                threadId TEXT,
                fromEmail TEXT NOT NULL,
                subject TEXT NOT NULL,
                originalBody TEXT,
                draftBody TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                errorMessage TEXT,
                createdAt INTEGER NOT NULL,
                updatedAt INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS emails (
                messageId TEXT PRIMARY KEY,
                threadId TEXT,
                fromEmail TEXT NOT NULL,
                subject TEXT NOT NULL,
                body TEXT,
                receivedAt TEXT,
                accountEntityId TEXT,
                firstSeenAt INTEGER NOT NULL,
                triageResult TEXT,
                agentProcessedAt INTEGER,
                platform TEXT NOT NULL DEFAULT 'gmail',
                kind TEXT NOT NULL DEFAULT 'dm'
            );
            CREATE TABLE IF NOT EXISTS gmail_accounts (
                id TEXT PRIMARY KEY,
                connectionId TEXT NOT NULL,
                email TEXT,
                label TEXT,
                entityId TEXT NOT NULL,
                active INTEGER DEFAULT 1,
                createdAt INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS channel_subscriptions (
                id TEXT PRIMARY KEY,
                platform TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                display_name TEXT NOT NULL,
                mode TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                last_seen_message_id TEXT,
                last_digest_at_ms INTEGER,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS slack_workspaces (
                id TEXT PRIMARY KEY,
                team_id TEXT NOT NULL UNIQUE,
                team_name TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                connection_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                created_at_ms INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();
        drop(conn);
        let store = Store::open(&path).unwrap();
        // Leak the tempdir so the file stays valid for the test's lifetime.
        std::mem::forget(dir);
        Arc::new(store)
    }

    fn sample_email() -> Email {
        Email {
            to: String::new(),
            cc: String::new(),
            message_id: "msg-i7-test".into(),
            thread_id: Some("t-i7".into()),
            from: "user@example.com".into(),
            subject: "hi".into(),
            body: "hello".into(),
            date: "2026-05-22".into(),
            account_entity_id: Some("acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    fn build_ctx<'a>(
        reasoner: &'a dyn Reasoner,
        store: Arc<Store>,
        broker: Arc<dyn ApprovalBroker>,
        gh: Arc<dyn GhIssueRunner>,
        email: Email,
    ) -> FailureCtx<'a> {
        FailureCtx {
            reasoner,
            opts: ReasonerOpts {
                system_prompt: String::new(),
                model: Some("test-model".into()),
                allowed_tools: Vec::new(),
                add_dirs: Vec::new(),
                permission_mode: "default".into(),
                cwd: None,
                env: Vec::new(),
                settings_json: None,
                restrict_env: false,
                audit_logger: None,
                audit_notifier: None,
                session_id: None,
            },
            user_msg: "draft a reply".to_string(),
            manifest: manifest_v1(),
            message_ctx: MessageContext {
                channel: "gmail".to_string(),
                email: email.clone(),
                account_id: Some("acc1".to_string()),
            },
            wiki_hint: String::new(),
            store,
            broker,
            gh,
            email,
            channel: "gmail".to_string(),
            model: None,
            manifest_version: "v1",
        }
    }

    #[tokio::test]
    async fn postmortem_template_renders_all_fields() {
        // Pure template render — no async needed, but kept as tokio for
        // sibling-test ergonomics.
        let store = tmp_store();
        let broker: Arc<dyn ApprovalBroker> = Arc::new(NoopBroker);
        let gh: Arc<dyn GhIssueRunner> = Arc::new(RecordingGh::new(1));
        let r = ScriptedReasoner::new(["unused"]);
        let ctx = build_ctx(&r, store, broker, gh, sample_email());
        let record = FailureRecord {
            original_source: "async function main(){ throw 'x'; }".into(),
            original_error: "runtime: throw 'x'".into(),
            original_stage: FailureStage::RunProgram,
            repair_source: Some("async function main(){}".into()),
            repair_error: Some("[runtime] still bad".into()),
            repair_stage: Some(FailureStage::RepairRunProgram),
        };
        let body = build_postmortem(&ctx, &record, "act-123", FinalDraftMode::Classic);
        assert!(body.contains("## Postmortem"));
        assert!(body.contains("**Action id:** act-123"));
        assert!(body.contains("**Message id:** msg-i7-test"));
        assert!(body.contains("**Channel:** gmail"));
        assert!(body.contains("**Model:** test-model"));
        assert!(body.contains("**Manifest version:** v1"));
        // Final stage reflects the repair stage when repair was attempted.
        assert!(body.contains("**Failure stage:** repair_run_program"));
        assert!(body.contains("throw 'x'"));
        assert!(body.contains("still bad"));
        assert!(body.contains("**Repair attempted:** yes"));
        assert!(body.contains("**Final draft mode:** classic"));
    }

    #[tokio::test]
    async fn issue_title_caps_at_60_chars_and_collapses_newlines() {
        let long = "a".repeat(120);
        let title = build_issue_title(&long);
        assert!(title.starts_with("[code-mode] "));
        // 12-char prefix + 60-char summary + ellipsis = 12 + 60 + 1.
        // We're not strict on length; just check truncation happened.
        assert!(title.contains('…'));
        assert!(title.len() <= "[code-mode] ".len() + 60 + 4); // 4 = utf8 ellipsis
        let multi = "line1\nline2\nline3";
        let t2 = build_issue_title(multi);
        assert!(!t2.contains('\n'));
    }

    /// #660 — a provider-side reasoner failure (rate limit / outage /
    /// timeout) must skip the self-repair spawn entirely. Pre-#655 one
    /// rate-limited email burned three back-to-back CLI spawns (code-mode →
    /// repair → classic); the repair leg is the one this module owns.
    #[tokio::test]
    async fn provider_side_failure_skips_self_repair() {
        let store = tmp_store();
        let broker: Arc<dyn ApprovalBroker> = Arc::new(NoopBroker);
        let gh: Arc<dyn GhIssueRunner> = Arc::new(RecordingGh::new(1));
        // One canned response queued: if repair ran, it would be consumed.
        let r = ScriptedReasoner::new(["```ts\nawait main();\n```"]);
        let ctx = build_ctx(&r, store, broker, gh, sample_email());

        let quota = CodeModeError::ReasonerFailed(anyhow::Error::new(
            crate::reasoner::ReasonerError::RateLimited {
                provider: "claude".into(),
                message: "You've hit your session limit · resets 9:30am".into(),
                reset_at: None,
            },
        ));
        let out =
            handle_code_mode_failure(&ctx, "", &quota, FailureStage::CallCodeMode).await;
        match out {
            DraftOutcome::ClassicNeeded(rec) => {
                assert!(rec.repair_source.is_none(), "no repair program must exist");
                assert!(
                    rec.repair_error.as_deref().unwrap_or("").contains("skipped"),
                    "record must say the repair was skipped: {:?}",
                    rec.repair_error
                );
            }
            other => panic!("expected ClassicNeeded, got {other:?}"),
        }
        assert_eq!(
            r.responses.lock().unwrap().len(),
            1,
            "the repair reasoner call must never fire on a provider-side failure"
        );

        // Contrast: a content-level failure (NoCodeBlock) still repairs.
        let content = CodeModeError::NoCodeBlock;
        let out2 =
            handle_code_mode_failure(&ctx, "", &content, FailureStage::CallCodeMode).await;
        assert_eq!(
            r.responses.lock().unwrap().len(),
            0,
            "content-level failures must still attempt self-repair"
        );
        drop(out2);
    }

    #[tokio::test]
    async fn repair_call_failure_returns_classic_needed() {
        // The repair reasoner call fails outright (empty queue → Err) →
        // FailureRecord with no repair_source.
        let store = tmp_store();
        let broker: Arc<dyn ApprovalBroker> = Arc::new(NoopBroker);
        let gh: Arc<dyn GhIssueRunner> = Arc::new(RecordingGh::new(1));
        let r = ScriptedReasoner::new([]); // empty → call errors
        let ctx = build_ctx(&r, store, broker, gh.clone(), sample_email());
        let original_src = "async function main(){ throw 'boom'; }";
        let original_err = CodeModeError::ReasonerFailed(anyhow::anyhow!("boom"));
        let outcome =
            handle_code_mode_failure(&ctx, original_src, &original_err, FailureStage::RunProgram)
                .await;
        match outcome {
            DraftOutcome::ClassicNeeded(rec) => {
                assert_eq!(rec.original_source, original_src);
                assert_eq!(rec.original_stage, FailureStage::RunProgram);
                assert!(rec.repair_source.is_none());
                assert_eq!(rec.repair_stage, Some(FailureStage::RepairCallCodeMode));
                assert!(rec.repair_error.is_some());
            }
            DraftOutcome::CodeMode { .. } => panic!("expected classic-needed"),
        }
    }

    #[tokio::test]
    async fn no_fenced_block_in_repair_response_also_classic() {
        // Repair call returns text WITHOUT a fenced ts block → extract_ts_block
        // errors → handle_code_mode_failure surfaces RepairCallCodeMode stage
        // (NoCodeBlock).
        let store = tmp_store();
        let broker: Arc<dyn ApprovalBroker> = Arc::new(NoopBroker);
        let gh: Arc<dyn GhIssueRunner> = Arc::new(RecordingGh::new(1));
        let r = ScriptedReasoner::new(["sorry, can't help"]);
        let ctx = build_ctx(&r, store, broker, gh.clone(), sample_email());
        let outcome = handle_code_mode_failure(
            &ctx,
            "",
            &CodeModeError::NoCodeBlock,
            FailureStage::CallCodeMode,
        )
        .await;
        let rec = match outcome {
            DraftOutcome::ClassicNeeded(r) => r,
            _ => panic!("expected classic-needed"),
        };
        assert_eq!(rec.repair_stage, Some(FailureStage::RepairCallCodeMode));
        // The original stage is preserved.
        assert_eq!(rec.original_stage, FailureStage::CallCodeMode);
    }

    #[tokio::test]
    async fn report_classic_fallback_files_issue_and_posts_notice() {
        let store = tmp_store();
        let broker = Arc::new(RecordingBroker::default());
        let gh = Arc::new(RecordingGh::new(42));
        let bk: Arc<dyn ApprovalBroker> = broker.clone();
        let gh_dyn: Arc<dyn GhIssueRunner> = gh.clone();
        let r = ScriptedReasoner::new([]);
        let ctx = build_ctx(&r, store.clone(), bk, gh_dyn, sample_email());

        // Land a classic action row first (mirrors what the channel does).
        use augmentagent_store::ActionStatus;
        let action_id = store
            .log_action(
                &ctx.email.message_id,
                ctx.email.thread_id.as_deref(),
                &ctx.email.from,
                &ctx.email.subject,
                Some(&ctx.email.body),
                Some("classic draft body"),
                ActionStatus::Pending,
            )
            .unwrap();

        let record = FailureRecord {
            original_source: "async function main(){ /* bad */ throw 'x'; }".into(),
            original_error: "runtime: throw 'x'".into(),
            original_stage: FailureStage::RunProgram,
            repair_source: Some("async function main(){ throw 'still bad'; }".into()),
            repair_error: Some("[runtime] still bad".into()),
            repair_stage: Some(FailureStage::RepairRunProgram),
        };
        report_classic_fallback(&ctx, &record, &action_id).await;

        // gh issue filed.
        let gh_calls = gh.calls.lock().unwrap();
        assert_eq!(gh_calls.len(), 1, "exactly one gh issue should be filed");
        let (title, body, labels) = &gh_calls[0];
        assert!(title.starts_with("[code-mode]"));
        assert!(body.contains("## Postmortem"));
        assert!(body.contains("**Action id:** "));
        assert!(body.contains(&action_id[..]));
        assert!(body.contains("**Final draft mode:** classic"));
        assert_eq!(labels, &vec!["code-mode-failure".to_string()]);

        // Discord notice posted with issue number.
        let notices = broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1, "exactly one Discord notice should be posted");
        let (_, reason) = &notices[0];
        assert!(reason.contains("#42"));
        assert!(reason.contains(&action_id[..]));
        assert!(reason.contains("classic"));

        // generatedSource was patched onto the classic row.
        let src: Option<String> = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT generatedSource FROM actions WHERE id = ?1",
                    augmentagent_store::rusqlite::params![action_id],
                    |r| r.get::<_, Option<String>>(0),
                )
            })
            .unwrap();
        assert_eq!(
            src.as_deref(),
            Some("async function main(){ /* bad */ throw 'x'; }"),
            "generatedSource must hold the failed original program"
        );
    }

    #[tokio::test]
    async fn report_classic_fallback_handles_gh_failure() {
        // If gh errors, the Discord notice still fires (with no issue
        // number), and generatedSource still gets patched.
        struct AlwaysFailGh;
        #[async_trait]
        impl GhIssueRunner for AlwaysFailGh {
            async fn create_issue(
                &self,
                _: &str,
                _: &str,
                _: &[&str],
            ) -> anyhow::Result<u64> {
                anyhow::bail!("gh not installed (test stub)")
            }
        }
        let store = tmp_store();
        let broker = Arc::new(RecordingBroker::default());
        let gh: Arc<dyn GhIssueRunner> = Arc::new(AlwaysFailGh);
        let bk: Arc<dyn ApprovalBroker> = broker.clone();
        let r = ScriptedReasoner::new([]);
        let ctx = build_ctx(&r, store.clone(), bk, gh, sample_email());
        use augmentagent_store::ActionStatus;
        let action_id = store
            .log_action(
                &ctx.email.message_id,
                ctx.email.thread_id.as_deref(),
                &ctx.email.from,
                &ctx.email.subject,
                Some(&ctx.email.body),
                Some("classic draft body"),
                ActionStatus::Pending,
            )
            .unwrap();
        let record = FailureRecord {
            original_source: "bad source".into(),
            original_error: "err".into(),
            original_stage: FailureStage::CallCodeMode,
            repair_source: None,
            repair_error: Some("repair err".into()),
            repair_stage: Some(FailureStage::RepairCallCodeMode),
        };
        report_classic_fallback(&ctx, &record, &action_id).await;
        let notices = broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        let (_, reason) = &notices[0];
        assert!(reason.contains("<gh issue not created>"));
        // generatedSource still patched.
        let src: Option<String> = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT generatedSource FROM actions WHERE id = ?1",
                    augmentagent_store::rusqlite::params![action_id],
                    |r| r.get::<_, Option<String>>(0),
                )
            })
            .unwrap();
        assert_eq!(src.as_deref(), Some("bad source"));
    }

    #[tokio::test]
    async fn mark_trace_repair_used_appends_flag_to_tail() {
        // Direct unit test for the trace-patch helper. Mirrors the path
        // `handle_code_mode_failure` takes on a successful repair.
        use augmentagent_store::ActionStatus;
        let store = tmp_store();
        let initial_trace =
            r#"[{"call":"db.recentEmailsFrom","args_summary":[],"result_summary":[],"error":null,"timestamp_ms":0},{"call":"draft","args_summary":[],"result_summary":null,"error":null,"timestamp_ms":1}]"#;
        let action_id = store
            .log_action_code_mode(
                "m-trace",
                Some("t-trace"),
                "x@y.com",
                "subj",
                Some("body"),
                Some("draft"),
                ActionStatus::Pending,
                "async function main(){}",
                initial_trace,
            )
            .unwrap();

        mark_trace_repair_used(&store, &action_id).unwrap();

        let trace: Option<String> = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT toolCallTrace FROM actions WHERE id = ?1",
                    augmentagent_store::rusqlite::params![action_id],
                    |r| r.get::<_, Option<String>>(0),
                )
            })
            .unwrap();
        let trace = trace.expect("trace persisted");
        let arr: Vec<Value> = serde_json::from_str(&trace).unwrap();
        assert_eq!(arr.len(), 2);
        let tail = arr.last().unwrap();
        assert_eq!(
            tail.get("repair_used").and_then(|v| v.as_bool()),
            Some(true),
            "tail entry must carry repair_used=true; got {tail}"
        );
        // Earlier entry untouched.
        assert!(arr[0].get("repair_used").is_none());
    }
}
