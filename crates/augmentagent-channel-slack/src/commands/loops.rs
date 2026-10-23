//! `loop` on Slack: the same `user_loops` registry and [`LoopScheduler`] as
//! Discord's `/loop`, with the result posted to the Slack conversation the
//! loop was created in.
//!
//! * A Slack loop's `owner` is `slack|<account>|<user>` and its `channel` is
//!   `slack`; its `channel_ref` is `slack:` + the destination
//!   [`SurfaceConversationRef`] as JSON ([`slack_loop_ref`]). Discord refs
//!   are bare channel IDs, so the two can never be confused.
//! * [`SlackLoopPoster`] queues a result on the durable Slack outbox (keyed
//!   `turn:loop:<uuid>`), which the interactive surface's sender delivers.
//!   [`SurfaceLoopPoster`] routes each loop to its surface, and
//!   [`SurfaceGatedRunner`] refuses to run a loop whose surface this daemon
//!   does not serve, so no provider call is made for output nobody can
//!   receive.
//! * Create parses with Discord's deterministic grammar first and falls back
//!   to the model-backed parser for free-form phrasing (`remind me each
//!   morning …`). Reminders (`nag_until_ack`) are closed with `loop ack` or
//!   `loop dismiss`, the text form of Discord's buttons.
//!
//! [`LoopScheduler`]: augmentagent_approval_discord::LoopScheduler

use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::{
    max_active_per_user, min_interval_secs, parse_create_args, validate_parsed, LoopPoster,
    LoopRunner, ParsedLoop,
};
use augmentagent_store::{Store, SurfaceConversationRef, SurfaceOwnerRef, UserLoop};

use super::{models, usage_of, CommandContext, SlackCommandDeps};
use crate::delivery::{enqueue_answer, Answer, PlanOptions};
use crate::surface::SlackWorkspace;

pub const SLACK_LOOP_OWNER_PREFIX: &str = "slack|";
pub const SLACK_LOOP_REF_PREFIX: &str = "slack:";

/// The loop owner string for a Slack owner.
pub fn slack_loop_owner(owner: &SurfaceOwnerRef) -> String {
    format!(
        "{SLACK_LOOP_OWNER_PREFIX}{}|{}",
        owner.account().account_id(),
        owner.sender_id()
    )
}

/// The `channel_ref` of a loop posting to `conversation`.
pub fn slack_loop_ref(conversation: &SurfaceConversationRef) -> String {
    format!(
        "{SLACK_LOOP_REF_PREFIX}{}",
        serde_json::to_string(conversation).expect("a conversation ref serializes")
    )
}

