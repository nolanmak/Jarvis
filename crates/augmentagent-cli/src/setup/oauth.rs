//! `augmentagent setup oauth <provider>` — localhost-callback OAuth driver.
//!
//! Drives the dashboard's existing per-provider OAuth flow end-to-end from
//! the CLI:
//!
//!   1. Preflight: GET `/api/v1/health` against the dashboard. If it doesn't
//!      answer 200, exit 30 with a JSON hint pointing at
//!      `augmentagent service start --unit dashboard`. (No point opening a
//!      browser when the callback won't be answered.)
//!   2. Snapshot the current connection set via GET `/api/v1/oauth/status`
//!      (the rollup endpoint that lands in this PR alongside the orchestrator).
//!   3. Open `http://localhost:${DASHBOARD_PORT:-3000}/oauth/<provider>/start`
//!      in a browser via `xdg-open` (Linux-only — see project memory). On a
//!      headless box (`$DISPLAY` empty) or with `--open-browser false`, just
//!      print the URL as JSON and let the operator open it on another host.
//!   4. Poll the rollup endpoint every 2s. When the connection set for the
//!      requested provider grows past the snapshot, emit the success JSON on
//!      stdout and exit 0. If `--timeout-secs` elapses first, emit the
//!      timeout JSON and exit 124. SIGINT cleanly aborts: stderr gets
//!      `{status:"interrupted"}`, exit 130.
//!
//! Output discipline: every poll iteration emits one JSON heartbeat on
//! STDERR (`{"event":"poll","elapsed_secs":N,"status":"waiting"}`) so the
//! `/setup` skill can stream progress without parsing the final result line.
//! The terminal/success/timeout/interrupt result lines all go to STDOUT (or
//! STDERR for interrupt) and are valid one-line JSON.
//!
//! Linux-only: `xdg-open` is the only browser bridge attempted. Project
//! memory notes there is no macOS counterpart on this box.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use serde_json::{json, Value};
use tokio::process::Command;

/// HTTP timeout for individual probe/poll calls. Kept tight so a wedged
/// dashboard doesn't strand the poll loop past the user-facing
/// `--timeout-secs` budget.
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

/// Sleep between rollup polls. The dashboard's callbacks land synchronously
/// from Composio, so 2s is a comfortable lower bound — much faster and we'd
/// just hammer the endpoint with no extra signal.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Exit code shared with `augmentagent status` (issue #1) for
/// dashboard-unreachable. Keeping the codes aligned so scripts can branch on
/// "is the dashboard up?" without caring which subcommand produced the code.
const EXIT_DASHBOARD_DOWN: i32 = 30;

/// Conventional UNIX timeout exit code (`timeout(1)` uses 124 too).
const EXIT_TIMEOUT: i32 = 124;

/// Conventional 128 + SIGINT(2) — what shells expect for an interrupted job.
const EXIT_INTERRUPTED: i32 = 130;

/// `augmentagent setup oauth <provider> [flags]` parsed args.
#[derive(Args, Debug, Clone)]
pub struct OauthArgs {
    /// Which OAuth provider to drive. Maps 1:1 to a dashboard
    /// `/oauth/<slug>/start` (or `/api/reddit/auth`) route — see
    /// [`OauthProvider::start_path`].
    #[arg(value_enum)]
    pub provider: OauthProvider,

    /// Maximum seconds to wait for a new active connection before giving up.
    /// Defaults to 5 minutes — long enough for a fresh consent + email
    /// verification round-trip, short enough to avoid hanging CI overnight.
    #[arg(long, default_value_t = 300)]
    pub timeout_secs: u64,

    /// Whether to attempt `xdg-open` on the start URL. Defaults to true.
    /// Pass `--open-browser false` on a headless box or when driving from a
    /// terminal multiplexer — the URL still prints as JSON so the operator
    /// can copy it elsewhere.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub open_browser: bool,

