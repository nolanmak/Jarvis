//! #1833 — trusted-contact autonomy in the daemon: wrapping the approval
//! broker in `AutonomyBroker` (the D6 choke point), the executor that runs
//! plans through `ReplyApprover` (the Approve button's own send path), the
//! minute sweep, the notice controls, and the side-effect-free
//! `augmentagent autonomy explain`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker};
use augmentagent_autonomy::broker::{AutonomyBroker, AutonomyExecutor};
use augmentagent_autonomy::calendar::{CalendarOps, GoogleCalendarOps};
use augmentagent_autonomy::control::Controls;
use augmentagent_autonomy::gate::{AutonomyGate, GateDeps, TrustSource};
use augmentagent_autonomy::watch::{snapshot_read_only, TrustWatch};
use augmentagent_autonomy::{config_path, load_config, master_switch_from_env, TrustSnapshot};
use augmentagent_channel_core::{FallbackReasoner, Reasoner};
use augmentagent_store::Store;

use crate::{Cli, ReplyApprover};

/// Owner addresses for the D9 email 1:1 rule beyond the connected Gmail
/// accounts: the `@` entries of `AUGMENTAGENT_OWNER_ALIASES` (its name
/// entries are for team coverage and are ignored here).
fn owner_addresses() -> Vec<String> {
    std::env::var(crate::team_coverage::ALIASES_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|a| a.trim().to_ascii_lowercase())
        .filter(|a| a.contains('@'))
        .collect()
}

/// The direct Google calendar client, when Google OAuth is configured.
fn calendar_ops(store: &Arc<Store>) -> Option<Arc<dyn CalendarOps>> {
    match augmentagent_channel_email::google::GoogleGmail::from_env() {
        Ok(google) => Some(Arc::new(GoogleCalendarOps {
            google: augmentagent_channel_calendar::GoogleCalendar::new(Arc::new(google)),
            store: Arc::clone(store),
            tz: augmentagent_approval_discord::timeparse::owner_zone(),
            now: None,
        })),
        Err(e) => {
            tracing::warn!("autonomy: no direct Google client ({e}); calendar plans will be held for approval");
            None
        }
    }
}

/// Plans run through the same `approve` the Approve button calls.
pub(crate) struct ApproverExecutor(pub Arc<ReplyApprover>);

#[async_trait]
impl AutonomyExecutor for ApproverExecutor {
    async fn approve(&self, action_id: &str) -> ApprovalActionOutcome {
        ApprovalActionHandler::approve(self.0.as_ref(), action_id).await
    }

    async fn replace_draft(&self, action_id: &str, text: &str) -> Result<(), String> {
        self.0.replace_pending_draft(action_id, text).await
    }
}