/// Inverse of [`slack_loop_ref`]; `None` for anything else (a Discord ID).
pub fn parse_slack_loop_ref(channel_ref: &str) -> Option<SurfaceConversationRef> {
    let json = channel_ref.strip_prefix(SLACK_LOOP_REF_PREFIX)?;
    let conversation: SurfaceConversationRef = serde_json::from_str(json).ok()?;
    SlackWorkspace::from_account(conversation.account()).ok()?;
    Some(conversation)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Posts loop results to their Slack conversation through the outbox.
pub struct SlackLoopPoster {
    store: Arc<Store>,
}

impl SlackLoopPoster {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn enqueue(&self, channel_ref: &str, body: &str) -> anyhow::Result<()> {
        let conversation = parse_slack_loop_ref(channel_ref)
            .ok_or_else(|| anyhow::anyhow!("not a Slack loop destination: {channel_ref}"))?;
        let turn_id = format!("loop:{}", uuid::Uuid::new_v4());
        enqueue_answer(
            &self.store,
            &conversation,
            &Answer {
                turn_id: &turn_id,
                markdown: body,
                files: &[],
            },
            &PlanOptions::default(),
            now_ms(),
        )?;
        Ok(())
    }
}

#[async_trait]
impl LoopPoster for SlackLoopPoster {
    async fn post_to(&self, channel_ref: &str, body: &str) -> anyhow::Result<()> {
        self.enqueue(channel_ref, body)
    }

    /// Slack reminders carry the text form of Discord's Acknowledge /
    /// Dismiss buttons.
    async fn post_reminder(
        &self,
        channel_ref: &str,
        body: &str,
        loop_id: &str,
        _cycle_ms: i64,
    ) -> anyhow::Result<()> {
        self.enqueue(
            channel_ref,
            &format!(
                "{body}\n\nReply `loop ack {loop_id}` when it's done, or `loop dismiss {loop_id}` \
                 to be reminded next cycle."
            ),
        )
    }
}

/// Routes each loop's output to its surface: Slack refs to `slack`, every
/// other ref (a Discord channel ID) to `other`. A surface this daemon does
/// not serve is an error the scheduler records on the loop.
pub struct SurfaceLoopPoster {
    pub slack: Option<Arc<dyn LoopPoster>>,
    pub other: Option<Arc<dyn LoopPoster>>,
}

impl SurfaceLoopPoster {
    fn route(&self, channel_ref: &str) -> anyhow::Result<&Arc<dyn LoopPoster>> {
        if channel_ref.starts_with(SLACK_LOOP_REF_PREFIX) {
            self.slack
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Slack is not configured in this daemon"))
        } else {
            self.other
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Discord is not configured in this daemon"))
        }
    }
}

#[async_trait]
impl LoopPoster for SurfaceLoopPoster {
    async fn post_to(&self, channel_ref: &str, body: &str) -> anyhow::Result<()> {
        self.route(channel_ref)?.post_to(channel_ref, body).await
    }

    async fn post_reminder(
        &self,
        channel_ref: &str,
        body: &str,
        loop_id: &str,
        cycle_ms: i64,
    ) -> anyhow::Result<()> {
        self.route(channel_ref)?
            .post_reminder(channel_ref, body, loop_id, cycle_ms)
            .await
    }
}

/// Runs a loop only if its surface is served by this daemon (Slack loops
/// are owned by `slack|…`, everything else is Discord's).
pub struct SurfaceGatedRunner {
    pub inner: Arc<dyn LoopRunner>,
    pub slack: bool,
    pub discord: bool,
}

#[async_trait]
impl LoopRunner for SurfaceGatedRunner {
    async fn run_prompt(
        &self,
        request_id: &str,
        owner: &str,
        prompt: &str,
        model_profile: Option<&str>,
    ) -> anyhow::Result<String> {
        let slack = owner.starts_with(SLACK_LOOP_OWNER_PREFIX);
        if slack && !self.slack {
            anyhow::bail!("Slack is not configured in this daemon; the loop was not run");
        }
        if !slack && !self.discord {
            anyhow::bail!("Discord is not configured in this daemon; the loop was not run");
        }
        self.inner
            .run_prompt(request_id, owner, prompt, model_profile)
            .await
    }
}

fn fmt_interval(secs: i64) -> String {
    if secs > 0 && secs % 86400 == 0 {
        format!("{}d", secs / 86400)
    } else if secs > 0 && secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs > 0 && secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn cadence(l: &ParsedLoop) -> String {
    match (&l.cron_expr, &l.tz) {
        (Some(cron), Some(tz)) => format!("on `{cron}` ({tz})"),
        _ => format!("every {}", fmt_interval(l.interval_secs)),
    }
}

/// `N loops (M active)` for `status`.
pub(super) fn summary(store: &Store, owner: &SurfaceOwnerRef) -> String {
    match store.list_user_loops(&slack_loop_owner(owner)) {
        Ok(loops) => {
            let active = loops.iter().filter(|l| l.status == "active").count();
            format!("{} ({active} active) — `loop list`", loops.len())
        }
        Err(e) => format!("unavailable ({e})"),
    }
}

pub(super) async fn command(
    store: &Store,
    deps: &SlackCommandDeps,
    cx: &CommandContext<'_>,
    args: &str,
) -> String {
    let owner = slack_loop_owner(cx.owner);
    let (sub, rest) = match args.trim().split_once(char::is_whitespace) {
        Some((s, r)) => (s.to_ascii_lowercase(), r.trim()),
        None => (args.trim().to_ascii_lowercase(), ""),
    };
    let needs_id = matches!(
        sub.as_str(),
        "stop" | "delete" | "remove" | "pause" | "resume" | "ack" | "acknowledge" | "dismiss"
    );
    if needs_id && (rest.is_empty() || rest.contains(char::is_whitespace)) {
        return usage_of("loop");
    }
    let id = rest;
    match sub.as_str() {
        "" | "help" => usage_of("loop"),
        "list" if rest.is_empty() => list(store, &owner),
        "stop" | "delete" | "remove" => match store.stop_user_loop(&owner, id) {
            Ok(true) => format!("🛑 stopped loop `{id}`"),
            Ok(false) => format!("no active loop `{id}` owned by you (already stopped?)"),
            Err(e) => format!("⚠️ failed to stop loop: {e}"),
        },
        "pause" => match store.pause_user_loop(&owner, id) {
            Ok(true) => format!("⏸️ paused loop `{id}`. `loop resume {id}` starts it again."),
            Ok(false) => format!("No active loop `{id}` of yours to pause."),
            Err(e) => format!("⚠️ failed to pause loop: {e}"),
        },
        "resume" => match store.resume_user_loop(&owner, id) {
            Ok(true) => format!("▶️ resumed loop `{id}`."),
            Ok(false) => format!("No paused loop `{id}` of yours to resume."),
            Err(e) => format!("⚠️ failed to resume loop: {e}"),
        },
        "ack" | "acknowledge" | "dismiss" => close_reminder(store, &owner, id, sub == "dismiss"),
        _ => create(store, deps, cx, &owner, args.trim()).await,
    }
}

fn close_reminder(store: &Store, owner: &str, id: &str, dismiss: bool) -> String {
    let row = match store.list_user_loops(owner) {
        Ok(loops) => loops.into_iter().find(|l| l.id == id),
        Err(e) => return format!("⚠️ failed to read loops: {e}"),
    };
    let Some(row) = row else {
        return format!("No loop `{id}` of yours.");
    };
    let Some(cycle) = row.nag_cycle_ms else {
        return format!("Loop `{id}` has no open reminder.");
    };
    match store.acknowledge_nag_cycle(id, cycle) {
        Ok(0) => "Already handled.".into(),
        Ok(_) if dismiss => "💤 dismissed — I'll remind you next cycle.".into(),
        Ok(_) => "✅ acknowledged — done for now.".into(),
        Err(e) => format!("⚠️ failed to close the reminder: {e}"),
    }
}

fn list(store: &Store, owner: &str) -> String {
    let loops = match store.list_user_loops(owner) {
        Ok(l) => l,
        Err(e) => return format!("⚠️ failed to list loops: {e}"),
    };
    if loops.is_empty() {
        return "You have no loops. Create one: `loop 1h check my inbox`.".into();
    }
    let mut out = String::from("*Your loops*\n");
    for l in &loops {
        out.push_str(&render(l));
    }
    out
}

fn render(l: &UserLoop) -> String {
    let badge = match l.status.as_str() {
        "active" if l.nag_cycle_ms.is_some() => format!("🔔 reminder open (`loop ack {}`)", l.id),
        "active" => "🟢".into(),
        "paused" => "⏸️ paused".into(),
        other => other.to_string(),
    };
    let cadence = match (&l.cron_expr, &l.tz) {
        (Some(cron), Some(tz)) => format!("on `{cron}` ({tz})"),
        _ => format!("every {}", fmt_interval(l.interval_secs)),
    };
    let last = match (l.last_run_ms, l.last_status.as_deref()) {
        (Some(_), Some(s)) => truncate(s, 80),
        _ => "not run yet".into(),
    };
    let surface = if l.channel == "slack" {
        ""
    } else {
        " · posts to Discord"
    };
    format!(
        "• `{}` {badge} {cadence} · model {}{surface} — _{}_\n   last: {last}\n",
        l.id,
        l.model_profile.as_deref().unwrap_or("default"),
        truncate(&l.prompt, 120),
    )
}

async fn create(
    store: &Store,
    deps: &SlackCommandDeps,
    cx: &CommandContext<'_>,
    owner: &str,
    raw: &str,
) -> String {
    let model = models::explicit_choice(deps, cx.conversation).map(|k| k.name());
    let parsed = match parse_create_args(raw) {
        Ok(p) => p,
        Err(grammar) => match &deps.loop_parser {
            Some(parser) => match parser.parse(raw, model).await {
                Ok(p) => p,
                Err(e) => return e,
            },
            None => return format!("{grammar}\n{}", usage_of("loop")),
        },
    };
    if let Err(e) = validate_parsed(&parsed, min_interval_secs()) {
        return e;
    }
    let cap = max_active_per_user();
    match store.count_active_user_loops(owner) {
        Ok(n) if n >= cap => {
            return format!(
                "you already have {cap} active loops (the max). stop one with `loop stop <id>` \
                 first."
            )
        }
        Ok(_) => {}
        Err(e) => return format!("⚠️ failed to check loop count: {e}"),
    }
    // Discord's budget: one interval of grace so the last iteration runs.
    let expires_at_ms = parsed.duration_secs.map(|d| {
        cx.now_ms
            .saturating_add(d.saturating_add(parsed.interval_secs).saturating_mul(1000))
    });
    let id = match store.create_user_loop_with_model(
        owner,
        "slack",
        &slack_loop_ref(cx.conversation),
        parsed.interval_secs,
        &parsed.prompt,
        expires_at_ms,
        parsed.cron_expr.as_deref(),
        parsed.tz.as_deref(),
        model,
        parsed.nag_until_ack,
    ) {
        Ok(id) => id,
        Err(e) => return format!("⚠️ failed to create loop: {e}"),
    };
    let stops = parsed
        .duration_secs
        .map(|d| format!(", auto-stops after {}", fmt_interval(d)))
        .unwrap_or_default();
    let nag = if parsed.nag_until_ack {
        format!(" It re-posts daily until you `loop ack {id}` or `loop dismiss {id}`.")
    } else {
        String::new()
    };
    format!(
        "✅ loop `{id}` created — {}{stops} I'll run: _{}_ (model {}).\nResults post here.{nag} \
         `loop pause {id}` / `loop stop {id}` to manage it.",
        cadence(&parsed),
        truncate(&parsed.prompt, 200),
        model.unwrap_or("default"),
    )
}
