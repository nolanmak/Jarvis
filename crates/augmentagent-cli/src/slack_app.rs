//! #1284 — `augmentagent slack app …`: install, verify, status, rotate and
//! remove the interactive Slack app (Socket Mode), separate from the
//! Composio connection managed by the other `slack` subcommands.
//!
//! Tokens are read from stdin (`--stdin`, one per line, any order), from
//! files (`--app-token-file` / `--bot-token-file`) or from
//! `AUGMENTAGENT_SLACK_APP_TOKEN` / `AUGMENTAGENT_SLACK_BOT_TOKEN`; never
//! from plain arguments, which leak into shell history and the process list.
//! Tokens are never printed or logged. Every failure prints a recovery hint
//! and exits 1; `--json` puts `{ok:false, error, message, recovery}` on
//! stdout. Operator guide: `docs/SLACK-APP.md`.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use augmentagent_channel_slack::app::{
    self, api_base_from, parse_app_token, parse_bot_token, HttpSlackAppConnector, SlackAppError,
    SlackAppStore, SlackAppSummary, TokenKind, APP_CREDENTIAL_PLATFORM, MANIFEST_JSON,
    REQUIRED_BOT_SCOPES, SLACK_API_BASE_ENV,
};
use augmentagent_channel_slack::owner_setup::{self, DirectConversation};
use augmentagent_channel_slack::transport::{AppLevelToken, BotToken};
use augmentagent_store::Store;
use clap::{Args, Subcommand};
use serde_json::{json, Value};

pub const APP_TOKEN_ENV: &str = "AUGMENTAGENT_SLACK_APP_TOKEN";
pub const BOT_TOKEN_ENV: &str = "AUGMENTAGENT_SLACK_BOT_TOKEN";

/// Largest stdin / token file we read. Tokens are ~100 bytes.
const MAX_TOKEN_INPUT: u64 = 64 * 1024;

const DAEMON_ACCESS_NOTE: &str = "unverified: a credential stored from a terminal is not proven readable by the launchd/systemd daemon; on macOS run `augmentagent doctor --keychain-probe`";

