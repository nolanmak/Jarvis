//! Issue #12 — `augmentagent env <list|get|set|unset>`.
//!
//! Centralises env-config editing so the `/setup` skill never has to parse or
//! rewrite the operator's `.env` file. Writes land in the sqlite `config`
//! table — the same row the dashboard's `getConfig()` reads (`src/db.ts:45`)
//! and the same surface builder-7 already uses for arming flags in
//! `channel_router.rs`. Reads merge config OVER `process.env`: a key set in
//! the sqlite table wins over an environment variable of the same name, which
//! mirrors the dashboard's precedence.
//!
//! Canonical key set comes from `.env.example` at the repo root. We try a
//! runtime read first (so a re-deployed `.env.example` is picked up without a
//! rebuild) and fall back to an `include_str!`-embedded copy so an installed
//! binary running from `/usr/local/bin` outside the repo still reports a
//! useful key list.
//!
//! `set` is intentionally NOT restricted to canonical keys — it warns on
//! stderr and proceeds, so operators can stash new flags before the
//! `.env.example` ships them. `unset` deletes only from sqlite — the OS env
//! and `.env` file are read-only from this command's point of view.

use anyhow::{Context, Result};
use augmentagent_store::rusqlite;
use clap::Subcommand;
use serde_json::json;
use std::collections::BTreeMap;

/// Embedded snapshot of `.env.example` so the binary still has a key list
/// when it runs outside the repo (systemd unit, /usr/local/bin install). The
/// runtime file at `./.env.example` takes precedence when present.
const EMBEDDED_DOTENV_EXAMPLE: &str = include_str!("../../../.env.example");

/// Substrings (case-insensitive) that mark a key as secret for masking.
/// Sourced from the issue spec — kept here as a single source of truth so the
/// dashboard can mirror the same rule when it adopts the API.
pub const SECRET_KEYWORDS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "PASS", "AUTH"];

/// `augmentagent env <op>` — read/write the sqlite `config` table.
///
/// `--json` lives on the top-level `Env` command (not per-op) so the skill
/// can flip output mode once for `list` + `get` without learning per-op
/// flags. `set` and `unset` always emit JSON receipts.
#[derive(Subcommand, Debug)]
pub enum EnvOp {
    /// Print every canonical key from `.env.example` plus any extras in the
    /// sqlite `config` table. Secrets are masked.
    List,
    /// Print the resolved value (raw, unmasked) for one key, or `(unset)`
    /// when neither the config table nor `process.env` has it.
    Get {
        /// The env var name (case-sensitive, e.g. `GROQ_API_KEY`).
        key: String,
    },
    /// Persist `key=value` to the sqlite `config` table. Warns on stderr if
    /// the key is not in `.env.example` but proceeds. The daemon picks up
    /// the change on next restart.
    Set {
        /// The env var name (e.g. `GROQ_API_KEY`).
        key: String,
        /// The value to store. No quoting / escaping applied — written
        /// verbatim, the same way the dashboard would.
        value: String,
    },
    /// Delete `key` from the sqlite `config` table. Does NOT touch the OS
    /// env or the `.env` file — those remain operator-managed.
    Unset {
        /// The env var name to delete from the sqlite `config` table.
        key: String,
    },
}

/// Resolve the sqlite path the same way the rest of the CLI does — matches
/// `main.rs` (`AUGMENTAGENT_DB` env-or-`./data.db`) and `channel_router.rs`.
fn db_path() -> String {
    std::env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string())
}

/// `CREATE TABLE IF NOT EXISTS config (key, value, updatedAt)` — defensive
/// because the dashboard's `src/db.ts` is what normally creates this table
/// and the daemon's `Store::migrate` does not. Mirrors `channel_router.rs`
/// so we agree on schema with builder-7.
fn ensure_config_table(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS config (\
             key TEXT PRIMARY KEY,\
             value TEXT NOT NULL,\
             updatedAt INTEGER NOT NULL\
         )",
        [],
    )
    .context("create config table")?;
    Ok(())
}

/// Snapshot the sqlite `config` table as `{key: value}`. Tolerates a missing
/// table — we create it on demand so a brand-new box (dashboard never
/// started) can still call `env list` without error.
fn read_config_map() -> Result<BTreeMap<String, String>> {
    let conn = rusqlite::Connection::open(db_path())
        .with_context(|| format!("open sqlite at {}", db_path()))?;
    ensure_config_table(&conn)?;
    let mut stmt = conn
        .prepare("SELECT key, value FROM config")
        .context("prepare config SELECT")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("scan config rows")?;
    let mut out = BTreeMap::new();
    for r in rows {
        let (k, v) = r.context("read config row")?;
        out.insert(k, v);
    }
    Ok(out)
}

