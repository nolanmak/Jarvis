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

/// Result of [`parse_create_args`]: the interval the loop ticks on, the
/// prompt to run each tick, and an optional total runtime before auto-stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLoop {
    pub interval_secs: i64,
    pub prompt: String,
    /// Total runtime in seconds before the scheduler stops the loop. `None`
    /// means run forever (until manually stopped).
    pub duration_secs: Option<i64>,
}

/// Read `<N><unit>` (or `<N> <unit-word>`) at the start of `s`, returning the
/// resolved seconds and how many bytes were consumed. Recognises:
///
/// * `s`, `sec[s]`, `second[s]`
/// * `m`, `min[s]`, `minute[s]`
/// * `h`, `hr[s]`, `hour[s]`
/// * `d`, `day[s]`
///
/// Case-insensitive on the unit word. The numeric portion must be a positive
/// integer.
fn parse_time_at(s: &str) -> Option<(i64, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let n: i64 = s[..i].parse().ok()?;
    if n <= 0 {
        return None;
    }
    // Optional whitespace between number and unit ("5 mins" or "5m").
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let unit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if unit_start == i {
        return None;
    }
    let unit_secs: i64 = match s[unit_start..i].to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    Some((n.checked_mul(unit_secs)?, i))
}

/// Whitespace-bordered, case-insensitive keyword match at `pos` in `bytes`.
fn match_kw(bytes: &[u8], pos: usize, kw: &[u8]) -> bool {
    if pos + kw.len() > bytes.len() {
        return false;
    }
    if !bytes[pos..pos + kw.len()].eq_ignore_ascii_case(kw) {
        return false;
    }
    let before_ok = pos == 0 || bytes[pos - 1].is_ascii_whitespace();
    let after_ok =
        pos + kw.len() == bytes.len() || bytes[pos + kw.len()].is_ascii_whitespace();
    before_ok && after_ok
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Parse the create-loop arguments. Accepts two shapes:
///
/// 1. **Terse:** `<interval> <prompt…>` — e.g. `5m /digest`.
/// 2. **Natural:** `<prompt…> every <N> <unit> [for [the next] <N> <unit>]`
///    — e.g. `say hi every 5 mins for the next 15 mins`.
///
/// The trailing-`for` clause sets an auto-stop deadline (returned as
/// `duration_secs`). Interval-floor / max-active enforcement happens in
/// [`handle_loop_command`], not here.
pub fn parse_create_args(rest: &str) -> Result<ParsedLoop, String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err("usage: `/loop <interval> <prompt>`".to_string());
    }

    // 1) Terse leading-token form.
    let mut leading = rest.splitn(2, char::is_whitespace);
    let first = leading.next().unwrap_or("");
    if let Some(secs) = parse_interval(first) {
        let prompt = leading.next().unwrap_or("").trim();
        if prompt.is_empty() {
            return Err("usage: `/loop <interval> <prompt>`".to_string());
        }
        return Ok(ParsedLoop {
            interval_secs: secs,
            prompt: prompt.to_string(),
            duration_secs: None,
        });
    }

    // 2) Natural form. Find every word-bounded "every" and try each from
    // right to left, so phrasings like "summarise every email every 1h"
    // pick the trailing clause as the cadence.
    let bytes = rest.as_bytes();
    let mut every_positions: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if match_kw(bytes, i, b"every") {
            every_positions.push(i);
            i += 5;
        } else {
            i += 1;
        }
    }

    for &start in every_positions.iter().rev() {
        let after_every = skip_ws(bytes, start + 5);
        // Must have whitespace after "every" before the number.
        if after_every == start + 5 && after_every != bytes.len() {
            continue;
        }
        let Some((interval_secs, consumed)) = parse_time_at(&rest[after_every..]) else {
            continue;
        };
        let mut tail = skip_ws(bytes, after_every + consumed);

        // Optional " for [the next] <N> <unit>" duration clause.
        let mut duration_secs: Option<i64> = None;
        if match_kw(bytes, tail, b"for") {
            let mut k = skip_ws(bytes, tail + 3);
            // Optional "the next" — both words must be present together.
            if match_kw(bytes, k, b"the") {
                let after_the = skip_ws(bytes, k + 3);
                if match_kw(bytes, after_the, b"next") {
                    k = skip_ws(bytes, after_the + 4);
                }
            }
            let Some((dur_secs, dur_consumed)) = parse_time_at(&rest[k..]) else {
                continue;
            };
            duration_secs = Some(dur_secs);
            tail = skip_ws(bytes, k + dur_consumed);
        }

        // The "every" clause (plus optional "for" tail) must extend to the
        // end of input — otherwise we'd be silently dropping user text.
        if tail != bytes.len() {
            continue;
        }
        let prompt = rest[..start].trim_end();
        if prompt.is_empty() {
            return Err("missing prompt before `every`".to_string());
        }
        return Ok(ParsedLoop {
            interval_secs,
            prompt: prompt.to_string(),
            duration_secs,
        });
    }

    Err(
        "couldn't parse interval. use e.g. `30m`, `2h`, `1d`, \
         or `<prompt> every 5 mins`."
            .to_string(),
    )
}

