//! `/loop` — user-defined scheduled tasks (#104).
//!
//! Channel-agnostic registry (Discord is the first surface). A user types a
//! command in the query channel / DM:
//!
//! ```text
//! /loop <interval> <prompt|/slash>   create a loop
//! /loop list                         list this user's loops + last status
//! /loop stop <id>                    stop a loop
//! ```
//!
//! `<interval>` accepts `30m`, `2h`, `1d`, or a bare number of minutes. A
//! minimum-interval floor and a per-user max-active cap are enforced here, in
//! the command layer, so the scheduler stays dumb.
//!
//! The [`LoopScheduler`] ticks every 30s, finds loops whose `last_run +
//! interval` is due, runs the stored prompt through an injected [`LoopRunner`]
//! (the CLI wires the same `claude` reasoner the wiki-ask path uses), and posts
//! the result back to the originating channel/DM via an injected [`LoopPoster`].
//! Repeated failures auto-pause the loop (handled in the store).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use augmentagent_store::{Store, UserLoop};

/// Floor on loop cadence. Tighter than this and a misbehaving loop could
/// hammer the reasoner / spam the channel.
pub const MIN_INTERVAL_SECS: i64 = 5 * 60;
/// Per-user cap on simultaneously-active loops.
pub const MAX_ACTIVE_PER_USER: i64 = 10;
/// Consecutive-failure count at which a loop auto-pauses.
pub const PAUSE_AFTER_FAILURES: i64 = 3;

/// Runs a loop's stored prompt and returns the agent's text answer. The CLI
/// implements this against the same `ClaudeCliReasoner` + `ask_opts` used for
/// wiki queries, so `/loop 1h what changed in my inbox` works exactly like
/// asking the bot directly.
#[async_trait]
pub trait LoopRunner: Send + Sync {
    async fn run_prompt(&self, prompt: &str) -> anyhow::Result<String>;
}

/// Posts a loop's result back to the surface it was created from. Keyed by the
/// loop's `channel_ref` (a Discord channel/DM id, as a string).
#[async_trait]
pub trait LoopPoster: Send + Sync {
    async fn post_to(&self, channel_ref: &str, body: &str) -> anyhow::Result<()>;
}

/// Parse a human interval into seconds. Accepts `45s`, `30m`, `2h`, `1d`, or a
/// bare integer interpreted as minutes. Returns `None` on garbage.
pub fn parse_interval(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (num, unit_secs): (&str, i64) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 3600),
        Some('d') => (&raw[..raw.len() - 1], 86400),
        Some(c) if c.is_ascii_digit() => (raw, 60),
        _ => return None,
    };
    let n: i64 = num.trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    n.checked_mul(unit_secs)
}