    /// Reserved for future text-mode output. Defaults to true (JSON is the
    /// only mode the orchestrator actually emits today; the `/setup` skill
    /// always wants machine output). The flag is accepted so the public CLI
    /// surface is stable from day one.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub json: bool,
}

/// One value per OAuth provider the dashboard already implements. Adding a
/// new variant here requires both a CLI start-URL mapping
/// ([`OauthProvider::start_path`]) AND a corresponding entry in the rollup
/// endpoint (`/api/v1/oauth/status`); the connection-grew predicate
/// ([`new_connection_appeared`]) understands the shape of each provider's
/// payload.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub enum OauthProvider {
    Gmail,
    Drive,
    Slack,
    Reddit,
}

impl OauthProvider {
    /// Provider slug used in JSON output. Stable — the `/setup` skill keys
    /// off this string. NOTE: `Drive` reports as `googledrive` to match the
    /// dashboard's existing toolkit slug; `Gmail/Slack/Reddit` are
    /// lowercased verbatim.
    pub fn slug(self) -> &'static str {
        match self {
            OauthProvider::Gmail => "gmail",
            OauthProvider::Drive => "googledrive",
            OauthProvider::Slack => "slack",
            OauthProvider::Reddit => "reddit",
        }
    }

    /// Path on the dashboard that bootstraps the OAuth redirect. Reddit
    /// lives under `/api/reddit/auth` (it's not a `/oauth/<x>/start` route —
    /// it shells out to `augmentagent reddit auth-url` internally), the
    /// rest follow the conventional shape.
    fn start_path(self) -> &'static str {
        match self {
            OauthProvider::Gmail => "/oauth/gmail/start",
            OauthProvider::Drive => "/oauth/googledrive/start",
            OauthProvider::Slack => "/oauth/slack/start",
            OauthProvider::Reddit => "/api/reddit/auth",
        }
    }
}

/// Public entrypoint called from `setup::run_setup`.
///
/// Returns once the orchestrator has emitted its terminal JSON line. The
/// concrete process exit code is applied via `std::process::exit` here
/// rather than bubbled — there's no useful work for `main.rs` to do after a
/// timeout/interrupt and the integer-exit-code contract is the public
/// surface the `/setup` skill matches against.
pub async fn run(args: &OauthArgs) -> Result<()> {
    let port = dashboard_port();
    let base = format!("http://127.0.0.1:{port}");
    let client = build_client()?;
    let api_key = std::env::var("AUGMENTAGENT_API_KEY").ok().filter(|s| !s.is_empty());

    // 1. Preflight — bail with exit 30 if dashboard isn't up. The CLI
    // refuses to even open the browser in that case because the localhost
    // callback won't be answered.
    if !health_ok(&client, &base).await {
        let doc = json!({
            "status": "dashboard_down",
            "error": "dashboard_down",
            "suggested_cmd": "augmentagent service start --unit dashboard",
        });
        print_json_stdout(&doc);
        std::process::exit(EXIT_DASHBOARD_DOWN);
    }

    // 2. Snapshot the BEFORE state so we can detect a *new* connection (vs.
    // simply confirming an already-present one).
    let before = fetch_status(&client, &base, api_key.as_deref())
        .await
        .context("snapshotting /api/v1/oauth/status before opening browser")?;

    // 3. Open the consent URL (or print it). Doesn't matter if xdg-open
    // succeeds — we always poll regardless.
    let start_url = format!("{base}{}", args.provider.start_path());
    open_browser_or_print(&start_url, args.open_browser).await;

    // 4. Poll until success / timeout / SIGINT.
    let timeout = Duration::from_secs(args.timeout_secs);
    let started = Instant::now();
    let mut tick = 0u64;

    loop {
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            let doc = json!({
                "status": "timeout",
                "provider": args.provider.slug(),
                "elapsed_secs": elapsed.as_secs(),
                "hint": "complete the consent flow then re-run",
            });
            print_json_stdout(&doc);
            std::process::exit(EXIT_TIMEOUT);
        }

        // Race a poll against the SIGINT future and a deadline. tokio::select!
        // cancels the losing branches cleanly — important so we don't leave
        // a half-issued HTTP request mid-flight after ^C.
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                eprintln!("{}", serde_json::to_string(&json!({
                    "status": "interrupted",
                    "provider": args.provider.slug(),
                    "elapsed_secs": elapsed.as_secs(),
                })).unwrap_or_else(|_| "{\"status\":\"interrupted\"}".into()));
                std::process::exit(EXIT_INTERRUPTED);
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                tick += 1;
                let heartbeat = json!({
                    "event": "poll",
                    "tick": tick,
                    "provider": args.provider.slug(),
                    "elapsed_secs": started.elapsed().as_secs(),
                    "status": "waiting",
                });
                eprintln!(
                    "{}",
                    serde_json::to_string(&heartbeat)
                        .unwrap_or_else(|_| "{\"event\":\"poll\"}".into())
                );

                // Probe — transient errors are logged to stderr and we keep
                // polling. Only a non-recoverable error (which `reqwest`
                // doesn't really produce here) would bubble.
                let now = match fetch_status(&client, &base, api_key.as_deref()).await {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "{}",
                            serde_json::to_string(&json!({
                                "event": "poll_error",
                                "error": e.to_string(),
                            }))
                            .unwrap_or_else(|_| "{\"event\":\"poll_error\"}".into())
                        );
                        continue;
                    }
                };

                if let Some(success) = new_connection_appeared(args.provider, &before, &now) {
                    print_json_stdout(&success);
                    std::process::exit(0);
                }
            }
        }
    }
}

