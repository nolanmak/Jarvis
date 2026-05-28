//! `augmentagent loop list|stop` (#212).
//!
//! Cross-surface control of `user_loops` — the sqlite-backed scheduler the
//! Discord `/loop` command writes into (#104, ticked by
//! `augmentagent_approval_discord::loops::LoopScheduler`). When a user asks
//! the bot in plain English "kill the hello world loop", *this* is the
//! table they actually want stopped — the loop is firing from the daemon
//! itself, not from any `claude` CLI process.
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
    }
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
}
