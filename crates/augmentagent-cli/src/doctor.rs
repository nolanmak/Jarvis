//! `augmentagent doctor` — read-only diagnostic checks (#11).
//!
//! Composes the `status` aggregator (#1) with a handful of additional
//! liveness probes (sqlite integrity, keyring reachability, tool binaries
//! on `$PATH`, build freshness, `.env` presence). Each check emits a
//! `Finding { name, severity, message, suggested_cmd }`; the run terminates
//! with exit code 0 when no error-severity findings are produced (warns are
//! tolerated) and 1 otherwise.
//!
//! `--deep` adds slower probes:
//!   * `composio_api`        — whoami-style ping against Composio (5s timeout)
//!   * `per_channel_validate` — one finding per configured channel, sourced
//!                              from `status::collect` (read-only).
//!
//! `--fix` is intentionally NOT implemented here — it lands as a follow-up
//! issue. Doctor stays strictly read-only.
//!
//! Linux-only by design — uses `secret-tool` (libsecret) and probes the
//! systemd-user dashboard unit indirectly through `status`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::timeout;

use augmentagent_store::{rusqlite, Store};

use crate::status;

/// Severity tag attached to every finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Error,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }

    fn icon(self) -> &'static str {
        // No emojis (per project convention). Plain unicode glyphs only.
        match self {
            Severity::Ok => "\u{2713}", // ✓
            Severity::Warn => "!",
            Severity::Error => "\u{2717}", // ✗
        }
    }
}

/// One diagnostic result. Serialised verbatim into the `--json` payload.
#[derive(Debug, Clone)]
pub struct Finding {
    pub name: String,
    pub severity: Severity,
    pub message: String,
    pub suggested_cmd: Option<String>,
}

impl Finding {
    fn ok(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            severity: Severity::Ok,
            message: message.into(),
            suggested_cmd: None,
        }
    }

    fn warn(name: &str, message: impl Into<String>, suggested: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            severity: Severity::Warn,
            message: message.into(),
            suggested_cmd: suggested.map(|s| s.to_string()),
        }
    }

    fn error(name: &str, message: impl Into<String>, suggested: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            severity: Severity::Error,
            message: message.into(),
            suggested_cmd: suggested.map(|s| s.to_string()),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "severity": self.severity.as_str(),
            "message": self.message,
            "suggested_cmd": self.suggested_cmd,
        })
    }
}

