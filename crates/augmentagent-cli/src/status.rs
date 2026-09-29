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
//!  * **channels**  — per-channel `configured`/`armed` derived from the
//!                   SAME gates `Cmd::Serve` evaluates (#374): keyring
//!                   slots via `augmentagent_auth::Auth::exists`, legacy
//!                   credential files via each channel's
//!                   `default_auth_path`, and store tables (workspaces,
//!                   bots, subscriptions, accounts). `configured` = the
//!                   credential/prereq is present; `armed` = the serve
//!                   loop would run a poller for it right now. The four
//!                   config-table arming keys (`twitter_real_enabled`
//!                   etc.) are NOT consulted — serve never reads them,
//!                   so they said nothing about runtime state (#374).
//!  * **queue**     — `pending_reply_count()` from the store
//!  * **delivery**  — durable backlog/retry/dead-letter counts per surface (#1285)
//!  * **interactive** — live listener health of interactive chat surfaces
//!    (#1287), from the report the daemon writes; not `channels.slack`,
//!    which is Composio ingestion
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
//! Service probes use systemd-user on Linux and launchd on macOS (#1079);
//! jobs keep their systemd unit names in the JSON either way, so the
//! document shape does not depend on the host.

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
    "socialapi",
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
    pub delivery: BTreeMap<String, DeliveryStatus>,
    pub interactive: BTreeMap<String, InteractiveStatus>,
    /// #1299 — the credential backend this process uses.
    pub credentials: CredentialsStatus,
    /// #1299 — what the daemon recorded at its last start.
    pub daemon_report: Option<DaemonReportStatus>,
    /// #1299 — configuration problems, each with a recovery step.
    pub config_issues: Vec<ConfigIssue>,
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

/// #1285 — durable delivery state for one chat surface. Every count is of
/// rows still needing work except the dead letters, which need the owner.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeliveryStatus {
    pub inbound_backlog: i64,
    pub inbound_dead_letter: i64,
    pub outbound_backlog: i64,
    pub outbound_retrying: i64,
    pub outbound_reconcile: i64,
    pub outbound_dead_letter: i64,
}

/// Chat surfaces always listed under `delivery`, with zeros when idle, so a
/// consumer can tell "nothing pending" from "not reported".
const DELIVERY_SURFACES: &[&str] = &["discord", "slack", "whatsapp"];

/// #1287 — one interactive surface's live listener, as the daemon last
/// reported it. `healthy` is true only for a `connected` listener whose
/// report is fresh; a report that stopped being refreshed reads as
/// `disconnected`, never as the last state it claimed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InteractiveStatus {
    /// `not_configured`, `disabled`, `misconfigured`, `connecting`,
    /// `connected`, `reconnecting`, `disconnected` or `stopped`.
    pub state: String,
    pub healthy: bool,
    pub detail: Option<String>,
    /// What the operator should do when the state needs action.
    pub recovery: Option<String>,
    pub workspaces: Vec<String>,
    pub dry_run: bool,
    pub last_event_unix: Option<i64>,
    pub last_send_unix: Option<i64>,
    pub state_since_unix: Option<i64>,
    pub heartbeat_unix: Option<i64>,
    /// #1299 — an install record exists in this process's credential store
    /// (checked without reading the secret).
    pub app_installed: bool,
    /// #1299 — an owner binding exists in the database.
    pub owner_bound: bool,
    /// #1299 — `missing` (no install record visible here), `present`
    /// (stored, not yet proven usable by the running daemon) or `usable`
    /// (the daemon's fresh `connected` report proves it read them and Slack
    /// accepted the app-level token). Never `usable` without that proof.
    pub credentials: String,
    /// #1299 — entries into `reconnecting` by the daemon that wrote the
    /// report; `None` without a report.
    pub reconnects: Option<i64>,
}

/// #1299 — what `status` knows about the Slack setup besides the daemon's
/// report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackSetup {
    pub app_installed: bool,
    pub owner_bound: bool,
    pub reconnects: Option<i64>,
    /// Whether the process that wrote the report is still alive; `None`
    /// when unknown (no report). A dead reporter's live state is not
    /// trusted even while its heartbeat is fresh.
    pub reporter_running: Option<bool>,
}

/// Interactive surfaces always listed under `interactive`.
const INTERACTIVE_SURFACES: &[&str] = &["slack"];

const RESTART_DAEMON: &str = "augmentagent service --unit daemon restart";

const INSTALL_RECOVERY: &str =
    "Install the Slack app (`augmentagent slack app install --stdin`, see \
     docs/SLACK-RUNBOOK.md), bind yourself with `augmentagent slack app owner bind --user \
     <member_id>`, then start the daemon (`augmentagent service --unit daemon restart`).";

