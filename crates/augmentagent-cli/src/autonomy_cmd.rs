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
use augmentagent_autonomy::outreach::{Composed, OutreachComposer, OutreachDecision, OutreachEngine, OutreachReport};
use augmentagent_autonomy::{config_path, load_config, master_switch_from_env, Handle, Platform, TrustSnapshot};
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
    // C10 — heartbeat outreach and `autonomy outreach` drive this broker.
    let _ = LIVE.set((Arc::clone(&broker), Arc::clone(approver)));
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
    /// Operator QA for heartbeat outreach (#1833 C10 / D16): run one
    /// proposed message to a trusted contact through exactly the heartbeat's
    /// path — the proactive gate, the owner-style compose on their channel,
    /// then Approve and the autonomy notice, or a noted approval card.
    /// Live runs need AUGMENTAGENT_AUTONOMY=on and
    /// AUGMENTAGENT_HEARTBEAT_OUTREACH=on and use the real trust config.
    Outreach {
        /// The contact's wiki slug, as in trusted-contacts.toml.
        #[arg(long)]
        contact: String,
        /// The text to send them.
        #[arg(long)]
        message: String,
        /// The one-line reason recorded on the plan and shown in the notice.
        #[arg(long, default_value = "operator QA (autonomy outreach)")]
        why: String,
        /// Show the decision only: the store is opened read-only and nothing
        /// is composed, sent, approved or posted.
        #[arg(long)]
        dry_run: bool,
        /// Dry run only: trusted-contacts config to read instead of the
        /// default.
        #[arg(long, requires = "dry_run")]
        config: Option<PathBuf>,
        /// Dry run only: evaluate as if both switches were on and a notice
        /// surface existed.
        #[arg(long, requires = "dry_run")]
        assume_on: bool,
        /// Dry run only: treat every valid contact in the config as
        /// confirmed (scratch configs; the store is not touched).
        #[arg(long, requires = "dry_run")]
        assume_active: bool,
        #[arg(long)]
        json: bool,
    },
}

/// `augmentagent autonomy …` — runs before the store is opened normally.
pub(crate) async fn run(cli: &Cli, op: &Op) -> Result<()> {
    let (message_id, action_id, config, assume_on, assume_active, no_planner, preview_plan) = match op {
        Op::Explain { message_id, action_id, config, assume_on, assume_active, no_planner, preview_plan } => {
            (message_id, action_id, config, assume_on, assume_active, no_planner, preview_plan)
        }
        Op::Outreach { contact, message, why, dry_run, config, assume_on, assume_active, json } => {
            return outreach_cmd(cli, contact, message, why, *dry_run, config.clone(), *assume_on, *assume_active, *json).await;
        }
    };
    let db = db_path(cli);
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

fn db_path(cli: &Cli) -> PathBuf {
    cli.db
        .clone()
        .or_else(|| std::env::var("AUGMENTAGENT_DB").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("data.db"))
}

// ----- C10 / D16 heartbeat outreach -------------------------------------

/// The wrapped broker and approver of this process, set by [`wrap`]: the
/// live outreach engine drives them (approve path, cards, notices).
static LIVE: std::sync::OnceLock<(Arc<AutonomyBroker>, Arc<ReplyApprover>)> = std::sync::OnceLock::new();

/// The owner-style compose machinery outreach reuses: `imessage compose`'s
/// synthetic `kind = compose` row + pending action (its outbound allowlist
/// and send kill switch included), or a new-email Gmail draft + pending
/// action shaped exactly like `gmail compose --post`'s new-email card. No
/// card is posted here; the outreach engine approves or cards it.
pub(crate) struct CliComposer {
    pub store: Arc<Store>,
    pub approver: Option<Arc<ReplyApprover>>,
    pub imessage: crate::imessage_compose::ComposeConfig,
}

/// The new email's subject: the message's first line, cut at a word near
/// 60 characters.
fn outreach_subject(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Quick note");
    if first.chars().count() <= 60 {
        return first.to_string();
    }
    let cut: String = first.chars().take(60).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head).trim_end();
    format!("{cut}…")
}