/// Entry point. `json = None` auto-detects (JSON when stdout piped).
pub async fn run(store: Arc<Store>, json: Option<bool>, deep: bool) -> Result<i32> {
    let mut findings: Vec<Finding> = Vec::new();

    // --- Compose the status aggregator. Doctor doesn't duplicate status's
    // probe logic; we just call `status::collect` and lift key signals out as
    // findings. Failures here are non-fatal (we still want the dedicated
    // probes below to run on a corrupt db).
    let status_doc = match status::collect(&store).await {
        Ok(doc) => Some(doc),
        Err(e) => {
            findings.push(Finding::warn(
                "status_collect",
                format!("status::collect failed: {e}"),
                Some("augmentagent status --json"),
            ));
            None
        }
    };

    // 1. sqlite_open + integrity_check
    findings.push(check_sqlite_open().await);
    // 2. sqlite_migrated — core tables exist
    findings.push(check_sqlite_migrated().await);
    // 3. keyring_reachable — secret-tool present + libsecret reachable
    findings.push(check_keyring_reachable().await);
    // 4. dashboard_reachable — sourced from the status doc when available
    findings.push(check_dashboard_reachable(&status_doc).await);
    // 5. claude_cli_in_path
    findings.push(
        check_which(
            "claude_cli_in_path",
            &std::env::var("CLAUDE_CLI").unwrap_or_else(|_| "claude".to_string()),
            Some("install the Claude CLI: see https://docs.claude.com/claude-code/install"),
        )
        .await,
    );
    // 6. python3_in_path
    findings.push(
        check_which(
            "python3_in_path",
            "python3",
            Some("apt-get install -y python3"),
        )
        .await,
    );
    // 7. node_in_path
    findings.push(
        check_which(
            "node_in_path",
            "node",
            Some("install node (e.g. nvm install --lts)"),
        )
        .await,
    );
    // 8. rust_binary_freshness
    findings.push(check_rust_binary_freshness().await);
    // 9. dashboard_build_present
    findings.push(check_dashboard_build_present().await);
    // 10. env_file_present
    findings.push(check_env_file_present());
    // 11. socialapi — key present? accounts active? (#245)
    findings.push(check_socialapi(&store));
    // 12. calendar — configured (Composio + gmail entities) but unscheduled? (#376)
    findings.push(check_calendar_scheduled(&store));

    // --- Deep checks (off by default).
    if deep {
        findings.push(check_composio_api().await);
        findings.extend(check_per_channel_validate(&status_doc));
    }

    // Tally severities.
    let mut ok = 0usize;
    let mut warn = 0usize;
    let mut error = 0usize;
    for f in &findings {
        match f.severity {
            Severity::Ok => ok += 1,
            Severity::Warn => warn += 1,
            Severity::Error => error += 1,
        }
    }
    let exit_code = if error > 0 { 1 } else { 0 };

    let want_json = json.unwrap_or_else(|| !std::io::stdout().is_terminal());
    if want_json {
        let payload = json!({
            "checks": findings.iter().map(|f| f.to_json()).collect::<Vec<_>>(),
            "summary": { "ok": ok, "warn": warn, "error": error },
            "exit_code": exit_code,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_table(&findings, ok, warn, error);
    }

    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// Individual checks.
// ---------------------------------------------------------------------------

const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(2);

async fn check_sqlite_open() -> Finding {
    let db_path = std::env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string());
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            return Finding::error(
                "sqlite_open",
                format!("could not open {db_path}: {e}"),
                Some("augmentagent status --json"),
            );
        }
    };
    let integrity: rusqlite::Result<String> =
        conn.query_row("PRAGMA integrity_check", [], |r| r.get(0));
    match integrity {
        Ok(v) if v == "ok" => Finding::ok(
            "sqlite_open",
            format!("{db_path}: integrity_check ok"),
        ),
        Ok(v) => Finding::error(
            "sqlite_open",
            format!("{db_path}: integrity_check returned {v}"),
            Some("sqlite3 \"$AUGMENTAGENT_DB\" 'PRAGMA integrity_check;'"),
        ),
        Err(e) => Finding::error(
            "sqlite_open",
            format!("{db_path}: integrity_check failed: {e}"),
            Some("sqlite3 \"$AUGMENTAGENT_DB\" 'PRAGMA integrity_check;'"),
        ),
    }
}

async fn check_sqlite_migrated() -> Finding {
    let db_path = std::env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string());
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            return Finding::error(
                "sqlite_migrated",
                format!("could not open {db_path}: {e}"),
                Some("augmentagent status --json"),
            );
        }
    };

    // `subscriptions` is named `channel_subscriptions` in the store. We accept
    // either name to stay friendly with the spec's plain-english listing.
    let mut required: Vec<&str> = vec!["actions", "config"];
    let has_subscriptions = table_exists(&conn, "channel_subscriptions").unwrap_or(false)
        || table_exists(&conn, "subscriptions").unwrap_or(false);

    let mut missing: Vec<&str> = Vec::new();
    for t in &required.clone() {
        if !table_exists(&conn, t).unwrap_or(false) {
            missing.push(t);
        }
    }
    if !has_subscriptions {
        missing.push("channel_subscriptions");
    }
    required.push("channel_subscriptions");

    if missing.is_empty() {
        Finding::ok(
            "sqlite_migrated",
            format!("core tables present: {}", required.join(", ")),
        )
    } else {
        Finding::error(
            "sqlite_migrated",
            format!("missing core tables: {}", missing.join(", ")),
            Some("augmentagent service restart --unit daemon"),
        )
    }
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

async fn check_keyring_reachable() -> Finding {
    // `secret-tool lookup augmentagent _probe`
    //   * exit 0     → probe entry exists (unlikely but ok)
    //   * exit !=0   → keyring reachable, just no probe entry (the "No such
    //                 schema" / "No matching results" case). Still ok.
    //   * ENOENT for the binary itself → error (libsecret tooling missing).
    let res = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("secret-tool")
            .args(["lookup", "augmentagent", "_probe"])
            .output(),
    )
    .await;
    match res {
        Ok(Ok(_out)) => Finding::ok(
            "keyring_reachable",
            "secret-tool ran (libsecret reachable)".to_string(),
        ),
        Ok(Err(e)) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                Finding::error(
                    "keyring_reachable",
                    "secret-tool not on $PATH (libsecret-tools missing)".to_string(),
                    Some("apt-get install -y libsecret-tools"),
                )
            } else {
                Finding::warn(
                    "keyring_reachable",
                    format!("secret-tool spawn failed: {e}"),
                    Some("apt-get install -y libsecret-tools"),
                )
            }
        }
        Err(_) => Finding::warn(
            "keyring_reachable",
            "secret-tool timed out after 2s".to_string(),
            None,
        ),
    }
}