/// Parse + validate create-loop arguments against the minimum-interval floor
/// and the duration-vs-interval invariant. Pure (no store access), so the
/// command layer can compose it and tests can call it directly.
pub fn validate_create_args(
    rest: &str,
    min_interval_secs: i64,
) -> Result<ParsedLoop, String> {
    let parsed = parse_create_args(rest)?;
    if parsed.interval_secs < min_interval_secs {
        return Err(format!(
            "interval too short — minimum is {}.",
            fmt_interval(min_interval_secs)
        ));
    }
    if let Some(dur) = parsed.duration_secs {
        if dur < parsed.interval_secs {
            return Err(format!(
                "duration {} is shorter than interval {} — loop would never fire.",
                fmt_interval(dur),
                fmt_interval(parsed.interval_secs),
            ));
        }
    }
    Ok(parsed)
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
            let parsed = match validate_create_args(rest, MIN_INTERVAL_SECS) {
                Ok(p) => p,
                Err(e) => return e,
            };
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
            let expires_at_ms = parsed
                .duration_secs
                .map(|d| now_millis().saturating_add(d.saturating_mul(1000)));
            match store.create_user_loop(
                owner,
                "discord",
                channel_ref,
                parsed.interval_secs,
                &parsed.prompt,
                expires_at_ms,
            ) {
                Ok(id) => match parsed.duration_secs {
                    Some(dur) => format!(
                        "✅ loop `{id}` created — every {}, auto-stops after {} — I'll run: _{}_\nfirst run within {}.",
                        fmt_interval(parsed.interval_secs),
                        fmt_interval(dur),
                        truncate(&parsed.prompt, 200),
                        fmt_interval(parsed.interval_secs),
                    ),
                    None => format!(
                        "✅ loop `{id}` created — every {} I'll run: _{}_\nfirst run within {}.",
                        fmt_interval(parsed.interval_secs),
                        truncate(&parsed.prompt, 200),
                        fmt_interval(parsed.interval_secs),
                    ),
                },
                Err(e) => format!("⚠️ failed to create loop: {e}"),
            }
        }
    }
}

