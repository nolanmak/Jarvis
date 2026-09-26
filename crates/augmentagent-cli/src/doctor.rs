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
//!   * `cerebras_models`     — is the pinned Cerebras model still in the
//!                              catalog? Only when cerebras is in the chain.
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

use augmentagent_channel_core::providers::{model_for, parse_chain, ModelTier, ProviderKind};
use augmentagent_channel_core::{cli_gate, handoff};
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
    if cfg!(target_os = "macos") {
        findings.push(check_launchd_agents());
    }
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
            Some(&crate::platform::package_install_hint("python3", "python")),
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
    findings.push(check_message_index(&store));
    findings.push(check_embeddings_provider());
    // 12. calendar — configured (Composio + gmail entities) but unscheduled? (#376)
    findings.push(check_calendar_scheduled(&store));
    // 13. reasoner chain — configured providers + the model each tier runs (#658)
    findings.push(check_reasoner_chain());
    findings.extend(check_reasoner_workloads());
    // 14. reasoner CLI gate — is the daemon's #898 gate wedged? (#954)
    findings.push(check_reasoner_gate());
    // 15. handoff journals — is the retention sweep keeping them bounded? (#1035)
    findings.push(check_handoff_journals());
    // 16. build VM — Codex cargo/npm/npx runner readiness (#1041)
    findings.push(check_build_vm());
    // 17. build scratch — admission-limit validity and capacity (#1092)
    findings.push(check_build_scratch());

    // --- Deep checks (off by default).
    if deep {
        findings.push(check_composio_api().await);
        findings.push(check_cerebras_models().await);
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
        Ok(v) if v == "ok" => Finding::ok("sqlite_open", format!("{db_path}: integrity_check ok")),
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
            Some("augmentagent service --unit daemon restart"),
        )
    }
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1 LIMIT 1")?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