/// Fold the daemon's last report (if any) into what `status` shows. Without
/// a report, the detail names the first missing setup step (#1299).
pub fn interactive_status(
    report: Option<augmentagent_store::surface_health::SurfaceListenerHealth>,
    setup: SlackSetup,
    now_ms: i64,
) -> InteractiveStatus {
    use augmentagent_channel_slack::interactive::{SurfaceState, STALE_AFTER};
    let secs = |ms: i64| ms / 1000;
    let unproven = if setup.app_installed {
        "present"
    } else {
        "missing"
    };
    let base = InteractiveStatus {
        app_installed: setup.app_installed,
        owner_bound: setup.owner_bound,
        credentials: unproven.into(),
        ..Default::default()
    };
    let Some(r) = report else {
        let not_configured = |detail: &str, recovery: String| InteractiveStatus {
            state: SurfaceState::NotConfigured.as_str().into(),
            detail: Some(detail.into()),
            recovery: Some(recovery),
            ..base.clone()
        };
        return match (setup.app_installed, setup.owner_bound) {
            (true, true) => InteractiveStatus {
                state: SurfaceState::Disconnected.as_str().into(),
                detail: Some(
                    "the daemon has not reported this surface (it is not running, or it \
                     predates the interactive Slack surface)"
                        .into(),
                ),
                recovery: Some(format!(
                    "Start or restart the daemon (`{RESTART_DAEMON}`); if it is running, \
                     check `augmentagent logs --unit daemon`."
                )),
                ..base
            },
            (false, false) => not_configured(
                "no interactive Slack app is installed",
                INSTALL_RECOVERY.into(),
            ),
            (true, false) => not_configured(
                "the Slack app is installed but no owner is bound",
                format!(
                    "Bind yourself as owner: `augmentagent slack app owner bind --user \
                     <member_id>` (add --team <team_id> when several workspaces are \
                     installed), then restart the daemon (`{RESTART_DAEMON}`)."
                ),
            ),
            (false, true) => not_configured(
                "an owner is bound but no Slack app is installed (or its credentials are not \
                 visible to this process)",
                format!(
                    "Install the Slack app (`augmentagent slack app install --stdin`, see \
                     docs/SLACK-RUNBOOK.md), then restart the daemon (`{RESTART_DAEMON}`)."
                ),
            ),
        };
    };
    let mut out = InteractiveStatus {
        state: r.state.clone(),
        healthy: false,
        detail: r.detail.clone(),
        recovery: r.recovery.clone(),
        workspaces: r.workspaces.clone(),
        dry_run: r.dry_run,
        last_event_unix: r.last_event_at_ms.map(secs),
        last_send_unix: r.last_send_at_ms.map(secs),
        state_since_unix: Some(secs(r.state_since_ms)),
        heartbeat_unix: Some(secs(r.heartbeat_at_ms)),
        reconnects: setup.reconnects,
        ..base
    };
    let live = SurfaceState::parse(&r.state).is_some_and(SurfaceState::is_live);
    let age_ms = now_ms.saturating_sub(r.heartbeat_at_ms);
    if live && setup.reporter_running == Some(false) {
        out.state = SurfaceState::Disconnected.as_str().into();
        out.detail = Some(format!(
            "the daemon that reported it (pid {}) is no longer running; its last report was `{}`",
            r.pid, r.state
        ));
        out.recovery = Some(format!(
            "Start the daemon (`{RESTART_DAEMON}`) and check `augmentagent logs --unit daemon`."
        ));
    } else if live && age_ms > STALE_AFTER.as_millis() as i64 {
        out.state = SurfaceState::Disconnected.as_str().into();
        out.detail = Some(format!(
            "no heartbeat from the daemon (pid {}) for {}s; its last report was `{}`",
            r.pid,
            age_ms / 1000,
            r.state
        ));
        out.recovery = Some(format!(
            "The daemon is not running or is stuck. Restart it (`{RESTART_DAEMON}`) and check \
             `augmentagent logs --unit daemon`."
        ));
    }
    out.healthy = SurfaceState::parse(&out.state).is_some_and(SurfaceState::is_healthy);
    if out.healthy {
        out.credentials = "usable".into();
    }
    out
}

// ---------------------------------------------------------------------------
// #1299 — credential backend, the daemon's start report, config issues.
// ---------------------------------------------------------------------------

/// The credential backend this `status` process uses (`default_store`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialsStatus {
    pub backend: String,
    pub persistent: bool,
    pub insecure_file_store: bool,
    pub note: Option<String>,
}

impl From<&augmentagent_auth::BackendDescription> for CredentialsStatus {
    fn from(d: &augmentagent_auth::BackendDescription) -> Self {
        Self {
            backend: d.backend.to_string(),
            persistent: d.persistent,
            insecure_file_store: d.insecure,
            note: d.note.clone(),
        }
    }
}

impl CredentialsStatus {
    fn to_json(&self) -> Value {
        json!({
            "backend": self.backend,
            "persistent": self.persistent,
            "insecure_file_store": self.insecure_file_store,
            "note": self.note,
        })
    }
}

/// The daemon's own record of its last start (`daemon_runtime_report`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonReportStatus {
    pub pid: u32,
    /// The recorded pid is alive, so the report describes a running daemon.
    pub running: bool,
    pub started_unix: i64,
    pub dry_run: bool,
    pub credential_backend: String,
    pub credential_persistent: bool,
    pub insecure_file_store: bool,
}

impl DaemonReportStatus {
    pub fn from_report(
        r: &augmentagent_store::daemon_report::DaemonRuntimeReport,
        running: bool,
    ) -> Self {
        Self {
            pid: r.pid,
            running,
            started_unix: r.started_at_ms / 1000,
            dry_run: r.dry_run,
            credential_backend: r.credential_backend.clone(),
            credential_persistent: r.credential_persistent,
            insecure_file_store: r.insecure_credential_store,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "pid": self.pid,
            "running": self.running,
            "started_unix": self.started_unix,
            "dry_run": self.dry_run,
            "credential_backend": self.credential_backend,
            "credential_persistent": self.credential_persistent,
            "insecure_file_store": self.insecure_file_store,
        })
    }
}

/// A configuration problem and how to fix it. `source` is `cli` (found by
/// this process from its environment) or `daemon` (recorded by the running
/// daemon at startup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    pub id: String,
    pub source: &'static str,
    /// `warn` or `error`.
    pub severity: String,
    pub detail: String,
    pub recovery: Option<String>,
}

impl ConfigIssue {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "source": self.source,
            "severity": self.severity,
            "detail": self.detail,
            "recovery": self.recovery,
        })
    }
}

pub const DISCORD_BROKER_ISSUE: &str = "discord.approval_broker";
pub const INSECURE_STORE_ISSUE: &str = "credentials.insecure_file_store";
pub const DAEMON_INSECURE_STORE_ISSUE: &str = "daemon.insecure_file_store";

/// What `serve` does with Discord settings (`build_broker`): a bot token
/// without a numeric `DISCORD_CHANNEL_ID` leaves serve running with no
/// Discord approval broker (#1333), which only its log said. `None` when
/// the settings are usable or Discord is not configured. Never echoes the
/// token.
pub fn discord_broker_issue(token: Option<&str>, channel_id: Option<&str>) -> Option<String> {
    token.filter(|t| !t.trim().is_empty())?;
    match channel_id.map(str::trim).filter(|c| !c.is_empty()) {
        None => Some("DISCORD_BOT_TOKEN is set but DISCORD_CHANNEL_ID is not".into()),
        Some(c) if c.parse::<u64>().is_err() => Some(
            "DISCORD_BOT_TOKEN is set but DISCORD_CHANNEL_ID is not a numeric channel ID".into(),
        ),
        Some(_) => None,
    }
}

