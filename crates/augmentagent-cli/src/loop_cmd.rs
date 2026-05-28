//! `augmentagent loop list|stop|create` (#212, #221).
//!
//! Cross-surface control of `user_loops` — the sqlite-backed scheduler the
//! Discord `/loop` command writes into (#104, ticked by
//! `augmentagent_approval_discord::loops::LoopScheduler`). When a user asks
//! the bot in plain English "kill the hello world loop" or "schedule a
//! morning ping", *this* is the table they actually want touched — the
//! loop is firing from the daemon itself, not from any `claude` CLI
//! process.
//!
//! NOT to be confused with `augmentagent loops` (plural, #175): that one
//! signals OS-level `claude` PIDs, for the orphan-Claude-Code-session case
//! (#174). Different table of contents entirely.
//!
//! Module is named `loop_cmd` because `loop` is a Rust keyword.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde::Serialize;

use augmentagent_approval_discord::{
    max_active_per_user, min_interval_secs, normalize_and_validate_cron, parse_interval,
    validate_tz,
};
use augmentagent_store::{rusqlite, Store};

#[derive(Debug, Clone, Subcommand)]
pub enum LoopOp {
    /// List active user-scheduled loops from sqlite. Use this to resolve a
    /// user's natural-language reference ("the hello world one") to a
    /// concrete loop id before calling `stop`.
    List {
        /// Include stopped/paused rows too. By default only active loops
        /// are shown — the common ask.
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Emit one JSON document instead of the human table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Stop a single loop by id, or every active loop with `--all`. Marks
    /// the row `status='stopped'` — the scheduler skips non-active rows on
    /// its next tick (within ~30s).
    Stop {
        /// Loop id to stop (UUID from `loop list`). Required unless `--all`.
        id: Option<String>,
        /// Stop every active loop. Mutually exclusive with passing an id.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Create a new user-scheduled loop. This is the CLI surface the wiki
    /// agent calls when the user asks for a scheduled task in natural
    /// language ("ping me every morning"). Owner + channel default to the
    /// env-configured Discord identity so the agent doesn't have to thread
    /// them through.
    Create {
        /// Fixed-interval cadence — accepts `45s`, `30m`, `2h`, `1d`, or
        /// a bare integer interpreted as minutes (same grammar as
        /// `/loop`). Mutually exclusive with `--cron`.
        #[arg(long, conflicts_with = "cron")]
        interval: Option<String>,
        /// #231 — cron-style cadence, 5 fields (`min hour dom month dow`).
        /// dow is Unix convention (0=Sun..6=Sat). Requires `--tz`. For
        /// non-numeric dow prefer names (`MON`, `MON-FRI`) which both
        /// Unix and cron agree on. Mutually exclusive with `--interval`.
        #[arg(long, requires = "tz", conflicts_with = "interval")]
        cron: Option<String>,
        /// #231 — IANA timezone anchor for `--cron` (e.g.
        /// `America/New_York`). Required iff `--cron` is set.
        #[arg(long, requires = "cron")]
        tz: Option<String>,
        /// Prompt the scheduler runs each tick. Quote multi-word prompts.
        #[arg(long)]
        prompt: String,
        /// Discord channel/DM id to post results back to. Defaults to
        /// `DISCORD_CHANNEL_ID` env var.
        #[arg(long)]
        channel_ref: Option<String>,
        /// Discord user id that owns the loop (counts against the per-user
        /// cap and gates `/loop stop`). Defaults to `DISCORD_ALLOWED_USER_ID`.
        #[arg(long)]
        owner: Option<String>,
        /// Optional auto-stop deadline — accepts the same grammar as
        /// `--interval`. The scheduler grants one interval of grace so the
        /// boundary iteration fires (mirrors `/loop`'s #108 fix). For
        /// cron loops, the grace is one hour (rough lower bound on
        /// expected fire cadence).
        #[arg(long)]
        expires_in: Option<String>,
        /// Emit JSON `{"id":"<uuid>","interval_secs":N,...}` instead of
        /// plain `<uuid>` on stdout. Lets callers parse the loop id without
        /// regex.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// Display-friendly slim view of a UserLoop. The full struct has audit
/// fields the agent/user don't need to see.
#[derive(Debug, Clone, Serialize)]
struct LoopRow {
    id: String,
    status: String,
    prompt: String,
    interval_secs: i64,
    owner: String,
    channel: String,
    last_run_ms: Option<i64>,
    last_status: Option<String>,
    fail_count: i64,
}

pub async fn run(store: Arc<Store>, op: LoopOp) -> Result<()> {
    let code = run_with(&store, op)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Synchronous core, separate from `run` so tests can exercise it without
/// a tokio runtime.
pub fn run_with(store: &Store, op: LoopOp) -> Result<i32> {
    match op {
        LoopOp::List { all, json } => {
            let rows = list_rows(store, all)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "loops": rows }))?
                );
            } else {
                print_table(&rows);
            }
            Ok(0)
        }
        LoopOp::Stop { id, all } => {
            if all {
                if id.is_some() {
                    return Err(anyhow!(
                        "loop stop: pass either <id> or --all, not both"
                    ));
                }
                let stopped = stop_all_active(store)?;
                if stopped.is_empty() {
                    eprintln!("no active loops to stop");
                    return Ok(0);
                }
                for id in &stopped {
                    println!("stopped {}", id);
                }
                println!("stopped {} loop(s)", stopped.len());
                return Ok(0);
            }
            let Some(id) = id else {
                return Err(anyhow!(
                    "usage: augmentagent loop stop <id> | --all"
                ));
            };
            let ok = stop_by_id(store, &id)?;
            if ok {
                println!("stopped {}", id);
                Ok(0)
            } else {
                eprintln!("loop stop: no active row with id `{}`", id);
                Ok(1)
            }
        }
        LoopOp::Create {
            interval,
            cron,
            tz,
            prompt,
            channel_ref,
            owner,
            expires_in,
            json,
        } => {
            let args = CreateArgs {
                interval,
                cron,
                tz,
                prompt,
                channel_ref,
                owner,
                expires_in,
            };
            let resolved = resolve_create_args(args, env_lookup)?;
            let id = store.create_user_loop(
                &resolved.owner,
                "discord",
                &resolved.channel_ref,
                resolved.interval_secs,
                &resolved.prompt,
                resolved.expires_at_ms,
                resolved.cron_expr.as_deref(),
                resolved.tz.as_deref(),
            )?;
            if json {
                let payload = serde_json::json!({
                    "id": id,
                    "interval_secs": resolved.interval_secs,
                    "owner": resolved.owner,
                    "channel_ref": resolved.channel_ref,
                    "expires_at_ms": resolved.expires_at_ms,
                    "cron_expr": resolved.cron_expr,
                    "tz": resolved.tz,
                });
                println!("{}", serde_json::to_string(&payload)?);
            } else {
                println!("{}", id);
            }
            // Soft post-hoc cap check. We don't pre-check (cheap race vs.
            // concurrent /loop in Discord), but if the agent just blew past
            // the cap we surface it on stderr so the operator notices —
            // the row still went in, matching how `/loop` reports it.
            let cap = max_active_per_user();
            if cap < i64::MAX {
                let active = count_active_for(store, &resolved.owner)?;
                if active > cap {
                    eprintln!(
                        "warning: owner `{}` now has {} active loops (cap is {})",
                        resolved.owner, active, cap
                    );
                }
            }
            Ok(0)
        }
    }
}