async fn check_keyring_reachable() -> Finding {
    if cfg!(target_os = "macos") {
        return check_keychain_reachable().await;
    }
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

/// #1079 — macOS: the `keyring` crate uses the login Keychain. `security
/// default-keychain` answers whether one is configured for this session.
async fn check_keychain_reachable() -> Finding {
    let res = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("security").arg("default-keychain").output(),
    )
    .await;
    match res {
        Ok(Ok(out)) if out.status.success() => Finding::ok(
            "keyring_reachable",
            format!(
                "login Keychain reachable ({})",
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .trim_matches('"')
            ),
        ),
        Ok(Ok(out)) => Finding::error(
            "keyring_reachable",
            format!(
                "no default Keychain: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Some("security default-keychain -s login.keychain-db"),
        ),
        Ok(Err(e)) => Finding::error(
            "keyring_reachable",
            format!("`security` could not run: {e}"),
            None,
        ),
        Err(_) => Finding::warn(
            "keyring_reachable",
            "security timed out after 2s".to_string(),
            None,
        ),
    }
}

/// #1079 — macOS: is the daemon installed and loaded as a launchd agent?
/// Linux reports the same through `status` (systemd unit probes).
fn check_launchd_agents() -> Finding {
    use crate::platform::{launchd_job, plist_path, LABEL_PREFIX};
    let installed = plist_path(LABEL_PREFIX).is_some_and(|p| p.exists());
    let job = launchd_job(LABEL_PREFIX);
    match (installed, job.loaded, job.running()) {
        (_, true, true) => Finding::ok(
            "launchd_agents",
            format!("{LABEL_PREFIX} running (pid {})", job.pid.unwrap_or(0)),
        ),
        (_, true, false) => Finding::warn(
            "launchd_agents",
            format!(
                "{LABEL_PREFIX} loaded but {} (last exit {})",
                job.state,
                job.last_exit_code
                    .map_or("n/a".to_string(), |c| c.to_string())
            ),
            Some("augmentagent logs --unit daemon"),
        ),
        (true, false, _) => Finding::warn(
            "launchd_agents",
            format!("{LABEL_PREFIX}.plist is installed but not loaded"),
            Some("augmentagent service --unit daemon start"),
        ),
        (false, false, _) => Finding::warn(
            "launchd_agents",
            "daemon is not installed as a launchd agent".to_string(),
            Some("augmentagent install autostart"),
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
            format!("no response from http://127.0.0.1:{port}/api/v1/stats (and /api/v1/health)"),
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
            format!("{} is {} days old (> 7d)", candidate.display(), days),
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
    for cand in [
        "target/release/augmentagent",
        "./target/release/augmentagent",
    ] {
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
///   * is the API key in place? (env `SOCIALAPI_API_KEY`, then the keyring
///     slot `augmentagent/socialapi/default`, then sqlite
///     `config.socialapi_api_key` — the same three steps, in the same order,
///     that `SocialApiAuth::load_with_store` uses to actually load it), and
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
            format!("SocialAPI.ai key set, {n} active account(s)"),
        ),
        (true, _) => Finding::warn(
            "socialapi",
            "SocialAPI.ai key set but no active accounts".to_string(),
            Some(connect_hint),
        ),
        (false, _) => Finding::warn(
            "socialapi",
            "no SocialAPI.ai key in env, keyring, or dashboard config \
             (SocialAPI.ai integration inactive)"
                .to_string(),
            Some(connect_hint),
        ),
    }
}

/// True iff the SocialAPI.ai key is configured, using the SAME three-step
/// order the daemon actually loads with (`SocialApiAuth::load_with_store`):
/// `SOCIALAPI_API_KEY` env, then the keyring slot
/// `augmentagent/socialapi/default`, then sqlite `config.socialapi_api_key`
/// (where the dashboard writes).
///
/// #525: this used to check env-then-sqlite and skip the keyring entirely, so
/// a key stored the canonical way — which is what `SocialApiAuth` writes and
/// reads — made doctor report the integration inactive while both channel
/// loops were running fine. The old doc comment also claimed sqlite was
/// checked first; the code checked env first.
fn socialapi_key_present() -> bool {
    if std::env::var(augmentagent_channel_socialapi::ENV_VAR)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    if augmentagent_auth::Auth::get(
        augmentagent_channel_socialapi::KEYCHAIN_PLATFORM,
        augmentagent_auth::DEFAULT_ACCOUNT,
    )
    .map(|b| !String::from_utf8_lossy(&b).trim().is_empty())
    .unwrap_or(false)
    {
        return true;
    }
    let db_path = std::env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string());
    let Ok(conn) = rusqlite::Connection::open(&db_path) else {
        return false;
    };
    let val: rusqlite::Result<String> = conn.query_row(
        "SELECT value FROM config WHERE key = ?1",
        [augmentagent_channel_socialapi::CONFIG_KEY],
        |r| r.get(0),
    );
    matches!(val, Ok(v) if !v.trim().is_empty())
}

/// #1102 — structured message index coverage. Warn-only: search still
/// works from `emails` when the index lags.
fn check_message_index(store: &Store) -> Finding {
    match augmentagent_messages::check(store) {
        Ok(h) if h.is_complete() => {
            Finding::ok("message_index", format!("{} messages indexed", h.indexed))
        }
        Ok(h) => Finding::warn(
            "message_index",
            format!(
                "message index incomplete: {} missing, {} stale, {} queued of {} messages",
                h.missing, h.stale, h.queued, h.emails
            ),
            Some("augmentagent messages reindex"),
        ),
        Err(e) => Finding::warn(
            "message_index",
            format!("message index check failed: {e:#}"),
            Some("augmentagent messages reindex"),
        ),
    }
}

/// #1131 — which embeddings provider is active. Hosted means message text
/// goes to a third party, so it is always surfaced; local is ok; off is
/// simply off (never an error: embeddings are optional).
fn check_embeddings_provider() -> Finding {
    use augmentagent_embeddings::{hosted, Provider, ENV_PROVIDER};
    let enabled = crate::embeddings_cmd::enabled();
    match Provider::from_env() {
        Err(e) => Finding::warn(
            "embeddings",
            format!("{e}"),
            Some(&format!("set {ENV_PROVIDER}=local or hosted")),
        ),
        Ok(Provider::Hosted) => {
            let cfg = hosted::HostedConfig::default();
            if hosted::load_key().is_some() {
                Finding::warn(
                    "embeddings",
                    format!(
                        "HOSTED embeddings provider selected ({}): message text is sent to a third party{}",
                        cfg.model,
                        if enabled { "" } else { " (worker disabled: AUGMENTAGENT_EMBEDDINGS unset)" }
                    ),
                    Some(&format!("{ENV_PROVIDER}=local keeps text on this machine")),
                )
            } else {
                Finding::warn(
                    "embeddings",
                    "hosted embeddings selected but no key in keyring/env; the provider refuses to start (no fallback to local)".to_string(),
                    Some("augmentagent migrate-secrets-to-keyring"),
                )
            }
        }
        Ok(Provider::Local) => Finding::ok(
            "embeddings",
            if enabled {
                "local embeddings provider (text stays on this machine)"
            } else {
                "embeddings off (local provider would be used)"
            },
        ),
    }
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
    if cfg!(target_os = "macos") {
        // #1079 — install-calendar.sh writes a launchd agent on macOS.
        let label = "com.nolanmak.augmentagent.calendar";
        let installed = crate::platform::plist_path(label).is_some_and(|p| p.exists());
        return if installed && crate::platform::launchd_job(label).loaded {
            Finding::ok(
                "calendar_scheduled",
                format!("{label} loaded ({gmail_accounts} gmail entity(ies))"),
            )
        } else if installed {
            Finding::warn(
                "calendar_scheduled",
                format!("{label}.plist is installed but not loaded, so nothing schedules calendar ingest"),
                Some("augmentagent install calendar"),
            )
        } else {
            Finding::warn(
                "calendar_scheduled",
                format!(
                    "calendar ingest is configured ({gmail_accounts} gmail entity(ies), Composio key set) but nothing schedules it"
                ),
                Some("augmentagent install calendar"),
            )
        };
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

/// #658 — what the reasoner will actually run: the configured provider chain
/// and the model each tier resolves to. Both are env-driven and swappable
/// without a rebuild, so a typo or a dark provider surfaces here rather than
/// in a failed call hours later.
fn check_reasoner_chain() -> Finding {
    let raw = std::env::var("AUGMENTAGENT_REASONER_CHAIN").unwrap_or_default();
    let ineligible: Vec<(ProviderKind, String)> = parse_chain(&raw)
        .providers
        .into_iter()
        .filter_map(|k| augmentagent_channel_core::ineligible_reason(k).map(|why| (k, why)))
        .collect();
    reasoner_chain_finding(&raw, &ineligible)
}

fn reasoner_chain_finding(raw: &str, ineligible: &[(ProviderKind, String)]) -> Finding {
    let parsed = parse_chain(raw);
    let chain = if raw.trim().is_empty() {
        "claude (default; failover off)".to_string()
    } else {
        parsed
            .providers
            .iter()
            .map(|k| k.name())
            .collect::<Vec<_>>()
            .join(" -> ")
    };
    let models = parsed
        .providers
        .iter()
        .map(|k| {
            let (q, f) = (
                model_for(*k, ModelTier::Quality),
                model_for(*k, ModelTier::Fast),
            );
            format!("{}: quality={q} fast={f}", k.name())
        })
        .collect::<Vec<_>>()
        .join("; ");

    let mut problems: Vec<String> = parsed
        .unknown
        .iter()
        .map(|t| format!("unknown provider skipped: {t}"))
        .collect();
    for (kind, why) in ineligible {
        problems.push(format!("{} configured but ineligible ({why})", kind.name()));
    }

    if problems.is_empty() {
        Finding::ok("reasoner_chain", format!("{chain} [{models}]"))
    } else {
        Finding::warn(
            "reasoner_chain",
            format!("{chain} [{models}] — {}", problems.join("; ")),
            Some("AUGMENTAGENT_REASONER_CHAIN=claude,codex,gemini,cerebras"),
        )
    }
}

fn check_reasoner_workloads() -> Vec<Finding> {
    let raw = std::env::var("AUGMENTAGENT_REASONER_CHAIN").unwrap_or_default();
    let providers = parse_chain(&raw).providers;
    let unavailable = providers
        .iter()
        .copied()
        .filter(|kind| {
            if *kind == ProviderKind::Claude {
                !augmentagent_channel_core::providers::bin_resolves(
                    &std::env::var("CLAUDE_CLI").unwrap_or_else(|_| "claude".into()),
                )
            } else {
                augmentagent_channel_core::ineligible_reason(*kind).is_some()
            }
        })
        .collect::<Vec<_>>();
    let latch = augmentagent_channel_core::CooldownLatch::system();
    let latched = providers
        .iter()
        .copied()
        .filter(|kind| latch.latched_until(kind.name()).is_some())
        .collect::<Vec<_>>();
    reasoner_workload_findings(&raw, &unavailable, &latched)
}

/// Routing capacity is not a promise that a particular MCP server or sandbox
/// is ready. The adapter must still validate that request's concrete policy.
fn reasoner_workload_findings(
    raw: &str,
    unavailable: &[ProviderKind],
    latched: &[ProviderKind],
) -> Vec<Finding> {
    use augmentagent_channel_core::providers::{allowed_for, CapabilityClass::*};
    let configured = parse_chain(raw).providers;
    [
        ("text", TextOnly),
        ("read", ReadTools),
        ("write", WriteTools),
        ("agentic", FullAgentic),
    ]
    .into_iter()
    .map(|(label, class)| {
        let mut candidates = 0;
        let states = configured
            .iter()
            .map(|kind| {
                let state = if !allowed_for(*kind, class) {
                    "capability excluded"
                } else if unavailable.contains(kind) {
                    "binary/auth unavailable"
                } else if latched.contains(kind) {
                    "cooldown"
                } else {
                    candidates += 1;
                    "candidate"
                };
                format!("{}: {state}", kind.name())
            })
            .collect::<Vec<_>>()
            .join("; ");
        let (severity, capacity) = match candidates {
            0 => (Severity::Error, "no usable provider"),
            1 => (Severity::Warn, "no usable backup"),
            _ => (Severity::Ok, "backup routing available"),
        };
        Finding {
            name: format!("reasoner_{label}_capacity"),
            severity,
            message: format!(
                "{states}; {capacity}. Tool/MCP/sandbox readiness is checked per invocation."
            ),
            suggested_cmd: None,
        }
    })
    .collect()
}

/// #954 — the #898 gate lives in the daemon, so doctor reads its snapshot: a
/// permit held past the timeout it promised is the freeze, and says whose.
fn check_reasoner_gate() -> Finding {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    gate_finding(cli_gate::read_snapshot(), now.map_or(0, |d| d.as_secs()))
}

fn gate_finding(snap: Option<cli_gate::GateSnapshot>, now: u64) -> Finding {
    let hint = if crate::platform::ServiceManager::detect().is_launchd() {
        "augmentagent logs --unit daemon --lines 2000 | grep 'CLI gate' | tail -n 50"
    } else {
        "journalctl --user -u augmentagent -g 'CLI gate' -n 50"
    };
    let Some(s) = snap else {
        return Finding::ok("reasoner_gate", "no reasoner CLI call yet this boot");
    };
    if !crate::platform::pid_alive(s.pid) {
        return Finding::ok(
            "reasoner_gate",
            format!("stale snapshot from pid {}", s.pid),
        );
    }
    let state = format!(
        "in_flight {}/{}, waiting {}",
        s.in_flight, s.capacity, s.waiting
    );
    // The permit carries its own class-aware budget (#655), so "overdue" is one
    // timeout — the same threshold the daemon's own watchdog reports at (#954).
    let (Some(provider), Some(since), Some(budget), Some(caller)) = (
        s.oldest_provider,
        s.oldest_since_unix,
        s.oldest_budget_secs,
        s.oldest_caller,
    ) else {
        return Finding::ok("reasoner_gate", format!("{state} (idle)"));
    };
    let age = now.saturating_sub(since);
    let msg = format!("{state}; oldest permit ({provider}, {caller}) held {age}s of {budget}s");
    if age <= budget {
        return Finding::ok("reasoner_gate", msg);
    }
    Finding::warn(
        "reasoner_gate",
        format!("{msg} — reasoning is wedged"),
        Some(hint),
    )
}

/// #1035 — doctor warns when the journal root is past either bound.
const HANDOFF_WARN_REQUESTS: u64 = 5_000;
const HANDOFF_WARN_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// A read-only dry run over the live root: no locks, nothing created.
fn check_handoff_journals() -> Finding {
    let Some(root) = handoff::journal_root() else {
        return Finding::ok("handoff_journals", "no HOME; journal root unknown");
    };
    let grace = handoff::retention_from_env();
    // #1071 — the same orphan pass the daemon runs at start, read-only, so doctor
    // can say how many markers are clearable and how many predate the upgrade.
    let orphans = handoff::clear_orphaned_markers(&root, &handoff::LivenessEnv::probe(), true);
    handoff_journal_finding(handoff::sweep_finished(&root, grace, true), orphans.unwrap_or_default(), grace)
}

fn handoff_journal_finding(report: Result<handoff::SweepReport>, orphans: handoff::OrphanReport,
    grace: Duration) -> Finding {
    const NAME: &str = "handoff_journals";
    const HINT: &str = "augmentagent handoff-prune --dry-run";
    let report = match report {
        Ok(report) => report,
        Err(e) => return Finding::warn(NAME, format!("journal root refused: {e:#}"), Some(HINT)),
    };
    let msg = format!(
        "{} request dirs, {} MB; {} finished past the {}h grace ({} by over two sweep intervals); \
         for information: {} unfinished (operator recovery), {} with lifecycle markers \
         ({} orphaned, {} written before the marker upgrade)",
        report.requests,
        report.bytes / (1024 * 1024),
        report.removed,
        grace.as_secs() / 3600,
        report.finished_overdue,
        report.kept_unfinished,
        report.kept_active,
        orphans.cleared,
        orphans.kept_legacy,
    );
    // The daemon clears every provable orphan at start, so one still here
    // means that pass is not running (#1071).
    if orphans.cleared > 0 {
        let msg = format!("{msg} — orphaned markers are not being cleared at daemon start");
        return Finding::warn(NAME, msg, Some(HINT));
    }
    // A live sweep removes every finished journal within two intervals of its
    // expiry, so one still here means the sweep stopped (#1035 review).
    if report.finished_overdue > 0 {
        return Finding::warn(
            NAME,
            format!("{msg} — the daemon's hourly sweep does not appear to be running"),
            Some(HINT),
        );
    }
    if report.requests > HANDOFF_WARN_REQUESTS || report.bytes > HANDOFF_WARN_BYTES {
        return Finding::warn(
            NAME,
            format!(
                "{msg} — over {HANDOFF_WARN_REQUESTS} dirs or {} GiB; is the daemon's hourly sweep running?",
                HANDOFF_WARN_BYTES / (1024 * 1024 * 1024)
            ),
            Some(HINT),
        );
    }
    Finding::ok(NAME, msg)
}

/// What doctor observed about `/dev/kvm` (injected in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
struct KvmProbe {
    exists: bool,
    /// `access(R_OK|W_OK)` for this process: owner, group, mode or ACL.
    read_write: bool,
    /// Access granted by owner, group membership or other bits alone — i.e.
    /// it does not depend on a logind seat ACL that a logout revokes.
    durable: bool,
}

/// What doctor observed about the VM runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VmConfigProbe {
    Missing,
    Invalid(String),
    /// Names of required artifacts (`qemu`, `kernel`) that do not exist.
    Loaded {
        missing: Vec<&'static str>,
    },
}

const KVM_DEVICE: &str = "/dev/kvm";

fn check_build_vm() -> Finding {
    use augmentagent_channel_core::codex_tools::{build_runner, BuildRunner};
    let runner = build_runner();
    let config = match &runner {
        BuildRunner::Vm(path) => probe_vm_config(path),
        _ => VmConfigProbe::Missing,
    };
    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".into());
    build_vm_finding(
        &runner,
        &config,
        &probe_kvm(std::path::Path::new(KVM_DEVICE)),
        &user,
    )
}

fn probe_vm_config(path: &std::path::Path) -> VmConfigProbe {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return VmConfigProbe::Missing,
        Err(e) => return VmConfigProbe::Invalid(format!("unreadable: {e}")),
    };
    let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
        return VmConfigProbe::Invalid("not valid JSON".into());
    };
    let missing = ["qemu", "kernel"]
        .into_iter()
        .filter(|key| {
            !value
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|p| std::path::Path::new(p).is_file())
        })
        .collect();
    VmConfigProbe::Loaded { missing }
}

