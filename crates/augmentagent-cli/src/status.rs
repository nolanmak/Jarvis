//! `augmentagent status` — single-document health aggregator.
//!
//! Source of truth for the `/setup` skill and ongoing maintenance. One pass
//! reads:
//!
//!  * **daemon**    — `systemctl --user show augmentagent.service`
//!  * **dashboard** — same, plus an HTTP GET against `/api/v1/stats`
//!  * **updater**   — `augmentagent-update.timer`
//!  * **core_keys** — env vars merged over the sqlite `config` table
//!                   (sqlite wins; mirrors `getConfigStatus()` in
//!                   `src/dashboard.ts:78`)
//!  * **channels**  — best-effort `configured` flag per known channel
//!                   (gmail probes the Composio accounts table; others
//!                   probe the sqlite `config` table for canonical keys).
//!                   `armed` defaults to `false` until #7 lands the
//!                   arm/disarm verbs.
//!  * **queue**     — `pending_reply_count()` from the store
//!
//! Output is JSON by default when stdout is piped (CI, dashboard shell-out)
//! and a hand-rolled ASCII table when stdout is a tty (no `comfy-table`
//! workspace dep yet). The JSON shape is locked at `schema_version: "1"`
//! and verified by the CI snapshot test in #14.
//!
//! Exit code policy (per the issue):
//!   0  → ok            (everything green)
//!  10  → degraded      (covers `needs_setup` and partial-config)
//!  20  → daemon_down
//!  30  → dashboard_down
//!  40  → config_invalid
//!
//! Linux-only by design — the entire surface assumes systemd-user (the
//! daemon is shipped via `augmentagent.service`). There is no macOS
//! counterpart.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::process::Command;

use augmentagent_store::{rusqlite, Store};

/// JSON schema version. Bump on any breaking change to the document shape.
/// The CI snapshot test in #14 keys off this constant.
pub const SCHEMA_VERSION: &str = "1";

/// Canonical list of channels surfaced in `status.channels`. Kept in sync
/// with the per-channel `Cmd::*` variants in `main.rs`. Insertion order
/// here drives the human-table row order; the JSON map is alphabetised by
/// `BTreeMap`.
const KNOWN_CHANNELS: &[&str] = &[
    "gmail",
    "slack",
    "discord",
    "twitter",
    "linkedin",
    "instagram",
    "reddit",
    "github",
    "meetup",
    "telegram",
    "whatsapp",
    "calendar",
    "voice",
    "gdrive",
    "contacts",
];