async fn check_dashboard_reachable(status_doc: &Option<status::StatusDoc>) -> Finding {
    // Prefer the status doc — it just probed `/api/v1/stats` on our behalf.
    // If status::collect failed earlier, fall back to a direct GET (same
    // shape as `status::dashboard_reachable`, kept local to avoid widening
    // the status module's public surface).
    let port: u16 = std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);

    let reachable = match status_doc {
        Some(doc) => doc.dashboard.reachable,
        None => probe_dashboard_direct(port).await,
    };

    if reachable {
        Finding::ok(
            "dashboard_reachable",
            format!("http://127.0.0.1:{port}/api/v1/stats responded"),
        )
    } else {
        Finding::error(
            "dashboard_reachable",
            format!(
                "no response from http://127.0.0.1:{port}/api/v1/stats (and /api/v1/health)"
            ),
            Some("augmentagent service start --unit dashboard"),
        )
    }
}

/// Local fallback dashboard probe. Mirrors `status::dashboard_reachable` but
/// also tries `/api/v1/health` (the endpoint added by #10) before giving up,
/// so doctor stays correct whichever lands first.
async fn probe_dashboard_direct(port: u16) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    for path in ["/api/v1/stats", "/api/v1/health"] {
        let url = format!("http://127.0.0.1:{port}{path}");
        let mut req = client.get(&url);
        if let Ok(key) = std::env::var("AUGMENTAGENT_API_KEY") {
            if !key.is_empty() {
                req = req.header("x-api-key", key);
            }
        }
        if let Ok(resp) = req.send().await {
            let s = resp.status();
            if s.is_success() || s.as_u16() == 401 {
                return true;
            }
        }
    }
    false
}

async fn check_which(name: &str, binary: &str, suggested: Option<&str>) -> Finding {
    let res = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("which").arg(binary).output(),
    )
    .await;
    match res {
        Ok(Ok(out)) if out.status.success() => {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Finding::ok(name, format!("{binary} -> {path}"))
        }
        Ok(Ok(_)) => Finding::error(name, format!("{binary} not on $PATH"), suggested),
        Ok(Err(e)) => Finding::error(name, format!("which {binary} failed: {e}"), suggested),
        Err(_) => Finding::warn(
            name,
            format!("which {binary} timed out after 2s"),
            suggested,
        ),
    }
}

async fn check_rust_binary_freshness() -> Finding {
    // Locate the release binary. Two locations are common: the installed
    // shim resolved from `which augmentagent` (preferred for a deployed
    // box), or `target/release/augmentagent` under the repo root.
    let candidate = match resolve_release_binary().await {
        Some(p) => p,
        None => {
            return Finding::warn(
                "rust_binary_freshness",
                "could not locate release binary on disk".to_string(),
                None,
            );
        }
    };
    let mtime = match std::fs::metadata(&candidate).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(e) => {
            return Finding::warn(
                "rust_binary_freshness",
                format!("stat {} failed: {e}", candidate.display()),
                None,
            );
        }
    };
    let age = SystemTime::now()
        .duration_since(mtime)
        .unwrap_or(Duration::ZERO);
    let days = age.as_secs() / 86_400;
    if days > 7 {
        Finding::warn(
            "rust_binary_freshness",
            format!(
                "{} is {} days old (> 7d)",
                candidate.display(),
                days
            ),
            Some("scripts/check-for-updates.sh"),
        )
    } else {
        Finding::ok(
            "rust_binary_freshness",
            format!("{} is {} days old", candidate.display(), days),
        )
    }
}

async fn resolve_release_binary() -> Option<PathBuf> {
    // Try `which augmentagent` first — that's the canonical install location.
    if let Ok(Ok(out)) = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("which").arg("augmentagent").output(),
    )
    .await
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                let p = PathBuf::from(s);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    // Fallback — repo-relative.
    for cand in ["target/release/augmentagent", "./target/release/augmentagent"] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