fn probe_kvm(path: &std::path::Path) -> KvmProbe {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return KvmProbe {
            exists: false,
            read_write: false,
            durable: false,
        };
    };
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return KvmProbe {
            exists: true,
            read_write: false,
            durable: false,
        };
    };
    // SAFETY: `c_path` is a valid NUL-terminated string for the call.
    let read_write = unsafe { libc::access(c_path.as_ptr(), libc::R_OK | libc::W_OK) } == 0;
    // SAFETY: getuid/getgid/getgroups have no preconditions; the buffer is sized by the first call.
    let (uid, gid, groups) = unsafe {
        let count = libc::getgroups(0, std::ptr::null_mut());
        let mut groups = vec![0 as libc::gid_t; count.max(0) as usize];
        let filled = libc::getgroups(count.max(0), groups.as_mut_ptr());
        groups.truncate(filled.max(0) as usize);
        (libc::getuid(), libc::getgid(), groups)
    };
    let mut gids = groups;
    gids.push(gid);
    let acl = read_posix_acl(&c_path);
    let durable = durable_kvm_access(
        meta.uid(),
        meta.gid(),
        meta.mode(),
        acl.as_deref(),
        uid,
        &gids,
    );
    KvmProbe {
        exists: true,
        read_write,
        durable,
    }
}