// ---------------------------------------------------------------------------
// Public JSON shape — locked at schema_version "1".
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct StatusDoc {
    pub schema_version: String,
    pub host: String,
    pub daemon: DaemonStatus,
    pub dashboard: DashboardStatus,
    pub updater: UpdaterStatus,
    pub core_keys: CoreKeys,
    pub channels: BTreeMap<String, ChannelStatus>,
    pub queue: QueueStatus,
    pub summary: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DaemonStatus {
    pub unit: String,
    pub active: bool,
    pub since_unix: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DashboardStatus {
    pub unit: String,
    pub active: bool,
    pub port: u16,
    pub reachable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UpdaterStatus {
    pub unit: String,
    pub timer_active: bool,
    pub last_run_unix: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CoreKeys {
    pub composio: bool,
    pub groq: bool,
    pub cerebras: bool,
    pub discord_bot: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ChannelStatus {
    pub configured: bool,
    pub armed: bool,
    pub accounts: u32,
    pub last_poll_unix: Option<i64>,
    pub needs: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct QueueStatus {
    pub pending: i64,
}

/// Symbolic summary string. Maps onto the issue's exit-code table; see
/// [`exit_code_for`].
pub mod summary {
    pub const OK: &str = "ok";
    pub const DEGRADED: &str = "degraded";
    pub const DAEMON_DOWN: &str = "daemon_down";
    pub const DASHBOARD_DOWN: &str = "dashboard_down";
    pub const NEEDS_SETUP: &str = "needs_setup";
    pub const CONFIG_INVALID: &str = "config_invalid";
}

/// Map a symbolic summary to its exit code. `needs_setup` collapses to the
/// `degraded` bucket (10) because the issue's exit-code list only enumerates
/// healthy / degraded / daemon_down / dashboard_down / config_invalid.
pub fn exit_code_for(s: &str) -> i32 {
    match s {
        summary::OK => 0,
        summary::DAEMON_DOWN => 20,
        summary::DASHBOARD_DOWN => 30,
        summary::CONFIG_INVALID => 40,
        // degraded + needs_setup + anything else
        _ => 10,
    }
}

// ---------------------------------------------------------------------------
// Public entrypoint (called from `main.rs`).
// ---------------------------------------------------------------------------

/// Run the aggregator and print to stdout. Returns the process exit code so
/// the caller can `std::process::exit(code)` after a clean store shutdown.
///
/// * `json` — `Some(true)` forces JSON, `Some(false)` forces table.
///            `None` auto-detects via `stdout().is_terminal()`.
/// * `channel` — when set, narrow the channels map to just this name.
/// * `refresh` — placeholder for a future cache layer (#1 follow-up). No-op
///   today; accepted so the flag is stable from day one.
pub async fn run(
    store: Arc<Store>,
    json: Option<bool>,
    channel: Option<String>,
    _refresh: bool,
) -> Result<i32> {
    let mut doc = collect(&store).await?;

    if let Some(name) = channel.as_deref() {
        let filtered: BTreeMap<String, ChannelStatus> = doc
            .channels
            .into_iter()
            .filter(|(k, _)| k == name)
            .collect();
        doc.channels = filtered;
    }

    let want_json = json.unwrap_or_else(|| !std::io::stdout().is_terminal());
    if want_json {
        println!("{}", serde_json::to_string_pretty(&doc.to_json())?);
    } else {
        print_table(&doc, channel.as_deref());
    }

    Ok(exit_code_for(&doc.summary))
}

impl StatusDoc {
    /// Hand-roll the JSON document. We do this instead of `derive(Serialize)`
    /// because adding `serde` (with derive) as a direct dep of this crate
    /// would expand the allowlist; `serde_json` is already present and the
    /// document shape is small + locked.
    pub fn to_json(&self) -> Value {
        let mut channels = serde_json::Map::new();
        for (k, v) in &self.channels {
            channels.insert(k.clone(), v.to_json());
        }
        json!({
            "schema_version": self.schema_version,
            "host": self.host,
            "daemon": {
                "unit": self.daemon.unit,
                "active": self.daemon.active,
                "since_unix": self.daemon.since_unix,
            },
            "dashboard": {
                "unit": self.dashboard.unit,
                "active": self.dashboard.active,
                "port": self.dashboard.port,
                "reachable": self.dashboard.reachable,
            },
            "updater": {
                "unit": self.updater.unit,
                "timer_active": self.updater.timer_active,
                "last_run_unix": self.updater.last_run_unix,
            },
            "core_keys": {
                "composio": self.core_keys.composio,
                "groq": self.core_keys.groq,
                "cerebras": self.core_keys.cerebras,
                "discord_bot": self.core_keys.discord_bot,
            },
            "channels": Value::Object(channels),
            "queue": { "pending": self.queue.pending },
            "summary": self.summary,
        })
    }
}

impl ChannelStatus {
    fn to_json(&self) -> Value {
        json!({
            "configured": self.configured,
            "armed": self.armed,
            "accounts": self.accounts,
            "last_poll_unix": self.last_poll_unix,
            "needs": self.needs,
        })
    }
}

/// One-shot probe + assemble. Public so the CI snapshot test in #14 can
/// import it directly without spawning the binary.
pub async fn collect(store: &Store) -> Result<StatusDoc> {
    let daemon = probe_daemon().await?;
    let dashboard = probe_dashboard().await?;
    let updater = probe_updater().await?;

    let cfg = read_config_table(store)?;
    let core_keys = collect_core_keys(&cfg);
    let channels = collect_channels(store, &cfg)?;
    let queue = QueueStatus {
        pending: store.pending_reply_count().context("queue depth")?,
    };

    let summary = classify(&daemon, &dashboard, &core_keys, &channels);

    Ok(StatusDoc {
        schema_version: SCHEMA_VERSION.to_string(),
        host: "linux".to_string(),
        daemon,
        dashboard,
        updater,
        core_keys,
        channels,
        queue,
        summary,
    })
}

// ---------------------------------------------------------------------------
// systemd probes.
// ---------------------------------------------------------------------------

/// Output of `systemctl --user show <unit> --property=...`. All keys are
/// strings; values present as empty when the property is unset.
#[derive(Debug, Default)]
struct UnitProps {
    active_state: String,
    sub_state: String,
    active_enter_timestamp_unix: i64,
}

/// Run `systemctl --user show` and parse its `KEY=value` lines. When
/// systemctl is missing or the unit doesn't exist, returns a zeroed struct
/// (everything reads as inactive). Never errors — a missing systemd is a
/// real production state on a freshly cloned dev box.
async fn show_unit(unit: &str) -> UnitProps {
    let out = Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "--property=ActiveState,SubState,ActiveEnterTimestamp",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let Ok(out) = out else {
        return UnitProps::default();
    };
    if !out.status.success() {
        return UnitProps::default();
    }
    parse_systemctl_show(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `KEY=value` lines. `ActiveEnterTimestamp` is systemd's local-time
/// human string (e.g. `Tue 2026-05-21 09:14:33 PDT`). We try to parse it via
/// `chrono`; on failure we leave the unix timestamp at `0` so the JSON shape
/// stays stable — downstream consumers should treat `0` as "unknown".
fn parse_systemctl_show(text: &str) -> UnitProps {
    let mut props = UnitProps::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "ActiveState" => props.active_state = v.to_string(),
            "SubState" => props.sub_state = v.to_string(),
            "ActiveEnterTimestamp" => {
                props.active_enter_timestamp_unix = parse_systemd_timestamp(v).unwrap_or(0);
            }
            _ => {}
        }
    }
    props
}

/// Best-effort parse of systemd's `ActiveEnterTimestamp` format.
/// Examples seen in the wild:
///   - `Tue 2026-05-21 09:14:33 PDT`
///   - `n/a`  (timer never armed)
///   - ``    (property unset)
fn parse_systemd_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || s == "n/a" {
        return None;
    }
    // Strip the leading 3-letter weekday + space.
    let rest = s.split_once(' ').map(|(_, r)| r).unwrap_or(s);
    // chrono will choke on the trailing `PDT`/`UTC` zone abbreviation when
    // parsed as %Z, so peel it off and parse the date+time as naive-local.
    let naive_part = rest.rsplit_once(' ').map(|(l, _)| l).unwrap_or(rest);
    let dt = chrono::NaiveDateTime::parse_from_str(naive_part, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(dt.and_utc().timestamp())
}

async fn probe_daemon() -> Result<DaemonStatus> {
    let unit = "augmentagent.service";
    let p = show_unit(unit).await;
    Ok(DaemonStatus {
        unit: unit.to_string(),
        active: p.active_state == "active",
        since_unix: p.active_enter_timestamp_unix,
    })
}

async fn probe_updater() -> Result<UpdaterStatus> {
    let unit = "augmentagent-update.timer";
    let p = show_unit(unit).await;
    Ok(UpdaterStatus {
        unit: unit.to_string(),
        timer_active: p.active_state == "active",
        last_run_unix: p.active_enter_timestamp_unix,
    })
}

async fn probe_dashboard() -> Result<DashboardStatus> {
    let unit = "augmentagent-dashboard.service";
    let p = show_unit(unit).await;
    let port: u16 = std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let reachable = dashboard_reachable(port).await;
    Ok(DashboardStatus {
        unit: unit.to_string(),
        active: p.active_state == "active",
        port,
        reachable,
    })
}

/// True iff the dashboard answers on `localhost:{port}`. Accepts any 2xx
/// *or* 401: an authenticated `/api/v1/stats` is real proof-of-life even
/// when we don't have the api key. Net errors → false. 2s ceiling so this
/// can't hang `status` on a wedged dashboard.
async fn dashboard_reachable(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/v1/stats");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut req = client.get(&url);
    if let Ok(key) = std::env::var("AUGMENTAGENT_API_KEY") {
        if !key.is_empty() {
            req = req.header("x-api-key", key);
        }
    }
    match req.send().await {
        Ok(resp) => {
            let s = resp.status();
            s.is_success() || s.as_u16() == 401
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// sqlite `config` table — generic key/value reads.
// ---------------------------------------------------------------------------

/// Slurp the entire `config` table into a map. The store doesn't expose a
/// generic helper (only `invoice_config`), so we go direct via the same db
/// path used to open the store. Schema is `(key TEXT PK, value TEXT)`; if
/// the table doesn't exist on a fresh install we return an empty map.
fn read_config_table(_store: &Store) -> Result<BTreeMap<String, String>> {
    let db_path = std::env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string());
    let mut out = BTreeMap::new();
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return Ok(out),
    };
    let mut stmt = match conn.prepare("SELECT key, value FROM config") {
        Ok(s) => s,
        Err(_) => return Ok(out), // table absent on a fresh box
    };
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .context("config table query")?;
    for row in rows.flatten() {
        out.insert(row.0, row.1);
    }
    Ok(out)
}

/// Resolve a config key. Precedence: sqlite `config` table OVER the env
/// var. Mirrors `getConfigStatus()` in `src/dashboard.ts:78` so the CLI and
/// the dashboard agree on which keys are "configured" on the same box.
fn cfg_or_env(cfg: &BTreeMap<String, String>, sqlite_key: &str, env_key: &str) -> bool {
    if cfg.get(sqlite_key).map(|s| !s.is_empty()).unwrap_or(false) {
        return true;
    }
    std::env::var(env_key).map(|v| !v.is_empty()).unwrap_or(false)
}

fn collect_core_keys(cfg: &BTreeMap<String, String>) -> CoreKeys {
    CoreKeys {
        composio: cfg_or_env(cfg, "composio_api_key", "COMPOSIO_API_KEY"),
        groq: cfg_or_env(cfg, "groq_api_key", "GROQ_API_KEY"),
        cerebras: cfg_or_env(cfg, "cerebras_api_key", "CEREBRAS_API_KEY"),
        discord_bot: cfg_or_env(cfg, "discord_bot_token", "DISCORD_BOT_TOKEN"),
    }
}

// ---------------------------------------------------------------------------
// Per-channel configured/armed probes.
// ---------------------------------------------------------------------------

/// Build the channels map. `armed` is hard-coded to `false` until #7 lands
/// the arm/disarm verbs (the CLI doesn't have a per-channel armed state to
/// query yet). `accounts` is best-effort and only filled for gmail today.
fn collect_channels(
    store: &Store,
    cfg: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, ChannelStatus>> {
    let mut out = BTreeMap::new();
    let gmail_accounts: u32 = store
        .get_active_gmail_accounts()
        .map(|v| v.len() as u32)
        .unwrap_or(0);

    for &name in KNOWN_CHANNELS {
        let (configured, accounts) = match name {
            "gmail" => {
                let n = gmail_accounts;
                // Composio is the auth path for gmail; consider gmail
                // "configured" iff Composio is set up AND at least one
                // account is connected.
                let composio_present = cfg_or_env(cfg, "composio_api_key", "COMPOSIO_API_KEY");
                (composio_present && n > 0, n)
            }
            // Each remaining channel is "configured" iff its canonical
            // sqlite config key (or env equivalent) is present. The key
            // names mirror the existing dashboard / channel-crate
            // conventions; #7 will replace this with first-class
            // per-channel armed/configured probes.
            "slack" => (cfg_or_env(cfg, "slack_bot_token", "SLACK_BOT_TOKEN"), 0),
            "discord" => (
                cfg_or_env(cfg, "discord_bot_token", "DISCORD_BOT_TOKEN"),
                0,
            ),
            "twitter" => (
                cfg_or_env(cfg, "twitter_session_b64", "TWITTER_SESSION_B64"),
                0,
            ),
            "linkedin" => (
                cfg_or_env(cfg, "linkedin_li_at", "LINKEDIN_LI_AT"),
                0,
            ),
            "instagram" => (
                cfg_or_env(cfg, "instagram_session_b64", "INSTAGRAM_SESSION_B64"),
                0,
            ),
            "reddit" => (
                cfg_or_env(cfg, "reddit_refresh_token", "REDDIT_REFRESH_TOKEN"),
                0,
            ),
            "github" => (cfg_or_env(cfg, "github_pat", "GITHUB_TOKEN"), 0),
            "meetup" => (
                cfg_or_env(cfg, "meetup_access_token", "MEETUP_ACCESS_TOKEN"),
                0,
            ),
            "telegram" => (
                cfg_or_env(cfg, "telegram_bot_token", "TELEGRAM_BOT_TOKEN"),
                0,
            ),
            "whatsapp" => (
                cfg_or_env(cfg, "whatsapp_session_b64", "WHATSAPP_SESSION_B64"),
                0,
            ),
            "calendar" => (
                cfg_or_env(cfg, "composio_api_key", "COMPOSIO_API_KEY"),
                0,
            ),
            "voice" => (
                cfg_or_env(cfg, "voice_drop_dir", "VOICE_DROP_DIR"),
                0,
            ),
            "gdrive" => {
                let drive = store
                    .get_active_drive_accounts()
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);
                (drive > 0, drive)
            }
            "contacts" => (
                cfg_or_env(cfg, "carddav_url", "CARDDAV_URL")
                    || cfg_or_env(cfg, "composio_api_key", "COMPOSIO_API_KEY"),
                0,
            ),
            _ => (false, 0),
        };

        let needs = if configured {
            Vec::new()
        } else {
            vec!["login".to_string()]
        };
        out.insert(
            name.to_string(),
            ChannelStatus {
                configured,
                armed: false, // #7 will flip this once arm/disarm lands.
                accounts,
                last_poll_unix: None, // #7 / per-channel last-poll table.
                needs,
            },
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Summary classification.
// ---------------------------------------------------------------------------

fn classify(
    daemon: &DaemonStatus,
    dashboard: &DashboardStatus,
    core: &CoreKeys,
    channels: &BTreeMap<String, ChannelStatus>,
) -> String {
    if !daemon.active {
        return summary::DAEMON_DOWN.into();
    }
    if !dashboard.reachable {
        return summary::DASHBOARD_DOWN.into();
    }
    let any_core_key = core.composio || core.groq || core.cerebras || core.discord_bot;
    let any_channel = channels.values().any(|c| c.configured);
    if !any_core_key && !any_channel {
        return summary::NEEDS_SETUP.into();
    }
    if !any_core_key || !any_channel {
        return summary::DEGRADED.into();
    }
    summary::OK.into()
}

// ---------------------------------------------------------------------------
// Human table rendering. Hand-rolled — no comfy-table workspace dep.
// ---------------------------------------------------------------------------

fn print_table(doc: &StatusDoc, channel_filter: Option<&str>) {
    println!("AugmentAgent status ({})", doc.host);
    println!("  summary    : {}", doc.summary);
    println!(
        "  daemon     : {} ({})",
        doc.daemon.unit,
        if doc.daemon.active { "active" } else { "inactive" }
    );
    println!(
        "  dashboard  : {} (port {}, {})",
        doc.dashboard.unit,
        doc.dashboard.port,
        if doc.dashboard.reachable { "reachable" } else { "unreachable" }
    );
    println!(
        "  updater    : {} ({})",
        doc.updater.unit,
        if doc.updater.timer_active { "armed" } else { "off" }
    );
    println!("  queue      : {} pending", doc.queue.pending);
    println!("  core keys  :");
    println!("      composio    : {}", yn(doc.core_keys.composio));
    println!("      groq        : {}", yn(doc.core_keys.groq));
    println!("      cerebras    : {}", yn(doc.core_keys.cerebras));
    println!("      discord_bot : {}", yn(doc.core_keys.discord_bot));

    if let Some(name) = channel_filter {
        println!("\nchannel {name}:");
    } else {
        println!("\nchannels:");
    }
    println!(
        "  {:<10} {:<10} {:<6} {:<8} {}",
        "name", "configured", "armed", "accounts", "needs"
    );
    println!("  {}", "-".repeat(60));
    // Iterate KNOWN_CHANNELS order so the table is stable; skip missing
    // entries (channel filter narrowed them out).
    for &name in KNOWN_CHANNELS {
        let Some(ch) = doc.channels.get(name) else {
            continue;
        };
        println!(
            "  {:<10} {:<10} {:<6} {:<8} {}",
            name,
            yn(ch.configured),
            yn(ch.armed),
            ch.accounts,
            if ch.needs.is_empty() {
                "-".to_string()
            } else {
                ch.needs.join(",")
            }
        );
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_channels() -> BTreeMap<String, ChannelStatus> {
        BTreeMap::new()
    }

    fn ch(configured: bool) -> ChannelStatus {
        ChannelStatus {
            configured,
            armed: false,
            accounts: 0,
            last_poll_unix: None,
            needs: if configured {
                Vec::new()
            } else {
                vec!["login".into()]
            },
        }
    }

    #[test]
    fn classify_daemon_down_wins() {
        let d = DaemonStatus {
            unit: "x".into(),
            active: false,
            since_unix: 0,
        };
        let dash = DashboardStatus {
            unit: "x".into(),
            active: true,
            port: 3000,
            reachable: true,
        };
        let c = CoreKeys {
            composio: true,
            groq: true,
            cerebras: true,
            discord_bot: true,
        };
        assert_eq!(classify(&d, &dash, &c, &empty_channels()), summary::DAEMON_DOWN);
    }

    #[test]
    fn classify_dashboard_down_after_daemon_ok() {
        let d = DaemonStatus {
            unit: "x".into(),
            active: true,
            since_unix: 0,
        };
        let dash = DashboardStatus {
            unit: "x".into(),
            active: false,
            port: 3000,
            reachable: false,
        };
        let c = CoreKeys {
            composio: true,
            groq: true,
            cerebras: true,
            discord_bot: true,
        };
        assert_eq!(classify(&d, &dash, &c, &empty_channels()), summary::DASHBOARD_DOWN);
    }

    #[test]
    fn classify_needs_setup_on_fresh_box() {
        let d = DaemonStatus {
            unit: "x".into(),
            active: true,
            since_unix: 0,
        };
        let dash = DashboardStatus {
            unit: "x".into(),
            active: true,
            port: 3000,
            reachable: true,
        };
        let c = CoreKeys {
            composio: false,
            groq: false,
            cerebras: false,
            discord_bot: false,
        };
        let mut channels = BTreeMap::new();
        for &n in KNOWN_CHANNELS {
            channels.insert(n.to_string(), ch(false));
        }
        assert_eq!(classify(&d, &dash, &c, &channels), summary::NEEDS_SETUP);
    }

    #[test]
    fn classify_degraded_when_only_core_set() {
        let d = DaemonStatus {
            unit: "x".into(),
            active: true,
            since_unix: 0,
        };
        let dash = DashboardStatus {
            unit: "x".into(),
            active: true,
            port: 3000,
            reachable: true,
        };
        let c = CoreKeys {
            composio: true,
            groq: false,
            cerebras: false,
            discord_bot: false,
        };
        let mut channels = BTreeMap::new();
        for &n in KNOWN_CHANNELS {
            channels.insert(n.to_string(), ch(false));
        }
        assert_eq!(classify(&d, &dash, &c, &channels), summary::DEGRADED);
    }

    #[test]
    fn classify_ok_when_core_and_a_channel_configured() {
        let d = DaemonStatus {
            unit: "x".into(),
            active: true,
            since_unix: 0,
        };
        let dash = DashboardStatus {
            unit: "x".into(),
            active: true,
            port: 3000,
            reachable: true,
        };
        let c = CoreKeys {
            composio: true,
            groq: true,
            cerebras: false,
            discord_bot: false,
        };
        let mut channels = BTreeMap::new();
        for &n in KNOWN_CHANNELS {
            channels.insert(n.to_string(), ch(n == "gmail"));
        }
        assert_eq!(classify(&d, &dash, &c, &channels), summary::OK);
    }

    #[test]
    fn exit_codes_match_issue_spec() {
        assert_eq!(exit_code_for(summary::OK), 0);
        assert_eq!(exit_code_for(summary::DEGRADED), 10);
        assert_eq!(exit_code_for(summary::NEEDS_SETUP), 10);
        assert_eq!(exit_code_for(summary::DAEMON_DOWN), 20);
        assert_eq!(exit_code_for(summary::DASHBOARD_DOWN), 30);
        assert_eq!(exit_code_for(summary::CONFIG_INVALID), 40);
        assert_eq!(exit_code_for("unknown"), 10);
    }

    #[test]
    fn systemctl_show_parser_handles_active_unit() {
        let text = "ActiveState=active\nSubState=running\nActiveEnterTimestamp=Tue 2026-05-21 09:14:33 PDT\n";
        let p = parse_systemctl_show(text);
        assert_eq!(p.active_state, "active");
        assert_eq!(p.sub_state, "running");
        assert!(p.active_enter_timestamp_unix > 0);
    }

    #[test]
    fn systemctl_show_parser_handles_inactive_or_missing_unit() {
        let text = "ActiveState=inactive\nSubState=dead\nActiveEnterTimestamp=\n";
        let p = parse_systemctl_show(text);
        assert_eq!(p.active_state, "inactive");
        assert_eq!(p.active_enter_timestamp_unix, 0);
    }

    #[test]
    fn systemctl_show_parser_handles_na_timer() {
        let text = "ActiveState=active\nSubState=waiting\nActiveEnterTimestamp=n/a\n";
        let p = parse_systemctl_show(text);
        assert_eq!(p.active_enter_timestamp_unix, 0);
    }

    #[test]
    fn schema_version_is_locked() {
        // The CI snapshot test in #14 keys off this. Bumping it is a
        // breaking change for the /setup skill.
        assert_eq!(SCHEMA_VERSION, "1");
    }

    #[test]
    fn known_channels_covers_per_channel_cmd_set() {
        // If main.rs gains a new top-level Cmd::* channel variant, this
        // list must grow with it so `status` doesn't silently omit it.
        for required in [
            "gmail", "slack", "discord", "twitter", "linkedin", "instagram", "reddit",
            "github", "meetup", "telegram", "whatsapp", "calendar", "voice", "gdrive",
            "contacts",
        ] {
            assert!(
                KNOWN_CHANNELS.contains(&required),
                "missing channel: {required}"
            );
        }
    }
}