async fn check_dashboard_build_present() -> Finding {
    // Resolve repo root from the augmentagent binary path. The installed
    // shim under a clean MyAgentAssistant checkout sits at
    // `<repo>/target/release/augmentagent`; production installs symlink
    // from `/usr/local/bin` to the same. We walk up from the binary path
    // looking for a parent that contains `dist/dashboard-server.js`.
    let bin = resolve_release_binary().await;
    let candidate_root = bin
        .as_ref()
        .and_then(|p| p.parent()) // target/release
        .and_then(|p| p.parent()) // target
        .and_then(|p| p.parent()) // repo root
        .map(PathBuf::from);

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(r) = candidate_root {
        roots.push(r);
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }

    for root in &roots {
        let p = root.join("dist/dashboard-server.js");
        if p.exists() {
            return Finding::ok(
                "dashboard_build_present",
                format!("{} present", p.display()),
            );
        }
    }
    // Surface a warn rather than error — the file's location is layout-
    // dependent and absent on slim/Rust-only deploys. The remediation hint
    // still points at the install path that produces it.
    Finding::warn(
        "dashboard_build_present",
        "dist/dashboard-server.js not found near binary or cwd".to_string(),
        Some("augmentagent install dashboard"),
    )
}

fn check_env_file_present() -> Finding {
    // `.env` in CWD wins. If absent, also probe the parent of the resolved
    // binary (best-effort — synchronous to keep this check trivial).
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".env"));
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".config/augmentagent/.env"));
    }
    for p in &candidates {
        if p.exists() {
            return Finding::ok("env_file_present", format!("{} present", p.display()));
        }
    }
    Finding::warn(
        "env_file_present",
        "no .env found in cwd or ~/.config/augmentagent/".to_string(),
        Some("cp .env.example .env && $EDITOR .env"),
    )
}

/// SocialAPI.ai readiness probe (#245). Two signals:
///   * is the API key in place? (sqlite `socialapi_api_key` OR env
///     `SOCIALAPI_API_KEY` — same precedence as the dashboard's
///     `getConfigStatus()`), and
///   * is there ≥1 active account in the local `socialapi_accounts` registry?
///
/// Maps to:
///   * ok    — key set AND ≥1 active account (channel is live),
///   * warn  — key set but no active accounts (connect one), or
///   * warn  — no key at all (optional integration, so never error).
/// The suggested_cmd points operators at the connect flow.
fn check_socialapi(store: &Store) -> Finding {
    let key_present = socialapi_key_present();
    let accounts = store
        .active_socialapi_account_ids()
        .map(|v| v.len())
        .unwrap_or(0);

    let connect_hint = "augmentagent socialapi connect (or connect via the dashboard)";

    match (key_present, accounts) {
        (true, n) if n > 0 => Finding::ok(
            "socialapi",
            format!("SOCIALAPI_API_KEY set, {n} active account(s)"),
        ),
        (true, _) => Finding::warn(
            "socialapi",
            "SOCIALAPI_API_KEY set but no active accounts".to_string(),
            Some(connect_hint),
        ),
        (false, _) => Finding::warn(
            "socialapi",
            "SOCIALAPI_API_KEY not set (SocialAPI.ai integration inactive)".to_string(),
            Some(connect_hint),
        ),
    }
}

/// True iff the SocialAPI.ai key is configured. Checks the sqlite `config`
/// table first (the dashboard writes `socialapi_api_key` there) then the
/// `SOCIALAPI_API_KEY` env var — same precedence as `status::cfg_or_env`.
fn socialapi_key_present() -> bool {
    if let Ok(v) = std::env::var("SOCIALAPI_API_KEY") {
        if !v.is_empty() {
            return true;
        }
    }
    let db_path = std::env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string());
    let Ok(conn) = rusqlite::Connection::open(&db_path) else {
        return false;
    };
    let val: rusqlite::Result<String> = conn.query_row(
        "SELECT value FROM config WHERE key = 'socialapi_api_key'",
        [],
        |r| r.get(0),
    );
    matches!(val, Ok(v) if !v.is_empty())
}