pub const DISCORD_BROKER_RECOVERY: &str = "Set DISCORD_CHANNEL_ID in .env to the numeric ID of \
     the Discord approvals channel (or remove DISCORD_BOT_TOKEN if you do not use Discord), then \
     restart the daemon (`augmentagent service --unit daemon restart`).";

pub const INSECURE_STORE_RECOVERY: &str = "unset AUGMENTAGENT_INSECURE_CREDENTIAL_DIR (remove \
     it from .env and the service environment), store the Slack app in the Keychain/keyring with \
     `augmentagent slack app install --stdin`, then restart the daemon (`augmentagent service \
     --unit daemon restart`).";

/// Issues this process can see from its own environment (which, like the
/// daemon's, includes `.env` from the working directory).
pub fn static_config_issues(
    env: &dyn Fn(&str) -> Option<String>,
    credentials: &augmentagent_auth::BackendDescription,
) -> Vec<ConfigIssue> {
    let mut out = Vec::new();
    if let Some(why) = discord_broker_issue(
        env("DISCORD_BOT_TOKEN").as_deref(),
        env("DISCORD_CHANNEL_ID").as_deref(),
    ) {
        out.push(ConfigIssue {
            id: DISCORD_BROKER_ISSUE.into(),
            source: "cli",
            severity: "warn".into(),
            detail: format!(
                "{why}: serve runs without the Discord approval broker, so Discord approvals \
                 and queries are off (other surfaces keep running)"
            ),
            recovery: Some(DISCORD_BROKER_RECOVERY.into()),
        });
    }
    if credentials.insecure {
        out.push(ConfigIssue {
            id: INSECURE_STORE_ISSUE.into(),
            source: "cli",
            severity: "error".into(),
            detail: format!(
                "{} is set for this process: credentials are read from and written to plaintext \
                 files (tests and local QA only)",
                augmentagent_auth::INSECURE_FILE_STORE_ENV
            ),
            recovery: Some(INSECURE_STORE_RECOVERY.into()),
        });
    }
    out
}

/// Issues from the daemon's start report; only while that daemon runs.
pub fn daemon_issues(
    report: Option<&augmentagent_store::daemon_report::DaemonRuntimeReport>,
    running: bool,
) -> Vec<ConfigIssue> {
    let Some(r) = report.filter(|_| running) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if r.insecure_credential_store {
        out.push(ConfigIssue {
            id: DAEMON_INSECURE_STORE_ISSUE.into(),
            source: "daemon",
            severity: "error".into(),
            detail: format!(
                "the running daemon (pid {}) reads its credentials from plaintext files: {} was \
                 set when it started",
                r.pid,
                augmentagent_auth::INSECURE_FILE_STORE_ENV
            ),
            recovery: Some(INSECURE_STORE_RECOVERY.into()),
        });
    }
    out.extend(r.notices.iter().map(|n| ConfigIssue {
        id: n.id.clone(),
        source: "daemon",
        severity: n.severity.clone(),
        detail: n.detail.clone(),
        recovery: n.recovery.clone(),
    }));
    out
}

/// #1299 — `serve` records how it started, so `status` and `doctor` in
/// another process can flag a daemon on the plaintext credential store and
/// the Discord approval broker it had to leave off. Never fails serve.
pub fn record_daemon_start(store: &Store, dry_run: bool, discord_broker_error: Option<&str>) {
    let backend = augmentagent_auth::describe_default_store();
    let mut notices = Vec::new();
    if let Some(err) = discord_broker_error {
        notices.push(augmentagent_store::daemon_report::DaemonNotice {
            id: DISCORD_BROKER_ISSUE.into(),
            severity: "warn".into(),
            detail: format!(
                "the Discord approval broker is off: {err}. Discord approvals and queries do \
                 not work; other surfaces keep running"
            ),
            recovery: Some(if err.contains("DISCORD_CHANNEL_ID") {
                DISCORD_BROKER_RECOVERY.into()
            } else {
                "Fix the Discord setting named above in .env, then restart the daemon \
                 (`augmentagent service --unit daemon restart`)."
                    .into()
            }),
        });
    }
    let report = augmentagent_store::daemon_report::DaemonRuntimeReport {
        pid: std::process::id(),
        started_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        dry_run,
        credential_backend: backend.backend.to_string(),
        credential_persistent: backend.persistent,
        insecure_credential_store: backend.insecure,
        notices,
    };
    if let Err(e) = store.put_daemon_runtime_report(&report) {
        tracing::warn!(error = %e, "could not record the daemon start report for status");
    }
}

/// A surface that is supposed to run but is not healthy.
fn interactive_needs_attention(s: &InteractiveStatus) -> bool {
    !s.healthy && !matches!(s.state.as_str(), "not_configured" | "disabled")
}