// Linux POSIX ACL xattr (`system.posix_acl_access`) entry tags.
const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AclEntry {
    tag: u16,
    perm: u16,
    id: u32,
}

/// Version-2 little-endian xattr: a u32 header, then 8-byte entries.
fn parse_posix_acl(raw: &[u8]) -> Option<Vec<AclEntry>> {
    let (header, body) = raw.split_at_checked(4)?;
    if u32::from_le_bytes(header.try_into().ok()?) != 2 || body.len() % 8 != 0 {
        return None;
    }
    Some(
        body.chunks_exact(8)
            .map(|e| AclEntry {
                tag: u16::from_le_bytes([e[0], e[1]]),
                perm: u16::from_le_bytes([e[2], e[3]]),
                id: u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            })
            .collect(),
    )
}

#[cfg(not(target_os = "linux"))]
fn read_posix_acl(_path: &std::ffi::CStr) -> Option<Vec<AclEntry>> {
    // `system.posix_acl_access` is a Linux xattr (and macOS's getxattr takes
    // six arguments, #1079): no POSIX ACL to read elsewhere.
    None
}

#[cfg(target_os = "linux")]
fn read_posix_acl(path: &std::ffi::CStr) -> Option<Vec<AclEntry>> {
    let name = c"system.posix_acl_access";
    let mut buffer = vec![0u8; 4096];
    // SAFETY: both strings are NUL-terminated; the buffer length is passed.
    let size = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    (size > 0)
        .then(|| parse_posix_acl(&buffer[..size as usize]))
        .flatten()
}

/// Read-write access that does not come from a per-user ACL entry (the
/// logind seat grant): owner bits, group membership or other bits. With an
/// ACL, st_mode's group bits are the mask, so group access is the `group::`
/// or a named-group entry, limited by the mask.
fn durable_kvm_access(
    owner: u32,
    group: u32,
    mode: u32,
    acl: Option<&[AclEntry]>,
    uid: u32,
    gids: &[u32],
) -> bool {
    const RW: u16 = 6;
    let rw = |perm: u16| perm & RW == RW;
    if owner == uid {
        return mode & 0o600 == 0o600;
    }
    let Some(acl) = acl.filter(|entries| entries.iter().any(|e| e.tag == ACL_MASK)) else {
        return if gids.contains(&group) {
            mode & 0o060 == 0o060
        } else {
            mode & 0o006 == 0o006
        };
    };
    let mask = acl.iter().find(|e| e.tag == ACL_MASK).map_or(0, |e| e.perm);
    let other = acl
        .iter()
        .find(|e| e.tag == ACL_OTHER)
        .map_or(0, |e| e.perm);
    let groups: Vec<u16> = acl
        .iter()
        .filter(|e| {
            (e.tag == ACL_GROUP_OBJ && gids.contains(&group))
                || (e.tag == ACL_GROUP && gids.contains(&e.id))
        })
        .map(|e| e.perm)
        .collect();
    if groups.is_empty() {
        rw(other)
    } else {
        groups.iter().any(|perm| rw(perm & mask))
    }
}

fn build_vm_finding(
    runner: &augmentagent_channel_core::codex_tools::BuildRunner,
    config: &VmConfigProbe,
    kvm: &KvmProbe,
    user: &str,
) -> Finding {
    use augmentagent_channel_core::codex_tools::BuildRunner;
    const NAME: &str = "build_vm";
    const DOCS: &str = "see docs/BUILD-VM.md";
    let group_cmd = format!("sudo usermod -aG kvm {user}  # then log out fully (or reboot) and restart augmentagent.service");
    match runner {
        BuildRunner::Host => return Finding::warn(NAME,
            "AUGMENTAGENT_BUILD_VM=host: Codex cargo/npm/npx commands run in the host command sandbox, not the VM",
            Some("unset AUGMENTAGENT_BUILD_VM once the build VM is provisioned")),
        BuildRunner::Unavailable { reason } => return Finding::error(NAME,
            format!("config missing: {reason}; Codex build commands fail closed (JARVIS_READINESS:build_vm_unavailable)"),
            Some("provision ~/.local/share/augmentagent/build-vm/runtime.json (docs/BUILD-VM.md), or set AUGMENTAGENT_BUILD_VM=host to opt out")),
        BuildRunner::Vm(_) => {}
    }
    match config {
        VmConfigProbe::Missing => return Finding::error(NAME,
            "config missing: the configured build VM runtime file does not exist; Codex builds will be denied",
            Some("check AUGMENTAGENT_BUILD_VM_CONFIG; see docs/BUILD-VM.md")),
        VmConfigProbe::Invalid(why) => return Finding::error(NAME,
            format!("config invalid: build VM runtime configuration is {why}"), Some(DOCS)),
        VmConfigProbe::Loaded { missing } if !missing.is_empty() => return Finding::error(NAME,
            format!("qemu or kernel missing: configured {} not found", missing.join(" and ")), Some(DOCS)),
        VmConfigProbe::Loaded { .. } => {}
    }
    if !kvm.exists {
        return Finding::error(
            NAME,
            "kvm not accessible: /dev/kvm does not exist (KVM disabled or kvm module not loaded)",
            Some("sudo modprobe kvm_intel || sudo modprobe kvm_amd"),
        );
    }
    if !kvm.read_write {
        return Finding::error(NAME,
            "kvm not accessible: this user cannot open /dev/kvm read-write; every Codex build is denied",
            Some(&group_cmd));
    }
    if !kvm.durable {
        return Finding::warn(NAME,
            "kvm not accessible after logout: /dev/kvm is reachable only through the login-seat (logind uaccess) ACL, \
             not kvm group membership; builds fail once the seat session ends",
            Some(&group_cmd));
    }
    Finding::ok(NAME, "ok: VM runtime configured, qemu and kernel present, /dev/kvm read-write via group or owner")
}

/// What doctor observed about the build scratch volume (#1092).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScratchUsage {
    /// Bytes available to this user on the scratch volume.
    free_bytes: u64,
    /// Allocated blocks of every existing session's build-cache image.
    allocated: u64,
    /// Those images' unallocated remainder (sparse growth still to come).
    outstanding: u64,
}