/// #376 — the calendar channel is deliberately not spawned by `serve`; an
/// external timer drives `calendar poll-once`. Its prerequisites (Composio
/// key + ≥1 gmail account as the entity list) are often satisfied long
/// before anyone schedules it, leaving the feature silently dead. Surface
/// that state. Linux-only probe: the timer is a systemd user unit.
fn check_calendar_scheduled(store: &Store) -> Finding {
    let composio = std::env::var("COMPOSIO_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let gmail_accounts = store
        .get_active_gmail_accounts()
        .map(|v| v.len())
        .unwrap_or(0);
    if !composio || gmail_accounts == 0 {
        return Finding::ok(
            "calendar_scheduled",
            "calendar not configured (needs COMPOSIO_API_KEY + a connected gmail account) — skipped".to_string(),
        );
    }
    if !cfg!(target_os = "linux") {
        return Finding::ok(
            "calendar_scheduled",
            "non-Linux host — timer probe skipped".to_string(),
        );
    }
    let unit_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        })
        .join("systemd/user");
    if unit_dir.join("augmentagent-calendar.timer").exists() {
        Finding::ok(
            "calendar_scheduled",
            format!("augmentagent-calendar.timer installed ({gmail_accounts} gmail entity(ies))"),
        )
    } else {
        Finding::warn(
            "calendar_scheduled",
            format!(
                "calendar ingest is configured ({gmail_accounts} gmail entity(ies), Composio key set) but nothing schedules it"
            ),
            Some("augmentagent install calendar"),
        )
    }
}

// ---------------------------------------------------------------------------
// `--deep` checks.
// ---------------------------------------------------------------------------

async fn check_composio_api() -> Finding {
    let key = std::env::var("COMPOSIO_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Finding::ok(
            "composio_api",
            "COMPOSIO_API_KEY not set — skipped".to_string(),
        );
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Finding::warn(
                "composio_api",
                format!("client build failed: {e}"),
                None,
            );
        }
    };
    // Composio's whoami-equivalent. A 2xx (or 401 — key recognised, scope
    // wrong) is proof the API is reachable. Anything else is surfaced as
    // an error.
    let url = "https://backend.composio.dev/api/v1/client/auth/client_info";
    let resp = client
        .get(url)
        .header("x-api-key", &key)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let s = r.status();
            if s.is_success() {
                Finding::ok("composio_api", format!("Composio reachable ({s})"))
            } else if s.as_u16() == 401 || s.as_u16() == 403 {
                Finding::warn(
                    "composio_api",
                    format!("Composio reachable but auth failed ({s})"),
                    Some("re-issue COMPOSIO_API_KEY at https://app.composio.dev"),
                )
            } else {
                Finding::error(
                    "composio_api",
                    format!("Composio responded {s}"),
                    None,
                )
            }
        }
        Err(e) => Finding::error(
            "composio_api",
            format!("Composio request failed: {e}"),
            None,
        ),
    }
}