fn collect_interactive(store: &Store, now_ms: i64) -> Result<BTreeMap<String, InteractiveStatus>> {
    let mut out = BTreeMap::new();
    for &name in INTERACTIVE_SURFACES {
        let platform = augmentagent_store::SurfacePlatform::new(name).expect("static platform");
        let report = store
            .surface_listener_health(&platform)
            .context("interactive surface health")?;
        let owner_bound = !store
            .surface_owner_bindings(&platform)
            .context("surface owner bindings")?
            .is_empty();
        let setup = SlackSetup {
            // The install index slot, checked without reading any secret (no
            // Keychain prompt). Same store the daemon's `slack app` code uses.
            app_installed: augmentagent_auth::Auth::exists(
                augmentagent_channel_slack::app::APP_CREDENTIAL_PLATFORM,
                augmentagent_channel_slack::app::APP_INDEX_ACCOUNT,
            ),
            owner_bound,
            reconnects: store
                .surface_listener_reconnects(&platform)
                .context("surface listener reconnects")?,
            reporter_running: report.as_ref().map(|r| crate::platform::pid_alive(r.pid)),
        };
        out.insert(name.to_string(), interactive_status(report, setup, now_ms));
    }
    Ok(out)
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
            "delivery": Value::Object(
                self.delivery
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect(),
            ),
            "interactive": Value::Object(
                self.interactive
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect(),
            ),
            "credentials": self.credentials.to_json(),
            "daemon_report": self.daemon_report.as_ref().map(DaemonReportStatus::to_json),
            "config_issues": self.config_issues.iter().map(ConfigIssue::to_json).collect::<Vec<_>>(),
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

impl InteractiveStatus {
    fn to_json(&self) -> Value {
        json!({
            "state": self.state,
            "healthy": self.healthy,
            "detail": self.detail,
            "recovery": self.recovery,
            "workspaces": self.workspaces,
            "dry_run": self.dry_run,
            "last_event_unix": self.last_event_unix,
            "last_send_unix": self.last_send_unix,
            "state_since_unix": self.state_since_unix,
            "heartbeat_unix": self.heartbeat_unix,
            "app_installed": self.app_installed,
            "owner_bound": self.owner_bound,
            "credentials": self.credentials,
            "reconnects": self.reconnects,
        })
    }
}

impl DeliveryStatus {
    fn to_json(&self) -> Value {
        json!({
            "inbound_backlog": self.inbound_backlog,
            "inbound_dead_letter": self.inbound_dead_letter,
            "outbound_backlog": self.outbound_backlog,
            "outbound_retrying": self.outbound_retrying,
            "outbound_reconcile": self.outbound_reconcile,
            "outbound_dead_letter": self.outbound_dead_letter,
        })
    }
}

/// Known chat surfaces with zeros, overlaid with whatever the store has
/// (including platforms outside the known list).
fn delivery_map(
    counts: Vec<augmentagent_store::delivery::SurfaceDeliveryCounts>,
) -> BTreeMap<String, DeliveryStatus> {
    let mut out: BTreeMap<String, DeliveryStatus> = DELIVERY_SURFACES
        .iter()
        .map(|name| (name.to_string(), DeliveryStatus::default()))
        .collect();
    for c in counts {
        out.insert(
            c.platform,
            DeliveryStatus {
                inbound_backlog: c.inbound_backlog,
                inbound_dead_letter: c.inbound_dead_letter,
                outbound_backlog: c.outbound_backlog,
                outbound_retrying: c.outbound_retrying,
                outbound_reconcile: c.outbound_reconcile,
                outbound_dead_letter: c.outbound_dead_letter,
            },
        );
    }
    out
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
    let delivery = delivery_map(
        store
            .surface_delivery_counts()
            .context("surface delivery counts")?,
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let interactive = collect_interactive(store, now_ms)?;

    let backend = augmentagent_auth::describe_default_store();
    let env = |k: &str| std::env::var(k).ok();
    let mut config_issues = static_config_issues(&env, &backend);
    let report = store
        .daemon_runtime_report()
        .context("daemon runtime report")?;
    let running = report
        .as_ref()
        .is_some_and(|r| crate::platform::pid_alive(r.pid));
    config_issues.extend(daemon_issues(report.as_ref(), running));
    let daemon_report = report
        .as_ref()
        .map(|r| DaemonReportStatus::from_report(r, running));

    let summary = classify(
        &daemon,
        &dashboard,
        &core_keys,
        &channels,
        &interactive,
        &config_issues,
    );

    Ok(StatusDoc {
        schema_version: SCHEMA_VERSION.to_string(),
        host: "linux".to_string(),
        daemon,
        dashboard,
        updater,
        core_keys,
        channels,
        queue,
        delivery,
        interactive,
        credentials: CredentialsStatus::from(&backend),
        daemon_report,
        config_issues,
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
    if crate::platform::ServiceManager::detect().is_launchd() {
        let unit = unit.to_string();
        return tokio::task::spawn_blocking(move || show_launchd_unit(&unit))
            .await
            .unwrap_or_default();
    }
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

/// #1079 — the launchd reading of the same three properties. A running job
/// is `active`; a timer's job counts as active once loaded, since launchd
/// keeps a scheduled job loaded but idle between runs. The start time comes
/// from the process itself (launchd does not report one).
fn show_launchd_unit(unit: &str) -> UnitProps {
    use crate::platform::{is_timer, launchd_job, launchd_label, process_start_unix};
    let Some(label) = launchd_label(unit) else {
        return UnitProps::default();
    };
    let job = launchd_job(&label);
    let active = if is_timer(unit) { job.loaded } else { job.running() };
    UnitProps {
        active_state: if active { "active" } else { "inactive" }.to_string(),
        sub_state: job.state.clone(),
        active_enter_timestamp_unix: job.pid.and_then(process_start_unix).unwrap_or(0),
    }
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

/// Build the channels map from the gates `Cmd::Serve` actually evaluates
/// (#374). Per channel:
///
///   * `configured` — the credential / prerequisite serve checks is
///     present: keyring slot (read-only `Auth::exists`, no migration side
///     effects), legacy credential file (each channel's `default_auth_path`,
///     honouring its env override), or store rows.
///   * `armed` — serve would run a poller/listener for this channel right
///     now. Channels serve never spawns (twitter, instagram, whatsapp,
///     telegram inbound, calendar, contacts) report `armed: false` even
///     when their credential is present — posting/CLI surfaces still work,
///     but nothing polls.
///
/// Caveat shared with the daemon: `Auth::exists` treats a keyring platform
/// failure as "present" (it can't distinguish unreachable from missing
/// without reading the secret); `doctor`'s `keyring_reachable` check covers
/// that failure mode.
fn collect_channels(
    store: &Store,
    cfg: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, ChannelStatus>> {
    use augmentagent_auth::{Auth, DEFAULT_ACCOUNT};

    let mut out = BTreeMap::new();
    let repo_root = std::env::current_dir().unwrap_or_default();
    let composio = cfg_or_env(cfg, "composio_api_key", "COMPOSIO_API_KEY");
    let gmail_accounts: u32 = store
        .get_active_gmail_accounts()
        .map(|v| v.len() as u32)
        .unwrap_or(0);
    let socialapi_accounts: u32 = store
        .active_socialapi_account_ids()
        .map(|v| v.len() as u32)
        .unwrap_or(0);

    for &name in KNOWN_CHANNELS {
        let (configured, armed, accounts) = match name {
            // Serve spawns the gmail channel whenever the Composio key is
            // present (`build_channel`); accounts are enumerated per poll.
            "gmail" => (composio, composio, gmail_accounts),
            // Serve arms slack when ≥1 workspace row exists; an empty table
            // falls back to the default keyring slot (`load_slack_clients`).
            "slack" => {
                let workspaces = store
                    .list_active_slack_workspaces()
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);
                let c = workspaces > 0 || Auth::exists("slack", DEFAULT_ACCOUNT);
                (c, c, workspaces)
            }
            // Discord-DM channel: keyring, else the creds file at
            // `default_creds_path` (AUGMENTAGENT_DISCORD_CREDS override).
            "discord" => {
                let c = Auth::exists("discord", DEFAULT_ACCOUNT)
                    || augmentagent_channel_discord_dm::auth::default_creds_path(&repo_root)
                        .exists();
                (c, c, 0)
            }
            // Session present ⇒ posting + publisher arm work, but serve
            // runs no twitter poller — inbound is CLI `poll-once` only.
            "twitter" => {
                let c = Auth::exists("twitter", DEFAULT_ACCOUNT)
                    || augmentagent_channel_twitter::auth::default_auth_path(&repo_root)
                        .exists();
                (c, false, 0)
            }
            // One auth gate arms every LinkedIn serve task (DM poll, feed +
            // own-post + friend-feed engagement, invite triage).
            "linkedin" => {
                let c = Auth::exists("linkedin", DEFAULT_ACCOUNT)
                    || augmentagent_channel_linkedin::auth::default_auth_path(&repo_root)
                        .exists();
                (c, c, 0)
            }
            // Keyring slot is keyed by ds_user_id (not enumerable without
            // reading it), so probe the auth file path only. Path mirrors
            // the instagram crate's `default_auth_path` (env override, then
            // repo root) — the crate isn't a dependency of the CLI, and
            // adding one for an unwired channel isn't worth it. Not in
            // serve at all — never armed.
            "instagram" => {
                let path = std::env::var("AUGMENTAGENT_INSTAGRAM_AUTH")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| repo_root.join("instagram-auth.json"));
                (path.exists(), false, 0)
            }
            "reddit" => {
                let c = augmentagent_channel_reddit::RedditAuth::exists();
                (c, c, 0)
            }
            // Serve loads whichever PAT slot `AUGMENTAGENT_GITHUB_LOGIN`
            // names, falling back to `default` (`load_any_github_auth`).
            "github" => {
                let login = std::env::var("AUGMENTAGENT_GITHUB_LOGIN")
                    .unwrap_or_else(|_| DEFAULT_ACCOUNT.to_string());
                let c = Auth::exists("github", &login);
                (c, c, 0)
            }
            // No credential — serve arms meetup iff ≥1 active subscription.
            "meetup" => {
                let subs = store
                    .list_active_subscriptions("meetup")
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);
                (subs > 0, subs > 0, subs)
            }
            // Bot rows enable outbound replies via the approver, but the
            // inbound long-poll is CLI `telegram-bot poll-once` only.
            "telegram" => {
                let bots = store
                    .list_active_telegram_bots()
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);
                (bots > 0, false, bots)
            }
            // Crate compiles but `Cmd::Whatsapp` is unimplemented and serve
            // has no wiring — credential presence is all we can report.
            "whatsapp" => (Auth::exists("whatsapp", DEFAULT_ACCOUNT), false, 0),
            // Driven by the external `augmentagent-calendar.timer`, not
            // serve; `doctor`'s `calendar_scheduled` check covers the timer.
            "calendar" => (composio && gmail_accounts > 0, false, 0),
            "voice" => {
                use augmentagent_channel_voice::{
                    default_allowlist_path, load_allowlist, load_token,
                };
                let c = load_token().is_some()
                    && !load_allowlist(&default_allowlist_path()).is_empty();
                (c, c, 0)
            }
            "gdrive" => {
                let drive = store
                    .get_active_drive_accounts()
                    .map(|v| v.len() as u32)
                    .unwrap_or(0);
                let c = composio && drive > 0;
                (c, c, drive)
            }
            // CLI `contacts sync` only; serve has no contacts task.
            "contacts" => (
                cfg_or_env(cfg, "carddav_url", "CARDDAV_URL") || composio,
                false,
                0,
            ),
            // Serve arms the socialapi pollers on the key alone
            // (`SocialApiAuth::load_with_store`: env, else keyring, else the
            // sqlite config row the dashboard writes); they idle until
            // accounts/posts are registered.
            "socialapi" => {
                let key = cfg_or_env(cfg, "socialapi_api_key", "SOCIALAPI_API_KEY")
                    || Auth::exists("socialapi", DEFAULT_ACCOUNT);
                (key, key, socialapi_accounts)
            }
            _ => (false, false, 0),
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
                armed,
                accounts,
                last_poll_unix: None, // future: per-channel last-poll table.
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
    interactive: &BTreeMap<String, InteractiveStatus>,
    config_issues: &[ConfigIssue],
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
    // #1287 — a configured interactive surface whose listener is down.
    if interactive.values().any(interactive_needs_attention) {
        return summary::DEGRADED.into();
    }
    // #1299 — a configuration problem the operator has to fix.
    if !config_issues.is_empty() {
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

    println!("\ndelivery:");
    for (name, d) in &doc.delivery {
        println!(
            "  {:<10} inbound {} dead {} | outbound {} retrying {} reconcile {} dead {}",
            name,
            d.inbound_backlog,
            d.inbound_dead_letter,
            d.outbound_backlog,
            d.outbound_retrying,
            d.outbound_reconcile,
            d.outbound_dead_letter
        );
    }
    print_interactive(doc);
}

fn print_interactive(doc: &StatusDoc) {
    println!("\ninteractive:");
    for (name, i) in &doc.interactive {
        let when = |t: Option<i64>| t.map_or_else(|| "never".to_string(), |t| t.to_string());
        println!(
            "  {:<10} {}{} | last event {} | last send {}{}",
            name,
            i.state,
            if i.healthy { " (healthy)" } else { "" },
            when(i.last_event_unix),
            when(i.last_send_unix),
            if i.dry_run { " | dry-run" } else { "" },
        );
        if let Some(detail) = &i.detail {
            println!("  {:<10} {detail}", "");
        }
        if let Some(recovery) = &i.recovery {
            println!("  {:<10} fix: {recovery}", "");
        }
        println!(
            "  {:<10} app installed {} | owner bound {} | credentials {} | reconnects {}",
            "",
            yn(i.app_installed),
            yn(i.owner_bound),
            i.credentials,
            i.reconnects
                .map_or_else(|| "-".to_string(), |n| n.to_string()),
        );
    }
    println!(
        "\ncredentials: {}{}{}",
        doc.credentials.backend,
        if doc.credentials.persistent {
            ""
        } else {
            " (NOT persistent)"
        },
        if doc.credentials.insecure_file_store {
            " (INSECURE plaintext files)"
        } else {
            ""
        },
    );
    if let Some(d) = &doc.daemon_report {
        println!(
            "daemon report: pid {} {} | credentials {}{}",
            d.pid,
            if d.running { "running" } else { "not running" },
            d.credential_backend,
            if d.insecure_file_store {
                " (INSECURE plaintext files)"
            } else {
                ""
            },
        );
    }
    if !doc.config_issues.is_empty() {
        println!("\nconfig issues:");
        for i in &doc.config_issues {
            println!("  [{}] {} ({}): {}", i.severity, i.id, i.source, i.detail);
            if let Some(r) = &i.recovery {
                println!("      fix: {r}");
            }
        }
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
        assert_eq!(
            classify(&d, &dash, &c, &empty_channels(), &BTreeMap::new(), &[]),
            summary::DAEMON_DOWN
        );
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
        assert_eq!(
            classify(&d, &dash, &c, &empty_channels(), &BTreeMap::new(), &[]),
            summary::DASHBOARD_DOWN
        );
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
        assert_eq!(
            classify(&d, &dash, &c, &channels, &BTreeMap::new(), &[]),
            summary::NEEDS_SETUP
        );
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
        assert_eq!(
            classify(&d, &dash, &c, &channels, &BTreeMap::new(), &[]),
            summary::DEGRADED
        );
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
        assert_eq!(
            classify(&d, &dash, &c, &channels, &BTreeMap::new(), &[]),
            summary::OK
        );
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
    fn delivery_lists_chat_surfaces_with_zeros_and_keeps_other_platforms() {
        use augmentagent_store::delivery::SurfaceDeliveryCounts;
        let map = delivery_map(vec![
            SurfaceDeliveryCounts {
                platform: "slack".into(),
                outbound_dead_letter: 2,
                ..Default::default()
            },
            SurfaceDeliveryCounts {
                platform: "telegram".into(),
                inbound_backlog: 1,
                ..Default::default()
            },
        ]);
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["discord", "slack", "telegram", "whatsapp"]
        );
        assert_eq!(map["slack"].outbound_dead_letter, 2);
        assert_eq!(map["discord"], DeliveryStatus::default());
        assert_eq!(
            map["telegram"].to_json(),
            json!({
                "inbound_backlog": 1,
                "inbound_dead_letter": 0,
                "outbound_backlog": 0,
                "outbound_retrying": 0,
                "outbound_reconcile": 0,
                "outbound_dead_letter": 0,
            })
        );
    }

    // --- #1287 interactive surfaces --------------------------------------

    use augmentagent_store::surface_health::SurfaceListenerHealth;

    const NOW_MS: i64 = 1_700_000_100_000;

    fn setup(app_installed: bool, owner_bound: bool) -> SlackSetup {
        SlackSetup {
            app_installed,
            owner_bound,
            reconnects: None,
            reporter_running: None,
        }
    }

    fn report(state: &str, heartbeat_age_ms: i64) -> SurfaceListenerHealth {
        SurfaceListenerHealth {
            platform: augmentagent_store::SurfacePlatform::new("slack").unwrap(),
            state: state.into(),
            detail: None,
            recovery: None,
            workspaces: vec!["T00000001".into()],
            dry_run: false,
            last_event_at_ms: Some(NOW_MS - 5_000),
            last_send_at_ms: Some(NOW_MS - 4_000),
            state_since_ms: NOW_MS - 60_000,
            heartbeat_at_ms: NOW_MS - heartbeat_age_ms,
            pid: 4242,
        }
    }

    #[test]
    fn interactive_without_a_report_or_owner_is_not_configured() {
        let s = interactive_status(None, setup(false, false), NOW_MS);
        assert_eq!(s.state, "not_configured");
        assert!(!s.healthy);
        assert!(s.recovery.unwrap().contains("slack app install"));
    }

    #[test]
    fn interactive_with_an_owner_but_no_report_is_disconnected() {
        let s = interactive_status(None, setup(true, true), NOW_MS);
        assert_eq!(s.state, "disconnected");
        assert!(!s.healthy);
        assert!(s
            .recovery
            .unwrap()
            .contains("service --unit daemon restart"));
    }

    #[test]
    fn a_fresh_connected_report_is_healthy_with_times_in_seconds() {
        let s = interactive_status(Some(report("connected", 1_000)), setup(true, true), NOW_MS);
        assert_eq!(s.state, "connected");
        assert!(s.healthy);
        assert_eq!(s.last_event_unix, Some((NOW_MS - 5_000) / 1000));
        assert_eq!(s.last_send_unix, Some((NOW_MS - 4_000) / 1000));
        assert_eq!(s.workspaces, vec!["T00000001".to_string()]);
    }

    #[test]
    fn a_connected_report_with_a_stale_heartbeat_is_never_healthy() {
        let s = interactive_status(
            Some(report("connected", 10 * 60_000)),
            setup(true, true),
            NOW_MS,
        );
        assert_eq!(s.state, "disconnected");
        assert!(!s.healthy);
        assert!(s.detail.unwrap().contains("heartbeat"));
        assert!(s.recovery.unwrap().contains("restart"));
    }

    #[test]
    fn reconnecting_and_misconfigured_are_not_healthy_and_keep_recovery() {
        let s = interactive_status(
            Some(report("reconnecting", 1_000)),
            setup(true, true),
            NOW_MS,
        );
        assert!(!s.healthy);
        assert_eq!(s.state, "reconnecting");
        let mut bad = report("misconfigured", 1_000);
        bad.recovery = Some("Bind yourself as owner".into());
        let s = interactive_status(Some(bad), setup(false, false), NOW_MS);
        assert_eq!(s.state, "misconfigured");
        assert_eq!(s.recovery.as_deref(), Some("Bind yourself as owner"));
        // An inactive report never goes stale: nothing is meant to run.
        let s = interactive_status(
            Some(report("disabled", 10 * 60_000)),
            setup(true, true),
            NOW_MS,
        );
        assert_eq!(s.state, "disabled");
    }

    fn ok_daemon() -> (
        DaemonStatus,
        DashboardStatus,
        CoreKeys,
        BTreeMap<String, ChannelStatus>,
    ) {
        let mut channels = BTreeMap::new();
        for &n in KNOWN_CHANNELS {
            channels.insert(n.to_string(), ch(n == "gmail"));
        }
        (
            DaemonStatus {
                unit: "x".into(),
                active: true,
                since_unix: 0,
            },
            DashboardStatus {
                unit: "x".into(),
                active: true,
                port: 3000,
                reachable: true,
            },
            CoreKeys {
                composio: true,
                groq: true,
                cerebras: false,
                discord_bot: false,
            },
            channels,
        )
    }

    #[test]
    fn an_unhealthy_enabled_interactive_surface_degrades_the_summary() {
        let (d, dash, c, channels) = ok_daemon();
        let mut interactive = BTreeMap::new();
        interactive.insert(
            "slack".to_string(),
            interactive_status(
                Some(report("reconnecting", 1_000)),
                setup(true, true),
                NOW_MS,
            ),
        );
        assert_eq!(
            classify(&d, &dash, &c, &channels, &interactive, &[]),
            summary::DEGRADED
        );
        interactive.insert(
            "slack".to_string(),
            interactive_status(Some(report("connected", 1_000)), setup(true, true), NOW_MS),
        );
        assert_eq!(
            classify(&d, &dash, &c, &channels, &interactive, &[]),
            summary::OK
        );
        interactive.insert(
            "slack".to_string(),
            interactive_status(None, setup(false, false), NOW_MS),
        );
        assert_eq!(
            classify(&d, &dash, &c, &channels, &interactive, &[]),
            summary::OK
        );
    }

    #[test]
    fn interactive_json_shape() {
        let s = interactive_status(Some(report("connected", 1_000)), setup(true, true), NOW_MS);
        let v = s.to_json();
        for key in [
            "state",
            "healthy",
            "detail",
            "recovery",
            "workspaces",
            "dry_run",
            "last_event_unix",
            "last_send_unix",
            "state_since_unix",
            "heartbeat_unix",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
    }

    // --- #1299 Slack diagnostics -----------------------------------------

    #[test]
    fn with_nothing_installed_the_detail_names_the_install_step() {
        let s = interactive_status(None, setup(false, false), NOW_MS);
        assert_eq!(s.state, "not_configured");
        assert_eq!(
            s.detail.as_deref(),
            Some("no interactive Slack app is installed")
        );
        assert!(s.recovery.unwrap().contains("slack app install --stdin"));
        assert!(!s.app_installed && !s.owner_bound);
        assert_eq!(s.credentials, "missing");
    }

    #[test]
    fn installed_without_an_owner_names_the_bind_step_not_the_install() {
        let s = interactive_status(None, setup(true, false), NOW_MS);
        assert_eq!(s.state, "not_configured");
        assert_eq!(
            s.detail.as_deref(),
            Some("the Slack app is installed but no owner is bound")
        );
        let recovery = s.recovery.unwrap();
        assert!(
            recovery.contains("slack app owner bind --user"),
            "{recovery}"
        );
        assert!(!recovery.contains("slack app install"), "{recovery}");
        assert!(s.app_installed && !s.owner_bound);
        assert_eq!(s.credentials, "present");
    }

    #[test]
    fn an_owner_without_an_install_names_the_install_step() {
        let s = interactive_status(None, setup(false, true), NOW_MS);
        assert_eq!(s.state, "not_configured");
        assert!(s.detail.unwrap().contains("no Slack app is installed"));
        assert!(s.recovery.unwrap().contains("slack app install --stdin"));
    }

    #[test]
    fn credentials_are_usable_only_when_a_fresh_connected_report_proves_it() {
        let usable =
            interactive_status(Some(report("connected", 1_000)), setup(true, true), NOW_MS);
        assert_eq!(usable.credentials, "usable");
        for (r, why) in [
            (Some(report("connected", 10 * 60_000)), "stale report"),
            (Some(report("reconnecting", 1_000)), "reconnecting"),
            (Some(report("connecting", 1_000)), "connecting"),
            (Some(report("misconfigured", 1_000)), "misconfigured"),
            (None, "no report"),
        ] {
            let s = interactive_status(r, setup(true, true), NOW_MS);
            assert_eq!(s.credentials, "present", "{why}");
        }
        // The daemon proved them even if this process cannot see them.
        let s = interactive_status(Some(report("connected", 1_000)), setup(false, true), NOW_MS);
        assert_eq!(s.credentials, "usable");
    }

    #[test]
    fn a_live_report_from_a_daemon_that_is_gone_is_disconnected_at_once() {
        // Heartbeat still fresh, but the pid that wrote it has exited (a
        // SIGTERM or crash leaves no `stopped` report): not healthy.
        let mut gone = setup(true, true);
        gone.reporter_running = Some(false);
        for state in ["connected", "reconnecting", "connecting"] {
            let s = interactive_status(Some(report(state, 1_000)), gone.clone(), NOW_MS);
            assert_eq!(s.state, "disconnected", "{state}");
            assert!(!s.healthy);
            assert_eq!(s.credentials, "present");
            assert!(s.detail.unwrap().contains("pid 4242"));
            assert!(s
                .recovery
                .unwrap()
                .contains("service --unit daemon restart"));
        }
        // An inactive report is kept as written.
        let s = interactive_status(Some(report("stopped", 1_000)), gone.clone(), NOW_MS);
        assert_eq!(s.state, "stopped");
        // Unknown liveness falls back to the heartbeat rule.
        let s = interactive_status(Some(report("connected", 1_000)), setup(true, true), NOW_MS);
        assert!(s.healthy);
        let mut alive = setup(true, true);
        alive.reporter_running = Some(true);
        let s = interactive_status(Some(report("connected", 1_000)), alive, NOW_MS);
        assert!(s.healthy);
    }

    #[test]
    fn reconnects_come_from_the_store_and_are_null_without_a_report() {
        let mut with_count = setup(true, true);
        with_count.reconnects = Some(3);
        let s = interactive_status(Some(report("connected", 1_000)), with_count.clone(), NOW_MS);
        assert_eq!(s.reconnects, Some(3));
        let s = interactive_status(None, with_count, NOW_MS);
        assert_eq!(s.reconnects, None);
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn keychain() -> augmentagent_auth::BackendDescription {
        augmentagent_auth::BackendDescription {
            backend: "macos-keychain",
            persistent: true,
            insecure: false,
            note: None,
        }
    }

    #[test]
    fn a_discord_token_without_a_usable_channel_id_is_a_config_issue_with_recovery() {
        for env in [
            env_of(&[("DISCORD_BOT_TOKEN", "synthetic-not-a-token")]),
            env_of(&[
                ("DISCORD_BOT_TOKEN", "synthetic-not-a-token"),
                ("DISCORD_CHANNEL_ID", "general"),
            ]),
        ] {
            let issues = static_config_issues(&env, &keychain());
            let i = issues
                .iter()
                .find(|i| i.id == "discord.approval_broker")
                .expect("discord issue");
            assert_eq!(i.source, "cli");
            assert_eq!(i.severity, "warn");
            assert!(i.detail.contains("DISCORD_CHANNEL_ID"));
            assert!(
                !i.detail.contains("synthetic-not-a-token"),
                "never echo the token"
            );
            assert!(i
                .recovery
                .as_deref()
                .unwrap()
                .contains("service --unit daemon restart"));
        }
        for fine in [
            env_of(&[]),
            env_of(&[("DISCORD_BOT_TOKEN", "")]),
            env_of(&[
                ("DISCORD_BOT_TOKEN", "synthetic-not-a-token"),
                ("DISCORD_CHANNEL_ID", "123456789"),
            ]),
        ] {
            assert!(static_config_issues(&fine, &keychain()).is_empty());
        }
    }

    #[test]
    fn the_insecure_credential_store_in_this_process_is_an_error_issue() {
        let insecure =
            augmentagent_auth::describe_store_for_override(Some(std::ffi::OsStr::new("/x")));
        let issues = static_config_issues(&env_of(&[]), &insecure);
        let i = issues
            .iter()
            .find(|i| i.id == "credentials.insecure_file_store")
            .expect("insecure issue");
        assert_eq!(i.severity, "error");
        assert!(i.detail.contains("AUGMENTAGENT_INSECURE_CREDENTIAL_DIR"));
        assert!(i
            .recovery
            .as_deref()
            .unwrap()
            .contains("unset AUGMENTAGENT_INSECURE_CREDENTIAL_DIR"));
    }

    fn daemon_report(insecure: bool) -> augmentagent_store::daemon_report::DaemonRuntimeReport {
        augmentagent_store::daemon_report::DaemonRuntimeReport {
            pid: 4242,
            started_at_ms: NOW_MS - 60_000,
            dry_run: false,
            credential_backend: if insecure {
                "insecure-file"
            } else {
                "macos-keychain"
            }
            .into(),
            credential_persistent: true,
            insecure_credential_store: insecure,
            notices: vec![augmentagent_store::daemon_report::DaemonNotice {
                id: "discord.approval_broker".into(),
                severity: "warn".into(),
                detail: "Discord approval broker disabled".into(),
                recovery: Some("Set DISCORD_CHANNEL_ID".into()),
            }],
        }
    }

    #[test]
    fn a_running_daemons_report_adds_its_notices_and_flags_an_insecure_store() {
        let r = daemon_report(true);
        let issues = daemon_issues(Some(&r), true);
        let ids: Vec<&str> = issues.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            ["daemon.insecure_file_store", "discord.approval_broker"]
        );
        assert!(issues.iter().all(|i| i.source == "daemon"));
        assert_eq!(issues[0].severity, "error");
        // A daemon that is no longer running reports nothing current.
        assert!(daemon_issues(Some(&r), false).is_empty());
        assert!(daemon_issues(None, true).is_empty());
        let clean = daemon_issues(Some(&daemon_report(false)), true);
        assert_eq!(clean.len(), 1);
    }

    #[test]
    fn any_config_issue_degrades_an_otherwise_ok_summary() {
        let (d, dash, c, channels) = ok_daemon();
        let interactive = BTreeMap::new();
        let issues = daemon_issues(Some(&daemon_report(false)), true);
        assert_eq!(
            classify(&d, &dash, &c, &channels, &interactive, &issues),
            summary::DEGRADED
        );
    }

    #[test]
    fn json_carries_the_1299_fields() {
        let mut with_count = setup(true, true);
        with_count.reconnects = Some(2);
        let v = interactive_status(Some(report("connected", 1_000)), with_count, NOW_MS).to_json();
        assert_eq!(v["app_installed"], true);
        assert_eq!(v["owner_bound"], true);
        assert_eq!(v["credentials"], "usable");
        assert_eq!(v["reconnects"], 2);
        let issue = &daemon_issues(Some(&daemon_report(true)), true)[0];
        let j = issue.to_json();
        for key in ["id", "source", "severity", "detail", "recovery"] {
            assert!(j.get(key).is_some(), "missing {key}");
        }
        let d = DaemonReportStatus::from_report(&daemon_report(true), true).to_json();
        for key in [
            "pid",
            "running",
            "started_unix",
            "dry_run",
            "credential_backend",
            "credential_persistent",
            "insecure_file_store",
        ] {
            assert!(d.get(key).is_some(), "missing {key}");
        }
        let c = CredentialsStatus::from(&keychain()).to_json();
        for key in ["backend", "persistent", "insecure_file_store", "note"] {
            assert!(c.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn known_channels_covers_per_channel_cmd_set() {
        // If main.rs gains a new top-level Cmd::* channel variant, this
        // list must grow with it so `status` doesn't silently omit it.
        for required in [
            "gmail", "slack", "discord", "twitter", "linkedin", "instagram", "reddit",
            "github", "meetup", "telegram", "whatsapp", "calendar", "voice", "gdrive",
            "contacts", "socialapi",
        ] {
            assert!(
                KNOWN_CHANNELS.contains(&required),
                "missing channel: {required}"
            );
        }
    }
}