const SCRATCH_NAME: &str = "build_scratch";
const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// The admission capacity finding: invalid limits fail closed under the named
/// `build_scratch_limits` readiness category (mirroring the bridge); otherwise
/// report free space, the effective limits, and how many sessions can be
/// admitted right now. Pure over injected volume numbers so it is unit-testable.
fn build_scratch_finding(
    limits: &augmentagent_channel_core::build_scratch::BuildScratchLimits,
    free_bytes: u64,
    allocated: u64,
    outstanding: u64,
) -> Finding {
    if let Err(why) = limits.validate() {
        return Finding::error(
            SCRATCH_NAME,
            format!(
                "admission limits invalid: {why}; every Codex build fails closed \
                 (JARVIS_READINESS:build_scratch_limits)"
            ),
            Some("set AUGMENTAGENT_BUILD_SCRATCH_{HEADROOM,IMAGE_CAP,BUDGET}_GIB in range; see docs/BUILD-VM.md"),
        );
    }
    let gib = |bytes: u64| bytes as f64 / BYTES_PER_GIB as f64;
    // max(0, min(floor((budget-allocated)/cap), floor((free-headroom-outstanding)/cap))).
    // saturating_sub keeps a full volume or over-budget state at zero admissible.
    let by_budget = limits.budget_bytes.saturating_sub(allocated) / limits.cache_bytes;
    let by_free =
        free_bytes.saturating_sub(limits.headroom_bytes).saturating_sub(outstanding) / limits.cache_bytes;
    let admissible = by_budget.min(by_free);
    let message = format!(
        "{:.1} GiB free; limits: {} GiB headroom, {} GiB image cap, {} GiB budget; \
         {admissible} session(s) admissible now ({:.1} GiB allocated across images, \
         {:.1} GiB reserved for their growth)",
        gib(free_bytes),
        limits.headroom_bytes / BYTES_PER_GIB,
        limits.cache_bytes / BYTES_PER_GIB,
        limits.budget_bytes / BYTES_PER_GIB,
        gib(allocated),
        gib(outstanding),
    );
    if admissible == 0 {
        Finding::warn(
            SCRATCH_NAME,
            format!(
                "{message} — no Codex build can be admitted until space frees up; the scratch volume is \
                 shared with the auto-PR gate cache (AUGMENTAGENT_GATE_CACHE_MAX_MB)"
            ),
            Some("free space on the scratch volume, or lower AUGMENTAGENT_GATE_CACHE_MAX_MB"),
        )
    } else {
        Finding::ok(SCRATCH_NAME, message)
    }
}

fn check_build_scratch() -> Finding {
    use augmentagent_channel_core::build_scratch;
    let limits = build_scratch::BuildScratchLimits::from_env();
    let usage = probe_scratch_usage(&build_scratch::scratch_dir());
    build_scratch_finding(&limits, usage.free_bytes, usage.allocated, usage.outstanding)
}

/// Free space (from the nearest existing ancestor) and the allocated/unallocated
/// bytes of every session's build-cache image under the scratch root.
fn probe_scratch_usage(root: &std::path::Path) -> ScratchUsage {
    use augmentagent_channel_core::build_scratch::{IMAGE_NAME, SESSION_PREFIX};
    use std::os::unix::fs::MetadataExt;
    let free_bytes = statvfs_available(root);
    let (mut allocated, mut outstanding) = (0u64, 0u64);
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(SESSION_PREFIX) {
                continue;
            }
            if let Ok(meta) = std::fs::symlink_metadata(entry.path().join(IMAGE_NAME)) {
                if meta.file_type().is_file() {
                    let used = meta.blocks() * 512;
                    allocated += used;
                    outstanding += meta.size().saturating_sub(used);
                }
            }
        }
    }
    ScratchUsage { free_bytes, allocated, outstanding }
}

/// Available bytes on the volume holding `path`, resolved through its nearest
/// existing ancestor (a not-yet-provisioned root sits on the same volume).
fn statvfs_available(path: &std::path::Path) -> u64 {
    let mut candidate = Some(path);
    while let Some(dir) = candidate {
        if dir.exists() {
            if let Some(free) = statvfs_bavail(dir) {
                return free;
            }
        }
        candidate = dir.parent();
    }
    0
}

fn statvfs_bavail(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated string; `buf` is zeroed and
    // fully written by a successful call.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    Some((buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64))
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
            return Finding::warn("composio_api", format!("client build failed: {e}"), None);
        }
    };
    // Composio's whoami-equivalent. A 2xx (or 401 — key recognised, scope
    // wrong) is proof the API is reachable. Anything else is surfaced as
    // an error.
    let url = "https://backend.composio.dev/api/v1/client/auth/client_info";
    let resp = client.get(url).header("x-api-key", &key).send().await;
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
                Finding::error("composio_api", format!("Composio responded {s}"), None)
            }
        }
        Err(e) => Finding::error(
            "composio_api",
            format!("Composio request failed: {e}"),
            None,
        ),
    }
}

/// #658 — is the pinned Cerebras model still in the catalog? Cerebras retired
/// five model families in twelve months (zai-glm-4.7 on 2026-08-17), so a pin
/// fine at deploy time can become a fallback that 404s every call it serves.
/// Chain membership gates the network call, not just its severity: a box that
/// merely retains an unused CEREBRAS_API_KEY must not pay a round trip — or
/// inherit a 401 warning — from an otherwise unrelated deep run.
async fn check_cerebras_models() -> Finding {
    let raw = std::env::var("AUGMENTAGENT_REASONER_CHAIN").unwrap_or_default();
    if !parse_chain(&raw)
        .providers
        .contains(&ProviderKind::Cerebras)
    {
        return Finding::ok(
            "cerebras_models",
            "cerebras is not in AUGMENTAGENT_REASONER_CHAIN — skipped".to_string(),
        );
    }
    let Some(key) = augmentagent_channel_core::secret_loader::load_provider_key("CEREBRAS_API_KEY")
    else {
        return Finding::ok(
            "cerebras_models",
            "no CEREBRAS_API_KEY in keyring or env — skipped".to_string(),
        );
    };
    // Read through `model_for` so an `AUGMENTAGENT_MODEL_CEREBRAS_*` override
    // is what gets validated — that is the pin most likely to name a model
    // nobody checked.
    let pinned = [ModelTier::Quality, ModelTier::Fast]
        .map(|t| (t, model_for(ProviderKind::Cerebras, t)))
        .to_vec();
    let catalog = augmentagent_channel_core::cerebras::list_models(
        &reqwest::Client::new(),
        &augmentagent_channel_core::cerebras::cerebras_base_url(),
        &key,
    )
    .await;
    cerebras_models_finding(&pinned, catalog)
}