impl ReplyApprover {
    /// #1833 D8 — a calendar plan sends the registry template instead of
    /// the drafter's text: replace a still-pending action's draft (the
    /// evidence receipt stays attached to the action). For Gmail the
    /// server draft is recreated sender-only on the same thread first, then
    /// repointed, then the old one deleted — the revise order (#500/#501).
    pub(crate) async fn replace_pending_draft(&self, action_id: &str, text: &str) -> Result<(), String> {
        let action = self
            .store
            .get_action_with_email(action_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no action {action_id}"))?;
        if action.action.status != "pending" {
            return Err(format!("action is {}, not pending", action.action.status));
        }
        let refresh = |store: &Store| match store.refresh_pending_draft(action_id, text) {
            Ok(true) => Ok(()),
            Ok(false) => Err("action is no longer pending".to_string()),
            Err(e) => Err(e.to_string()),
        };
        match action.email.platform.as_str() {
            crate::imessage_send::PLATFORM => refresh(&self.store),
            augmentagent_channel_email::inbound::PLATFORM => {
                let entity = action
                    .email
                    .account_entity_id
                    .as_deref()
                    .ok_or("no account on the email")?;
                let to = augmentagent_channel_email::gmail::extract_bare_email(&action.email.from);
                let subject = if action.email.subject.to_ascii_lowercase().starts_with("re:") {
                    action.email.subject.clone()
                } else {
                    format!("Re: {}", action.email.subject)
                };
                let new_id = self
                    .gmail
                    .create_draft_with_attachment(entity, &to, &subject, text, action.email.thread_id.as_deref(), &[], &[], &[])
                    .await
                    .map_err(|e| format!("create_draft: {e}"))?;
                if let Err(e) = refresh(&self.store) {
                    let _ = augmentagent_channel_email::gmail::GmailApi::delete_draft(self.gmail.as_ref(), entity, &new_id).await;
                    return Err(e);
                }
                self.store.set_action_draft_id(action_id, &new_id).map_err(|e| e.to_string())?;
                let _ = self.store.set_action_envelope(action_id, Some(&to), None, None);
                if let Some(old) = action.draft_id.as_deref() {
                    if let Err(e) = augmentagent_channel_email::gmail::GmailApi::delete_draft(self.gmail.as_ref(), entity, old).await {
                        tracing::warn!(action_id, "autonomy: deleting the replaced Gmail draft failed: {e}");
                    }
                }
                Ok(())
            }
            other => Err(format!("autonomy cannot replace a {other} draft")),
        }
    }
}

/// D6 — wrap serve's broker. With `AUGMENTAGENT_AUTONOMY` off the wrapper
/// forwards every card unchanged (NG6). `serve` also starts the minute
/// sweep and installs the notice controls.
pub(crate) fn wrap(
    cli: &Cli,
    inner: Arc<dyn ApprovalBroker>,
    approver: &Arc<ReplyApprover>,
    reasoner: &Arc<FallbackReasoner>,
    serve: bool,
) -> Arc<AutonomyBroker> {
    let store = Arc::clone(&approver.store);
    let master_on = master_switch_from_env();
    let path = config_path();
    let watch = Arc::new(TrustWatch::new(
        Arc::clone(&store),
        path.clone(),
        cli.wiki_dir.as_ref().map(|w| w.join("people")),
    ));
    let calendar = if master_on { calendar_ops(&store) } else { None };
    let gate = Arc::new(AutonomyGate::new(GateDeps {
        store: Arc::clone(&store),
        master_on,
        trust: watch.clone(),
        calendar: calendar.clone(),
        planner: Some(Arc::clone(reasoner) as Arc<dyn Reasoner>),
        owner_addresses: owner_addresses(),
        tz: augmentagent_approval_discord::timeparse::owner_zone(),
    }));
    let broker = Arc::new(AutonomyBroker::new(
        inner,
        gate,
        Arc::new(ApproverExecutor(Arc::clone(approver))),
        Some(Arc::clone(&watch)),
    ));
    if master_on {
        tracing::info!(config = %path.display(), "trusted-contact autonomy is ON");
        if serve {
            // D3: catch edits made while the daemon was down, at startup.
            let _ = watch.snapshot(chrono::Utc::now().timestamp_millis());
            augmentagent_approval_discord::autonomy::set_autonomy_control(Arc::new(Controls {
                store: Arc::clone(&store),
                calendar,
                config_path: path,
            }));
            broker.spawn_sweeper();
        }
    }
    broker
}

#[derive(clap::Subcommand, Debug, Clone)]
pub(crate) enum Op {
    /// Dry-run the autonomy gate for one stored action: the decision, the
    /// reason, the calendar candidates and the plan. No side effects — the
    /// store is opened read-only, nothing is sent, approved or changed.
    Explain {
        /// The inbound message id (its newest action is explained).
        #[arg(long, conflicts_with = "action_id")]
        message_id: Option<String>,
        /// Explain this action instead.
        #[arg(long)]
        action_id: Option<String>,
        /// Trusted-contacts config (default: AUGMENTAGENT_TRUSTED_CONTACTS
        /// or config/trusted-contacts.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Evaluate as if AUGMENTAGENT_AUTONOMY=on and a notice surface
        /// existed (QA before enabling it).
        #[arg(long)]
        assume_on: bool,
        /// Treat every valid contact in the config as confirmed (QA with a
        /// scratch config; the store is not touched).
        #[arg(long)]
        assume_active: bool,
        /// Skip the planning call (no model spend).
        #[arg(long)]
        no_planner: bool,
        /// When the gate stops before planning, also show the calendar
        /// candidates and the plan the planner would propose for this
        /// message (QA: checks 4–9 are skipped for the preview only; it is
        /// never acted on).
        #[arg(long)]
        preview_plan: bool,
    },
}

/// `augmentagent autonomy …` — runs before the store is opened normally.
pub(crate) async fn run(cli: &Cli, op: &Op) -> Result<()> {
    let Op::Explain { message_id, action_id, config, assume_on, assume_active, no_planner, preview_plan } = op;
    let db = cli
        .db
        .clone()
        .or_else(|| std::env::var("AUGMENTAGENT_DB").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("data.db"));
    let store = Arc::new(Store::open_read_only(&db).with_context(|| format!("open {} read-only", db.display()))?);
    let action_id = match (action_id, message_id) {
        (Some(a), _) => a.clone(),
        (None, Some(m)) => store
            .latest_action_for_message(m)?
            .map(|(id, _, _)| id)
            .with_context(|| format!("no action for message {m}"))?,
        (None, None) => anyhow::bail!("pass --message-id or --action-id"),
    };
    let path = config.clone().unwrap_or_else(config_path);
    let report = load_config(&path);
    for alert in report.alerts() {
        println!("config alert:  {alert}");
    }
    let snapshot = if *assume_active {
        TrustSnapshot { active: report.contacts.clone(), report, ..TrustSnapshot::default() }
    } else {
        snapshot_read_only(&store, report)
    };
    println!(
        "config:        {} ({} active, {} pending)",
        path.display(),
        snapshot.active.len(),
        snapshot.pending.len()
    );
    let master_on = *assume_on || master_switch_from_env();
    let planner: Option<Arc<dyn Reasoner>> = if *no_planner {
        None
    } else {
        Some(augmentagent_channel_core::build_reasoner() as Arc<dyn Reasoner>)
    };
    let gate = AutonomyGate::new(GateDeps {
        store: Arc::clone(&store),
        master_on,
        trust: Arc::new(snapshot),
        calendar: calendar_ops(&store),
        planner,
        owner_addresses: owner_addresses(),
        tz: augmentagent_approval_discord::timeparse::owner_zone(),
    });
    let deliverable = *assume_on || std::env::var("DISCORD_BOT_TOKEN").is_ok_and(|t| !t.trim().is_empty());
    let text = augmentagent_autonomy::explain::explain(&gate, &store, &action_id, deliverable, *preview_plan).await?;
    print!("{text}");
    Ok(())
}
