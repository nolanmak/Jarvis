//! The heartbeat loop and one run of it: the daily sweep (#2285).
//!
//! Gate order matters: everything before the model call is free, so a run
//! that is not due, is outside active hours, has no checklist, has hit its
//! notice cap, finds nothing to judge, or finds another process mid-run
//! never spends a token.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::providers::ModelTier;
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_store::{Email, Store};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::checklist;
use crate::config::HeartbeatConfig;
use crate::decision::{self, Pick, Sweep};
use crate::outreach::{Outreach, OutreachResult};
use crate::replies::{ReplyEvidence, StoreReplyEvidence};
use crate::store_ext::{item_state, status, HeartbeatStore, ItemRecord, Outcome};
use crate::sweep::{self, Candidate, Source, Window};

/// How often the loop checks whether a run is due. The cadence itself comes
/// from the persisted runs, so restarts neither double-fire nor reset the
/// clock, and missed days coalesce into one sweep.
pub const TICK: Duration = Duration::from_secs(60);
/// Consecutive errors that post one "heartbeat failing" notice.
pub const FAILURE_ALERT_AFTER: u32 = 3;
pub const DEDUP_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Item bookkeeping outlives the sweep's 14-day lookback with room to spare.
pub const ITEM_RETENTION_MS: i64 = 60 * 24 * 60 * 60 * 1000;
/// A sweep that ended in error is retried after this long, same day.
pub const RETRY_AFTER_MS: i64 = 60 * 60 * 1000;
/// The approval card truncates its reason line here.
pub const MAX_NOTICE_CHARS: usize = 500;

const SYSTEM_PROMPT: &str = include_str!("prompt.md");

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// Bypass the due check and active hours; every other gate still holds.
    pub force: bool,
    /// Call the model but deliver and record nothing.
    pub dry_run: bool,
}

/// One item on a sweep notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SurfacedItem {
    pub key: String,
    #[serde(rename = "ref")]
    pub item_ref: String,
    pub source: Source,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunReport {
    /// `not-due`, `skipped`, `silent`, `sent`, `error`, or `dry-run`.
    pub status: String,
    pub reason: Option<String>,
    /// The model's decision when it was asked: `silent`, `notify`, `invalid`.
    pub decision: Option<String>,
    pub message: Option<String>,
    pub run_id: Option<i64>,
    /// Candidates collected for this sweep (absent when none were looked for).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<usize>,
    /// What the notice surfaced.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<SurfacedItem>,
    /// D16 — what became of each proposed outreach message. Empty (and
    /// absent from the JSON) whenever outreach is off.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outreach: Vec<OutreachResult>,
}

/// Read-only wiki access on the fast tier: the heartbeat checks facts, it
/// never writes, and it runs often enough that cost matters.
pub fn heartbeat_opts(wiki_root: &Path) -> ReasonerOpts {
    heartbeat_opts_for(wiki_root, false)
}

/// D16 — the marker in `prompt.md` after which the outreach contract
/// starts. Everything before it is the system prompt with outreach off.
const OUTREACH_MARKER: &str = "<!-- outreach:";

/// The system prompt, with the outreach contract only when `outreach`.
pub fn system_prompt(outreach: bool) -> &'static str {
    let at = SYSTEM_PROMPT
        .find(OUTREACH_MARKER)
        .unwrap_or(SYSTEM_PROMPT.len());
    let (base, rest) = SYSTEM_PROMPT.split_at(at);
    if !outreach {
        return base;
    }
    // Static so the opts can borrow it; built once.
    static WITH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    WITH.get_or_init(|| {
        let contract = rest.split_once("-->").map_or("", |(_, c)| c);
        format!("{base}\n{}", contract.trim_start_matches('\n'))
    })
}

/// [`heartbeat_opts`], with the outreach contract in the system prompt
/// when `outreach` (D16). Same tools, model and scope either way.
pub fn heartbeat_opts_for(wiki_root: &Path, outreach: bool) -> ReasonerOpts {
    let mut opts = ReasonerOpts::pinned(ModelTier::Fast, system_prompt(outreach));
    opts.allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    opts.add_dirs = vec![wiki_root.to_path_buf()];
    opts
}

impl RunReport {
    fn new(status: &str) -> Self {
        Self {
            status: status.into(),
            reason: None,
            decision: None,
            message: None,
            run_id: None,
            candidates: None,
            items: Vec::new(),
            outreach: Vec::new(),
        }
    }