/// Upsert one row into the sqlite `config` table. Schema matches builder-7's
/// writer in `channel_router.rs` — same `(key, value, updatedAt)` shape, same
/// `ON CONFLICT` upsert.
fn write_config_value(key: &str, value: &str) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path())
        .with_context(|| format!("open sqlite at {}", db_path()))?;
    ensure_config_table(&conn)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO config (key, value, updatedAt) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updatedAt = excluded.updatedAt",
        rusqlite::params![key, value, now_ms],
    )
    .with_context(|| format!("upsert config row {key}"))?;
    Ok(())
}

/// Delete one row from `config`. Idempotent — returns `Ok` even when no row
/// matched (the spec just says "remove the row"; nothing requires erroring
/// on absent keys, and that matches how the dashboard handles deletes).
fn delete_config_value(key: &str) -> Result<u64> {
    let conn = rusqlite::Connection::open(db_path())
        .with_context(|| format!("open sqlite at {}", db_path()))?;
    ensure_config_table(&conn)?;
    let n = conn
        .execute(
            "DELETE FROM config WHERE key = ?",
            rusqlite::params![key],
        )
        .with_context(|| format!("delete config row {key}"))?;
    Ok(n as u64)
}

/// Extract every `KEY=...` line (commented OR uncommented) from one
/// `.env.example` blob. Returns a sorted, deduped Vec of bare key names.
///
/// Rules:
///   - Strip leading `#` and surrounding whitespace.
///   - Accept lines matching `^[A-Z_][A-Z0-9_]*=`.
///   - Drop the `=` and anything after.
///
/// Pure function — split out from `canonical_keys()` so it's trivially unit
/// testable without touching the filesystem.
pub fn parse_dotenv_keys(blob: &str) -> Vec<String> {
    let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for raw in blob.lines() {
        // Strip leading `#` and surrounding whitespace so commented examples
        // (`# AUGMENTAGENT_CARDDAV_URL=`) still register as canonical keys.
        let line = raw.trim_start();
        let body = line.strip_prefix('#').unwrap_or(line).trim();
        let Some(eq_idx) = body.find('=') else {
            continue;
        };
        let key = &body[..eq_idx];
        if key.is_empty() {
            continue;
        }
        // `^[A-Z_][A-Z0-9_]*$` — keeps us from matching arbitrary `foo=bar`
        // shell snippets in comments.
        let first_ok = key
            .chars()
            .next()
            .map(|c| c.is_ascii_uppercase() || c == '_')
            .unwrap_or(false);
        let rest_ok = key
            .chars()
            .skip(1)
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !first_ok || !rest_ok {
            continue;
        }
        out.insert(key.to_string());
    }
    out.into_iter().collect()
}

/// Canonical key set sourced from `.env.example`. Prefers a runtime read at
/// `./.env.example` (so deployed boxes get the latest list without rebuild);
/// falls back to the `include_str!`-embedded snapshot when the file isn't
/// reachable (binary invoked outside the repo dir).
pub fn canonical_keys() -> Vec<String> {
    let blob = std::fs::read_to_string(".env.example")
        .unwrap_or_else(|_| EMBEDDED_DOTENV_EXAMPLE.to_string());
    parse_dotenv_keys(&blob)
}

/// `true` ⇒ the key name contains any of `SECRET_KEYWORDS` (case-insensitive).
/// Public so unit tests can pin the rule and the dashboard can mirror it.
pub fn is_secret(key: &str) -> bool {
    let up = key.to_ascii_uppercase();
    SECRET_KEYWORDS.iter().any(|k| up.contains(k))
}

/// Fixed mask. Five stars matches the dashboard's existing redaction.
fn mask_value() -> &'static str {
    "*****"
}

/// Render the value for display: returns the mask when `is_secret(key)` and
/// the value is non-empty; otherwise the value verbatim. An empty value is
/// passed through (so `list` can distinguish "set to empty" from "unset").
fn display_value(key: &str, value: &str) -> String {
    if !value.is_empty() && is_secret(key) {
        mask_value().to_string()
    } else {
        value.to_string()
    }
}