#[derive(Debug, Clone)]
struct CreateArgs {
    /// `Some` for interval-style; `None` when `cron` is set (clap
    /// enforces exclusivity).
    interval: Option<String>,
    /// `Some` for cron-style; `None` for interval-style.
    cron: Option<String>,
    /// IANA tz; required iff `cron` is `Some` (clap requires this).
    tz: Option<String>,
    prompt: String,
    channel_ref: Option<String>,
    owner: Option<String>,
    expires_in: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCreate {
    /// 0 for cron-based loops; positive integer for interval-based.
    /// Reflects what we persist into `user_loops.interval_secs`.
    interval_secs: i64,
    prompt: String,
    channel_ref: String,
    owner: String,
    expires_at_ms: Option<i64>,
    /// Normalised cron expression (always 6-field with Quartz-converted
    /// dow), or `None` for interval-based loops.
    cron_expr: Option<String>,
    /// Canonical IANA tz, or `None` for interval-based loops.
    tz: Option<String>,
}

/// Live env lookup. Split out as a function pointer so tests can inject a
/// deterministic stub instead of mutating process env (which races with
/// other tests in the same binary).
fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn resolve_create_args<F>(args: CreateArgs, env: F) -> Result<ResolvedCreate>
where
    F: Fn(&str) -> Option<String>,
{
    resolve_create_args_with(args, env, min_interval_secs())
}

/// Floor-injected core. Live callers use `min_interval_secs()`; tests pass
/// an explicit floor so they don't have to mutate process env.
fn resolve_create_args_with<F>(
    args: CreateArgs,
    env: F,
    floor: i64,
) -> Result<ResolvedCreate>
where
    F: Fn(&str) -> Option<String>,
{
    // Cron-style branch (#231). Must supply both --cron and --tz; clap
    // already enforces that (`requires_with`), but we re-check here in
    // case the resolver is called from a path that bypasses clap.
    let (interval_secs, cron_expr, tz_canonical) = match (args.cron.as_deref(), args.tz.as_deref()) {
        (Some(cron), Some(tz)) => {
            let normalized =
                normalize_and_validate_cron(cron).map_err(|e| anyhow!("--cron: {e}"))?;
            let canon = validate_tz(tz).map_err(|e| anyhow!("--tz: {e}"))?;
            (0i64, Some(normalized), Some(canon))
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(anyhow!("--cron and --tz must be passed together"));
        }
        (None, None) => {
            // Interval-style branch — existing logic.
            let raw = args.interval.as_deref().ok_or_else(|| {
                anyhow!("either --interval or (--cron + --tz) is required")
            })?;
            let secs = parse_interval(raw).ok_or_else(|| {
                anyhow!(
                    "--interval: couldn't parse `{}`; use e.g. `30m`, `2h`, `1d`",
                    raw
                )
            })?;
            if floor > 0 && secs < floor {
                return Err(anyhow!(
                    "--interval: {}s is below the configured floor of {}s \
                     (AUGMENTAGENT_LOOP_MIN_INTERVAL_SECS)",
                    secs,
                    floor
                ));
            }
            (secs, None, None)
        }
    };
    let prompt = args.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(anyhow!("--prompt cannot be empty"));
    }
    let owner = args
        .owner
        .or_else(|| env("DISCORD_ALLOWED_USER_ID"))
        .ok_or_else(|| {
            anyhow!(
                "--owner not given and DISCORD_ALLOWED_USER_ID is not set"
            )
        })?;
    let channel_ref = args
        .channel_ref
        .or_else(|| env("DISCORD_CHANNEL_ID"))
        .ok_or_else(|| {
            anyhow!(
                "--channel-ref not given and DISCORD_CHANNEL_ID is not set"
            )
        })?;
    let expires_at_ms = match args.expires_in.as_deref() {
        None => None,
        Some(raw) => {
            let dur = parse_interval(raw).ok_or_else(|| {
                anyhow!(
                    "--expires-in: couldn't parse `{}`; use e.g. `7d`, `2h`",
                    raw
                )
            })?;
            // Cron-based loops use 1 hour as the grace lower-bound (a
            // cron expression's tick cadence isn't a single number we
            // can echo). Interval-based loops use their own interval as
            // grace per `/loop`'s #108 fix so the boundary tick lands
            // before the expiry sweep.
            let grace = if cron_expr.is_some() {
                3600
            } else {
                interval_secs
            };
            if cron_expr.is_none() && dur < interval_secs {
                return Err(anyhow!(
                    "--expires-in: {}s is shorter than --interval {}s — \
                     loop would never fire",
                    dur,
                    interval_secs
                ));
            }
            let total_ms = dur.saturating_add(grace).saturating_mul(1000);
            Some(now_millis().saturating_add(total_ms))
        }
    };
    Ok(ResolvedCreate {
        interval_secs,
        prompt,
        channel_ref,
        owner,
        expires_at_ms,
        cron_expr,
        tz: tz_canonical,
    })
}