    fn with_reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Case- and whitespace-insensitive fingerprint, so a reworded-by-spacing
/// repeat still counts as the same notice.
fn message_hash(message: &str) -> String {
    let normalised = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("{:x}", Sha256::digest(normalised.as_bytes()))
}

fn ago(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    match minutes {
        0..=119 => format!("{minutes}m ago"),
        120..=2879 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// `5d` for the notice.
fn age_days(ms: i64) -> String {
    format!("{}d", (ms.max(0) / sweep::DAY_MS).max(1))
}

/// Keep candidate text on one line and short.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

fn notice_email(subject: &str, body: &str, now_ms: i64) -> Email {
    Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: format!("heartbeat:{now_ms}"),
        thread_id: None,
        from: "heartbeat".into(),
        subject: subject.into(),
        body: body.into(),
        date: String::new(),
        account_entity_id: None,
        platform: "heartbeat".into(),
        kind: "heartbeat_notice".into(),
    }
}

/// The notice text: one line per item, with its age and short ref for
/// `heartbeat done|snooze <ref>`. Only whole lines that fit the card's
/// [`MAX_NOTICE_CHARS`] are included; returns the text and how many items
/// made it.
fn compose_notice(items: &[Dated], now_ms: i64) -> (String, usize) {
    let mut out = String::from("Slipped through:");
    let mut fitted = 0;
    for (item, at_ms) in items {
        let line = format!(
            "\n• {} ({}, {})",
            item.note,
            age_days(now_ms - at_ms),
            item.item_ref
        );
        if out.chars().count() + line.chars().count() > MAX_NOTICE_CHARS {
            break;
        }
        out.push_str(&line);
        fitted += 1;
    }
    (out, fitted)
}

/// A notice item and when its underlying message arrived.
type Dated = (SurfacedItem, i64);

/// Why a model call produced no decision.
enum CallError {
    Timeout,
    Reasoner(String),
}

pub struct HeartbeatRunner<R: Reasoner + ?Sized> {
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    reasoner: Arc<R>,
    wiki_root: PathBuf,
    config: HeartbeatConfig,
    clock: Arc<dyn Clock>,
    holder: String,
    /// D16 — set only when outreach is enabled (both switches on).
    outreach: Option<Arc<dyn Outreach>>,
    /// Whether the owner already answered a candidate (#2284 plugs in here).
    replies: Arc<dyn ReplyEvidence>,
}

impl<R: Reasoner + ?Sized + 'static> HeartbeatRunner<R> {
    pub fn new(
        store: Arc<Store>,
        broker: Arc<dyn ApprovalBroker>,
        reasoner: Arc<R>,
        wiki_root: PathBuf,
        config: HeartbeatConfig,
    ) -> Self {
        let replies = Arc::new(StoreReplyEvidence::new(Arc::clone(&store)));
        Self {
            store,
            broker,
            reasoner,
            wiki_root,
            config,
            clock: Arc::new(SystemClock),
            holder: format!("pid:{}", std::process::id()),
            outreach: None,
            replies,
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_holder(mut self, holder: impl Into<String>) -> Self {
        self.holder = holder.into();
        self
    }

    /// D16 — enable proactive outreach through `outreach`. The CLI calls
    /// this only when `AUGMENTAGENT_HEARTBEAT_OUTREACH=on` and the autonomy
    /// master switch is on; without it the heartbeat is unchanged.
    pub fn with_outreach(mut self, outreach: Arc<dyn Outreach>) -> Self {
        self.outreach = Some(outreach);
        self
    }

    /// Replace the store-backed reply check, e.g. with #2284's
    /// cross-channel reply detection.
    pub fn with_reply_evidence(mut self, replies: Arc<dyn ReplyEvidence>) -> Self {
        self.replies = replies;
        self
    }

    pub fn config(&self) -> &HeartbeatConfig {
        &self.config
    }

    /// Tick every [`TICK`] until `shutdown`, running whenever one is due.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        for warning in &self.config.warnings {
            warn!("heartbeat config: {warning}");
        }
        info!(
            sweep_at = %self.config.sweep_at_label(),
            max_items = self.config.max_items,
            active_hours = ?self.config.active_hours.map(|w| w.to_string()),
            "heartbeat runner started"
        );
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = ticker.tick() => {}
            }
            let report = tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                report = self.run_once(RunOptions::default()) => report,
            };
            match report {
                Ok(r) if r.status == "not-due" => continue,
                // A due sweep outside the window waits, unrecorded, for it to open.
                Ok(r) if r.reason.as_deref() == Some("quiet-hours") => continue,
                Ok(r) => {
                    info!(status = %r.status, reason = ?r.reason, items = r.items.len(), "heartbeat run")
                }
                Err(e) => warn!("heartbeat run failed: {e:#}"),
            }
            let now_ms = self.clock.now().timestamp_millis();
            if let Err(e) = self.store.prune_runs(now_ms - RETENTION_MS) {
                warn!("heartbeat prune failed: {e}");
            }
            if let Err(e) = self.store.prune_items(now_ms - ITEM_RETENTION_MS, now_ms) {
                warn!("heartbeat item prune failed: {e}");
            }
        }
    }

    /// Due when today's sweep time has passed and no run since then settled
    /// it; a sweep that errored is retried after [`RETRY_AFTER_MS`].
    fn is_due(&self, now: DateTime<Utc>) -> Result<bool> {
        let now_ms = now.timestamp_millis();
        let slot_ms = self.config.latest_slot(now).timestamp_millis();
        if self.store.last_settled_ms()?.is_some_and(|s| s >= slot_ms) {
            return Ok(false);
        }
        Ok(self
            .store
            .last_attempt_ms()?
            .is_none_or(|a| a < slot_ms || now_ms - a >= RETRY_AFTER_MS))
    }

    /// One pass through the gates and, if they all pass, one model call.
    pub async fn run_once(&self, opts: RunOptions) -> Result<RunReport> {
        let now = self.clock.now();
        let now_ms = now.timestamp_millis();
        if !opts.force {
            if !self.is_due(now)? {
                return Ok(RunReport::new("not-due"));
            }
            if !self.config.is_active_at(now) {
                // Not recorded, so the sweep runs once the window opens.
                return Ok(RunReport::new(status::SKIPPED).with_reason("quiet-hours"));
            }
        }
        let last_attempt = self.store.last_attempt_ms()?;
        if opts.dry_run {
            return self.gated_run(now, last_attempt, opts).await;
        }

        let ttl_ms = self.config.timeout.as_millis() as i64 + 60_000;
        if !self.store.try_claim_lease(&self.holder, now_ms, ttl_ms)? {
            return self.skip(now_ms, "busy");
        }
        let result = async {
            self.store.mark_interrupted(now_ms - ttl_ms, now_ms)?;
            self.gated_run(now, last_attempt, opts).await
        }
        .await;
        if let Err(e) = self.store.release_lease(&self.holder) {
            warn!("heartbeat lease release failed: {e}");
        }
        result
    }

    fn skip(&self, now_ms: i64, reason: &str) -> Result<RunReport> {
        let id = self.store.record_skip(now_ms, reason)?;
        Ok(RunReport {
            run_id: Some(id),
            ..RunReport::new(status::SKIPPED).with_reason(reason)
        })
    }

    /// Everything the sweep may judge today: expired snoozes first (already
    /// chosen by the owner), then the collectors' candidates.
    fn collect(&self, now_ms: i64) -> Result<(Vec<Dated>, Vec<Candidate>)> {
        let snoozed: Vec<Dated> = self
            .store
            .expired_snoozes(now_ms)?
            .into_iter()
            .take(self.config.max_items)
            .map(|item| {
                let note = item
                    .summary
                    .clone()
                    .unwrap_or_else(|| item.item_key.clone());
                (
                    SurfacedItem {
                        key: item.item_key,
                        item_ref: item.item_ref,
                        source: Source::Snoozed,
                        note,
                    },
                    item.first_seen_ms,
                )
            })
            .collect();
        let window = Window { now_ms };
        let mut candidates = sweep::unanswered(&self.store, self.replies.as_ref(), window);
        candidates.extend(sweep::automated(&self.store, window));
        candidates.retain(|c| !snoozed.iter().any(|(s, _)| s.key == c.key));
        Ok((snoozed, candidates))
    }

    /// The free gates, then the call. Dry runs write nothing.
    async fn gated_run(
        &self,
        now: DateTime<Utc>,
        last_attempt: Option<i64>,
        opts: RunOptions,
    ) -> Result<RunReport> {
        let now_ms = now.timestamp_millis();
        let skip = |reason: &str| -> Result<RunReport> {
            if opts.dry_run {
                Ok(RunReport::new(status::SKIPPED).with_reason(reason))
            } else {
                self.skip(now_ms, reason)
            }
        };
        let Some(checklist) = checklist::load(&self.wiki_root) else {
            return skip("empty-checklist");
        };
        if self.store.sent_count_since(now_ms - DEDUP_WINDOW_MS)? >= self.config.daily_cap {
            return skip("cap");
        }
        let (snoozed, candidates) = self.collect(now_ms)?;
        if snoozed.is_empty() && candidates.is_empty() {
            let mut r = skip("no-candidates")?;
            r.candidates = Some(0);
            return Ok(r);
        }
        let room = self.config.max_items.saturating_sub(snoozed.len());

        let run_id = if opts.dry_run {
            None
        } else {
            Some(self.store.start_run(now_ms)?)
        };

        // Snoozes alone fill the notice, or nothing new to judge: no call.
        if room == 0 || candidates.is_empty() {
            let mut report = self
                .deliver(run_id, now_ms, snoozed, &[], opts.dry_run)
                .await?;
            report.candidates = Some(candidates.len());
            if opts.dry_run {
                report.status = "dry-run".into();
            }
            report.run_id = run_id;
            return Ok(report);
        }

        // D16: the roster is the only list of people outreach may name.
        let roster = match &self.outreach {
            Some(outreach) => outreach.roster(now_ms),
            None => Vec::new(),
        };
        let mut prompt = self.build_prompt(now, last_attempt, &checklist, &candidates, room)?;
        prompt.push_str(&crate::outreach::prompt_section(&roster));
        let call = self.call(now_ms, &prompt, !roster.is_empty()).await;
        let ids: Vec<String> = (1..=candidates.len()).map(|i| format!("c{i}")).collect();
        let raw = match &call {
            Ok(raw) => Some(raw.clone()),
            Err(_) => None,
        };

        let mut report = match call {
            Err(CallError::Timeout) => self.fail(run_id, "timeout", "model call timed out").await?,
            Err(CallError::Reasoner(e)) => self.fail(run_id, "reasoner", &e).await?,
            Ok(raw) => match decision::parse_sweep(&raw, &ids, room) {
                Sweep::Invalid(why) => {
                    let mut r = self.fail(run_id, "invalid-output", &why).await?;
                    r.decision = Some("invalid".into());
                    r
                }
                Sweep::Silent => {
                    let mut r = self
                        .deliver(run_id, now_ms, snoozed, &candidates, opts.dry_run)
                        .await?;
                    r.decision = Some("silent".into());
                    r
                }
                Sweep::Items(picks) => {
                    let mut items = snoozed;
                    items.extend(picked(&picks, &candidates));
                    let mut r = self
                        .deliver(run_id, now_ms, items, &candidates, opts.dry_run)
                        .await?;
                    r.decision = Some("notify".into());
                    r
                }
            },
        };
        report.candidates = Some(candidates.len());
        // D16: only a run that produced a valid decision may reach out.
        if let (Some(outreach), Some(raw)) = (&self.outreach, raw.as_deref()) {
            if !roster.is_empty() && matches!(report.decision.as_deref(), Some("silent" | "notify"))
            {
                let slugs: Vec<String> = roster.iter().map(|r| r.slug.clone()).collect();
                let parsed = decision::parse_outreach(raw, &slugs);
                for entry in &parsed.entries {
                    let result = outreach.propose(entry, opts.dry_run, now_ms).await;
                    info!(
                        contact = %result.contact,
                        status = %result.status,
                        reason = ?result.reason,
                        "heartbeat outreach"
                    );
                    report.outreach.push(result);
                }
                for dropped in parsed.dropped {
                    warn!(contact = %dropped.contact, detail = ?dropped.detail, "heartbeat outreach entry dropped");
                    report.outreach.push(dropped);
                }
            }
        }
        if opts.dry_run {
            report.status = "dry-run".into();
        }
        report.run_id = run_id;
        Ok(report)
    }

    async fn call(
        &self,
        now_ms: i64,
        prompt: &str,
        outreach: bool,
    ) -> std::result::Result<String, CallError> {
        let mut opts = heartbeat_opts_for(&self.wiki_root, outreach);
        opts.session_id = Some(format!("heartbeat:{now_ms}"));
        match tokio::time::timeout(self.config.timeout, self.reasoner.call(&opts, prompt)).await {
            Err(_) => Err(CallError::Timeout),
            Ok(Err(e)) => Err(CallError::Reasoner(format!("{e:#}"))),
            Ok(Ok(reply)) => Ok(reply),
        }
    }

    /// Post `items` (if any) as one notice, then record every item: the
    /// surfaced ones as `surfaced`, the rest of `offered` as `offered`.
    /// Nothing is recorded on a dry run or a failed delivery, so the next
    /// sweep sees the same candidates.
    async fn deliver(
        &self,
        run_id: Option<i64>,
        now_ms: i64,
        mut items: Vec<Dated>,
        offered: &[Candidate],
        dry_run: bool,
    ) -> Result<RunReport> {
        let (message, fitted) = compose_notice(&items, now_ms);
        // Items that did not fit the card are left unrecorded, so the next
        // sweep offers them again.
        let unshown: Vec<String> = items
            .split_off(fitted)
            .into_iter()
            .map(|(i, _)| i.key)
            .collect();
        let surfaced: Vec<SurfacedItem> = items.iter().map(|(i, _)| i.clone()).collect();
        let record = |state_of_rest: &str| -> Result<()> {
            let mut rows: Vec<ItemRecord<'_>> = surfaced
                .iter()
                .map(|i| ItemRecord {
                    key: &i.key,
                    source: i.source.as_str(),
                    state: item_state::SURFACED,
                    summary: Some(&i.note),
                })
                .collect();
            rows.extend(
                offered
                    .iter()
                    .filter(|c| {
                        !surfaced.iter().any(|s| s.key == c.key) && !unshown.contains(&c.key)
                    })
                    .map(|c| ItemRecord {
                        key: &c.key,
                        source: c.source.as_str(),
                        state: state_of_rest,
                        summary: None,
                    }),
            );
            self.store.record_items(&rows, now_ms)?;
            Ok(())
        };

        if surfaced.is_empty() {
            if let Some(id) = run_id.filter(|_| !dry_run) {
                record(item_state::OFFERED)?;
                self.finish(
                    id,
                    &Outcome {
                        status: status::SILENT,
                        ..Default::default()
                    },
                )?;
            }
            return Ok(RunReport::new(status::SILENT));
        }

        let hash = message_hash(&message);
        let duplicate = self
            .store
            .delivered_since(&hash, now_ms - DEDUP_WINDOW_MS)?;
        let decided = RunReport {
            message: Some(message.clone()),
            items: surfaced.clone(),
            ..RunReport::new(status::SENT)
        };
        let Some(id) = run_id.filter(|_| !dry_run) else {
            let r = RunReport {
                status: "dry-run".into(),
                ..decided
            };
            return Ok(if duplicate {
                r.with_reason("duplicate")
            } else {
                r
            });
        };
        if duplicate {
            record(item_state::OFFERED)?;
            self.finish(
                id,
                &Outcome {
                    status: status::SKIPPED,
                    reason: Some("duplicate"),
                    message: Some(&message),
                    message_hash: None,
                },
            )?;
            return Ok(RunReport {
                status: status::SKIPPED.into(),
                ..decided
            }
            .with_reason("duplicate"));
        }
        let email = notice_email("Heartbeat: slipped through the cracks", &message, now_ms);
        if let Err(e) = self.broker.post_flag_notice(&email, &message).await {
            let mut r = self.fail(Some(id), "delivery", &e.to_string()).await?;
            r.items = Vec::new();
            return Ok(r);
        }
        record(item_state::OFFERED)?;
        self.finish(
            id,
            &Outcome {
                status: status::SENT,
                reason: None,
                message: Some(&message),
                message_hash: Some(&hash),
            },
        )?;
        Ok(decided)
    }

    /// Record an error and, on the [`FAILURE_ALERT_AFTER`]th in a row, tell
    /// the operator once. Failures are never silent (Hermes' rule).
    async fn fail(&self, run_id: Option<i64>, reason: &str, detail: &str) -> Result<RunReport> {
        let report = RunReport {
            message: Some(detail.into()),
            ..RunReport::new(status::ERROR).with_reason(reason)
        };
        let Some(id) = run_id else {
            return Ok(report);
        };
        warn!(reason, detail, "heartbeat run failed");
        self.finish(
            id,
            &Outcome {
                status: status::ERROR,
                reason: Some(reason),
                message: Some(detail),
                message_hash: None,
            },
        )?;
        if self.store.consecutive_errors()? == FAILURE_ALERT_AFTER {
            let text = format!(
                "Heartbeat failing: {FAILURE_ALERT_AFTER} runs in a row ended in error (last: {reason}: {detail}). \
                 Check `augmentagent heartbeat status`."
            );
            let text: String = text.chars().take(MAX_NOTICE_CHARS).collect();
            let email = notice_email(
                "Heartbeat failing",
                &text,
                self.clock.now().timestamp_millis(),
            );
            if let Err(e) = self.broker.post_flag_notice(&email, &text).await {
                warn!("heartbeat failure alert not delivered: {e}");
            }
        }
        Ok(report)
    }

    fn finish(&self, id: i64, outcome: &Outcome<'_>) -> Result<()> {
        self.store
            .finish_run(id, self.clock.now().timestamp_millis(), outcome)?;
        Ok(())
    }

    fn build_prompt(
        &self,
        now: DateTime<Utc>,
        last_attempt: Option<i64>,
        checklist: &str,
        candidates: &[Candidate],
        room: usize,
    ) -> Result<String> {
        let now_ms = now.timestamp_millis();
        let mut out = format!(
            "Current local time: {}\n",
            self.config.local_time_label(now)
        );
        match last_attempt {
            Some(last) => out.push_str(&format!("Last heartbeat: {}\n", ago(now_ms - last))),
            None => out.push_str("Last heartbeat: never\n"),
        }
        match self.store.last_sent()? {
            Some(run) => out.push_str(&format!(
                "Last notice you sent ({}): {}\n",
                ago(now_ms - run.started_at_ms),
                one_line(&run.message.unwrap_or_default(), 500)
            )),
            None => out.push_str("Last notice you sent: none\n"),
        }
        out.push_str(&format!("Pick at most {room} items.\n"));
        out.push_str(&format!("\n## Candidates ({})\n", candidates.len()));
        for (i, c) in candidates.iter().enumerate() {
            let mut line = format!(
                "- [c{}] {} · {} · {} · from {}",
                i + 1,
                c.source.as_str(),
                c.channel,
                ago(now_ms - c.at_ms),
                one_line(&c.who, 120)
            );
            if !c.title.trim().is_empty() {
                line.push_str(&format!(" · \"{}\"", one_line(&c.title, 160)));
            }
            if !c.snippet.is_empty() {
                line.push_str(&format!(
                    " — {}",
                    one_line(&c.snippet, sweep::SNIPPET_CHARS)
                ));
            }
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(&format!(
            "\n## Checklist ({})\n{}\n",
            checklist::FILE_NAME,
            checklist.trim_end()
        ));
        Ok(out)
    }
}