/// Only reached with cerebras in the chain, so a dead pin is an error: every
/// call that fails over to it will 404.
fn cerebras_models_finding(
    pinned: &[(ModelTier, String)],
    catalog: Result<Vec<String>, String>,
) -> Finding {
    let catalog = match catalog {
        Ok(c) => c,
        // Never an error: an offline box must not fail `doctor`.
        Err(e) => {
            return Finding::warn(
                "cerebras_models",
                format!("could not list the Cerebras catalog: {e}"),
                None,
            );
        }
    };
    let missing: Vec<&(ModelTier, String)> = pinned
        .iter()
        .filter(|(_, model)| !catalog.contains(model))
        .collect();
    let Some((tier, _)) = missing.first() else {
        return Finding::ok(
            "cerebras_models",
            format!(
                "pinned models present in the catalog: {}",
                pinned
                    .iter()
                    .map(|(_, m)| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    };
    let names = missing
        .iter()
        .map(|(_, m)| m.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let env_key = match tier {
        ModelTier::Quality => "AUGMENTAGENT_MODEL_CEREBRAS_QUALITY",
        ModelTier::Fast => "AUGMENTAGENT_MODEL_CEREBRAS_FAST",
    };
    Finding::error(
        "cerebras_models",
        format!(
            "pinned Cerebras model(s) no longer in the catalog: {names} — every call \
             that falls over to cerebras will fail"
        ),
        Some(&format!("{env_key}=<one of: {}>", catalog.join(", "))),
    )
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
    let name_w = findings
        .iter()
        .map(|f| f.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<3} {:<width$}  {}",
        "sev",
        "name",
        "message",
        width = name_w
    );
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

    /// A pin that has left the catalog is a fallback that fails every call
    /// it serves — Cerebras dropped zai-glm-4.7 on 2026-08-17 with the model
    /// still named in a config somewhere.
    #[test]
    fn cerebras_models_finding_flags_a_missing_pin() {
        let catalog = || Ok(vec!["gpt-oss-120b".to_string(), "gemma-4-31b".to_string()]);
        let dead = cerebras_models_finding(&[(ModelTier::Fast, "zai-glm-4.7".into())], catalog());
        assert_eq!(dead.severity, Severity::Error);
        assert!(dead.message.contains("zai-glm-4.7"), "{}", dead.message);
        assert!(dead
            .suggested_cmd
            .as_deref()
            .unwrap_or_default()
            .contains("AUGMENTAGENT_MODEL_CEREBRAS_FAST="));

        let live = vec![
            (ModelTier::Quality, "gpt-oss-120b".to_string()),
            (ModelTier::Fast, "gemma-4-31b".to_string()),
        ];
        assert_eq!(
            cerebras_models_finding(&live, catalog()).severity,
            Severity::Ok
        );

        // An unreachable catalog is a network fact, not a config fault — an
        // offline box must not fail `doctor`.
        assert_eq!(
            cerebras_models_finding(&live, Err("request failed: timeout".into())).severity,
            Severity::Warn
        );
    }

    /// A box that merely keeps an unused CEREBRAS_API_KEY must get no live
    /// catalog request out of `doctor --deep`. The base URL below points at a
    /// closed port: any attempt surfaces as the "could not list" warning.
    #[tokio::test]
    async fn cerebras_models_skips_the_call_when_cerebras_is_not_in_the_chain() {
        std::env::set_var("AUGMENTAGENT_REASONER_CHAIN", "claude,gemini");
        std::env::set_var("AUGMENTAGENT_CEREBRAS_BASE_URL", "http://127.0.0.1:1/v1");
        let f = check_cerebras_models().await;
        std::env::remove_var("AUGMENTAGENT_REASONER_CHAIN");
        std::env::remove_var("AUGMENTAGENT_CEREBRAS_BASE_URL");
        assert_eq!(f.severity, Severity::Ok);
        assert!(
            f.message.contains("not in AUGMENTAGENT_REASONER_CHAIN"),
            "{}",
            f.message
        );
    }

    #[test]
    fn reasoner_chain_finding_names_unknown_tokens() {
        let typo = reasoner_chain_finding("claude,openai", &[]);
        assert_eq!(typo.severity, Severity::Warn);
        assert!(typo.message.contains("openai"), "{}", typo.message);
        assert!(typo
            .suggested_cmd
            .as_deref()
            .unwrap_or_default()
            .contains("AUGMENTAGENT_REASONER_CHAIN="));

        // The resolved models are the point of the check — read the
        // expectation through `model_for` so a developer with an
        // `AUGMENTAGENT_MODEL_*` override in their shell still passes.
        let default = reasoner_chain_finding("", &[]);
        assert_eq!(default.severity, Severity::Ok);
        let want = model_for(ProviderKind::Claude, ModelTier::Quality);
        assert!(
            default.message.contains("failover off") && default.message.contains(&want),
            "{}",
            default.message
        );

        let dark = reasoner_chain_finding(
            "claude,codex",
            &[(
                ProviderKind::Codex,
                "no CODEX_API_KEY and no auth.json".to_string(),
            )],
        );
        assert_eq!(dark.severity, Severity::Warn);
        assert!(dark.message.contains("codex"), "{}", dark.message);
    }

    #[test]
    fn workload_diagnostics_distinguish_capability_cooldown_and_missing_capacity() {
        let classes = reasoner_workload_findings("claude,cerebras", &[], &[]);
        assert_eq!(classes[0].severity, Severity::Ok);
        assert_eq!(classes[3].severity, Severity::Warn);
        assert!(classes[3].message.contains("cerebras: capability excluded"));
        assert!(classes[3].message.contains("no usable backup"));
        let latched = reasoner_workload_findings("claude,codex", &[], &[ProviderKind::Claude]);
        assert!(latched
            .iter()
            .all(|finding| finding.severity == Severity::Warn
                && finding.message.contains("claude: cooldown")
                && finding.message.contains("codex: candidate")));
        let unavailable = reasoner_workload_findings(
            "claude,codex",
            &[ProviderKind::Codex],
            &[ProviderKind::Claude],
        );
        assert!(unavailable
            .iter()
            .all(|finding| finding.severity == Severity::Error
                && finding.message.contains("codex: binary/auth unavailable")
                && finding.message.contains("no usable provider")));
    }

    #[test]
    fn workload_diagnostics_detect_missing_primary_binary() {
        if std::env::var_os("JARVIS_DOCTOR_CAPACITY_CHILD").is_none() {
            let state = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "doctor::tests::workload_diagnostics_detect_missing_primary_binary",
                ])
                .env("JARVIS_DOCTOR_CAPACITY_CHILD", "1")
                .env("AUGMENTAGENT_REASONER_CHAIN", "claude")
                .env("CLAUDE_CLI", "/nonexistent-synthetic-claude")
                .env(
                    "AUGMENTAGENT_COOLDOWN_FILE",
                    state.path().join("cooldown.json"),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let findings = check_reasoner_workloads();
        assert_eq!(findings.len(), 4);
        assert!(findings
            .iter()
            .all(|finding| finding.severity == Severity::Error
                && finding.message.contains("claude: binary/auth unavailable")));
    }

    /// #954 — name the wedge one timeout in, with the holder's caller preset.
    #[test]
    fn gate_finding_flags_a_permit_past_its_budget() {
        let now = 100_000u64;
        let wedged = |held_for: u64| cli_gate::GateSnapshot {
            pid: std::process::id(),
            capacity: 4,
            in_flight: 4,
            waiting: 7,
            oldest_provider: Some("claude".to_string()),
            oldest_caller: Some("TextOnly:triage-42".to_string()),
            oldest_since_unix: Some(now - held_for),
            oldest_budget_secs: Some(900),
        };
        // One second past its own budget is already the report #954 wanted.
        let stuck = gate_finding(Some(wedged(901)), now);
        assert_eq!(stuck.severity, Severity::Warn);
        let want = "in_flight 4/4, waiting 7; oldest permit (claude, TextOnly:triage-42) held 901s";
        assert!(stuck.message.starts_with(want), "{}", stuck.message);
        // Inside budget, a dead daemon and a fresh box are all fine.
        let dead = cli_gate::GateSnapshot {
            pid: u32::MAX,
            ..wedged(54_000)
        };
        for ok in [Some(wedged(900)), Some(dead), None] {
            assert_eq!(gate_finding(ok, now).severity, Severity::Ok);
        }
    }

    /// #1035 — a journal root past the count or size bound is a warning that
    /// names the operator entry point; an unreadable or public root too.
    #[test]
    fn handoff_journal_finding_warns_over_count_or_size() {
        let grace = Duration::from_secs(24 * 3600);
        let healthy = handoff::SweepReport {
            entries: 423,
            requests: 420,
            bytes: 900 * 1024 * 1024,
            removed: 40,
            finished_overdue: 0,
            kept_active: 3,
            kept_unfinished: 2,
            ..Default::default()
        };
        // #1071 — a legacy marker is information, not a warning: only an
        // operator can retire one, and the count shows that backlog draining.
        // A *clearable* orphan is a warning: the daemon's start-up pass should
        // have cleared it already.
        let legacy = handoff::OrphanReport { kept_live: 1, kept_legacy: 2, ..Default::default() };
        let ok = handoff_journal_finding(Ok(healthy), legacy, grace);
        assert_eq!(ok.severity, Severity::Ok, "{}", ok.message);
        assert!(ok.message.contains("0 orphaned, 2 written before the marker upgrade"), "{}", ok.message);
        let stuck = handoff::OrphanReport { cleared: 1, ..legacy };
        let stuck_finding = handoff_journal_finding(Ok(healthy), stuck, grace);
        assert!(stuck_finding.message.contains("not being cleared at daemon start"),
            "{}", stuck_finding.message);
        // Counts only request dirs, and reports what needs an operator as information.
        assert!(
            ok.message.contains("420 request dirs") && !ok.message.contains("423"),
            "{}",
            ok.message
        );
        assert!(
            ok.message.contains("3 with lifecycle markers") && ok.message.contains("2 unfinished"),
            "{}",
            ok.message
        );
        let many = handoff::SweepReport {
            requests: HANDOFF_WARN_REQUESTS + 1,
            ..healthy
        };
        let large = handoff::SweepReport {
            bytes: HANDOFF_WARN_BYTES + 1,
            ..healthy
        };
        // Past grace by more than two sweep intervals: a live sweep removes
        // every such journal, so even one means the sweep is not running.
        let stalled = handoff::SweepReport {
            finished_overdue: 1,
            ..healthy
        };
        let stalled_finding = handoff_journal_finding(Ok(stalled), legacy, grace);
        assert!(
            stalled_finding
                .message
                .contains("sweep does not appear to be running"),
            "{}",
            stalled_finding.message
        );
        let refused = Err(anyhow::anyhow!("handoff directory is not private"));
        for finding in [
            handoff_journal_finding(Ok(many), legacy, grace),
            handoff_journal_finding(Ok(large), legacy, grace),
            stalled_finding,
            stuck_finding,
            handoff_journal_finding(refused, legacy, grace),
        ] {
            assert_eq!(finding.severity, Severity::Warn, "{}", finding.message);
            assert_eq!(
                finding.suggested_cmd.as_deref(),
                Some("augmentagent handoff-prune --dry-run")
            );
        }
    }

    /// #1041 — doctor names each build-VM readiness state, with injected probes.
    #[test]
    fn build_vm_finding_distinguishes_every_readiness_state() {
        use augmentagent_channel_core::codex_tools::BuildRunner;
        let vm = BuildRunner::Vm(PathBuf::from("/synthetic/runtime.json"));
        let loaded = VmConfigProbe::Loaded { missing: vec![] };
        let durable = KvmProbe {
            exists: true,
            read_write: true,
            durable: true,
        };
        let ok = build_vm_finding(&vm, &loaded, &durable, "synthetic-user");
        assert_eq!(ok.severity, Severity::Ok, "{}", ok.message);
        assert!(ok.message.starts_with("ok"), "{}", ok.message);

        let unavailable = BuildRunner::Unavailable {
            reason: "build VM runtime configuration is missing",
        };
        for finding in [
            build_vm_finding(&unavailable, &VmConfigProbe::Missing, &durable, "u"),
            build_vm_finding(&vm, &VmConfigProbe::Missing, &durable, "u"),
        ] {
            assert_eq!(finding.severity, Severity::Error);
            assert!(
                finding.message.starts_with("config missing"),
                "{}",
                finding.message
            );
        }
        let host = build_vm_finding(&BuildRunner::Host, &VmConfigProbe::Missing, &durable, "u");
        assert_eq!(host.severity, Severity::Warn);
        assert!(
            host.message.contains("AUGMENTAGENT_BUILD_VM=host"),
            "{}",
            host.message
        );

        let no_qemu = build_vm_finding(
            &vm,
            &VmConfigProbe::Loaded {
                missing: vec!["qemu"],
            },
            &durable,
            "u",
        );
        assert_eq!(no_qemu.severity, Severity::Error);
        assert!(
            no_qemu.message.starts_with("qemu or kernel missing")
                && no_qemu.message.contains("qemu")
        );
        let no_kernel = build_vm_finding(
            &vm,
            &VmConfigProbe::Loaded {
                missing: vec!["kernel"],
            },
            &durable,
            "u",
        );
        assert!(
            no_kernel.message.starts_with("qemu or kernel missing")
                && no_kernel.message.contains("kernel")
        );

        for kvm in [
            KvmProbe {
                exists: false,
                read_write: false,
                durable: false,
            },
            KvmProbe {
                exists: true,
                read_write: false,
                durable: false,
            },
        ] {
            let finding = build_vm_finding(&vm, &loaded, &kvm, "u");
            assert_eq!(finding.severity, Severity::Error, "{kvm:?}");
            assert!(
                finding.message.starts_with("kvm not accessible"),
                "{}",
                finding.message
            );
        }
    }

    #[test]
    fn build_vm_finding_warns_when_kvm_depends_on_the_login_seat_acl() {
        use augmentagent_channel_core::codex_tools::BuildRunner;
        let vm = BuildRunner::Vm(PathBuf::from("/synthetic/runtime.json"));
        let acl_only = KvmProbe {
            exists: true,
            read_write: true,
            durable: false,
        };
        let finding = build_vm_finding(
            &vm,
            &VmConfigProbe::Loaded { missing: vec![] },
            &acl_only,
            "synthetic-user",
        );
        assert_eq!(finding.severity, Severity::Warn);
        assert!(
            finding.message.contains("ACL") && finding.message.contains("logout"),
            "{}",
            finding.message
        );
        assert!(finding
            .suggested_cmd
            .as_deref()
            .unwrap()
            .starts_with("sudo usermod -aG kvm synthetic-user"));
    }

    #[test]
    fn build_scratch_finding_reports_capacity_and_fails_closed_on_invalid_limits() {
        use augmentagent_channel_core::build_scratch::BuildScratchLimits;
        let gib = 1024u64 * 1024 * 1024;
        let limits = BuildScratchLimits {
            headroom_bytes: 20 * gib,
            cache_bytes: 12 * gib,
            budget_bytes: 24 * gib,
        };
        // 37 GiB free, empty root: budget admits floor(24/12)=2, free admits
        // floor((37-20)/12)=1 → min is 1.
        let one = build_scratch_finding(&limits, 37 * gib, 0, 0);
        assert_eq!(one.severity, Severity::Ok, "{}", one.message);
        assert!(one.message.contains("37.0 GiB free"), "{}", one.message);
        assert!(
            one.message.contains("20 GiB headroom")
                && one.message.contains("12 GiB image cap")
                && one.message.contains("24 GiB budget"),
            "{}",
            one.message
        );
        assert!(one.message.contains("1 session(s) admissible"), "{}", one.message);

        // Below the headroom → nothing admissible → a warning naming the
        // shared gate-cache trade-off.
        let none = build_scratch_finding(&limits, 15 * gib, 0, 0);
        assert_eq!(none.severity, Severity::Warn, "{}", none.message);
        assert!(none.message.contains("0 session(s) admissible"), "{}", none.message);
        assert!(none.message.contains("AUGMENTAGENT_GATE_CACHE_MAX_MB"), "{}", none.message);

        // An out-of-range limit (budget below the image cap) is an error that
        // names the fail-closed readiness category.
        let bad = BuildScratchLimits { budget_bytes: 4 * gib, ..limits };
        let err = build_scratch_finding(&bad, 500 * gib, 0, 0);
        assert_eq!(err.severity, Severity::Error, "{}", err.message);
        assert!(err.message.contains("build_scratch_limits"), "{}", err.message);
    }

    #[test]
    fn kvm_probe_reads_owner_and_group_bits_of_a_synthetic_device() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("kvm");
        std::fs::write(&device, "").unwrap();
        std::fs::set_permissions(&device, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            probe_kvm(&device),
            KvmProbe {
                exists: true,
                read_write: true,
                durable: true
            }
        );
        assert!(!probe_kvm(&dir.path().join("absent")).exists);
    }

    /// With an ACL, st_mode's group bits are the mask, not the owning group.
    #[test]
    fn durable_kvm_access_reads_the_group_acl_entry_not_the_mask() {
        const UID: u32 = 4242;
        const KVM: u32 = 993;
        let entry = |tag, perm, id| AclEntry { tag, perm, id };
        // root:kvm, st_mode 0660 where the group bits are the ACL mask (rw-).
        let seat_acl = vec![
            entry(ACL_USER_OBJ, 6, 0),
            entry(ACL_USER, 6, UID),
            entry(ACL_GROUP_OBJ, 0, 0),
            entry(ACL_MASK, 6, 0),
            entry(ACL_OTHER, 0, 0),
        ];
        assert!(
            !durable_kvm_access(0, KVM, 0o20660, Some(&seat_acl), UID, &[KVM]),
            "mask=rw- with group::--- grants the kvm group nothing"
        );
        let group_acl = vec![
            entry(ACL_USER_OBJ, 6, 0),
            entry(ACL_USER, 6, UID),
            entry(ACL_GROUP_OBJ, 6, 0),
            entry(ACL_MASK, 6, 0),
            entry(ACL_OTHER, 0, 0),
        ];
        assert!(durable_kvm_access(
            0,
            KVM,
            0o20660,
            Some(&group_acl),
            UID,
            &[KVM]
        ));
        assert!(
            !durable_kvm_access(0, KVM, 0o20660, Some(&group_acl), UID, &[]),
            "seat ACL alone is not durable"
        );
        let masked = vec![
            entry(ACL_USER_OBJ, 6, 0),
            entry(ACL_GROUP_OBJ, 6, 0),
            entry(ACL_MASK, 4, 0),
            entry(ACL_OTHER, 0, 0),
        ];
        assert!(
            !durable_kvm_access(0, KVM, 0o20640, Some(&masked), UID, &[KVM]),
            "the mask limits group::rw-"
        );
        let named_group = vec![
            entry(ACL_USER_OBJ, 6, 0),
            entry(ACL_GROUP_OBJ, 0, 0),
            entry(ACL_GROUP, 6, 77),
            entry(ACL_MASK, 6, 0),
            entry(ACL_OTHER, 0, 0),
        ];
        assert!(durable_kvm_access(
            0,
            KVM,
            0o20660,
            Some(&named_group),
            UID,
            &[77]
        ));
        // No ACL: plain mode bits decide.
        assert!(durable_kvm_access(0, KVM, 0o20660, None, UID, &[KVM]));
        assert!(!durable_kvm_access(0, KVM, 0o20660, None, UID, &[]));
        assert!(durable_kvm_access(UID, KVM, 0o20600, None, UID, &[]));
    }

    #[test]
    fn posix_acl_xattr_parses_entries_and_rejects_garbage() {
        let mut raw = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (ACL_USER_OBJ, 6u16, u32::MAX),
            (ACL_USER, 6, 1000),
            (ACL_GROUP_OBJ, 6, u32::MAX),
        ] {
            raw.extend(tag.to_le_bytes());
            raw.extend(perm.to_le_bytes());
            raw.extend(id.to_le_bytes());
        }
        let entries = parse_posix_acl(&raw).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[1],
            AclEntry {
                tag: ACL_USER,
                perm: 6,
                id: 1000
            }
        );
        assert!(parse_posix_acl(&raw[..7]).is_none());
        assert!(parse_posix_acl(&[1, 0, 0, 0]).is_none(), "unknown version");
    }

    #[test]
    fn vm_config_probe_reports_missing_invalid_and_absent_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_vm_config(&dir.path().join("runtime.json")),
            VmConfigProbe::Missing
        );
        let config = dir.path().join("runtime.json");
        std::fs::write(&config, "not json").unwrap();
        assert!(matches!(
            probe_vm_config(&config),
            VmConfigProbe::Invalid(_)
        ));
        let kernel = dir.path().join("vmlinuz");
        std::fs::write(&kernel, "").unwrap();
        std::fs::write(
            &config,
            serde_json::json!({"qemu": dir.path().join("absent-qemu"), "kernel": kernel})
                .to_string(),
        )
        .unwrap();
        assert_eq!(
            probe_vm_config(&config),
            VmConfigProbe::Loaded {
                missing: vec!["qemu"]
            }
        );
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
        let gmail = v
            .iter()
            .find(|f| f.name == "channel.gmail.validate")
            .unwrap();
        assert_eq!(gmail.severity, Severity::Ok);
        let slack = v
            .iter()
            .find(|f| f.name == "channel.slack.validate")
            .unwrap();
        assert_eq!(slack.severity, Severity::Warn);
        assert_eq!(
            slack.suggested_cmd.as_deref(),
            Some("augmentagent channel slack arm")
        );
    }
}