/// Per-key resolution. Precedence: sqlite `config` row > `process.env` > unset.
/// Returns `(value, source)` where source is `"config" | "env" | "unset"`.
fn resolve(key: &str, cfg: &BTreeMap<String, String>) -> (String, &'static str) {
    if let Some(v) = cfg.get(key) {
        return (v.clone(), "config");
    }
    if let Ok(v) = std::env::var(key) {
        return (v, "env");
    }
    (String::new(), "unset")
}

/// Top-level dispatcher for `augmentagent env <op>`. `json` is the top-level
/// `--json` flag (applies to `List` and `Get`); `Set` and `Unset` always
/// print JSON receipts so the `/setup` skill can key off them uniformly.
pub fn run_env(op: &EnvOp, json: bool) -> Result<()> {
    match op {
        EnvOp::List => run_list(json),
        EnvOp::Get { key } => run_get(key, json),
        EnvOp::Set { key, value } => run_set(key, value),
        EnvOp::Unset { key } => run_unset(key),
    }
}

/// `env list` — full canonical key set (from `.env.example`) UNION any extras
/// living only in the sqlite `config` table, alphabetically.
fn run_list(json: bool) -> Result<()> {
    let cfg = read_config_map()?;
    let canon = canonical_keys();

    // Union, deduped & sorted via BTreeSet.
    let mut all: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for k in canon {
        all.insert(k);
    }
    for k in cfg.keys() {
        all.insert(k.clone());
    }

    if json {
        let keys: Vec<_> = all
            .iter()
            .map(|k| {
                let (raw, source) = resolve(k, &cfg);
                let shown = display_value(k, &raw);
                json!({
                    "name": k,
                    "source": source,
                    "value": shown,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&json!({
                "keys": keys,
                "schema_version": "1",
            }))
            .context("render env list JSON")?
        );
        return Ok(());
    }

    // tty: pretty fixed-width table. Width is computed once over the actual
    // keys instead of hardcoded so long names (AUGMENTAGENT_…) render flush.
    let name_w = all.iter().map(|s| s.len()).max().unwrap_or(4).max(4);
    let source_w = 6; // "config" | "env" | "unset"
    println!(
        "{:<nw$}  {:<sw$}  {}",
        "NAME",
        "SOURCE",
        "VALUE",
        nw = name_w,
        sw = source_w
    );
    println!("{}", "-".repeat(name_w + source_w + 8));
    for k in &all {
        let (raw, source) = resolve(k, &cfg);
        let shown = display_value(k, &raw);
        println!(
            "{:<nw$}  {:<sw$}  {}",
            k,
            source,
            shown,
            nw = name_w,
            sw = source_w
        );
    }
    Ok(())
}

/// `env get KEY` — print raw (unmasked) value, or `(unset)` when missing.
/// `--json` adds masking guidance (`secret: true`) so callers know whether
/// to redact in logs/UI without re-deriving the secret rule.
fn run_get(key: &str, json: bool) -> Result<()> {
    let cfg = read_config_map()?;
    let (raw, source) = resolve(key, &cfg);
    if json {
        let payload = if source == "unset" {
            json!({
                "key": key,
                "value": null,
                "source": "unset",
                "secret": is_secret(key),
            })
        } else {
            json!({
                "key": key,
                "value": raw,
                "source": source,
                "secret": is_secret(key),
            })
        };
        println!("{}", serde_json::to_string(&payload).context("render env get JSON")?);
        return Ok(());
    }

    if source == "unset" {
        println!("(unset)");
    } else {
        println!("{raw}");
    }
    Ok(())
}

/// `env set KEY VALUE` — upsert into sqlite `config`. Warns on stderr when
/// the key isn't in `.env.example`. Always emits a JSON receipt for the
/// skill (`restart_required: true`, `restart_cmd`).
fn run_set(key: &str, value: &str) -> Result<()> {
    let canon = canonical_keys();
    if !canon.iter().any(|k| k == key) {
        eprintln!("warning: {key} is not in .env.example");
    }
    write_config_value(key, value)?;
    let shown = display_value(key, value);
    println!(
        "{}",
        serde_json::to_string(&json!({
            "key": key,
            "value": shown,
            "source": "config",
            "restart_required": true,
            "restart_cmd": "augmentagent service restart",
        }))
        .context("render env set JSON")?
    );
    Ok(())
}

/// `env unset KEY` — delete from sqlite `config`. Reports `removed: bool` so
/// callers can tell a no-op apart from a real delete. The OS env / .env file
/// are NOT touched.
fn run_unset(key: &str) -> Result<()> {
    let n = delete_config_value(key)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "key": key,
            "removed": n > 0,
            "source": "config",
            "restart_required": n > 0,
            "restart_cmd": "augmentagent service restart",
        }))
        .context("render env unset JSON")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keys_picks_up_uncommented_and_commented() {
        let blob = "\
# header comment with no =
GROQ_API_KEY=
DISCORD_BOT_TOKEN=secret
# AUGMENTAGENT_CARDDAV_URL=
# AUGMENTAGENT_CARDDAV_USER=
# free prose: not a key
not_a_key=foo
ALSO_KEY=value
";
        let keys = parse_dotenv_keys(blob);
        assert!(keys.contains(&"GROQ_API_KEY".to_string()));
        assert!(keys.contains(&"DISCORD_BOT_TOKEN".to_string()));
        assert!(keys.contains(&"AUGMENTAGENT_CARDDAV_URL".to_string()));
        assert!(keys.contains(&"AUGMENTAGENT_CARDDAV_USER".to_string()));
        assert!(keys.contains(&"ALSO_KEY".to_string()));
        // Lowercase / shell snippets must not match.
        assert!(!keys.iter().any(|k| k == "not_a_key"));
        // Sorted, deduped.
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn embedded_env_example_has_known_keys() {
        // Anchor a couple of canonical keys to make sure the include_str!
        // path resolved at compile time and the parser handles the real file.
        let keys = parse_dotenv_keys(EMBEDDED_DOTENV_EXAMPLE);
        assert!(keys.contains(&"GROQ_API_KEY".to_string()));
        assert!(keys.contains(&"DISCORD_BOT_TOKEN".to_string()));
        assert!(keys.contains(&"DASHBOARD_PORT".to_string()));
    }

    #[test]
    fn secret_detection_matches_spec() {
        // KEY / TOKEN / SECRET / PASSWORD / PASS / AUTH (case-insensitive).
        assert!(is_secret("GROQ_API_KEY"));
        assert!(is_secret("DISCORD_BOT_TOKEN"));
        assert!(is_secret("GITHUB_WEBHOOK_SECRET"));
        assert!(is_secret("DB_PASSWORD"));
        assert!(is_secret("AUGMENTAGENT_LINKEDIN_PASS"));
        assert!(is_secret("AUGMENTAGENT_TELEGRAM_BOT_AUTH"));
        // Case-insensitive.
        assert!(is_secret("groq_api_key"));
        // Non-secrets.
        assert!(!is_secret("DASHBOARD_PORT"));
        assert!(!is_secret("AUGMENTAGENT_DEFAULT_REGION"));
        assert!(!is_secret("RUST_LOG"));
    }

    #[test]
    fn mask_only_applies_to_non_empty_secrets() {
        // Empty value isn't masked — list needs to distinguish "set to empty"
        // from a wall of asterisks.
        assert_eq!(display_value("GROQ_API_KEY", ""), "");
        assert_eq!(display_value("GROQ_API_KEY", "xxx"), "*****");
        assert_eq!(display_value("DASHBOARD_PORT", "3000"), "3000");
    }

    #[test]
    fn resolve_precedence_config_over_env() {
        // sqlite config wins over process.env, both lose to nothing.
        let mut cfg = BTreeMap::new();
        cfg.insert("FOO_KEY".to_string(), "from-config".to_string());
        std::env::set_var("FOO_KEY", "from-env");
        let (v, src) = resolve("FOO_KEY", &cfg);
        assert_eq!(v, "from-config");
        assert_eq!(src, "config");
        std::env::remove_var("FOO_KEY");

        // env-only.
        std::env::set_var("BAR_TOKEN", "from-env");
        let (v, src) = resolve("BAR_TOKEN", &BTreeMap::new());
        assert_eq!(v, "from-env");
        assert_eq!(src, "env");
        std::env::remove_var("BAR_TOKEN");

        // unset.
        let (v, src) = resolve("ZZ_DOES_NOT_EXIST_4242", &BTreeMap::new());
        assert_eq!(v, "");
        assert_eq!(src, "unset");
    }
}