/// Dashboard port — same precedence as `status.rs::probe_dashboard`.
fn dashboard_port() -> u16 {
    std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000)
}

/// Build the shared HTTP client. Tight 2s timeout — if the dashboard wedges
/// mid-poll we'd rather drop the request and retry on the next tick than
/// stall the poll loop.
fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building reqwest client")
}

/// Preflight probe — true iff the dashboard answers 200 on `/api/v1/health`.
/// 401 is NOT treated as alive here (unlike `dashboard_reachable` in
/// `status.rs`): `/api/v1/health` is explicitly un-authed in the matching
/// dashboard route so a 401 means we hit something else entirely (proxy,
/// wrong port, …).
async fn health_ok(client: &reqwest::Client, base: &str) -> bool {
    let url = format!("{base}/api/v1/health");
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Fetch the rollup. Auth header is added when `AUGMENTAGENT_API_KEY` is set
/// (this endpoint sits behind the same gate as the rest of `/api/v1/*`).
async fn fetch_status(
    client: &reqwest::Client,
    base: &str,
    api_key: Option<&str>,
) -> Result<Value> {
    let url = format!("{base}/api/v1/oauth/status");
    let mut req = client.get(&url);
    if let Some(k) = api_key {
        req = req.header("x-api-key", k);
    }
    let resp = req.send().await.context("GET /api/v1/oauth/status")?;
    if !resp.status().is_success() {
        anyhow::bail!("status endpoint returned HTTP {}", resp.status().as_u16());
    }
    let v: Value = resp.json().await.context("parsing rollup JSON")?;
    Ok(v)
}

/// Spawn `xdg-open <url>` detached. We don't wait on it — `xdg-open`
/// typically forks the real browser process and exits immediately, but on
/// some desktops it lingers. Either way we don't care about the result;
/// the polling loop is the source of truth for success.
///
/// Falls back to the JSON `{action:"open_url", url:"..."}` line on stdout
/// when:
///   - `--open-browser false` was passed,
///   - `$DISPLAY` is empty (headless box — `xdg-open` would just print the
///     URL itself, which would race our heartbeats on the same stdout), or
///   - `xdg-open` couldn't be spawned (binary missing).
async fn open_browser_or_print(url: &str, open_browser: bool) {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    let want_browser = open_browser && !display.is_empty();
    if want_browser {
        let spawn = Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawn.is_ok() {
            return;
        }
        // Spawn failed → fall through to the print path so the operator
        // still has a URL to paste.
    }
    let doc = json!({ "action": "open_url", "url": url });
    print_json_stdout(&doc);
}

/// Pretty-printer used for every JSON line the orchestrator emits on stdout.
/// One-line JSON keeps the output friendly for `jq -c`–style consumers and
/// for the `/setup` skill, which reads the last stdout line as the result.
fn print_json_stdout(v: &Value) {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    println!("{s}");
}

/// Detect whether `now` shows a new active connection for `provider` vs.
/// `before`. Returns the success JSON document on hit (so the caller can
/// emit it verbatim), `None` otherwise.
///
/// "Grew" means: the set of *ids* present in `now` is not a subset of the
/// set present in `before`. Comparing by id (not by count) avoids a
/// false-positive when one account is removed and another is added in the
/// same window.
fn new_connection_appeared(provider: OauthProvider, before: &Value, now: &Value) -> Option<Value> {
    match provider {
        OauthProvider::Gmail => connection_diff(before, now, "gmail", "accounts", "id", "gmail"),
        OauthProvider::Drive => connection_diff(before, now, "googledrive", "accounts", "id", "googledrive"),
        OauthProvider::Slack => connection_diff(before, now, "slack", "workspaces", "team_id", "slack"),
        OauthProvider::Reddit => {
            let before_conn = before
                .pointer("/reddit/connected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let now_conn = now
                .pointer("/reddit/connected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !before_conn && now_conn {
                Some(json!({
                    "status": "connected",
                    "provider": "reddit",
                    "connected": true,
                }))
            } else {
                None
            }
        }
    }
}

/// Generic "did the connection set grow?" check shared by the account/workspace
/// providers. Returns the success JSON with the FULL `now` list (not just the
/// new entries) so the `/setup` skill can render a complete picture without
/// a follow-up GET.
fn connection_diff(
    before: &Value,
    now: &Value,
    provider_key: &str,
    list_key: &str,
    id_field: &str,
    out_provider: &str,
) -> Option<Value> {
    let before_list = extract_list(before, provider_key, list_key);
    let now_list = extract_list(now, provider_key, list_key);

    let before_ids: std::collections::HashSet<String> = before_list
        .iter()
        .filter_map(|v| v.get(id_field).and_then(|s| s.as_str()).map(String::from))
        .collect();

    let has_new = now_list.iter().any(|v| {
        v.get(id_field)
            .and_then(|s| s.as_str())
            .map(|s| !before_ids.contains(s))
            .unwrap_or(false)
    });

    if !has_new {
        return None;
    }

    let mut doc = serde_json::Map::new();
    doc.insert("status".into(), json!("connected"));
    doc.insert("provider".into(), json!(out_provider));
    doc.insert(list_key.into(), Value::Array(now_list.clone()));
    Some(Value::Object(doc))
}

/// Pull the inner list out of `{provider:{list_key:[...]}}`. Returns an
/// empty vec when any layer is missing — the dashboard returns the keys
/// unconditionally, but a half-built rollup endpoint shouldn't crash the
/// orchestrator.
fn extract_list(root: &Value, provider_key: &str, list_key: &str) -> Vec<Value> {
    root.get(provider_key)
        .and_then(|p| p.get(list_key))
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_slugs_are_stable() {
        assert_eq!(OauthProvider::Gmail.slug(), "gmail");
        assert_eq!(OauthProvider::Drive.slug(), "googledrive");
        assert_eq!(OauthProvider::Slack.slug(), "slack");
        assert_eq!(OauthProvider::Reddit.slug(), "reddit");
    }

    #[test]
    fn start_paths_match_dashboard_routes() {
        // These must match the routes in src/dashboard.ts / src/apiV1.ts —
        // if the dashboard renames a route, this test still passes (it's a
        // string compare), but the integration breaks. Hardcoded so a
        // future grep for the route name lands here.
        assert_eq!(OauthProvider::Gmail.start_path(), "/oauth/gmail/start");
        assert_eq!(OauthProvider::Drive.start_path(), "/oauth/googledrive/start");
        assert_eq!(OauthProvider::Slack.start_path(), "/oauth/slack/start");
        assert_eq!(OauthProvider::Reddit.start_path(), "/api/reddit/auth");
    }

    #[test]
    fn gmail_diff_detects_new_account() {
        let before = json!({
            "gmail": {"accounts": [{"id": "a1", "email": "old@x.com"}], "lastError": null}
        });
        let now = json!({
            "gmail": {"accounts": [
                {"id": "a1", "email": "old@x.com"},
                {"id": "a2", "email": "new@x.com"},
            ], "lastError": null}
        });
        let hit = new_connection_appeared(OauthProvider::Gmail, &before, &now);
        let doc = hit.expect("should detect new gmail account");
        assert_eq!(doc.get("status").and_then(|v| v.as_str()), Some("connected"));
        assert_eq!(doc.get("provider").and_then(|v| v.as_str()), Some("gmail"));
        let accounts = doc.get("accounts").and_then(|v| v.as_array()).unwrap();
        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn gmail_diff_ignores_unchanged_set() {
        let same = json!({
            "gmail": {"accounts": [{"id": "a1"}], "lastError": null}
        });
        assert!(new_connection_appeared(OauthProvider::Gmail, &same, &same).is_none());
    }

    #[test]
    fn gmail_diff_ignores_pure_removal() {
        // a1 removed but nothing added → not a success.
        let before = json!({"gmail": {"accounts": [{"id": "a1"}, {"id": "a2"}]}});
        let now = json!({"gmail": {"accounts": [{"id": "a1"}]}});
        assert!(new_connection_appeared(OauthProvider::Gmail, &before, &now).is_none());
    }

    #[test]
    fn drive_diff_uses_googledrive_key() {
        let before = json!({"googledrive": {"accounts": []}});
        let now = json!({"googledrive": {"accounts": [{"id": "d1"}]}});
        let hit = new_connection_appeared(OauthProvider::Drive, &before, &now);
        let doc = hit.expect("should detect new drive account");
        assert_eq!(doc.get("provider").and_then(|v| v.as_str()), Some("googledrive"));
    }

    #[test]
    fn slack_diff_keys_on_team_id() {
        let before = json!({"slack": {"workspaces": [{"team_id": "T1"}]}});
        let now = json!({
            "slack": {"workspaces": [{"team_id": "T1"}, {"team_id": "T2"}]}
        });
        let hit = new_connection_appeared(OauthProvider::Slack, &before, &now);
        let doc = hit.expect("should detect new slack workspace");
        assert_eq!(doc.get("provider").and_then(|v| v.as_str()), Some("slack"));
        assert_eq!(
            doc.get("workspaces").and_then(|v| v.as_array()).unwrap().len(),
            2
        );
    }

    #[test]
    fn reddit_diff_detects_false_to_true() {
        let before = json!({"reddit": {"connected": false}});
        let now = json!({"reddit": {"connected": true}});
        let hit = new_connection_appeared(OauthProvider::Reddit, &before, &now);
        let doc = hit.expect("should detect reddit connect");
        assert_eq!(doc.get("status").and_then(|v| v.as_str()), Some("connected"));
        assert_eq!(doc.get("connected").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn reddit_diff_ignores_unchanged() {
        let same = json!({"reddit": {"connected": true}});
        assert!(new_connection_appeared(OauthProvider::Reddit, &same, &same).is_none());

        let same_false = json!({"reddit": {"connected": false}});
        assert!(new_connection_appeared(OauthProvider::Reddit, &same_false, &same_false).is_none());
    }

    #[test]
    fn extract_list_tolerates_missing_keys() {
        // Half-built rollup payloads shouldn't crash the orchestrator.
        let empty = json!({});
        assert!(extract_list(&empty, "gmail", "accounts").is_empty());
        let partial = json!({"gmail": {}});
        assert!(extract_list(&partial, "gmail", "accounts").is_empty());
    }
}
