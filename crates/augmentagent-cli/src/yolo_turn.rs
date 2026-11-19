//! A turn in a yolo thread (#2219, spec #2243 sections 5.4 and 5.5).
//!
//! The Discord handler decides that a message is a yolo turn and sets the
//! yolo flag; this module builds the turn. It starts from `yolo_opts` and
//! adds only what is listed here, so a tool that `answer` attaches to normal
//! turns can never reach a yolo turn by accident.
use anyhow::Context as _;
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_core::Reasoner as _;
use augmentagent_channel_core::{yolo, yolo_preset};

/// Web tools in yolo turns send the model's queries and fetched URLs, which
/// may carry private data, to the web vendors. That is the owner's call, so
/// it is off unless this is exactly `1`.
pub const WEB_ENV: &str = "AUGMENTAGENT_YOLO_WEB";

pub(crate) fn web_enabled() -> bool {
    std::env::var(WEB_ENV).ok().as_deref() == Some("1")
}

/// What `/yolo` reports about this host.
pub(crate) fn facts() -> crate::yolo_command::Facts {
    use crate::web::{Kind, ProviderId};
    let (availability, _) =
        crate::web::availability(&augmentagent_channel_core::secret_loader::load_provider_key);
    let search = availability.available(Kind::Search);
    let web = web_enabled().then(|| {
        (
            !search.is_empty(),
            !availability.available(Kind::Fetch).is_empty(),
        )
    });
    let web_caveat = web.and(search.first()).and_then(|id| match id {
        ProviderId::Context => Some(
            "Search runs on context, which has no unfiltered switch; results may be filtered."
                .to_string(),
        ),
        ProviderId::Brightdata => Some(
            "Search runs on brightdata, which is not asked for unfiltered results yet.".to_string(),
        ),
        _ => None,
    });
    crate::yolo_command::Facts {
        readiness: yolo::readiness(),
        model: yolo::model_id(),
        zdr_requested: yolo::zdr_requested(),
        // Sandboxed workspace shell commands land with #2217.
        shell: false,
        web,
        web_caveat,
    }
}

impl crate::WikiQuerier {
    /// Answer one yolo turn. `question` already holds the thread's own
    /// history and the current message.
    pub(crate) async fn answer_yolo(
        &self,
        ctx: &augmentagent_approval_discord::AuditCtx,
        question: &str,
    ) -> anyhow::Result<String> {
        // Readiness is re-checked on every turn: the flag or key may have
        // changed since the thread was opened. No other model answers.
        if let Err(cause) = yolo::readiness() {
            return Ok(format!("not available: {}", cause.sentence()));
        }
        let channel = ctx
            .channel_id
            .context("a yolo turn needs a Discord thread")?;
        let workspace = yolo_preset::workspace_dir(channel.get())
            .context("no state directory for the yolo workspace")?;
        yolo_preset::ensure_workspace(&workspace)?;
        let mut opts =
            yolo_preset::yolo_opts(self.wiki_root.clone(), self.repo_root.clone(), workspace);
        opts.session_id = Some(ctx.session_id.clone());
        if web_enabled() {
            #[cfg(test)]
            let bin = self.repo_root.join("target/release/augmentagent");
            #[cfg(not(test))]
            let bin = std::env::current_exe()?;
            // Search and fetch only, unfiltered where the vendor allows it.
            // Never the provider-switch tool: a yolo turn changes nothing
            // that outlives the thread.
            crate::web::tool::configure(&mut opts, ctx, true, false, &bin);
        }
        // No tool notifications are posted for a yolo turn; the audit log
        // records every call. No owner rules, no approval ledger.
        opts.audit_notifier = None;
        let prompt = format!("{}{question}", crate::now_awareness_line());
        augmentagent_channel_core::model_selection::SELECTED_PROFILE
            .scope(
                Some(ProviderKind::Yolo),
                self.reasoner.call_transcript(&opts, &prompt),
            )
            .await
    }
}