#[async_trait]
impl OutreachComposer for CliComposer {
    async fn check(&self, target: &Handle, text: &str) -> Result<String, String> {
        use crate::imessage_compose::{compose, ComposeOutcome};
        match target.platform() {
            Platform::Imessage => match compose(&self.store, &self.imessage, target.value(), text, true) {
                ComposeOutcome::Preview { recipient, target } => {
                    Ok(format!("new iMessage to {} ({})", recipient.label(), target.service))
                }
                ComposeOutcome::Ambiguous { candidates, .. } => Err(format!("ambiguous: {}", candidates.join("; "))),
                ComposeOutcome::Unknown { reason, .. } => Err(reason),
                ComposeOutcome::Card { .. } => Err("a dry-run compose stored a card".into()),
            },
            Platform::Email => {
                let (_, account) = crate::resolve_gmail_entity_id(&self.store, None).map_err(|e| format!("{e:#}"))?;
                Ok(format!("new email to {} from {account}, subject \"{}\"", target.value(), outreach_subject(text)))
            }
        }
    }

    async fn compose(&self, target: &Handle, text: &str) -> Result<Composed, String> {
        use crate::imessage_compose::{compose, ComposeOutcome};
        match target.platform() {
            Platform::Imessage => match compose(&self.store, &self.imessage, target.value(), text, false) {
                ComposeOutcome::Card { action_id, email, .. } => Ok(Composed { action_id, email: *email }),
                ComposeOutcome::Preview { .. } => Err("compose stored nothing".into()),
                ComposeOutcome::Ambiguous { candidates, .. } => Err(format!("ambiguous: {}", candidates.join("; "))),
                ComposeOutcome::Unknown { reason, .. } => Err(reason),
            },
            Platform::Email => {
                let approver = self.approver.as_ref().ok_or("no Gmail client in this process")?;
                let (entity, account) = crate::resolve_gmail_entity_id(&self.store, None).map_err(|e| format!("{e:#}"))?;
                let to = target.value().to_string();
                let subject = outreach_subject(text);
                let draft_id = approver
                    .gmail
                    .create_draft_with_attachment(&entity, &to, &subject, text, None, &[], &[], &[])
                    .await
                    .map_err(|e| format!("create_draft: {e:#}"))?;
                // The new-email card shape of `gmail compose --post` (#412,
                // #962): synthetic `compose:<draft>` row, From = the sending
                // account, recipient in the #473 envelope.
                let email = augmentagent_store::Email {
                    attachments: Vec::new(),
                    message_id: format!("compose:{draft_id}"),
                    to: String::new(),
                    cc: String::new(),
                    thread_id: None,
                    from: account.clone(),
                    subject: subject.clone(),
                    body: String::new(),
                    date: String::new(),
                    account_entity_id: Some(entity.clone()),
                    platform: augmentagent_channel_email::inbound::PLATFORM.into(),
                    kind: "dm".into(),
                };
                let stored = (|| -> Result<String, String> {
                    self.store.upsert_email(&email).map_err(|e| e.to_string())?;
                    let action_id = self
                        .store
                        .log_action_with_envelope(
                            &email.message_id,
                            None,
                            &account,
                            &subject,
                            Some(""),
                            Some(text),
                            augmentagent_store::ActionStatus::Pending,
                            Some(&to),
                            Some(""),
                            Some(""),
                        )
                        .map_err(|e| e.to_string())?;
                    self.store.set_action_draft_id(&action_id, &draft_id).map_err(|e| e.to_string())?;
                    // Reserve the card for a minute, as `imessage compose`
                    // does, so the approval queue cannot post it while the
                    // gate decides.
                    let next = chrono::Utc::now().timestamp_millis() + 60_000;
                    self.store.record_nudge(&action_id, next).map_err(|e| e.to_string())?;
                    Ok(action_id)
                })();
                match stored {
                    Ok(action_id) => Ok(Composed { action_id, email }),
                    Err(e) => {
                        let _ = augmentagent_channel_email::gmail::GmailApi::delete_draft(approver.gmail.as_ref(), &entity, &draft_id).await;
                        Err(format!("could not store the compose: {e}"))
                    }
                }
            }
        }
    }
}

/// The live outreach engine of this process: the wrapped broker's gate,
/// approve path and notice surface. `None` before [`wrap`] ran (dry runs,
/// no approval surface).
fn live_engine(cli: &Cli) -> Option<Arc<OutreachEngine>> {
    let (broker, approver) = LIVE.get()?;
    Some(Arc::new(OutreachEngine {
        gate: Arc::clone(broker.gate()),
        broker: Some(Arc::clone(broker)),
        composer: Arc::new(CliComposer {
            store: Arc::clone(&approver.store),
            approver: Some(Arc::clone(approver)),
            imessage: crate::imessage_compose::ComposeConfig::from_env(cli.wiki_dir.clone()),
        }),
        wiki_root: cli.wiki_dir.clone(),
        notices_deliverable: false,
    }))
}