/// One finding per configured channel. Read-only — we just lift the
/// `configured` / `armed` / `needs` signals already collected by status::run
/// and surface them as severity-tagged findings. The expensive per-channel
/// `validate` op (re-signing tokens, etc.) would belong here under a future
/// `validate` trait on the channel router; today we stay strictly read-only.
fn check_per_channel_validate(status_doc: &Option<status::StatusDoc>) -> Vec<Finding> {
    let doc = match status_doc {
        Some(d) => d,
        None => {
            return vec![Finding::warn(
                "per_channel_validate",
                "status doc unavailable — skipped".to_string(),
                None,
            )];
        }
    };
    let mut out: Vec<Finding> = Vec::new();
    for (name, ch) in &doc.channels {
        if !ch.configured {
            continue;
        }
        let needs_empty = ch.needs.is_empty();
        if ch.armed && needs_empty {
            out.push(Finding::ok(
                &format!("channel.{name}.validate"),
                "configured + armed + no missing fields".to_string(),
            ));
        } else if !ch.armed {
            out.push(Finding::warn(
                &format!("channel.{name}.validate"),
                "configured but not armed (channel is dark)".to_string(),
                Some(&format!("augmentagent channel {name} arm")),
            ));
        } else {
            out.push(Finding::warn(
                &format!("channel.{name}.validate"),
                format!("armed but missing fields: {}", ch.needs.join(", ")),
                Some(&format!("augmentagent setup harvest {name}")),
            ));
        }
    }
    if out.is_empty() {
        out.push(Finding::ok(
            "per_channel_validate",
            "no configured channels — nothing to validate".to_string(),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Human-readable table output.
// ---------------------------------------------------------------------------

fn print_table(findings: &[Finding], ok: usize, warn: usize, error: usize) {
    // Compute the widest name for stable column alignment.
    let name_w = findings.iter().map(|f| f.name.len()).max().unwrap_or(4).max(4);
    println!("{:<3} {:<width$}  {}", "sev", "name", "message", width = name_w);
    println!("{}", "-".repeat(3 + 1 + name_w + 2 + 40));
    for f in findings {
        println!(
            "{:<3} {:<width$}  {}",
            f.severity.icon(),
            f.name,
            f.message,
            width = name_w
        );
        if let Some(cmd) = &f.suggested_cmd {
            println!("{:<3} {:<width$}    -> {}", "", "", cmd, width = name_w);
        }
    }
    println!();
    println!("summary: {ok} ok, {warn} warn, {error} error");
}

// ---------------------------------------------------------------------------
// Unit tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn severity_strings() {
        assert_eq!(Severity::Ok.as_str(), "ok");
        assert_eq!(Severity::Warn.as_str(), "warn");
        assert_eq!(Severity::Error.as_str(), "error");
    }

    #[test]
    fn finding_json_shape() {
        let f = Finding::error("x", "boom", Some("fix it"));
        let v = f.to_json();
        assert_eq!(v["name"], "x");
        assert_eq!(v["severity"], "error");
        assert_eq!(v["message"], "boom");
        assert_eq!(v["suggested_cmd"], "fix it");
    }

    #[test]
    fn finding_json_null_suggested() {
        let f = Finding::ok("x", "fine");
        let v = f.to_json();
        assert!(v["suggested_cmd"].is_null());
    }

    #[test]
    fn env_file_present_warns_in_empty_tempdir() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        // Mask HOME so the ~/.config/augmentagent/.env candidate misses too.
        let prev_home = std::env::var_os("HOME");
        std::env::set_current_dir(tmp.path()).unwrap();
        std::env::set_var("HOME", tmp.path());
        let f = check_env_file_present();
        // Restore before assertions to avoid leaking on panic.
        std::env::set_current_dir(&prev).unwrap();
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.name, "env_file_present");
    }

    #[test]
    fn per_channel_validate_with_none_doc() {
        let v = check_per_channel_validate(&None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "per_channel_validate");
        assert_eq!(v[0].severity, Severity::Warn);
    }

    #[test]
    fn per_channel_validate_armed_clean() {
        let mut channels: BTreeMap<String, status::ChannelStatus> = BTreeMap::new();
        channels.insert(
            "gmail".to_string(),
            status::ChannelStatus {
                configured: true,
                armed: true,
                accounts: 1,
                last_poll_unix: None,
                needs: vec![],
            },
        );
        channels.insert(
            "slack".to_string(),
            status::ChannelStatus {
                configured: true,
                armed: false,
                accounts: 0,
                last_poll_unix: None,
                needs: vec![],
            },
        );
        let doc = status::StatusDoc {
            schema_version: "1".to_string(),
            host: "linux".to_string(),
            daemon: status::DaemonStatus {
                unit: "x".to_string(),
                active: true,
                since_unix: 0,
            },
            dashboard: status::DashboardStatus {
                unit: "x".to_string(),
                active: true,
                port: 3000,
                reachable: true,
            },
            updater: status::UpdaterStatus {
                unit: "x".to_string(),
                timer_active: true,
                last_run_unix: 0,
            },
            core_keys: status::CoreKeys {
                composio: true,
                groq: true,
                cerebras: true,
                discord_bot: true,
            },
            channels,
            queue: status::QueueStatus { pending: 0 },
            summary: "ok".to_string(),
        };
        let v = check_per_channel_validate(&Some(doc));
        // Exactly two findings — one ok (gmail), one warn (slack not armed).
        assert_eq!(v.len(), 2);
        let gmail = v.iter().find(|f| f.name == "channel.gmail.validate").unwrap();
        assert_eq!(gmail.severity, Severity::Ok);
        let slack = v.iter().find(|f| f.name == "channel.slack.validate").unwrap();
        assert_eq!(slack.severity, Severity::Warn);
        assert_eq!(
            slack.suggested_cmd.as_deref(),
            Some("augmentagent channel slack arm")
        );
    }
}
