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

/// Floor on loop cadence (seconds). Defaults to `0` — no floor. Set
/// `AUGMENTAGENT_LOOP_MIN_INTERVAL_SECS` to re-enable a minimum if a
/// misbehaving loop ever becomes a real problem.
pub fn min_interval_secs() -> i64 {
    std::env::var("AUGMENTAGENT_LOOP_MIN_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Per-user cap on simultaneously-active loops. Defaults to `i64::MAX` —
/// effectively no cap. Set `AUGMENTAGENT_LOOP_MAX_ACTIVE_PER_USER` to limit.
pub fn max_active_per_user() -> i64 {
    std::env::var("AUGMENTAGENT_LOOP_MAX_ACTIVE_PER_USER")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(i64::MAX)
}

/// Consecutive-failure count at which a loop auto-pauses. Defaults to
/// `i64::MAX` — never auto-pause. Set `AUGMENTAGENT_LOOP_PAUSE_AFTER_FAILURES`
/// to re-enable.
pub fn pause_after_failures() -> i64 {
    std::env::var("AUGMENTAGENT_LOOP_PAUSE_AFTER_FAILURES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(i64::MAX)
}

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

/// Parses free-form `/loop <…>` create-text into a [`ParsedLoop`]. The CLI's
/// concrete impl asks Claude (Haiku, no tools) to extract `{interval, prompt,
/// duration?}` from arbitrary phrasing so we're not at the mercy of a
/// hand-written regex. Unit tests inject a deterministic stub that delegates
/// to the legacy [`parse_create_args`] regex parser — no `claude` spawn.
#[async_trait]
pub trait LoopCommandParser: Send + Sync {
    /// `raw` is the create-args after the `loop`/`/loop` prefix and
    /// subcommand keyword have been stripped — e.g. `every 5m hello world 🙂`.
    /// Returns a user-facing error string on failure (surfaced verbatim in the
    /// channel reply).
    async fn parse(&self, raw: &str) -> Result<ParsedLoop, String>;
}

/// If `text` starts with `/loop` or bare `loop` as a word, return everything
/// after the keyword (still untrimmed). Otherwise `None`.
///
/// "As a word" = followed by whitespace or end-of-input, so `loops are nice`
/// and `looper` do not match.
pub fn match_loop_prefix(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    for prefix in ["/loop", "loop"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Some(rest);
            }
        }
    }
    None
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
#[allow(dead_code)] // used by tests + kept as a deterministic fallback reference
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
#[allow(dead_code)]
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

#[allow(dead_code)]
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Parse the create-loop arguments via a hand-written regex. Now used only by
/// tests (the production path uses an injected [`LoopCommandParser`] backed
/// by Claude); kept here as a deterministic test fixture covering two shapes:
///
/// 1. **Terse:** `<interval> <prompt…>` — e.g. `5m /digest`.
/// 2. **Natural:** `<prompt…> every <N> <unit> [for [the next] <N> <unit>]`
///    — e.g. `say hi every 5 mins for the next 15 mins`.
///
/// The trailing-`for` clause sets an auto-stop deadline (returned as
/// `duration_secs`). Interval-floor / max-active enforcement happens in
/// [`handle_loop_command`], not here.
#[allow(dead_code)]
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

/// Validate an already-parsed loop spec against the minimum-interval floor
/// and the duration-vs-interval invariant. Used by [`handle_loop_command`]
/// after the LLM parser returns, and by [`validate_create_args`] for tests.
///
/// `min_interval_secs <= 0` disables the floor (default behavior — see
/// [`min_interval_secs`]).
pub fn validate_parsed(
    parsed: &ParsedLoop,
    min_interval_secs: i64,
) -> Result<(), String> {
    if parsed.interval_secs <= 0 {
        return Err("interval must be positive".to_string());
    }
    if min_interval_secs > 0 && parsed.interval_secs < min_interval_secs {
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
    Ok(())
}

/// Parse + validate create-loop arguments via the legacy regex parser. Kept
/// for unit tests so the test corpus doesn't need a `claude` spawn. Prod code
/// uses an injected [`LoopCommandParser`] + [`validate_parsed`] instead.
#[allow(dead_code)]
pub fn validate_create_args(
    rest: &str,
    min_interval_secs: i64,
) -> Result<ParsedLoop, String> {
    let parsed = parse_create_args(rest)?;
    validate_parsed(&parsed, min_interval_secs)?;
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
/// the reply text to post. No Discord types here — fully unit-testable with a
/// stub [`LoopCommandParser`].
///
/// `text` may include the leading `loop` or `/loop` keyword — both are
/// accepted via [`match_loop_prefix`]. If neither matches, the input is used
/// as-is (the dispatcher already gated on the keyword).
pub async fn handle_loop_command(
    store: Option<&Store>,
    parser: Option<&dyn LoopCommandParser>,
    owner: &str,
    channel_ref: &str,
    text: &str,
) -> String {
    let Some(store) = store else {
        return "loop registry unavailable (store not wired)".to_string();
    };
    let rest = match_loop_prefix(text).unwrap_or(text).trim();
    let mut parts = rest.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();

    match sub {
        "" | "help" => USAGE.to_string(),
        "list" => match store.list_user_loops(owner) {
            Ok(loops) if loops.is_empty() => {
                "you have no loops. create one: `loop 1h /digest`".to_string()
            }
            Ok(loops) => render_list(&loops),
            Err(e) => format!("⚠️ failed to list loops: {e}"),
        },
        "stop" => {
            let id = parts.next().unwrap_or("").trim();
            if id.is_empty() {
                return "usage: `loop stop <id>` (get ids from `loop list`)".to_string();
            }
            match store.stop_user_loop(owner, id) {
                Ok(true) => format!("🛑 stopped loop `{id}`"),
                Ok(false) => format!("no active loop `{id}` owned by you (already stopped?)"),
                Err(e) => format!("⚠️ failed to stop loop: {e}"),
            }
        }
        _ => {
            let Some(parser) = parser else {
                return "loop parser unavailable (reasoner not wired)".to_string();
            };
            let parsed = match parser.parse(rest).await {
                Ok(p) => p,
                Err(e) => return e,
            };
            if let Err(e) = validate_parsed(&parsed, min_interval_secs()) {
                return e;
            }
            let cap = max_active_per_user();
            match store.count_active_user_loops(owner) {
                Ok(n) if n >= cap => {
                    return format!(
                        "you already have {cap} active loops (the max). \
                         stop one with `loop stop <id>` first."
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

const USAGE: &str = "**loop** — scheduled tasks (leading `/` optional)\n\
    • `loop <interval> <prompt or /slash>` — e.g. `loop 30m /digest`\n\
    • `loop <prompt> every <N> <unit>` — e.g. `loop say hi every 5m`\n\
    • clauses may appear in any order; phrasing is parsed by Claude\n\
    • append `for <N> <unit>` to auto-stop — e.g. `… every 5m for the next 15m`\n\
    • `loop list` — your loops + last status\n\
    • `loop stop <id>` — stop a loop";

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
        let pause_after = pause_after_failures();
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
                        pause_after,
                    );
                    return;
                }
                let _ = self
                    .store
                    .record_user_loop_run(&l.id, true, "ok", pause_after);
            }
            Err(e) => {
                warn!(loop_id = %l.id, "loop prompt failed: {e:#}");
                let _ = self.store.record_user_loop_run(
                    &l.id,
                    false,
                    &truncate(&format!("error: {e}"), 200),
                    pause_after,
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

    #[tokio::test]
    async fn command_help_and_unwired() {
        // Store missing → unavailable message; parser doesn't matter for this path.
        let reply = handle_loop_command(None, None, "u", "c", "/loop list").await;
        assert!(reply.contains("unavailable"), "got: {reply}");
    }

    /// Test parser that delegates to the legacy regex parser — keeps the
    /// async surface of [`handle_loop_command`] testable without spawning
    /// `claude`.
    struct RegexParser;

    #[async_trait]
    impl LoopCommandParser for RegexParser {
        async fn parse(&self, raw: &str) -> Result<ParsedLoop, String> {
            parse_create_args(raw)
        }
    }

    fn tmp_store() -> (Store, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        let store = Store::open(file.path()).expect("open store");
        (store, file)
    }

    #[tokio::test]
    async fn create_path_accepts_bare_loop_prefix() {
        let (store, _file) = tmp_store();
        let parser = RegexParser;
        let reply = handle_loop_command(
            Some(&store),
            Some(&parser as &dyn LoopCommandParser),
            "user-1",
            "chan-1",
            "loop ping every 5m for 15m",
        )
        .await;
        assert!(reply.contains("loop `"), "expected loop-created reply: {reply}");
        assert!(reply.contains("every 5m"), "should report cadence: {reply}");
    }

    #[tokio::test]
    async fn create_path_accepts_slash_loop_prefix() {
        let (store, _file) = tmp_store();
        let parser = RegexParser;
        let reply = handle_loop_command(
            Some(&store),
            Some(&parser as &dyn LoopCommandParser),
            "user-1",
            "chan-1",
            "/loop ping every 10m",
        )
        .await;
        assert!(reply.contains("loop `"), "expected loop-created reply: {reply}");
    }

    #[tokio::test]
    async fn create_path_surfaces_parser_error_verbatim() {
        struct AlwaysErr;
        #[async_trait]
        impl LoopCommandParser for AlwaysErr {
            async fn parse(&self, _raw: &str) -> Result<ParsedLoop, String> {
                Err("nope, couldn't tell what you meant".to_string())
            }
        }
        let (store, _file) = tmp_store();
        let parser = AlwaysErr;
        let reply = handle_loop_command(
            Some(&store),
            Some(&parser as &dyn LoopCommandParser),
            "user-1",
            "chan-1",
            "loop frobnicate",
        )
        .await;
        assert_eq!(reply, "nope, couldn't tell what you meant");
    }

    #[tokio::test]
    async fn create_path_missing_parser_reports_unavailable() {
        let (store, _file) = tmp_store();
        let reply = handle_loop_command(
            Some(&store),
            None,
            "user-1",
            "chan-1",
            "loop frobnicate every 5m",
        )
        .await;
        assert!(reply.contains("parser unavailable"), "got: {reply}");
    }

    #[tokio::test]
    async fn validate_parsed_rejects_zero_interval() {
        let p = ParsedLoop {
            interval_secs: 0,
            prompt: "x".into(),
            duration_secs: None,
        };
        let err = validate_parsed(&p, 0).unwrap_err();
        assert!(err.contains("positive"), "got: {err}");
    }

    // --- match_loop_prefix --------------------------------------------------

    #[test]
    fn match_loop_prefix_accepts_both_forms() {
        assert_eq!(match_loop_prefix("/loop list"), Some(" list"));
        assert_eq!(match_loop_prefix("loop list"), Some(" list"));
        assert_eq!(match_loop_prefix("  /loop  hello"), Some("  hello"));
        assert_eq!(match_loop_prefix("/loop"), Some(""));
        assert_eq!(match_loop_prefix("loop"), Some(""));
    }

    #[test]
    fn match_loop_prefix_rejects_word_continuations() {
        // `loops are nice` and `looper` are NOT loop commands.
        assert_eq!(match_loop_prefix("loops are great"), None);
        assert_eq!(match_loop_prefix("looper"), None);
        assert_eq!(match_loop_prefix("/loops"), None);
        assert_eq!(match_loop_prefix("hello loop"), None);
        assert_eq!(match_loop_prefix(""), None);
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

    /// Tests pin the floor at 5m explicitly so they don't depend on the
    /// (env-driven) prod default, which is `0` (no floor).
    const TEST_MIN_INTERVAL: i64 = 5 * 60;

    #[test]
    fn validate_accepts_users_actual_failing_input() {
        // The exact input that surfaced the bug in the original report.
        let p = validate_create_args(
            "and say hello world every 5 mins for the next 15 mins",
            TEST_MIN_INTERVAL,
        )
        .unwrap();
        assert_eq!(p.interval_secs, 300);
        assert_eq!(p.duration_secs, Some(900));
        assert_eq!(p.prompt, "and say hello world");
    }

    #[test]
    fn validate_rejects_duration_shorter_than_interval() {
        let err = validate_create_args("ping every 10m for 1m", TEST_MIN_INTERVAL).unwrap_err();
        assert!(err.contains("shorter than interval"), "err: {err}");
    }

    #[test]
    fn validate_rejects_below_min_interval() {
        let err = validate_create_args("ping every 30s", TEST_MIN_INTERVAL).unwrap_err();
        assert!(err.contains("too short"), "err: {err}");
    }

    #[test]
    fn validate_floor_disabled_when_zero() {
        // Floor of 0 = no floor (the prod default). Sub-minute intervals
        // parse + validate cleanly.
        let p = validate_create_args("ping every 30s", 0).unwrap();
        assert_eq!(p.interval_secs, 30);
    }

    #[test]
    fn validate_accepts_duration_equal_to_interval() {
        // Edge case: dur == interval fires exactly once before expiry sweep.
        let p = validate_create_args("ping every 5m for 5m", TEST_MIN_INTERVAL).unwrap();
        assert_eq!(p.interval_secs, 300);
        assert_eq!(p.duration_secs, Some(300));
    }
}