/// A side-effect-free engine: trust read without writing the store, no
/// broker (nothing composed, approved or posted).
fn dry_run_engine(
    cli: &Cli,
    store: Arc<Store>,
    config: Option<PathBuf>,
    assume_on: bool,
    assume_active: bool,
) -> (OutreachEngine, PathBuf, TrustSnapshot) {
    let path = config.unwrap_or_else(config_path);
    let report = load_config(&path);
    let snapshot = if assume_active {
        TrustSnapshot { active: report.contacts.clone(), report, ..TrustSnapshot::default() }
    } else {
        snapshot_read_only(&store, report)
    };
    let gate = AutonomyGate::new(GateDeps {
        store: Arc::clone(&store),
        master_on: assume_on || master_switch_from_env(),
        trust: Arc::new(snapshot.clone()),
        calendar: None,
        planner: None,
        owner_addresses: owner_addresses(),
        tz: augmentagent_approval_discord::timeparse::owner_zone(),
    });
    let engine = OutreachEngine {
        gate: Arc::new(gate),
        broker: None,
        composer: Arc::new(CliComposer {
            store,
            approver: None,
            imessage: crate::imessage_compose::ComposeConfig::from_env(cli.wiki_dir.clone()),
        }),
        wiki_root: cli.wiki_dir.clone(),
        notices_deliverable: assume_on || std::env::var("DISCORD_BOT_TOKEN").is_ok_and(|t| !t.trim().is_empty()),
    };
    (engine, path, snapshot)
}

/// D16 — the heartbeat's outreach sink: `None` unless
/// `AUGMENTAGENT_HEARTBEAT_OUTREACH=on` and `AUGMENTAGENT_AUTONOMY=on`.
/// A dry run gets a side-effect-free engine; a live run the wrapped
/// broker's (`None` when this process has no approval surface).
pub(crate) fn heartbeat_outreach(cli: &Cli, store: &Arc<Store>, dry_run: bool) -> Option<Arc<dyn augmentagent_heartbeat::Outreach>> {
    if !augmentagent_autonomy::heartbeat_outreach_from_env() {
        return None;
    }
    let engine = if dry_run {
        Arc::new(dry_run_engine(cli, Arc::clone(store), None, false, false).0)
    } else {
        match live_engine(cli) {
            Some(engine) => engine,
            None => {
                tracing::warn!("heartbeat outreach is on but no approval surface is wrapped; outreach stays off");
                return None;
            }
        }
    };
    tracing::info!("heartbeat outreach is ON");
    augmentagent_autonomy::outreach::heartbeat_outreach(engine)
}