const USAGE: &str = "**/loop** — scheduled tasks\n\
    • `/loop <interval> <prompt or /slash>` — e.g. `/loop 30m /digest` (min 5m)\n\
    • `/loop <prompt> every <N> <unit>` — e.g. `/loop say hi every 5m`\n\
    • append `for <N> <unit>` to auto-stop — e.g. `… every 5m for the next 15m`\n\
    • `/loop list` — your loops + last status\n\
    • `/loop stop <id>` — stop a loop";

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
        let expiry = match l.expires_at_ms {
            Some(t) => {
                let now = now_millis();
                if t > now {
                    format!(" · stops in {}", fmt_interval((t - now) / 1000))
                } else {
                    " · expired".to_string()
                }
            }
            None => String::new(),
        };
        out.push_str(&format!(
            "• `{}` {} every {}{} — _{}_\n   {}\n",
            l.id,
            status_badge,
            fmt_interval(l.interval_secs),
            expiry,
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
        // Sweep any loops whose `for <duration>` deadline has passed before
        // we look at what's due — an expired loop should never fire again,
        // even if it's also due. The store stops them in one statement and
        // returns the surface info so we can post a one-line notice back.
        match self.store.stop_expired_user_loops(now) {
            Ok(expired) => {
                for (id, _channel, channel_ref) in expired {
                    let body = format!("🛑 loop `{id}` expired (duration reached)");
                    if let Err(e) = self.poster.post_to(&channel_ref, &body).await {
                        warn!(loop_id = %id, "expiry-notice post failed: {e:#}");
                    }
                }
            }
            Err(e) => warn!("stop_expired_user_loops failed: {e:#}"),
        }
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

    // --- parse_create_args (natural-language /loop input) ---

    #[test]
    fn parse_create_args_leading_token() {
        let p = parse_create_args("5m do thing").unwrap();
        assert_eq!(p.interval_secs, 300);
        assert_eq!(p.prompt, "do thing");
        assert_eq!(p.duration_secs, None);
    }

    #[test]
    fn parse_create_args_leading_token_requires_prompt() {
        assert!(parse_create_args("5m").is_err());
        assert!(parse_create_args("5m    ").is_err());
    }

    #[test]
    fn parse_create_args_trailing_every_short_unit() {
        let p = parse_create_args("say hi every 5m").unwrap();
        assert_eq!(p.interval_secs, 300);
        assert_eq!(p.prompt, "say hi");
        assert_eq!(p.duration_secs, None);
    }

    #[test]
    fn parse_create_args_trailing_every_word_unit() {
        for input in [
            "say hi every 5 mins",
            "say hi every 5 minutes",
            "say hi EVERY 5 MINUTES",
            "say hi every 5 min",
            "say hi every 5 minute",
        ] {
            let p = parse_create_args(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(p.interval_secs, 300, "{input}");
            assert_eq!(p.prompt, "say hi", "{input}");
            assert_eq!(p.duration_secs, None, "{input}");
        }
    }

    #[test]
    fn parse_create_args_every_hours_and_days() {
        let p = parse_create_args("digest my inbox every 2 hours").unwrap();
        assert_eq!(p.interval_secs, 7200);
        assert_eq!(p.prompt, "digest my inbox");

        let p = parse_create_args("weekly recap every 1 day").unwrap();
        assert_eq!(p.interval_secs, 86400);
        assert_eq!(p.prompt, "weekly recap");
    }

    #[test]
    fn parse_create_args_users_actual_failing_input() {
        // Verbatim from the Discord error report.
        let p =
            parse_create_args("and say hello world every 5 mins for the next 15 mins").unwrap();
        assert_eq!(p.interval_secs, 300);
        assert_eq!(p.duration_secs, Some(900));
        assert_eq!(p.prompt, "and say hello world");
    }

    #[test]
    fn parse_create_args_for_without_the_next() {
        let p = parse_create_args("ping every 10m for 1h").unwrap();
        assert_eq!(p.interval_secs, 600);
        assert_eq!(p.duration_secs, Some(3600));
        assert_eq!(p.prompt, "ping");
    }

    #[test]
    fn parse_create_args_check_every_pr_rejected() {
        // "every" not followed by a time → no match, should fall through.
        assert!(parse_create_args("check every PR").is_err());
    }

    #[test]
    fn parse_create_args_missing_prompt_before_every() {
        let err = parse_create_args("every 5m").unwrap_err();
        assert!(err.contains("missing prompt"), "got: {err}");
    }

    #[test]
    fn parse_create_args_trailing_garbage_after_for_rejected() {
        // "for 15 mins …with trailing junk" doesn't end cleanly → no match.
        assert!(
            parse_create_args("ping every 5m for 15 mins extra stuff at the end")
                .is_err()
        );
    }

    #[test]
    fn parse_create_args_rightmost_every_wins() {
        // The trailing "every 1h" is the cadence; the earlier "every email"
        // is just prompt text.
        let p = parse_create_args("triage every email every 1h").unwrap();
        assert_eq!(p.interval_secs, 3600);
        assert_eq!(p.prompt, "triage every email");
    }

    #[test]
    fn parse_create_args_below_min_interval_still_parses() {
        // Floor check happens in handle_loop_command, not the parser.
        let p = parse_create_args("ping every 30s").unwrap();
        assert_eq!(p.interval_secs, 30);
    }

    // --- validate_create_args (parser + floor + duration-vs-interval guard) ---

    #[test]
    fn validate_accepts_users_actual_failing_input() {
        // The exact input that surfaced the bug in the original report.
        let p = validate_create_args(
            "and say hello world every 5 mins for the next 15 mins",
            MIN_INTERVAL_SECS,
        )
        .unwrap();
        assert_eq!(p.interval_secs, 300);
        assert_eq!(p.duration_secs, Some(900));
        assert_eq!(p.prompt, "and say hello world");
    }

    #[test]
    fn validate_rejects_duration_shorter_than_interval() {
        let err = validate_create_args("ping every 10m for 1m", MIN_INTERVAL_SECS).unwrap_err();
        assert!(err.contains("shorter than interval"), "err: {err}");
    }

    #[test]
    fn validate_rejects_below_min_interval() {
        let err = validate_create_args("ping every 30s", MIN_INTERVAL_SECS).unwrap_err();
        assert!(err.contains("too short"), "err: {err}");
    }

    #[test]
    fn validate_accepts_duration_equal_to_interval() {
        // Edge case: dur == interval fires exactly once before expiry sweep.
        let p = validate_create_args("ping every 5m for 5m", MIN_INTERVAL_SECS).unwrap();
        assert_eq!(p.interval_secs, 300);
        assert_eq!(p.duration_secs, Some(300));
    }
}