#[derive(Subcommand, Debug, Clone)]
pub enum SlackAppOp {
    /// Print the Slack app manifest (JSON) to create the app from at
    /// api.slack.com/apps > Create New App > From a manifest.
    Manifest,
    /// Verify both tokens live (`auth.test`, `apps.connections.open`) and
    /// store them for the workspace they belong to. Reinstalling replaces
    /// the stored tokens.
    Install {
        #[command(flatten)]
        tokens: TokenArgs,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Live-check the stored tokens and report workspace, bot user and
    /// granted scopes.
    Verify {
        /// Workspace team id; defaults to the only installed workspace.
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Show installed workspaces from local state (no network).
    Status {
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Replace the app-level token, the bot token, or both. The new tokens
    /// must belong to the same workspace; on failure nothing changes.
    Rotate {
        #[arg(long)]
        team: Option<String>,
        #[command(flatten)]
        tokens: TokenArgs,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Delete the stored app credentials for a workspace. Leaves the
    /// Composio connection alone. Does not revoke the tokens at Slack.
    #[command(visible_alias = "disconnect")]
    Remove {
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Who owns the agent on Slack (#1286): bind the owner's member ID,
    /// choose the private control channel, show or unbind.
    Owner {
        #[command(subcommand)]
        op: OwnerOp,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum OwnerOp {
    /// Verify a member ID live (full member of the installed workspace, not
    /// a guest, bot or external user), bind it as the only owner and record
    /// the owner's DM with the app. Replaces any previous owner.
    Bind {
        /// Slack member ID (profile > "Copy member ID"). Names and emails
        /// are not accepted.
        #[arg(long, value_name = "MEMBER_ID")]
        user: String,
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Show the bound owner, DM, control channel and rejection count
    /// (local state, no network).
    Show {
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Remove the owner binding and its control conversations. Idempotent.
    Unbind {
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Choose or clear the owner's private control channel.
    Control {
        #[command(subcommand)]
        op: ControlOp,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ControlOp {
    /// Verify the channel live (private, not shared with another
    /// organization, not archived, the app is a member) and make it the
    /// control channel, replacing any previous one.
    Set {
        #[arg(long, value_name = "CHANNEL_ID")]
        channel: String,
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Stop using the control channel. The DM with the app keeps working.
    Remove {
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
}

impl SlackAppOp {
    fn json(&self) -> bool {
        match self {
            SlackAppOp::Manifest => false,
            SlackAppOp::Install { json, .. }
            | SlackAppOp::Verify { json, .. }
            | SlackAppOp::Status { json, .. }
            | SlackAppOp::Rotate { json, .. }
            | SlackAppOp::Remove { json, .. } => *json,
            SlackAppOp::Owner { op } => match op {
                OwnerOp::Bind { json, .. }
                | OwnerOp::Show { json, .. }
                | OwnerOp::Unbind { json, .. } => *json,
                OwnerOp::Control { op } => match op {
                    ControlOp::Set { json, .. } | ControlOp::Remove { json, .. } => *json,
                },
            },
        }
    }
}

/// Where tokens come from. There is deliberately no flag that takes a token
/// value.
#[derive(Args, Debug, Clone, Default)]
pub struct TokenArgs {
    /// Read tokens from stdin, one per line in any order (app-level and/or
    /// bot token, told apart by prefix).
    #[arg(long)]
    pub stdin: bool,
    /// File holding the app-level token.
    #[arg(long, value_name = "PATH")]
    pub app_token_file: Option<PathBuf>,
    /// File holding the bot token.
    #[arg(long, value_name = "PATH")]
    pub bot_token_file: Option<PathBuf>,
}

/// Tokens resolved from the allowed sources.
#[derive(Debug, Default)]
pub struct ResolvedTokens {
    pub app: Option<AppLevelToken>,
    pub bot: Option<BotToken>,
}

/// Resolve tokens: file flag, then stdin, then environment. A token given
/// by two explicit sources (file and stdin) is an error.
pub fn resolve_tokens(
    args: &TokenArgs,
    stdin: &mut dyn Read,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedTokens, SlackAppError> {
    let mut from_stdin = ResolvedTokens::default();
    if args.stdin {
        let mut buf = String::new();
        stdin
            .take(MAX_TOKEN_INPUT)
            .read_to_string(&mut buf)
            .map_err(|e| SlackAppError::TokenInput(format!("read stdin: {e}")))?;
        for (n, line) in buf.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let label = format!("stdin line {}", n + 1);
            match TokenKind::of(line) {
                TokenKind::AppLevel => {
                    if from_stdin.app.is_some() {
                        return Err(SlackAppError::TokenInput(
                            "stdin has more than one app-level token".into(),
                        ));
                    }
                    from_stdin.app = Some(parse_app_token(line)?);
                }
                TokenKind::Bot => {
                    if from_stdin.bot.is_some() {
                        return Err(SlackAppError::TokenInput(
                            "stdin has more than one bot token".into(),
                        ));
                    }
                    from_stdin.bot = Some(parse_bot_token(line)?);
                }
                other => {
                    return Err(SlackAppError::TokenInput(format!(
                        "{label} is a {}; only an app-level token and a bot token are accepted (never user or refresh tokens)",
                        other.label()
                    )))
                }
            }
        }
    }

    let app = match (&args.app_token_file, from_stdin.app) {
        (Some(_), Some(_)) => {
            return Err(SlackAppError::TokenInput(
                "app-level token given both on stdin and with --app-token-file".into(),
            ))
        }
        (Some(path), None) => Some(parse_app_token(&read_token_file(path)?)?),
        (None, Some(t)) => Some(t),
        (None, None) => match env(APP_TOKEN_ENV).filter(|v| !v.trim().is_empty()) {
            Some(v) => Some(parse_app_token(&v)?),
            None => None,
        },
    };
    let bot = match (&args.bot_token_file, from_stdin.bot) {
        (Some(_), Some(_)) => {
            return Err(SlackAppError::TokenInput(
                "bot token given both on stdin and with --bot-token-file".into(),
            ))
        }
        (Some(path), None) => Some(parse_bot_token(&read_token_file(path)?)?),
        (None, Some(t)) => Some(t),
        (None, None) => match env(BOT_TOKEN_ENV).filter(|v| !v.trim().is_empty()) {
            Some(v) => Some(parse_bot_token(&v)?),
            None => None,
        },
    };
    Ok(ResolvedTokens { app, bot })
}

fn read_token_file(path: &Path) -> Result<String, SlackAppError> {
    let file = std::fs::File::open(path)
        .map_err(|e| SlackAppError::TokenInput(format!("open {}: {e}", path.display())))?;
    let mut buf = String::new();
    file.take(MAX_TOKEN_INPUT)
        .read_to_string(&mut buf)
        .map_err(|e| SlackAppError::TokenInput(format!("read {}: {e}", path.display())))?;
    Ok(buf)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fmt_time(secs: Option<u64>) -> String {
    match secs.and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s as i64, 0)) {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "never".into(),
    }
}

fn composio_teams(store: &Store) -> Vec<String> {
    store
        .list_active_slack_workspaces()
        .map(|ws| ws.into_iter().map(|w| w.team_id).collect())
        .unwrap_or_default()
}

fn connector() -> Result<HttpSlackAppConnector, SlackAppError> {
    let raw = std::env::var(SLACK_API_BASE_ENV).ok();
    let base = api_base_from(raw.as_deref())?;
    if raw.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        tracing::warn!(api_base = %base, "{SLACK_API_BASE_ENV} overrides the Slack Web API base URL");
    }
    Ok(HttpSlackAppConnector::new(base))
}

fn scope_line(s: &SlackAppSummary) -> String {
    match (&s.scopes, &s.missing_scopes) {
        (None, _) => {
            "unknown (Slack did not report granted scopes; run `slack app verify` later)".into()
        }
        (Some(g), Some(m)) if m.is_empty() => format!(
            "all {} required scopes granted ({} total)",
            REQUIRED_BOT_SCOPES.len(),
            g.len()
        ),
        (Some(g), Some(m)) => format!("{} granted; MISSING {}", g.len(), m.join(", ")),
        (Some(g), None) => format!("{} granted", g.len()),
    }
}

fn print_summary(s: &SlackAppSummary, backend: &str, composio: bool) {
    let team = s.team_name.as_deref().unwrap_or("(unnamed workspace)");
    println!("  workspace:  {team} ({})", s.team_id);
    println!(
        "  bot user:   {}{}{}",
        s.bot_user_name
            .as_deref()
            .map(|n| format!("@{n} "))
            .unwrap_or_default(),
        format_args!("({})", s.bot_user_id),
        s.bot_id
            .as_deref()
            .map(|b| format!(", bot {b}"))
            .unwrap_or_default(),
    );
    if let Some(a) = &s.app_id {
        println!("  app:        {a}");
    }
    println!("  scopes:     {}", scope_line(s));
    println!(
        "  stored in:  {backend} (augmentagent/{APP_CREDENTIAL_PLATFORM}/{})",
        s.team_id
    );
    println!(
        "  installed:  {}   verified: {}   rotated: {}",
        fmt_time(Some(s.installed_at)),
        fmt_time(s.verified_at),
        fmt_time(s.rotated_at)
    );
    println!(
        "  composio:   {}",
        if composio {
            "also connected (ingestion runs separately; see docs/SLACK-APP.md)"
        } else {
            "not connected for this workspace"
        }
    );
    println!("  daemon access: {DAEMON_ACCESS_NOTE}");
}

fn success(json_out: bool, value: Value, human: impl FnOnce()) -> Result<()> {
    if json_out {
        println!("{value}");
    } else {
        human();
    }
    Ok(())
}

fn fail(json_out: bool, e: &SlackAppError) -> ! {
    if json_out {
        println!(
            "{}",
            json!({
                "ok": false,
                "error": e.code(),
                "message": e.to_string(),
                "recovery": e.recovery(),
            })
        );
    } else {
        eprintln!("error: {e}");
        eprintln!("recovery: {}", e.recovery());
    }
    std::process::exit(1);
}

/// Entry point for `augmentagent slack app …`.
pub async fn run(op: &SlackAppOp, store: &Store) -> Result<()> {
    let creds = SlackAppStore::default_store();
    let json_out = op.json();
    match run_inner(op, store, &creds).await {
        Ok(()) => Ok(()),
        Err(e) => fail(json_out, &e),
    }
}

async fn run_inner(
    op: &SlackAppOp,
    store: &Store,
    creds: &SlackAppStore,
) -> Result<(), SlackAppError> {
    let env = |k: &str| std::env::var(k).ok();
    match op {
        SlackAppOp::Manifest => {
            print!("{MANIFEST_JSON}");
            Ok(())
        }
        SlackAppOp::Owner { op } => run_owner(op, store, creds).await,
        SlackAppOp::Install { tokens, json } => {
            let t = resolve_tokens(tokens, &mut std::io::stdin().lock(), &env)?;
            let (Some(app_token), Some(bot_token)) = (t.app, t.bot) else {
                return Err(SlackAppError::TokenInput(
                    "install needs both the app-level token and the bot token".into(),
                ));
            };
            let connector = connector()?;
            let out = app::install(creds, &connector, app_token, bot_token, now_secs()).await?;
            let composio = composio_teams(store).contains(&out.summary.team_id);
            let action = if out.replaced {
                "reinstalled"
            } else {
                "installed"
            };
            let backend = creds.backend();
            let _ = success(
                *json,
                json!({
                    "ok": true,
                    "action": action,
                    "install": out.summary,
                    "credential_backend": backend,
                    "composio_connected": composio,
                    "daemon_credential_access": "unverified",
                }),
                || {
                    println!(
                        "Slack app {action} for {} ({})",
                        out.summary.team_name.as_deref().unwrap_or("workspace"),
                        out.summary.team_id
                    );
                    print_summary(&out.summary, backend, composio);
                },
            );
            Ok(())
        }
        SlackAppOp::Verify { team, json } => {
            let team = creds.resolve_team(team.as_deref())?;
            let connector = connector()?;
            let summary = app::verify(creds, &connector, &team, now_secs()).await?;
            let composio = composio_teams(store).contains(&team);
            let backend = creds.backend();
            let _ = success(
                *json,
                json!({
                    "ok": true,
                    "install": summary,
                    "credential_backend": backend,
                    "composio_connected": composio,
                    "daemon_credential_access": "unverified",
                }),
                || {
                    println!("Slack app tokens for {team} are valid (checked live)");
                    print_summary(&summary, backend, composio);
                },
            );
            Ok(())
        }
        SlackAppOp::Rotate { team, tokens, json } => {
            let team = creds.resolve_team(team.as_deref())?;
            let t = resolve_tokens(tokens, &mut std::io::stdin().lock(), &env)?;
            let connector = connector()?;
            let summary = app::rotate(creds, &connector, &team, t.app, t.bot, now_secs()).await?;
            let composio = composio_teams(store).contains(&team);
            let backend = creds.backend();
            let _ = success(
                *json,
                json!({
                    "ok": true,
                    "action": "rotated",
                    "install": summary,
                    "credential_backend": backend,
                    "composio_connected": composio,
                    "daemon_credential_access": "unverified",
                }),
                || {
                    println!("Slack app tokens rotated for {team}. Restart the daemon so it picks up the new tokens.");
                    print_summary(&summary, backend, composio);
                },
            );
            Ok(())
        }
        SlackAppOp::Remove { team, json } => {
            let team = creds.resolve_team(team.as_deref())?;
            let removed = creds.remove(&team)?;
            let composio = composio_teams(store).contains(&team);
            let _ = success(
                *json,
                json!({
                    "ok": true,
                    "team_id": team,
                    "removed": removed,
                    "composio_connected": composio,
                }),
                || {
                    if removed {
                        println!("Removed Slack app credentials for {team} (augmentagent/{APP_CREDENTIAL_PLATFORM}/{team}).");
                    } else {
                        println!(
                            "No Slack app credentials were stored for {team}; nothing to remove."
                        );
                    }
                    if composio {
                        println!("The Composio connection for {team} is unchanged; ingestion keeps running.");
                    }
                    println!("This does not revoke the tokens at Slack. To revoke them, uninstall the app from the workspace or regenerate its tokens at api.slack.com/apps.");
                },
            );
            Ok(())
        }
        SlackAppOp::Status { team, json } => {
            let teams = match team {
                Some(t) => vec![t.clone()],
                None => creds.teams()?,
            };
            let composio = composio_teams(store);
            let mut rows = Vec::new();
            for t in &teams {
                let row = match creds.load(t) {
                    Ok(Some(c)) => {
                        let mut v = serde_json::to_value(c.summary()).unwrap_or(Value::Null);
                        v["credentials"] = json!("ok");
                        v
                    }
                    Ok(None) => json!({
                        "team_id": t,
                        "credentials": "missing",
                        "recovery": SlackAppError::NotInstalled { team_id: t.clone() }.recovery(),
                    }),
                    Err(e) => json!({
                        "team_id": t,
                        "credentials": e.code(),
                        "message": e.to_string(),
                        "recovery": e.recovery(),
                    }),
                };
                let mut row = row;
                row["composio_connected"] = json!(composio.contains(t));
                rows.push(row);
            }
            let backend = creds.backend();
            let _ = success(
                *json,
                json!({
                    "ok": true,
                    "credential_backend": backend,
                    "daemon_credential_access": "unverified",
                    "installs": rows,
                    "composio_workspaces": composio,
                }),
                || {
                    if rows.is_empty() {
                        println!("No Slack app installed. Create it from `augmentagent slack app manifest`, then run `augmentagent slack app install --stdin`.");
                    }
                    for (t, row) in teams.iter().zip(&rows) {
                        if row["credentials"] == json!("ok") {
                            if let Ok(Some(c)) = creds.load(t) {
                                println!("Slack app: {t}");
                                print_summary(&c.summary(), backend, composio.contains(t));
                            }
                        } else {
                            println!(
                                "Slack app: {t}  credentials {}",
                                row["credentials"].as_str().unwrap_or("?")
                            );
                            if let Some(r) = row["recovery"].as_str() {
                                println!("  recovery: {r}");
                            }
                        }
                    }
                    println!(
                        "Status is local; run `augmentagent slack app verify` for a live check."
                    );
                },
            );
            Ok(())
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fmt_ms(ms: i64) -> String {
    match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms) {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "unknown".into(),
    }
}

async fn run_owner(
    op: &OwnerOp,
    store: &Store,
    creds: &SlackAppStore,
) -> Result<(), SlackAppError> {
    match op {
        OwnerOp::Bind { user, team, json } => {
            let connector = connector()?;
            let out = owner_setup::bind_owner(
                store,
                creds,
                &connector,
                team.as_deref(),
                user.trim(),
                now_ms(),
            )
            .await?;
            let action = if out.replaced_owner.is_some() {
                "rebound"
            } else {
                "bound"
            };
            let _ = success(
                *json,
                json!({
                    "ok": true,
                    "action": action,
                    "team_id": out.team_id,
                    "enterprise_id": out.enterprise_id,
                    "owner": {
                        "user_id": out.owner_user_id,
                        "name": out.owner_name,
                        "real_name": out.owner_real_name,
                    },
                    "direct_conversation": out.direct_conversation,
                    "replaced_owner": out.replaced_owner,
                    "confirmed_at_ms": out.confirmed_at_ms,
                }),
                || {
                    let who = match (&out.owner_real_name, &out.owner_name) {
                        (Some(r), Some(n)) => format!("{r} (@{n}, {})", out.owner_user_id),
                        (_, Some(n)) => format!("@{n} ({})", out.owner_user_id),
                        _ => out.owner_user_id.clone(),
                    };
                    println!("Slack owner {action} for {}: {who}", out.team_id);
                    if let Some(prev) = &out.replaced_owner {
                        println!("  replaced:   {prev} (its control channel was cleared)");
                    }
                    match &out.direct_conversation {
                        DirectConversation::Recorded(dm) => {
                            println!("  DM:         {dm} (the owner's DM with the app)")
                        }
                        DirectConversation::Unresolved(why) => println!(
                            "  DM:         not recorded ({why}); any DM with the app from this owner is accepted. Add the im:write scope and bind again to record it."
                        ),
                    }
                    println!("  identity:   member ID only; names and emails are never used for authority");
                    println!("Optional: `augmentagent slack app owner control set --channel <private channel id>`.");
                },
            );
            Ok(())
        }
        OwnerOp::Show { team, json } => {
            let st = owner_setup::owner_status(store, creds, team.as_deref())?;
            let b = st.binding.as_ref();
            let dm = b
                .and_then(|b| b.direct_conversation())
                .map(|c| c.conversation_id().to_string());
            let channel = b
                .and_then(|b| b.control_channel())
                .map(|c| c.conversation_id().to_string());
            let dm_rule = match (&b, &dm) {
                (None, _) => Value::Null,
                (_, Some(_)) => json!("recorded"),
                (_, None) => json!("any_dm_with_app"),
            };
            let _ = success(
                *json,
                json!({
                    "ok": true,
                    "team_id": st.team_id,
                    "bound": b.is_some(),
                    "account": b.map(|b| b.owner.account().account_id().to_string()),
                    "owner_user_id": b.map(|b| b.owner.sender_id().to_string()),
                    "confirmed_at_ms": b.map(|b| b.confirmed_at_ms),
                    "direct_conversation": dm,
                    "direct_conversation_rule": dm_rule,
                    "control_channel": channel,
                    "app_installed": st.bot.is_some(),
                    "bot": st.bot.as_ref().map(|bot| json!({
                        "bot_user_id": bot.bot_user_id,
                        "bot_id": bot.bot_id,
                        "app_id": bot.app_id,
                    })),
                    "rejections": st.rejections,
                }),
                || {
                    let Some(b) = b else {
                        println!("No Slack owner bound for {}. Run `augmentagent slack app owner bind --user <member_id>`.", st.team_id);
                        return;
                    };
                    println!(
                        "Slack owner for {} ({})",
                        st.team_id,
                        b.owner.account().account_id()
                    );
                    println!("  owner:      {}", b.owner.sender_id());
                    println!("  confirmed:  {}", fmt_ms(b.confirmed_at_ms));
                    println!(
                        "  DM:         {}",
                        dm.clone().unwrap_or_else(|| {
                            "not recorded; any DM with the app from the owner is accepted".into()
                        })
                    );
                    println!(
                        "  control:    {}",
                        channel.clone().unwrap_or_else(|| "none (DM only)".into())
                    );
                    match &st.bot {
                        Some(bot) => println!(
                            "  bot:        {}{}",
                            bot.bot_user_id.as_deref().unwrap_or("?"),
                            bot.bot_id
                                .as_deref()
                                .map(|b| format!(", bot {b}"))
                                .unwrap_or_default()
                        ),
                        None => println!("  bot:        app not installed for this workspace"),
                    }
                    println!("  rejected:   {} input(s) audited", st.rejections);
                },
            );
            Ok(())
        }
        OwnerOp::Unbind { team, json } => {
            let (team_id, removed) = owner_setup::unbind_owner(store, creds, team.as_deref())?;
            let _ = success(
                *json,
                json!({"ok": true, "team_id": team_id, "removed": removed}),
                || {
                    if removed {
                        println!("Removed the Slack owner binding for {team_id}. Nobody has owner authority there until you bind again.");
                    } else {
                        println!("No Slack owner was bound for {team_id}; nothing to remove.");
                    }
                },
            );
            Ok(())
        }
        OwnerOp::Control { op } => match op {
            ControlOp::Set {
                channel,
                team,
                json,
            } => {
                let connector = connector()?;
                let conv = owner_setup::set_control_channel(
                    store,
                    creds,
                    &connector,
                    team.as_deref(),
                    channel.trim(),
                    now_ms(),
                )
                .await?;
                let team_id = creds.resolve_team(team.as_deref())?;
                let _ = success(
                    *json,
                    json!({"ok": true, "team_id": team_id, "control_channel": conv.conversation_id()}),
                    || {
                        println!(
                            "Control channel for {team_id} is now {} (private; only the owner is authorized there).",
                            conv.conversation_id()
                        );
                    },
                );
                Ok(())
            }
            ControlOp::Remove { team, json } => {
                let (team_id, removed) =
                    owner_setup::remove_control_channel(store, creds, team.as_deref())?;
                let _ = success(
                    *json,
                    json!({"ok": true, "team_id": team_id, "removed": removed}),
                    || {
                        if removed {
                            println!("Control channel cleared for {team_id}; the DM with the app still works.");
                        } else {
                            println!("No control channel was set for {team_id}.");
                        }
                    },
                );
                Ok(())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn stdin_tokens_are_classified_by_prefix_in_any_order() {
        let args = TokenArgs {
            stdin: true,
            ..TokenArgs::default()
        };
        let mut input = "\n# comment\nxoxb-test-000\n  xapp-test-000  \n".as_bytes();
        let t = resolve_tokens(&args, &mut input, &no_env).unwrap();
        assert_eq!(t.app.unwrap().expose_secret(), "xapp-test-000");
        assert_eq!(t.bot.unwrap().expose_secret(), "xoxb-test-000");
    }

    #[test]
    fn duplicate_or_foreign_stdin_tokens_are_rejected_without_echo() {
        let args = TokenArgs {
            stdin: true,
            ..TokenArgs::default()
        };
        let mut two_bots = "xoxb-test-000\nxoxb-test-001\n".as_bytes();
        let err = resolve_tokens(&args, &mut two_bots, &no_env).unwrap_err();
        assert!(!err.to_string().contains("xoxb-test"));
        let mut user = "xoxp-test-000\n".as_bytes();
        let err = resolve_tokens(&args, &mut user, &no_env).unwrap_err();
        assert!(matches!(err, SlackAppError::TokenInput(_)), "{err:?}");
        assert!(
            err.to_string().contains("stdin line 1 is a user token"),
            "{err}"
        );
        assert!(!format!("{err} {}", err.recovery()).contains("xoxp-test"));
    }

    #[test]
    fn env_is_the_fallback_and_explicit_sources_conflict() {
        let env = |k: &str| match k {
            APP_TOKEN_ENV => Some("xapp-test-000".to_string()),
            BOT_TOKEN_ENV => Some("xoxb-test-000".to_string()),
            _ => None,
        };
        let t = resolve_tokens(&TokenArgs::default(), &mut std::io::empty(), &env).unwrap();
        assert!(t.app.is_some() && t.bot.is_some());

        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("bot token");
        std::fs::write(&f, "xoxb-test-001\n").unwrap();
        let args = TokenArgs {
            stdin: true,
            bot_token_file: Some(f),
            ..TokenArgs::default()
        };
        let err = resolve_tokens(&args, &mut "xoxb-test-000\n".as_bytes(), &no_env).unwrap_err();
        assert!(matches!(err, SlackAppError::TokenInput(_)), "{err:?}");
    }

    #[test]
    fn nothing_supplied_resolves_to_nothing() {
        let t = resolve_tokens(&TokenArgs::default(), &mut std::io::empty(), &no_env).unwrap();
        assert!(t.app.is_none() && t.bot.is_none());
    }
}
