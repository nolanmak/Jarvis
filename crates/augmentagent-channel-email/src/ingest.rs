//! Wiki ingest pipeline.
//!
//! After each triage (and optional draft) completes, we fire a Haiku call that
//! reads/writes the wiki. The call is best-effort — failures log a warning
//! and do not affect triage/draft correctness.

use std::path::PathBuf;
use std::sync::Arc;

use augmentagent_store::Email;
use tracing::{debug, warn};

use crate::channel::Reasoner;
use crate::decision::DecisionKind;
use crate::reasoner::ingest_opts;

/// What the ingest call should know about the outcome of the surrounding flow.
/// Drives the log.md entry and any commitment tracking.
#[derive(Debug, Clone)]
pub enum IngestTrigger {
    Triaged,       // skip / flag
    Drafted,       // draft created, awaiting approval (live mode)
    DryRunDrafted, // draft created in dry-run
    Sent,          // approved + sent
    Rejected,      // approver skipped or timeout
}

impl IngestTrigger {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Triaged => "triaged",
            Self::Drafted => "drafted",
            Self::DryRunDrafted => "dry-run-drafted",
            Self::Sent => "sent",
            Self::Rejected => "rejected",
        }
    }
}

/// Build the user message for the ingest call.
pub fn ingest_user_message(
    email: &Email,
    decision: DecisionKind,
    reason: Option<&str>,
    draft: Option<&str>,
    trigger: &IngestTrigger,
) -> String {
    let decision_str = match decision {
        DecisionKind::Reply => "reply",
        DecisionKind::Skip => "skip",
        DecisionKind::Flag => "flag",
    };
    let draft_block = match draft {
        Some(d) if !d.trim().is_empty() => format!("\n\n<draft>\n{d}\n</draft>"),
        _ => String::new(),
    };

    format!(
        "Update the wiki for this email per your schema. Follow the ingest workflow in your system prompt exactly — touch only the pages that the workflow calls for. When finished, reply with one line: \"ingested\".\n\n<email>\nMessageId: {message_id}\nThreadId: {thread_id}\nFrom: {from}\nSubject: {subject}\nDate: {date}\n\n{body}\n</email>\n\n<triage>\ndecision: {decision_str}\nreason: {reason}\ntrigger: {trigger}\n</triage>{draft_block}\n",
        message_id = email.message_id,
        thread_id = email.thread_id.as_deref().unwrap_or("(none)"),
        from = email.from,
        subject = email.subject,
        date = email.date,
        body = email.body,
        reason = reason.unwrap_or(""),
        trigger = trigger.label(),
    )
}

/// Fire-and-forget ingest. Spawns a background tokio task; never errors to
/// the caller. Returns immediately.
#[allow(clippy::too_many_arguments)]
pub fn spawn_ingest<R>(
    reasoner: Arc<R>,
    wiki_root: PathBuf,
    schema: String,
    email: Email,
    decision: DecisionKind,
    reason: Option<String>,
    draft: Option<String>,
    trigger: IngestTrigger,
) where
    R: Reasoner + 'static,
{
    tokio::spawn(async move {
        let opts = ingest_opts(schema, wiki_root);
        let user_msg = ingest_user_message(
            &email,
            decision,
            reason.as_deref(),
            draft.as_deref(),
            &trigger,
        );
        match reasoner.call(&opts, &user_msg).await {
            Ok(reply) => {
                let first_line = reply.lines().next().unwrap_or("").trim();
                debug!(
                    message_id = %email.message_id,
                    trigger = trigger.label(),
                    reply = first_line,
                    "wiki ingest ok"
                );
            }
            Err(e) => {
                warn!(
                    message_id = %email.message_id,
                    trigger = trigger.label(),
                    "wiki ingest failed: {e:#}"
                );
            }
        }
    });
}