fn render_report(r: &OutreachReport, dry_run: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!("contact:       {}\n", r.contact));
    out.push_str(&format!("channel:       {}\n", r.handle.as_deref().unwrap_or("-")));
    out.push_str(&format!("message:       {}\n", r.text));
    let decision = match r.decision {
        OutreachDecision::AutoSent => "AUTO-SENT".to_string(),
        OutreachDecision::WouldAutoSend => "AUTO-EXECUTE (would send without a click)".to_string(),
        OutreachDecision::NeedsApproval => "NEEDS APPROVAL (composed; noted approval card posted)".to_string(),
        OutreachDecision::WouldNeedApproval => "NEEDS APPROVAL (would compose and post a noted card)".to_string(),
        OutreachDecision::Refused => "REFUSED (nothing composed, no card)".to_string(),
        OutreachDecision::Failed => "FAILED (see the card)".to_string(),
    };
    out.push_str(&format!("decision:      {decision}\n"));
    if let Some(reason) = r.reason {
        out.push_str(&format!("reason:        {reason}\n"));
    }
    if let Some(detail) = &r.detail {
        out.push_str(&format!("detail:        {detail}\n"));
    }
    if let Some(key) = &r.idem_key {
        out.push_str(&format!("idem key:      {key}\n"));
    }
    if let Some(id) = &r.action_id {
        out.push_str(&format!("action:        {id}\n"));
    }
    if let Some(id) = r.plan_id {
        out.push_str(&format!("plan:          {id}\n"));
    }
    if dry_run {
        out.push_str("side effects:  none (dry run)\n");
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn outreach_cmd(
    cli: &Cli,
    contact: &str,
    message: &str,
    why: &str,
    dry_run: bool,
    config: Option<PathBuf>,
    assume_on: bool,
    assume_active: bool,
    json: bool,
) -> Result<()> {
    let db = db_path(cli);
    let now = chrono::Utc::now().timestamp_millis();
    let (report, header) = if dry_run {
        let store = Arc::new(Store::open_read_only(&db).with_context(|| format!("open {} read-only", db.display()))?);
        let (engine, path, snapshot) = dry_run_engine(cli, store, config, assume_on, assume_active);
        let mut header = String::new();
        for alert in snapshot.report.alerts() {
            header.push_str(&format!("config alert:  {alert}\n"));
        }
        header.push_str(&format!(
            "config:        {} ({} active, {} pending)\n",
            path.display(),
            snapshot.active.len(),
            snapshot.pending.len()
        ));
        header.push_str(&format!(
            "switches:      master {} · outreach {}{}\n",
            if master_switch_from_env() { "on" } else { "off" },
            if augmentagent_autonomy::heartbeat_outreach_from_env() { "on" } else { "off" },
            if assume_on { " (evaluated as on: --assume-on)" } else { "" }
        ));
        let roster = engine.roster(now);
        header.push_str(&format!(
            "roster:        {}\n",
            if roster.is_empty() {
                "(empty: no active full-tier contact)".to_string()
            } else {
                roster.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>().join(", ")
            }
        ));
        (engine.run(contact, message, why, true, now).await, header)
    } else {
        anyhow::ensure!(
            augmentagent_autonomy::heartbeat_outreach_from_env(),
            "outreach is off: it needs AUGMENTAGENT_AUTONOMY=on and AUGMENTAGENT_HEARTBEAT_OUTREACH=on \
             (use --dry-run to see the decision without them)"
        );
        let store = Arc::new(Store::open(&db).with_context(|| format!("open {}", db.display()))?);
        let (_broker, approver) = crate::build_broker(cli, Arc::clone(&store), false).await?;
        anyhow::ensure!(approver.is_some(), "no approval surface is configured; cannot run outreach live");
        let engine = live_engine(cli).context("the autonomy broker was not wrapped")?;
        (engine.run(contact, message, why, false, now).await, String::new())
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "contact": report.contact,
                "status": report.status(),
                "reason": report.reason.map(|r| r.code()),
                "detail": report.detail,
                "handle": report.handle,
                "message": report.text,
                "action_id": report.action_id,
                "plan_id": report.plan_id,
                "idem_key": report.idem_key,
                "dry_run": dry_run,
            }))?
        );
    } else {
        print!("{header}{}", render_report(&report, dry_run));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn outreach_subject_is_the_first_line_cut_at_a_word() {
        assert_eq!(outreach_subject("Deck by 3?\nThanks"), "Deck by 3?");
        assert_eq!(outreach_subject("\n\n  hi  "), "hi");
        assert_eq!(outreach_subject(""), "Quick note");
        let long = "Can you send me the latest version of the quarterly planning deck before the call";
        let s = outreach_subject(long);
        assert!(s.ends_with('…') && s.chars().count() <= 61, "{s}");
        assert!(long.starts_with(s.trim_end_matches('…')));
    }

    #[test]
    fn autonomy_outreach_parses_and_scratch_flags_need_dry_run() {
        let cli = crate::Cli::try_parse_from([
            "augmentagent", "autonomy", "outreach", "--contact", "jane-doe", "--message", "hi", "--dry-run",
            "--config", "/tmp/x.toml", "--assume-on", "--assume-active",
        ])
        .unwrap();
        match cli.cmd {
            crate::Cmd::Autonomy { op: Op::Outreach { contact, message, dry_run, config, assume_on, assume_active, .. } } => {
                assert_eq!((contact.as_str(), message.as_str()), ("jane-doe", "hi"));
                assert!(dry_run && assume_on && assume_active);
                assert_eq!(config, Some(PathBuf::from("/tmp/x.toml")));
            }
            _ => panic!("not `autonomy outreach`"),
        }
        // A live run always uses the real trust config and switches.
        for flag in ["--assume-on", "--assume-active"] {
            assert!(crate::Cli::try_parse_from([
                "augmentagent", "autonomy", "outreach", "--contact", "jane-doe", "--message", "hi", flag,
            ])
            .is_err());
        }
        assert!(crate::Cli::try_parse_from([
            "augmentagent", "autonomy", "outreach", "--contact", "jane-doe", "--message", "hi", "--config", "/tmp/x.toml",
        ])
        .is_err());
    }
}