fn count_active_for(store: &Store, owner: &str) -> Result<i64> {
    let n = store.with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM user_loops \
              WHERE owner = ?1 AND status = 'active'",
            rusqlite::params![owner],
            |r| r.get::<_, i64>(0),
        )
    })?;
    Ok(n)
}

fn list_rows(store: &Store, all: bool) -> Result<Vec<LoopRow>> {
    let sql = if all {
        "SELECT id, status, prompt, interval_secs, owner, channel, \
                last_run_ms, last_status, fail_count \
           FROM user_loops \
          ORDER BY (status = 'active') DESC, updated_at_ms DESC"
    } else {
        "SELECT id, status, prompt, interval_secs, owner, channel, \
                last_run_ms, last_status, fail_count \
           FROM user_loops \
          WHERE status = 'active' \
          ORDER BY updated_at_ms DESC"
    };
    let rows = store.with_conn(|c| {
        let mut stmt = c.prepare(sql)?;
        let v = stmt
            .query_map([], |r| {
                Ok(LoopRow {
                    id: r.get(0)?,
                    status: r.get(1)?,
                    prompt: r.get(2)?,
                    interval_secs: r.get(3)?,
                    owner: r.get(4)?,
                    channel: r.get(5)?,
                    last_run_ms: r.get(6)?,
                    last_status: r.get(7)?,
                    fail_count: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })?;
    Ok(rows)
}

fn stop_by_id(store: &Store, id: &str) -> Result<bool> {
    let n = store.with_conn(|c| {
        c.execute(
            "UPDATE user_loops SET status='stopped', updated_at_ms=?2 \
              WHERE id=?1 AND status != 'stopped'",
            rusqlite::params![id, now_millis()],
        )
    })?;
    Ok(n == 1)
}

fn stop_all_active(store: &Store) -> Result<Vec<String>> {
    let ids: Vec<String> = store.with_conn(|c| {
        let mut stmt =
            c.prepare("SELECT id FROM user_loops WHERE status='active'")?;
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })?;
    if ids.is_empty() {
        return Ok(ids);
    }
    let now = now_millis();
    store.with_conn(|c| {
        c.execute(
            "UPDATE user_loops SET status='stopped', updated_at_ms=?1 \
              WHERE status='active'",
            rusqlite::params![now],
        )
    })?;
    Ok(ids)
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn print_table(rows: &[LoopRow]) {
    if rows.is_empty() {
        println!("(no loops)");
        return;
    }
    println!(
        "{:<38}  {:<8}  {:>9}  {:<24}  {}",
        "ID", "STATUS", "INTERVAL", "OWNER", "PROMPT"
    );
    for r in rows {
        let prompt = truncate(&r.prompt, 60);
        let owner = truncate(&r.owner, 24);
        println!(
            "{:<38}  {:<8}  {:>8}s  {:<24}  {}",
            r.id, r.status, r.interval_secs, owner, prompt
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::Store;
    use rusqlite::params;
    use tempfile::NamedTempFile;

    fn seed_store() -> (NamedTempFile, Store) {
        let tmp = NamedTempFile::new().unwrap();
        let s = Store::open(tmp.path()).unwrap();
        // Seed two active loops and one stopped one. Direct inserts via
        // `with_conn` so we don't need to plumb the full create_user_loop
        // signature (channel/channel_ref/etc).
        let now = now_millis();
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO user_loops \
                 (id, owner, channel, channel_ref, interval_secs, prompt, \
                  status, fail_count, created_at_ms, updated_at_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8,?8)",
                params![
                    "a-active-1",
                    "u1",
                    "discord",
                    "ch1",
                    300i64,
                    "say hello",
                    "active",
                    now,
                ],
            )?;
            c.execute(
                "INSERT INTO user_loops \
                 (id, owner, channel, channel_ref, interval_secs, prompt, \
                  status, fail_count, created_at_ms, updated_at_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8,?8)",
                params![
                    "b-active-2",
                    "u1",
                    "discord",
                    "ch2",
                    600i64,
                    "do a thing",
                    "active",
                    now,
                ],
            )?;
            c.execute(
                "INSERT INTO user_loops \
                 (id, owner, channel, channel_ref, interval_secs, prompt, \
                  status, fail_count, created_at_ms, updated_at_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8,?8)",
                params![
                    "c-stopped",
                    "u1",
                    "discord",
                    "ch3",
                    300i64,
                    "old",
                    "stopped",
                    now,
                ],
            )?;
            Ok(())
        })
        .unwrap();
        (tmp, s)
    }

    #[test]
    fn list_default_returns_only_active() {
        let (_tmp, s) = seed_store();
        let rows = list_rows(&s, false).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"a-active-1"));
        assert!(ids.contains(&"b-active-2"));
        assert!(!ids.contains(&"c-stopped"));
    }

    #[test]
    fn list_all_includes_stopped() {
        let (_tmp, s) = seed_store();
        let rows = list_rows(&s, true).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"c-stopped"));
    }

    #[test]
    fn stop_by_id_marks_stopped_and_is_idempotent() {
        let (_tmp, s) = seed_store();
        let ok1 = stop_by_id(&s, "a-active-1").unwrap();
        assert!(ok1, "first stop should succeed");
        // Already stopped — should return false, no panic.
        let ok2 = stop_by_id(&s, "a-active-1").unwrap();
        assert!(!ok2, "second stop on already-stopped row should be false");
        // Confirm other active row untouched.
        let rows = list_rows(&s, false).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(!ids.contains(&"a-active-1"));
        assert!(ids.contains(&"b-active-2"));
    }

    #[test]
    fn stop_by_id_unknown_returns_false() {
        let (_tmp, s) = seed_store();
        let ok = stop_by_id(&s, "does-not-exist").unwrap();
        assert!(!ok);
    }

    #[test]
    fn stop_all_active_stops_only_active_rows() {
        let (_tmp, s) = seed_store();
        let stopped = stop_all_active(&s).unwrap();
        assert_eq!(stopped.len(), 2);
        let rows = list_rows(&s, false).unwrap();
        assert!(rows.is_empty(), "no active rows after --all");
        // Stopped row still present.
        let all = list_rows(&s, true).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn run_stop_id_and_all_together_errors() {
        let (_tmp, s) = seed_store();
        let err = run_with(
            &s,
            LoopOp::Stop {
                id: Some("a-active-1".into()),
                all: true,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[test]
    fn run_stop_neither_errors() {
        let (_tmp, s) = seed_store();
        let err = run_with(
            &s,
            LoopOp::Stop {
                id: None,
                all: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("usage"));
    }

    #[test]
    fn run_stop_unknown_id_exits_nonzero() {
        let (_tmp, s) = seed_store();
        let code = run_with(
            &s,
            LoopOp::Stop {
                id: Some("nope".into()),
                all: false,
            },
        )
        .unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn run_list_json_succeeds() {
        let (_tmp, s) = seed_store();
        let code =
            run_with(&s, LoopOp::List { all: false, json: true }).unwrap();
        assert_eq!(code, 0);
    }

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(kk, _)| kk == k)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn resolve_create_args_uses_env_defaults() {
        let args = CreateArgs {
            interval: Some("30m".into()),
            cron: None,
            tz: None,
            prompt: "  ping me  ".into(),
            channel_ref: None,
            owner: None,
            expires_in: None,
        };
        let env = fake_env(&[
            ("DISCORD_ALLOWED_USER_ID", "user-from-env"),
            ("DISCORD_CHANNEL_ID", "chan-from-env"),
        ]);
        let r = resolve_create_args(args, env).unwrap();
        assert_eq!(r.interval_secs, 1800);
        assert_eq!(r.prompt, "ping me");
        assert_eq!(r.owner, "user-from-env");
        assert_eq!(r.channel_ref, "chan-from-env");
        assert!(r.expires_at_ms.is_none());
    }

    #[test]
    fn resolve_create_args_explicit_flags_win() {
        let args = CreateArgs {
            interval: Some("2h".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: Some("override-chan".into()),
            owner: Some("override-owner".into()),
            expires_in: None,
        };
        // Env has different values; explicit flags should win.
        let env = fake_env(&[
            ("DISCORD_ALLOWED_USER_ID", "should-not-be-used"),
            ("DISCORD_CHANNEL_ID", "should-not-be-used"),
        ]);
        let r = resolve_create_args(args, env).unwrap();
        assert_eq!(r.interval_secs, 7200);
        assert_eq!(r.owner, "override-owner");
        assert_eq!(r.channel_ref, "override-chan");
    }

    #[test]
    fn resolve_create_args_rejects_bad_interval() {
        let args = CreateArgs {
            interval: Some("garbage".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("--interval"));
    }

    #[test]
    fn resolve_create_args_rejects_empty_prompt() {
        let args = CreateArgs {
            interval: Some("1h".into()),
            cron: None,
            tz: None,
            prompt: "   ".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("--prompt"));
    }

    #[test]
    fn resolve_create_args_missing_owner_errors() {
        let args = CreateArgs {
            interval: Some("1h".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: Some("c".into()),
            owner: None,
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("--owner"));
    }

    #[test]
    fn resolve_create_args_missing_channel_errors() {
        let args = CreateArgs {
            interval: Some("1h".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: None,
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("--channel-ref"));
    }

    #[test]
    fn resolve_create_args_expires_in_shorter_than_interval_errors() {
        let args = CreateArgs {
            interval: Some("1h".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: Some("5m".into()),
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("shorter than"));
    }

    #[test]
    fn resolve_create_args_floor_rejects_too_short_interval() {
        // 30m, floor 1h → reject.
        let args = CreateArgs {
            interval: Some("30m".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args_with(args, |_| None, 3600).unwrap_err();
        assert!(err.to_string().contains("floor"));
    }

    #[test]
    fn resolve_create_args_floor_zero_allows_anything_positive() {
        // Floor 0 (default) disables the check — short intervals OK.
        let args = CreateArgs {
            interval: Some("45s".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let r = resolve_create_args_with(args, |_| None, 0).unwrap();
        assert_eq!(r.interval_secs, 45);
    }

    #[test]
    fn resolve_create_args_expires_in_grants_grace_interval() {
        let args = CreateArgs {
            interval: Some("1h".into()),
            cron: None,
            tz: None,
            prompt: "hi".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: Some("2h".into()),
        };
        let before = now_millis();
        let r = resolve_create_args(args, |_| None).unwrap();
        let after = now_millis();
        let exp = r.expires_at_ms.expect("expires_at_ms set");
        // 2h + 1h grace = 3h = 10_800_000ms past now.
        let target = 10_800_000i64;
        assert!(exp - before >= target);
        assert!(exp - after <= target + 5_000);
    }

    #[test]
    fn run_create_writes_row_with_explicit_flags() {
        let (_tmp, s) = seed_store();
        // No env mutation — pass everything explicitly so this test is
        // safe to run in parallel with other env-touching tests.
        let code = run_with(
            &s,
            LoopOp::Create {
                interval: Some("30m".into()),
            cron: None,
            tz: None,
                prompt: "say hi".into(),
                channel_ref: Some("test-channel".into()),
                owner: Some("test-owner".into()),
                expires_in: None,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(code, 0);
        // One more active row for test-owner now (the seeded ones use "u1").
        let n = count_active_for(&s, "test-owner").unwrap();
        assert_eq!(n, 1);
        // Confirm the row has the expected shape.
        let row: (String, String, String, i64, String) = s
            .with_conn(|c| {
                c.query_row(
                    "SELECT prompt, channel, channel_ref, interval_secs, status \
                       FROM user_loops WHERE owner = 'test-owner'",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                        ))
                    },
                )
            })
            .unwrap();
        assert_eq!(row.0, "say hi");
        assert_eq!(row.1, "discord");
        assert_eq!(row.2, "test-channel");
        assert_eq!(row.3, 1800);
        assert_eq!(row.4, "active");
    }

    // ----- #231 cron-style scheduling -----

    #[test]
    fn resolve_create_args_cron_path_populates_cron_and_tz() {
        let args = CreateArgs {
            interval: None,
            cron: Some("0 9 * * 1".into()),
            tz: Some("America/New_York".into()),
            prompt: "morning".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let r = resolve_create_args(args, |_| None).unwrap();
        assert_eq!(r.interval_secs, 0, "cron loops persist 0 for interval_secs");
        assert_eq!(r.cron_expr.as_deref(), Some("0 0 9 * * MON"));
        assert_eq!(r.tz.as_deref(), Some("America/New_York"));
    }

    #[test]
    fn resolve_create_args_cron_without_tz_errors() {
        // clap normally blocks this via `requires = "tz"`, but the
        // resolver guards too in case it's called from a non-clap path.
        let args = CreateArgs {
            interval: None,
            cron: Some("0 9 * * 1".into()),
            tz: None,
            prompt: "x".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("--cron and --tz"));
    }

    #[test]
    fn resolve_create_args_neither_interval_nor_cron_errors() {
        let args = CreateArgs {
            interval: None,
            cron: None,
            tz: None,
            prompt: "x".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("either --interval or"));
    }

    #[test]
    fn resolve_create_args_cron_invalid_expr_errors() {
        let args = CreateArgs {
            interval: None,
            cron: Some("not a cron".into()),
            tz: Some("UTC".into()),
            prompt: "x".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("--cron"));
    }

    #[test]
    fn resolve_create_args_cron_invalid_tz_errors() {
        let args = CreateArgs {
            interval: None,
            cron: Some("0 9 * * 1".into()),
            tz: Some("Mars/Olympus_Mons".into()),
            prompt: "x".into(),
            channel_ref: Some("c".into()),
            owner: Some("o".into()),
            expires_in: None,
        };
        let err = resolve_create_args(args, |_| None).unwrap_err();
        assert!(err.to_string().contains("--tz"));
    }
}