/// The candidates behind `picks`, in pick order, as notice items.
fn picked(picks: &[Pick], candidates: &[Candidate]) -> Vec<(SurfacedItem, i64)> {
    let by_id: HashMap<String, &Candidate> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (format!("c{}", i + 1), c))
        .collect();
    picks
        .iter()
        .filter_map(|p| {
            let c = by_id.get(&p.id)?;
            Some((
                SurfacedItem {
                    key: c.key.clone(),
                    item_ref: sweep::item_ref(&c.key),
                    source: c.source,
                    note: p.note.clone(),
                },
                c.at_ms,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_approval_discord::ApprovalError;
    use chrono::TimeZone;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    enum Reply {
        Text(&'static str),
        Fail,
        Hang,
    }

    #[derive(Default)]
    struct ScriptedReasoner {
        replies: Mutex<VecDeque<Reply>>,
        prompts: Mutex<Vec<String>>,
        opts: Mutex<Vec<ReasonerOpts>>,
    }

    impl ScriptedReasoner {
        fn with(replies: Vec<Reply>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                ..Default::default()
            })
        }
        fn calls(&self) -> usize {
            self.prompts.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
            self.prompts.lock().unwrap().push(user_message.to_string());
            self.opts.lock().unwrap().push(opts.clone());
            let next = self.replies.lock().unwrap().pop_front();
            match next {
                Some(Reply::Text(t)) => Ok(t.to_string()),
                Some(Reply::Fail) => Err(anyhow::anyhow!("provider exploded")),
                Some(Reply::Hang) => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    unreachable!()
                }
                None => panic!("reasoner called more times than scripted"),
            }
        }
    }

    #[derive(Default)]
    struct RecordingBroker {
        notices: Mutex<Vec<(Email, String)>>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl RecordingBroker {
        fn reasons(&self) -> Vec<String> {
            self.notices
                .lock()
                .unwrap()
                .iter()
                .map(|(_, r)| r.clone())
                .collect()
        }
    }

    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
            Ok(())
        }
        async fn post_flag_notice(&self, email: &Email, reason: &str) -> Result<(), ApprovalError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ApprovalError::NotReady);
            }
            self.notices
                .lock()
                .unwrap()
                .push((email.clone(), reason.to_string()));
            Ok(())
        }
    }

    struct FixedClock(Mutex<DateTime<Utc>>);

    impl FixedClock {
        fn at(t: DateTime<Utc>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(t)))
        }
        fn advance(&self, d: chrono::Duration) {
            let mut t = self.0.lock().unwrap();
            *t += d;
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    const NOON: fn() -> DateTime<Utc> = || Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();

    struct Harness {
        _dir: tempfile::TempDir,
        store: Arc<Store>,
        reasoner: Arc<ScriptedReasoner>,
        broker: Arc<RecordingBroker>,
        clock: Arc<FixedClock>,
        runner: HeartbeatRunner<ScriptedReasoner>,
    }

    fn config() -> HeartbeatConfig {
        HeartbeatConfig {
            enabled: true,
            tz: Some(chrono_tz::UTC),
            timeout: Duration::from_millis(200),
            ..Default::default()
        }
    }

    fn harness_with(
        replies: Vec<Reply>,
        config: HeartbeatConfig,
        checklist: Option<&str>,
    ) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        if let Some(text) = checklist {
            std::fs::write(wiki.join(checklist::FILE_NAME), text).unwrap();
        }
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let reasoner = ScriptedReasoner::with(replies);
        let broker = Arc::new(RecordingBroker::default());
        let clock = FixedClock::at(NOON());
        let runner = HeartbeatRunner::new(
            Arc::clone(&store),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            Arc::clone(&reasoner),
            wiki,
            config,
        )
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
        .with_holder("test");
        Harness {
            _dir: dir,
            store,
            reasoner,
            broker,
            clock,
            runner,
        }
    }

    fn harness(replies: Vec<Reply>) -> Harness {
        harness_with(replies, config(), Some("- tell me if a flight changes\n"))
    }

    const SILENT: Reply = Reply::Text(r#"{"notify": false}"#);
    const PICK_C1: Reply = Reply::Text(
        r#"{"notify": true, "items": [{"id": "c1", "note": "Example Cloud approved your credits; claim them"}]}"#,
    );
    const RUN: RunOptions = RunOptions {
        force: false,
        dry_run: false,
    };
    const FORCE: RunOptions = RunOptions {
        force: true,
        dry_run: false,
    };
    const DAY: i64 = sweep::DAY_MS;
    const HOUR: i64 = 60 * 60 * 1000;

    fn days(n: i64) -> chrono::Duration {
        chrono::Duration::days(n)
    }

    fn now_ms(h: &Harness) -> i64 {
        h.clock.now().timestamp_millis()
    }

    /// An inbound Gmail message `age_ms` old with the given triage.
    fn seed_mail(
        h: &Harness,
        id: &str,
        from: &str,
        subject: &str,
        age_ms: i64,
        triage: Option<&str>,
    ) {
        let at = now_ms(h) - age_ms;
        h.store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, firstSeenAt, \
                     triageResult, platform, kind) VALUES (?1, ?1, ?2, ?3, 'Body text here.', ?4, ?5, 'gmail', 'dm')",
                    params![id, from, subject, at, triage],
                )
            })
            .unwrap();
    }

    /// A direct message from a person, indexed the way the message index does it.
    fn seed_dm(
        h: &Harness,
        id: &str,
        conv: &str,
        sender: &str,
        label: &str,
        age_ms: i64,
        from_me: bool,
    ) {
        let at = now_ms(h) - age_ms;
        h.store
            .with_conn(|c| {
                if !from_me {
                    c.execute(
                        "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, firstSeenAt, \
                         platform, kind) VALUES (?1, ?2, ?3, '', 'Can you send the deck?', ?4, 'imessage', 'dm')",
                        params![id, conv, sender, at],
                    )?;
                }
                c.execute(
                    "INSERT INTO message_index (message_id, platform, conv_kind, conversation_id, sender_handle, \
                     sender_label, counterpart_handle, from_me, ts_ms, has_attachment, extractor_version) \
                     VALUES (?1, 'imessage', 'dm', ?2, ?3, ?4, ?5, ?6, ?7, 0, 1)",
                    params![
                        id,
                        conv,
                        if from_me { "me" } else { sender },
                        label,
                        if from_me { Some(sender) } else { None },
                        from_me,
                        at
                    ],
                )
            })
            .unwrap();
    }

    fn credits_mail(h: &Harness) {
        seed_mail(
            h,
            "m-credits",
            "Example Cloud <no-reply@startups.example.com>",
            "Your Example Cloud credits application was approved",
            10 * DAY,
            Some("skip"),
        );
    }

    fn prompt(h: &Harness, n: usize) -> String {
        h.reasoner.prompts.lock().unwrap()[n].clone()
    }

    use augmentagent_store::rusqlite::params;

    #[tokio::test]
    async fn credits_approved_mail_surfaces_once_and_never_again_after_done() {
        let h = harness(vec![PICK_C1]);
        credits_mail(&h);
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.status, status::SENT, "{report:?}");
        assert!(
            prompt(&h, 0).contains("[c1] automated · gmail · 10d ago"),
            "{}",
            prompt(&h, 0)
        );
        let notices = h.broker.reasons();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].starts_with("Slipped through:"), "{}", notices[0]);
        assert!(notices[0].contains("Example Cloud approved your credits; claim them (10d, "));
        assert_eq!(report.items.len(), 1);
        let key = report.items[0].key.clone();
        assert_eq!(key, "automated:m-credits");

        // Next day: already surfaced, so nothing to judge and no model call.
        h.clock.advance(days(1));
        let next = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            (next.status.as_str(), next.reason.as_deref()),
            (status::SKIPPED, Some("no-candidates"))
        );
        assert_eq!(h.reasoner.calls(), 1);

        // Done is final.
        assert!(h
            .store
            .set_item_state(&key, item_state::DONE, None, now_ms(&h))
            .unwrap());
        h.clock.advance(days(1));
        assert_eq!(
            h.runner.run_once(RUN).await.unwrap().reason.as_deref(),
            Some("no-candidates")
        );
        assert_eq!(h.broker.reasons().len(), 1);
    }

    #[tokio::test]
    async fn a_snoozed_item_comes_back_once_without_a_model_call() {
        let h = harness(vec![PICK_C1]);
        credits_mail(&h);
        let key = h.runner.run_once(RUN).await.unwrap().items[0].key.clone();
        h.store
            .set_item_state(
                &key,
                item_state::SNOOZED,
                Some(now_ms(&h) + 3 * DAY),
                now_ms(&h),
            )
            .unwrap();

        h.clock.advance(days(1));
        assert_eq!(
            h.runner.run_once(RUN).await.unwrap().reason.as_deref(),
            Some("no-candidates")
        );
        h.clock.advance(days(2));
        let back = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(back.status, status::SENT, "{back:?}");
        assert_eq!(back.items[0].source, Source::Snoozed);
        assert!(h.broker.reasons()[1].contains("Example Cloud approved your credits"));
        assert_eq!(
            h.reasoner.calls(),
            1,
            "a snooze is the owner's choice, not the model's"
        );

        h.clock.advance(days(1));
        assert_eq!(
            h.runner.run_once(RUN).await.unwrap().reason.as_deref(),
            Some("no-candidates")
        );
    }

    #[tokio::test]
    async fn an_unanswered_person_surfaces_and_one_answered_on_another_channel_does_not() {
        let h = harness(vec![SILENT]);
        seed_dm(
            &h,
            "dm-a",
            "conv-a",
            "phone:+15550000001",
            "Sample Person A",
            5 * DAY,
            false,
        );
        seed_dm(
            &h,
            "dm-b",
            "conv-b",
            "phone:+15550000002",
            "Sample Person B",
            5 * DAY,
            false,
        );
        // B was answered by email, linked through the people index.
        h.store
            .with_conn(|c| {
                c.execute_batch(
                    "INSERT INTO message_people VALUES ('phone:+15550000002', 'person-b'),
                                                       ('email:b@example.com', 'person-b');",
                )
            })
            .unwrap();
        seed_dm(
            &h,
            "out-b",
            "mail-thread-b",
            "email:b@example.com",
            "",
            4 * DAY,
            true,
        );

        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.candidates, Some(1));
        let p = prompt(&h, 0);
        assert!(
            p.contains("unanswered · imessage · 5d ago · from Sample Person A"),
            "{p}"
        );
        assert!(!p.contains("Sample Person B"), "{p}");
        assert_eq!(
            h.store
                .find_items("unanswered:imessage:conv-a:dm-a")
                .unwrap()[0]
                .state,
            item_state::OFFERED,
            "a silent sweep records what it showed the model"
        );
    }

    #[tokio::test]
    async fn the_reply_check_is_pluggable() {
        struct AlwaysReplied;
        impl ReplyEvidence for AlwaysReplied {
            fn owner_replied_since(&self, _: &crate::replies::InboundRef) -> bool {
                true
            }
        }
        let mut h = harness(vec![]);
        seed_dm(
            &h,
            "dm-a",
            "conv-a",
            "phone:+15550000001",
            "Sample Person A",
            5 * DAY,
            false,
        );
        h.runner = h.runner.with_reply_evidence(Arc::new(AlwaysReplied));
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.reason.as_deref(), Some("no-candidates"));
    }

    #[tokio::test]
    async fn messages_from_the_last_two_days_are_never_candidates() {
        let h = harness(vec![]);
        seed_dm(
            &h,
            "dm-new",
            "conv-n",
            "phone:+15550000003",
            "Sample Person C",
            HOUR,
            false,
        );
        seed_mail(
            &h,
            "m-new",
            "Example Cloud <no-reply@startups.example.com>",
            "Credits approved",
            HOUR,
            Some("skip"),
        );
        seed_mail(
            &h,
            "m-day",
            "Example Bank <alerts@bank.example.com>",
            "Transfer received",
            DAY,
            Some("skip"),
        );
        // A person who wrote 5 days ago and again an hour ago is real-time's job.
        seed_dm(
            &h,
            "dm-old",
            "conv-n",
            "phone:+15550000003",
            "Sample Person C",
            5 * DAY,
            false,
        );
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.reason.as_deref(), Some("no-candidates"));
        assert_eq!(h.reasoner.calls(), 0);
    }

    #[tokio::test]
    async fn trial_renewal_bulk_and_developer_mail_never_reach_the_model() {
        let h = harness(vec![]);
        seed_mail(
            &h,
            "t1",
            "Example App <billing@app.example.com>",
            "Your free trial ends in 3 days",
            5 * DAY,
            Some("skip"),
        );
        seed_mail(
            &h,
            "t2",
            "Example Host <billing@host.example.net>",
            "Subscription renewal notice",
            5 * DAY,
            Some("skip"),
        );
        seed_mail(
            &h,
            "t3",
            "Example Shop <hi@promotion.shop.example.com>",
            "New arrivals",
            5 * DAY,
            Some("skip"),
        );
        seed_mail(
            &h,
            "t4",
            // Built at runtime so the fixture is not an address literal.
            &format!("Sample Dev <notifications@{}>", "github.com"),
            "[repo] PR run failed",
            5 * DAY,
            Some("skip"),
        );
        for i in 0..sweep::BULK_ADDRESS_MIN {
            seed_mail(
                &h,
                &format!("n{i}"),
                "Daily Example <daily@letters.example.org>",
                "Today's issue",
                (3 + i as i64) * DAY,
                Some("skip"),
            );
        }
        // Flagged or replied mail is a person, not skipped automated mail.
        seed_mail(
            &h,
            "t5",
            "Example Cloud <no-reply@startups.example.com>",
            "Credits approved",
            5 * DAY,
            Some("flag"),
        );
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            report.reason.as_deref(),
            Some("no-candidates"),
            "{report:?}"
        );
    }

    #[tokio::test]
    async fn the_prompt_never_mentions_the_approval_backlog() {
        let h = harness(vec![SILENT]);
        h.store
            .with_conn(|c| {
                for i in 0..60 {
                    c.execute(
                        "INSERT INTO actions (id, messageId, fromEmail, subject, status, createdAt, updatedAt) \
                         VALUES (?1, ?1, 'x@example.com', 's', 'pending', 0, 0)",
                        [format!("a{i}")],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        // A full approval queue alone is never a reason to run.
        assert_eq!(
            h.runner.run_once(RUN).await.unwrap().reason.as_deref(),
            Some("no-candidates")
        );
        credits_mail(&h);
        h.clock.advance(days(1));
        h.runner.run_once(RUN).await.unwrap();
        let p = prompt(&h, 0);
        assert!(!p.to_lowercase().contains("approval"), "{p}");
    }

    #[tokio::test]
    async fn at_most_max_items_surface() {
        let mut c = config();
        c.max_items = 2;
        let h = harness_with(
            vec![Reply::Text(
                r#"{"notify": true, "items": [{"id": "c1", "note": "one"}, {"id": "c2", "note": "two"}, {"id": "c3", "note": "three"}]}"#,
            )],
            c,
            Some("- anything\n"),
        );
        for (i, d) in ["a", "b", "c"].iter().enumerate() {
            seed_mail(
                &h,
                &format!("m{i}"),
                &format!("Sender {d} <ops@{d}.example.com>"),
                "Account change",
                (3 + i as i64) * DAY,
                Some("skip"),
            );
        }
        let report = h.runner.run_once(RUN).await.unwrap();
        assert!(prompt(&h, 0).contains("Pick at most 2 items."));
        assert_eq!(report.items.len(), 2);
        let notice = &h.broker.reasons()[0];
        assert!(
            notice.contains("one") && notice.contains("two") && !notice.contains("three"),
            "{notice}"
        );
        // The unpicked third is recorded as offered: never shown again.
        let third = h.store.find_items("automated:m2").unwrap();
        assert_eq!(third[0].state, item_state::OFFERED);
    }

    #[tokio::test]
    async fn five_full_length_notes_fit_and_an_item_that_does_not_fit_is_not_recorded() {
        let note = "n".repeat(decision::MAX_NOTE_CHARS);
        let picks: Vec<String> = (1..=6)
            .map(|i| format!(r#"{{"id": "c{i}", "note": "{i}{}"}}"#, &note[1..]))
            .collect();
        let reply = format!(r#"{{"notify": true, "items": [{}]}}"#, picks.join(","));
        let mut c = config();
        c.max_items = 6;
        let h = harness_with(
            vec![Reply::Text(Box::leak(reply.into_boxed_str()))],
            c,
            Some("- anything\n"),
        );
        for i in 0..6 {
            seed_mail(
                &h,
                &format!("m{i}"),
                &format!("S{i} <ops@s{i}.example.com>"),
                "Account change",
                (3 + i as i64) * DAY,
                Some("skip"),
            );
        }
        let report = h.runner.run_once(RUN).await.unwrap();
        let notice = &h.broker.reasons()[0];
        assert!(notice.chars().count() <= MAX_NOTICE_CHARS, "{notice}");
        assert_eq!(report.items.len(), 5, "five full-length lines fit");
        for item in &report.items {
            assert!(
                notice.contains(&format!("{})", item.item_ref)),
                "whole line kept: {notice}"
            );
        }
        assert!(
            h.store.find_items("automated:m5").unwrap().is_empty(),
            "the item that did not fit is neither surfaced nor offered"
        );
    }

    #[tokio::test]
    async fn an_older_message_in_a_surfaced_conversation_does_not_resurface() {
        let h = harness(vec![PICK_C1]);
        seed_dm(
            &h,
            "dm-1",
            "conv-a",
            "phone:+15550000001",
            "Sample Person A",
            5 * DAY,
            false,
        );
        seed_dm(
            &h,
            "dm-2",
            "conv-a",
            "phone:+15550000001",
            "Sample Person A",
            4 * DAY,
            false,
        );
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.items[0].key, "unanswered:imessage:conv-a:dm-2");
        h.clock.advance(days(1));
        assert_eq!(
            h.runner.run_once(RUN).await.unwrap().reason.as_deref(),
            Some("no-candidates")
        );
    }

    #[tokio::test]
    async fn the_sweep_runs_once_a_day_at_the_sweep_time() {
        let h = harness(vec![SILENT, SILENT]);
        credits_mail(&h);
        // Noon, sweep at 09:00, never run: due.
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SILENT);
        h.clock.advance(chrono::Duration::hours(1));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, "not-due");
        // Tomorrow 08:59: still not due; 09:00: due.
        h.clock
            .advance(chrono::Duration::hours(19) + chrono::Duration::minutes(59));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, "not-due");
        seed_mail(
            &h,
            "m2",
            "Example Bank <alerts@bank.example.com>",
            "Wire received",
            3 * DAY,
            Some("skip"),
        );
        h.clock.advance(chrono::Duration::minutes(1));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SILENT);
        assert_eq!(h.reasoner.calls(), 2);
    }

    #[tokio::test]
    async fn a_failed_sweep_is_retried_an_hour_later_not_tomorrow() {
        let h = harness(vec![Reply::Fail, PICK_C1]);
        credits_mail(&h);
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::ERROR);
        h.clock.advance(chrono::Duration::minutes(59));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, "not-due");
        h.clock.advance(chrono::Duration::minutes(1));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SENT);
    }

    #[tokio::test]
    async fn failed_delivery_records_no_items_and_the_retry_resends() {
        let h = harness(vec![PICK_C1, PICK_C1]);
        credits_mail(&h);
        h.broker
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let first = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            (first.status.as_str(), first.reason.as_deref()),
            (status::ERROR, Some("delivery"))
        );
        assert!(h
            .store
            .find_items("automated:m-credits")
            .unwrap()
            .is_empty());

        h.broker
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        h.clock.advance(chrono::Duration::hours(1));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SENT);
        assert_eq!(
            h.store.find_items("automated:m-credits").unwrap()[0].state,
            item_state::SURFACED
        );
    }

    #[tokio::test]
    async fn quiet_hours_wait_unrecorded_until_the_window_opens() {
        let mut c = config();
        c.active_hours = Some(crate::ActiveHours::parse("13:00-22:00").unwrap());
        let h = harness_with(vec![SILENT, SILENT], c, Some("- anything\n"));
        credits_mail(&h);
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            (report.status.as_str(), report.reason.as_deref()),
            (status::SKIPPED, Some("quiet-hours"))
        );
        assert!(h.store.recent_runs(10).unwrap().is_empty(), "not recorded");
        assert_eq!(h.reasoner.calls(), 0);
        h.clock.advance(chrono::Duration::hours(1));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SILENT);
        // Forcing ignores the window.
        seed_mail(
            &h,
            "m2",
            "Example Bank <alerts@bank.example.com>",
            "Wire received",
            3 * DAY,
            Some("skip"),
        );
        assert_eq!(
            h.runner.run_once(FORCE).await.unwrap().status,
            status::SILENT
        );
    }

    #[tokio::test]
    async fn missing_or_empty_checklist_skips_without_a_call() {
        for checklist in [None, Some("# Heartbeat\n- [ ]\n<!-- todo -->\n")] {
            let h = harness_with(vec![], config(), checklist);
            credits_mail(&h);
            let report = h.runner.run_once(FORCE).await.unwrap();
            assert_eq!(
                report.reason.as_deref(),
                Some("empty-checklist"),
                "{checklist:?}"
            );
            assert_eq!(h.reasoner.calls(), 0);
        }
    }

    #[tokio::test]
    async fn daily_cap_stops_calls_once_reached() {
        let mut c = config();
        c.daily_cap = 1;
        let h = harness_with(vec![PICK_C1], c, Some("- anything\n"));
        credits_mail(&h);
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SENT);
        seed_mail(
            &h,
            "m2",
            "Example Bank <alerts@bank.example.com>",
            "Wire received",
            3 * DAY,
            Some("skip"),
        );
        let capped = h.runner.run_once(FORCE).await.unwrap();
        assert_eq!(
            (capped.status.as_str(), capped.reason.as_deref()),
            (status::SKIPPED, Some("cap"))
        );
        assert_eq!(h.reasoner.calls(), 1);
    }

    #[tokio::test]
    async fn busy_lease_skips_and_an_expired_lease_is_taken_over() {
        let h = harness(vec![SILENT]);
        credits_mail(&h);
        let now = NOON().timestamp_millis();
        assert!(h.store.try_claim_lease("other", now, 60_000).unwrap());
        let busy = h.runner.run_once(FORCE).await.unwrap();
        assert_eq!(
            (busy.status.as_str(), busy.reason.as_deref()),
            (status::SKIPPED, Some("busy"))
        );
        assert_eq!(h.reasoner.calls(), 0);

        h.clock.advance(chrono::Duration::minutes(2));
        assert_eq!(
            h.runner.run_once(FORCE).await.unwrap().status,
            status::SILENT
        );
        assert!(
            h.store.try_claim_lease("next", now + 180_000, 1).unwrap(),
            "lease released after the run"
        );
    }

    #[tokio::test]
    async fn timeout_reasoner_error_and_invalid_output_are_errors() {
        let h = harness(vec![
            Reply::Hang,
            Reply::Fail,
            Reply::Text("Your credits were approved."),
            Reply::Text(r#"{"notify": true, "items": [{"id": "c7", "note": "made up"}]}"#),
        ]);
        credits_mail(&h);
        for expected in ["timeout", "reasoner", "invalid-output", "invalid-output"] {
            let report = h.runner.run_once(FORCE).await.unwrap();
            assert_eq!(
                (report.status.as_str(), report.reason.as_deref()),
                (status::ERROR, Some(expected))
            );
        }
        let alerts = h.broker.reasons();
        assert_eq!(alerts.len(), 1, "only the third-error alert: {alerts:?}");
        assert!(alerts[0].starts_with("Heartbeat failing"));
        assert!(h
            .store
            .find_items("automated:m-credits")
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn third_consecutive_error_alerts_once_and_success_resets() {
        let h = harness(vec![
            Reply::Fail,
            Reply::Fail,
            Reply::Fail,
            Reply::Fail,
            SILENT,
            Reply::Fail,
            Reply::Fail,
            Reply::Fail,
        ]);
        credits_mail(&h);
        for _ in 0..4 {
            h.runner.run_once(FORCE).await.unwrap();
        }
        let alerts = h.broker.reasons();
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert!(alerts[0].starts_with("Heartbeat failing"), "{}", alerts[0]);

        assert_eq!(
            h.runner.run_once(FORCE).await.unwrap().status,
            status::SILENT
        );
        seed_mail(
            &h,
            "m2",
            "Example Bank <alerts@bank.example.com>",
            "Wire received",
            3 * DAY,
            Some("skip"),
        );
        for _ in 0..3 {
            h.runner.run_once(FORCE).await.unwrap();
        }
        assert_eq!(h.broker.reasons().len(), 2);
    }

    #[tokio::test]
    async fn dry_run_calls_the_model_but_delivers_and_records_nothing() {
        let h = harness(vec![PICK_C1]);
        credits_mail(&h);
        let report = h
            .runner
            .run_once(RunOptions {
                force: true,
                dry_run: true,
            })
            .await
            .unwrap();
        assert_eq!(report.status, "dry-run");
        assert_eq!(report.decision.as_deref(), Some("notify"));
        assert!(report.message.as_deref().unwrap().contains("claim them"));
        assert!(h.broker.reasons().is_empty());
        assert!(h.store.recent_runs(10).unwrap().is_empty());
        assert!(h
            .store
            .find_items("automated:m-credits")
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn interrupted_run_from_a_dead_process_becomes_an_error() {
        let h = harness(vec![SILENT]);
        credits_mail(&h);
        let stale = h
            .store
            .start_run(NOON().timestamp_millis() - 3_600_000)
            .unwrap();
        h.runner.run_once(FORCE).await.unwrap();
        let runs = h.store.recent_runs(10).unwrap();
        let row = runs.iter().find(|r| r.id == stale).unwrap();
        assert_eq!(
            (row.status.as_str(), row.reason.as_deref()),
            (status::ERROR, Some("interrupted"))
        );
    }

    #[tokio::test]
    async fn prompt_carries_checklist_time_history_and_candidates() {
        let h = harness(vec![PICK_C1, SILENT]);
        credits_mail(&h);
        h.runner.run_once(RUN).await.unwrap();
        seed_mail(
            &h,
            "m2",
            "Example Bank <alerts@bank.example.com>",
            "Wire received",
            3 * DAY,
            Some("skip"),
        );
        h.clock.advance(days(1));
        h.runner.run_once(RUN).await.unwrap();

        let first = prompt(&h, 0);
        assert!(first.contains("- tell me if a flight changes"), "{first}");
        assert!(first.contains("2026-09-29 12:00 UTC (Tuesday)"), "{first}");
        assert!(first.contains("Last heartbeat: never"), "{first}");
        assert!(
            first.contains("Your Example Cloud credits application was approved"),
            "{first}"
        );
        let second = prompt(&h, 1);
        assert!(second.contains("Last heartbeat: 24h ago"), "{second}");
        assert!(second.contains("claim them"), "last notice: {second}");
        assert!(
            second.contains("Example Bank <alerts@bank.example.com> · \"Wire received\""),
            "{second}"
        );
        assert!(
            !second.contains("credits application"),
            "surfaced items are not offered again"
        );

        let opts = h.reasoner.opts.lock().unwrap();
        assert!(opts[0]
            .session_id
            .as_deref()
            .is_some_and(|s| s.starts_with("heartbeat:")));
    }

    #[test]
    fn heartbeat_opts_are_read_only_fast_and_scoped_to_the_wiki() {
        let opts = heartbeat_opts(Path::new("/srv/wiki"));
        assert_eq!(opts.allowed_tools, ["Read", "Grep", "Glob"]);
        assert_eq!(opts.add_dirs, [PathBuf::from("/srv/wiki")]);
        assert_eq!(
            opts.model.as_deref(),
            Some(augmentagent_channel_core::providers::model_for(
                augmentagent_channel_core::providers::ProviderKind::Claude,
                ModelTier::Fast,
            ))
            .as_deref()
        );
        assert!(opts.settings_json.is_none() && !opts.restrict_env);
        assert!(opts.system_prompt.contains(r#"{"notify": false}"#));
        assert!(opts.system_prompt.contains(r#""items""#));
    }

    #[test]
    fn outreach_off_system_prompt_is_the_base_contract() {
        let off = system_prompt(false);
        assert!(!off.contains("outreach"), "{off}");
        assert_eq!(heartbeat_opts(Path::new("/w")).system_prompt, off);
        let on = system_prompt(true);
        assert!(on.starts_with(off));
        assert!(on.contains(r#""outreach""#) && on.contains("At most 2 messages"));
        assert!(!on.contains("<!--"), "the marker never reaches the model");
        let (a, b) = (
            heartbeat_opts_for(Path::new("/w"), true),
            heartbeat_opts(Path::new("/w")),
        );
        assert_eq!(
            (a.allowed_tools, a.add_dirs, a.model),
            (b.allowed_tools, b.add_dirs, b.model)
        );
    }

    struct Sink {
        roster: Vec<crate::outreach::RosterEntry>,
        proposed: Mutex<Vec<(crate::outreach::OutreachEntry, bool)>>,
    }

    #[async_trait]
    impl crate::outreach::Outreach for Sink {
        fn roster(&self, _: i64) -> Vec<crate::outreach::RosterEntry> {
            self.roster.clone()
        }
        async fn propose(
            &self,
            entry: &crate::outreach::OutreachEntry,
            dry_run: bool,
            _: i64,
        ) -> OutreachResult {
            self.proposed.lock().unwrap().push((entry.clone(), dry_run));
            OutreachResult {
                contact: entry.contact.clone(),
                status: "auto-sent".into(),
                reason: None,
                detail: None,
            }
        }
    }

    fn sink(slugs: &[&str]) -> Arc<Sink> {
        Arc::new(Sink {
            roster: slugs
                .iter()
                .map(|s| crate::outreach::RosterEntry {
                    slug: s.to_string(),
                    display_name: format!("Name of {s}"),
                })
                .collect(),
            proposed: Mutex::new(Vec::new()),
        })
    }

    const WITH_OUTREACH: Reply = Reply::Text(
        r#"{"notify": true, "items": [{"id": "c1", "note": "Credits approved"}], "outreach": [{"contact": "jane-doe", "message": "Running late", "why": "flight moved"}]}"#,
    );

    fn runner_with(h: &Harness, outreach: Arc<Sink>) -> HeartbeatRunner<ScriptedReasoner> {
        HeartbeatRunner::new(
            Arc::clone(&h.store),
            Arc::clone(&h.broker) as Arc<dyn ApprovalBroker>,
            Arc::clone(&h.reasoner),
            h.runner.wiki_root.clone(),
            config(),
        )
        .with_clock(Arc::clone(&h.clock) as Arc<dyn Clock>)
        .with_holder("test")
        .with_outreach(outreach)
    }

    #[tokio::test]
    async fn without_outreach_the_run_is_unchanged() {
        let h = harness(vec![WITH_OUTREACH]);
        credits_mail(&h);
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.status, status::SENT);
        assert!(report.outreach.is_empty());
        assert!(!serde_json::to_string(&report).unwrap().contains("outreach"));
        assert!(!prompt(&h, 0).contains("outreach"));
        assert_eq!(
            h.reasoner.opts.lock().unwrap()[0].system_prompt,
            system_prompt(false)
        );

        // A sink with an empty roster is the same as none.
        let h = harness(vec![WITH_OUTREACH]);
        credits_mail(&h);
        let empty = sink(&[]);
        let report = runner_with(&h, empty.clone()).run_once(RUN).await.unwrap();
        assert!(report.outreach.is_empty());
        assert!(empty.proposed.lock().unwrap().is_empty());
        assert_eq!(
            h.reasoner.opts.lock().unwrap()[0].system_prompt,
            system_prompt(false)
        );
    }

    #[tokio::test]
    async fn valid_entries_reach_the_sink_and_the_notice_still_posts() {
        let h = harness(vec![WITH_OUTREACH, WITH_OUTREACH]);
        credits_mail(&h);
        let s = sink(&["jane-doe"]);
        let runner = runner_with(&h, s.clone());
        let report = runner
            .run_once(RunOptions {
                force: true,
                dry_run: true,
            })
            .await
            .unwrap();
        assert!(
            s.proposed.lock().unwrap()[0].1,
            "a dry run hands the sink dry_run = true"
        );
        assert_eq!(report.outreach.len(), 1);

        let report = runner.run_once(RUN).await.unwrap();
        assert_eq!(report.status, status::SENT);
        assert!(h.broker.reasons()[0].contains("Credits approved"));
        assert_eq!(report.outreach.len(), 1);
        let proposed = s.proposed.lock().unwrap().clone();
        assert_eq!(proposed[1].0.message, "Running late");
        assert!(!proposed[1].1);
        assert!(prompt(&h, 0).contains("- jane-doe — Name of jane-doe"));
        assert_eq!(
            h.reasoner.opts.lock().unwrap()[0].system_prompt,
            system_prompt(true)
        );
    }

    #[tokio::test]
    async fn failed_runs_never_outreach() {
        let h = harness(vec![Reply::Fail]);
        credits_mail(&h);
        let s = sink(&["jane-doe"]);
        let report = runner_with(&h, s.clone()).run_once(FORCE).await.unwrap();
        assert_eq!(report.status, status::ERROR);
        assert!(s.proposed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_loop_exits_on_shutdown() {
        let h = harness(vec![SILENT]);
        credits_mail(&h);
        let shutdown = CancellationToken::new();
        let reasoner = Arc::clone(&h.reasoner);
        let handle = tokio::spawn(h.runner.run(shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reasoner.calls(), 1, "first tick runs immediately");
    }
}