fn fmt_interval(secs: i64) -> String {
    if secs % 86400 == 0 {
        format!("{}d", secs / 86400)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Pure command handler. `owner` and `channel_ref` are the Discord user id and
/// channel/DM id (as strings) so the registry stays channel-agnostic. Returns
/// the reply text to post. No Discord types here — fully unit-testable.
pub fn handle_loop_command(
    store: Option<&Store>,
    owner: &str,
    channel_ref: &str,
    text: &str,
) -> String {
    let Some(store) = store else {
        return "loop registry unavailable (store not wired)".to_string();
    };
    let rest = text.trim().strip_prefix("/loop").unwrap_or("").trim();
    let mut parts = rest.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();

    match sub {
        "" | "help" => USAGE.to_string(),
        "list" => match store.list_user_loops(owner) {
            Ok(loops) if loops.is_empty() => {
                "you have no loops. create one: `/loop 1h /digest`".to_string()
            }
            Ok(loops) => render_list(&loops),
            Err(e) => format!("⚠️ failed to list loops: {e}"),
        },
        "stop" => {
            let id = parts.next().unwrap_or("").trim();
            if id.is_empty() {
                return "usage: `/loop stop <id>` (get ids from `/loop list`)".to_string();
            }
            match store.stop_user_loop(owner, id) {
                Ok(true) => format!("🛑 stopped loop `{id}`"),
                Ok(false) => format!("no active loop `{id}` owned by you (already stopped?)"),
                Err(e) => format!("⚠️ failed to stop loop: {e}"),
            }
        }
        _ => {
            let prompt = parts.next().unwrap_or("").trim();
            if prompt.is_empty() {
                return USAGE.to_string();
            }
            let Some(secs) = parse_interval(sub) else {
                return format!("couldn't parse interval `{sub}`. use e.g. `30m`, `2h`, `1d`.");
            };
            if secs < MIN_INTERVAL_SECS {
                return format!(
                    "interval too short — minimum is {}.",
                    fmt_interval(MIN_INTERVAL_SECS)
                );
            }
            match store.count_active_user_loops(owner) {
                Ok(n) if n >= MAX_ACTIVE_PER_USER => {
                    return format!(
                        "you already have {MAX_ACTIVE_PER_USER} active loops (the max). \
                         stop one with `/loop stop <id>` first."
                    );
                }
                Ok(_) => {}
                Err(e) => return format!("⚠️ failed to check loop count: {e}"),
            }
            match store.create_user_loop(owner, "discord", channel_ref, secs, prompt) {
                Ok(id) => format!(
                    "✅ loop `{id}` created — every {} I'll run: _{}_\nfirst run within {}.",
                    fmt_interval(secs),
                    truncate(prompt, 200),
                    fmt_interval(secs),
                ),
                Err(e) => format!("⚠️ failed to create loop: {e}"),
            }
        }
    }
}

const USAGE: &str = "**/loop** — scheduled tasks\n\
    • `/loop <interval> <prompt or /slash>` — create (interval: `30m`, `2h`, `1d`; min 5m)\n\
    • `/loop list` — your loops + last status\n\
    • `/loop stop <id>` — stop a loop\n\
    e.g. `/loop 6h summarize my unread inbox and flag anything urgent`";

fn render_list(loops: &[UserLoop]) -> String {
    let mut out = String::from("**Your loops**\n");
    for l in loops {
        let last = match (l.last_run_ms, l.last_status.as_deref()) {
            (Some(_), Some(s)) => format!("last: {}", truncate(s, 80)),
            _ => "last: (not run yet)".to_string(),
        };
        let status_badge = match l.status.as_str() {
            "active" => "🟢",
            "paused" => "⏸️ (paused after repeated failures)",
            _ => "⚪",
        };
        out.push_str(&format!(
            "• `{}` {} every {} — _{}_\n   {}\n",
            l.id,
            status_badge,
            fmt_interval(l.interval_secs),
            truncate(&l.prompt, 120),
            last,
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// Drives due loops. Ticks every `tick_interval`; for each `active` loop whose
/// `last_run_ms + interval` is in the past (or which has never run), runs the
/// prompt and posts the result back. Failures are recorded so the store can
/// auto-pause after repeated trouble.
pub struct LoopScheduler {
    store: Arc<Store>,
    runner: Arc<dyn LoopRunner>,
    poster: Arc<dyn LoopPoster>,
    tick_interval: Duration,
}

impl LoopScheduler {
    pub fn new(
        store: Arc<Store>,
        runner: Arc<dyn LoopRunner>,
        poster: Arc<dyn LoopPoster>,
    ) -> Self {
        Self {
            store,
            runner,
            poster,
            tick_interval: Duration::from_secs(30),
        }
    }

    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!("loop scheduler online (tick {:?})", self.tick_interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("loop scheduler: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.tick().await {
                        error!("loop scheduler tick failed: {e:#}");
                    }
                }
            }
        }
    }

    async fn tick(&self) -> anyhow::Result<()> {
        let now = now_millis();
        let loops = self.store.list_active_user_loops()?;
        for l in loops {
            let due_at = l
                .last_run_ms
                .map(|t| t + l.interval_secs * 1000)
                .unwrap_or(0);
            if due_at > now {
                continue;
            }
            self.run_one(&l).await;
        }
        Ok(())
    }

    async fn run_one(&self, l: &UserLoop) {
        info!(loop_id = %l.id, "running loop");
        match self.runner.run_prompt(&l.prompt).await {
            Ok(answer) => {
                let header = format!("🔁 loop `{}` · _{}_", l.id, truncate(&l.prompt, 80));
                let body = format!("{header}\n\n{answer}");
                if let Err(e) = self.poster.post_to(&l.channel_ref, &body).await {
                    warn!(loop_id = %l.id, "loop post failed: {e:#}");
                    let _ = self.store.record_user_loop_run(
                        &l.id,
                        false,
                        &format!("post failed: {e}"),
                        PAUSE_AFTER_FAILURES,
                    );
                    return;
                }
                let _ = self
                    .store
                    .record_user_loop_run(&l.id, true, "ok", PAUSE_AFTER_FAILURES);
            }
            Err(e) => {
                warn!(loop_id = %l.id, "loop prompt failed: {e:#}");
                let _ = self.store.record_user_loop_run(
                    &l.id,
                    false,
                    &truncate(&format!("error: {e}"), 200),
                    PAUSE_AFTER_FAILURES,
                );
            }
        }
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_interval("45s"), Some(45));
        assert_eq!(parse_interval("30m"), Some(1800));
        assert_eq!(parse_interval("2h"), Some(7200));
        assert_eq!(parse_interval("1d"), Some(86400));
        assert_eq!(parse_interval("15"), Some(900));
        assert_eq!(parse_interval("0m"), None);
        assert_eq!(parse_interval("-5m"), None);
        assert_eq!(parse_interval("abc"), None);
        assert_eq!(parse_interval(""), None);
    }

    #[test]
    fn interval_formatting_roundtrips() {
        assert_eq!(fmt_interval(86400), "1d");
        assert_eq!(fmt_interval(7200), "2h");
        assert_eq!(fmt_interval(1800), "30m");
        assert_eq!(fmt_interval(45), "45s");
    }

    #[test]
    fn command_help_and_unwired() {
        assert!(handle_loop_command(None, "u", "c", "/loop list").contains("unavailable"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel…");
    }
}
