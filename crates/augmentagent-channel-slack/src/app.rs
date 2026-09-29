//! #1284 — the interactive Slack app: manifest, token handling and the
//! credential lifecycle (install, verify, rotate, remove).
//!
//! This is separate from the Composio connection in [`crate::auth`]:
//!
//! | Connection | Credential slot | Index |
//! | --- | --- | --- |
//! | Composio ingestion | `augmentagent/slack/<team_id>` | `slack_workspaces` table |
//! | Interactive app | `augmentagent/slack-app/<team_id>` | `augmentagent/slack-app/_installs` |
//!
//! Either can be installed, rotated or removed without touching the other.
//! The interactive slot holds the app-level token (`xapp-…`, Socket Mode),
//! the bot token (`xoxb-…`, Web API) and non-secret metadata (bot user,
//! granted scopes, timestamps). Nothing here writes to the database; the
//! index slot holds team ids only. See `docs/SLACK-APP.md`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_auth::{AuthError as CredentialError, CredentialStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::transport::socket::{ConnectError, SlackConnector};
use crate::transport::token::{redact, AppLevelToken, BotToken};
use crate::transport::web::{
    AuthTest, HttpSlackWebApi, SlackWebApi, WebApiConfig, WebApiError, DEFAULT_BASE_URL,
};

/// Credential-store platform for the interactive app. Distinct from
/// [`crate::auth::KEYCHAIN_PLATFORM`] (`slack`, Composio).
pub const APP_CREDENTIAL_PLATFORM: &str = "slack-app";

/// Account under [`APP_CREDENTIAL_PLATFORM`] listing installed team ids.
/// Slack team ids start with `T`/`E`, so this cannot collide.
pub const APP_INDEX_ACCOUNT: &str = "_installs";

/// Web API base override for tests and local QA. Must be `https://`, or
/// `http://` on a loopback host.
pub const SLACK_API_BASE_ENV: &str = "AUGMENTAGENT_SLACK_API_BASE";

/// The checked-in manifest (`docs/slack-app-manifest.json`).
pub const MANIFEST_JSON: &str = include_str!("../../../docs/slack-app-manifest.json");

/// The slash command the manifest declares.
pub const SLASH_COMMAND: &str = "/jarvis";

/// Bot scopes the code requires. The manifest must grant exactly these
/// (`tests/app_manifest.rs`). Reasons, from `docs/SLACK-TRANSPORT.md`:
pub const REQUIRED_BOT_SCOPES: &[&str] = &[
    // `app_mention` event.
    "app_mentions:read",
    // `message.channels` / `conversations.info` on public channels.
    "channels:history",
    "channels:read",
    // chat.postMessage / update / delete / postEphemeral.
    "chat:write",
    // The `/jarvis` slash command.
    "commands",
    // Attachments in (#1293) and generated files out (#1294).
    "files:read",
    "files:write",
    // `message.groups` / `conversations.info` on private channels.
    "groups:history",
    "groups:read",
    // `message.im` (owner DM) / `conversations.info` on DMs.
    "im:history",
    "im:read",
    // `message.mpim` / `conversations.info` on group DMs.
    "mpim:history",
    "mpim:read",
    // Progress reactions.
    "reactions:write",
    // users.info (owner identity, #1286).
    "users:read",
];

/// Bot events the manifest must subscribe to.
pub const REQUIRED_BOT_EVENTS: &[&str] = &[
    "app_mention",
    "message.channels",
    "message.groups",
    "message.im",
    "message.mpim",
];

// ---------------------------------------------------------------------------
// Token kinds
// ---------------------------------------------------------------------------

/// What a token string is, judged by its prefix only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// `xapp-` — app-level token (Socket Mode).
    AppLevel,
    /// `xoxb-` — bot user OAuth token.
    Bot,
    /// `xoxp-` — user OAuth token. Never accepted.
    User,
    /// `xoxe` — refresh / rotating token. Never accepted.
    Refresh,
    /// Any other `xox?-` Slack token (legacy, workspace, session).
    OtherSlack,
    /// Not a Slack token.
    Unknown,
}

impl TokenKind {
    pub fn of(raw: &str) -> Self {
        let t = raw.trim();
        if t.starts_with("xapp-") {
            Self::AppLevel
        } else if t.starts_with("xoxb-") {
            Self::Bot
        } else if t.starts_with("xoxp-") {
            Self::User
        } else if t.starts_with("xoxe") {
            Self::Refresh
        } else if t.len() > 5 && t.starts_with("xox") && t.as_bytes()[4] == b'-' {
            Self::OtherSlack
        } else {
            Self::Unknown
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AppLevel => "app-level token",
            Self::Bot => "bot token",
            Self::User => "user token",
            Self::Refresh => "refresh token",
            Self::OtherSlack => "different kind of Slack token",
            Self::Unknown => "value that is not a Slack token",
        }
    }
}

fn parse_token(raw: &str, expected: TokenKind) -> Result<String, SlackAppError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(SlackAppError::EmptyToken { which: expected });
    }
    let found = TokenKind::of(t);
    if found != expected {
        return Err(SlackAppError::WrongTokenType { expected, found });
    }
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(SlackAppError::MalformedToken { which: expected });
    }
    Ok(t.to_string())
}

/// Validate an app-level token by prefix (`xapp-`). Surrounding whitespace
/// is trimmed; anything else is rejected with an operator-facing message.
pub fn parse_app_token(raw: &str) -> Result<AppLevelToken, SlackAppError> {
    parse_token(raw, TokenKind::AppLevel).map(AppLevelToken::new)
}

/// Validate a bot token by prefix (`xoxb-`).
pub fn parse_bot_token(raw: &str) -> Result<BotToken, SlackAppError> {
    parse_token(raw, TokenKind::Bot).map(BotToken::new)
}

/// Validate the [`SLACK_API_BASE_ENV`] override. `None`/empty → production.
pub fn api_base_from(value: Option<&str>) -> Result<String, SlackAppError> {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(DEFAULT_BASE_URL.to_string());
    };
    let bad = || SlackAppError::InvalidApiBase {
        value: redact(v).chars().take(200).collect(),
    };
    let url = reqwest::Url::parse(v).map_err(|_| bad())?;
    let loopback = match url.host_str() {
        Some("localhost") => true,
        Some(h) => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
        None => false,
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(bad()),
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err(bad());
    }
    Ok(v.trim_end_matches('/').to_string())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every failure carries an operator-facing [`recovery`](Self::recovery)
/// hint. No variant ever holds token bytes.
#[derive(Debug, Error)]
pub enum SlackAppError {
    #[error("expected the {} but this is a {}", .expected.label(), .found.label())]
    WrongTokenType {
        expected: TokenKind,
        found: TokenKind,
    },
    #[error("the {} is empty", .which.label())]
    EmptyToken { which: TokenKind },
    #[error("the {} contains whitespace or control characters", .which.label())]
    MalformedToken { which: TokenKind },
    #[error("Slack rejected the {} ({slack_error})", .which.label())]
    InvalidToken {
        which: TokenKind,
        slack_error: String,
    },
    #[error("bot token for workspace {team_id} is missing required scopes: {}", .missing.join(", "))]
    MissingScopes {
        team_id: String,
        missing: Vec<String>,
    },
    #[error("token belongs to workspace {actual}, not {expected}")]
    WrongWorkspace { expected: String, actual: String },
    #[error("app-level token is for app {app_token_app}, bot token is for app {bot_token_app}")]
    AppMismatch {
        app_token_app: String,
        bot_token_app: String,
    },
    #[error("no interactive Slack app is installed for workspace {team_id}")]
    NotInstalled { team_id: String },
    #[error("no interactive Slack app is installed")]
    NothingInstalled,
    #[error("several workspaces have the app installed ({}); pick one with --team", .teams.join(", "))]
    AmbiguousTeam { teams: Vec<String> },
    #[error("nothing to rotate: supply a new app-level token, a new bot token, or both")]
    NothingToRotate,
    #[error("Slack API call failed: {0}")]
    SlackApi(String),
    #[error("credential store: {0}")]
    CredentialStore(String),
    #[error("stored credentials for {team_id} are unreadable: {reason}")]
    Corrupt { team_id: String, reason: String },
    #[error("invalid {SLACK_API_BASE_ENV} value `{value}`")]
    InvalidApiBase { value: String },
    #[error("{0}")]
    TokenInput(String),
}

impl SlackAppError {
    /// Stable machine-readable code for `--json` output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::WrongTokenType { .. } => "wrong_token_type",
            Self::EmptyToken { .. } => "empty_token",
            Self::MalformedToken { .. } => "malformed_token",
            Self::InvalidToken { .. } => "invalid_token",
            Self::MissingScopes { .. } => "missing_scopes",
            Self::WrongWorkspace { .. } => "wrong_workspace",
            Self::AppMismatch { .. } => "app_mismatch",
            Self::NotInstalled { .. } => "not_installed",
            Self::NothingInstalled => "nothing_installed",
            Self::AmbiguousTeam { .. } => "ambiguous_team",
            Self::NothingToRotate => "nothing_to_rotate",
            Self::SlackApi(_) => "slack_api",
            Self::CredentialStore(_) => "credential_store",
            Self::Corrupt { .. } => "corrupt_credentials",
            Self::InvalidApiBase { .. } => "invalid_api_base",
            Self::TokenInput(_) => "token_input",
        }
    }

    /// What the operator should do next.
    pub fn recovery(&self) -> String {
        const APP_TOKEN_HELP: &str = "Use the app-level token from api.slack.com/apps > your app > Basic Information > App-Level Tokens (create one with the connections:write scope; it begins with \"xapp\").";
        const BOT_TOKEN_HELP: &str = "Use the Bot User OAuth Token from your app > OAuth & Permissions (it begins with \"xoxb\"). User and refresh tokens are never accepted.";
        match self {
            Self::WrongTokenType { expected, .. }
            | Self::EmptyToken { which: expected }
            | Self::MalformedToken { which: expected } => match expected {
                TokenKind::AppLevel => APP_TOKEN_HELP.into(),
                _ => BOT_TOKEN_HELP.into(),
            },
            Self::InvalidToken {
                which: TokenKind::AppLevel,
                ..
            } => "The app-level token was revoked or is wrong, or Socket Mode is off. Enable Settings > Socket Mode, regenerate an app-level token with the connections:write scope under Basic Information > App-Level Tokens, then run `augmentagent slack app rotate --team <team_id> --stdin` (or `slack app install --stdin` for a first install).".into(),
            Self::InvalidToken { .. } => "The bot token was revoked or the app was uninstalled from the workspace. Reinstall the app from OAuth & Permissions, copy the new Bot User OAuth Token, then run `augmentagent slack app rotate --team <team_id> --stdin` (or `slack app install --stdin` for a first install).".into(),
            Self::MissingScopes { missing, .. } => format!(
                "Add {} under OAuth & Permissions > Bot Token Scopes (or recreate the app from `augmentagent slack app manifest`), reinstall the app to the workspace so Slack issues a token with the new scopes, then rerun with that bot token.",
                missing.join(", ")
            ),
            Self::WrongWorkspace { expected, actual } => format!(
                "Rotate {expected} with tokens issued for {expected}. To add {actual} as a separate workspace, run `augmentagent slack app install --stdin` with its tokens."
            ),
            Self::AppMismatch { .. } => "Both tokens must come from the same Slack app. Copy the app-level and bot tokens from the same app's settings pages.".into(),
            Self::NotInstalled { .. } | Self::NothingInstalled => "Install it first: create the app from `augmentagent slack app manifest`, install it to the workspace, then pipe both tokens to `augmentagent slack app install --stdin` (see docs/SLACK-APP.md).".into(),
            Self::AmbiguousTeam { .. } => "Rerun with --team <team_id>; `augmentagent slack app status` lists installed workspaces.".into(),
            Self::NothingToRotate => "Pass --stdin, --app-token-file/--bot-token-file, or set AUGMENTAGENT_SLACK_APP_TOKEN / AUGMENTAGENT_SLACK_BOT_TOKEN.".into(),
            Self::SlackApi(_) => "Check the network and https://slack-status.com, then retry. Nothing was changed.".into(),
            Self::CredentialStore(_) => "The Keychain/keyring could not be read or written. On macOS unlock the login keychain and run `augmentagent doctor --keychain-probe`; on Linux make sure the Secret Service (gnome-keyring or KWallet) is running for this user. Nothing was changed.".into(),
            Self::Corrupt { team_id, .. } => format!(
                "Run `augmentagent slack app remove --team {team_id}` and install again."
            ),
            Self::InvalidApiBase { .. } => format!(
                "Unset {SLACK_API_BASE_ENV}, or set it to an https:// URL (http:// is allowed only for localhost/127.0.0.1 test servers)."
            ),
            Self::TokenInput(_) => "Provide tokens on stdin (--stdin, one per line), with --app-token-file/--bot-token-file, or via AUGMENTAGENT_SLACK_APP_TOKEN / AUGMENTAGENT_SLACK_BOT_TOKEN. Tokens are never accepted as plain arguments.".into(),
        }
    }
}

fn store_err(e: CredentialError) -> SlackAppError {
    SlackAppError::CredentialStore(redact(&e.to_string()))
}

const INVALID_TOKEN_ERRORS: &[&str] = &[
    "invalid_auth",
    "not_authed",
    "account_inactive",
    "token_revoked",
    "token_expired",
    "invalid_token",
    "not_allowed_token_type",
];

fn map_web_error(which: TokenKind, e: WebApiError) -> SlackAppError {
    match e {
        WebApiError::Slack { error, .. } if INVALID_TOKEN_ERRORS.contains(&error.as_str()) => {
            SlackAppError::InvalidToken {
                which,
                slack_error: error,
            }
        }
        other => SlackAppError::SlackApi(redact(&other.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Stored credentials
// ---------------------------------------------------------------------------

/// Everything stored for one workspace. `Debug` redacts both tokens.
#[derive(Debug, Clone)]
pub struct SlackAppCredentials {
    pub team_id: String,
    pub team_name: Option<String>,
    pub team_url: Option<String>,
    pub bot_user_id: String,
    pub bot_user_name: Option<String>,
    pub bot_id: Option<String>,
    pub app_id: Option<String>,
    /// Granted bot scopes at the last live check; `None` = unknown.
    pub scopes: Option<Vec<String>>,
    /// Unix seconds.
    pub installed_at: u64,
    pub verified_at: Option<u64>,
    pub rotated_at: Option<u64>,
    pub app_token: AppLevelToken,
    pub bot_token: BotToken,
}

/// Token-free view for CLI output and status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlackAppSummary {
    pub team_id: String,
    pub team_name: Option<String>,
    pub team_url: Option<String>,
    pub bot_user_id: String,
    pub bot_user_name: Option<String>,
    pub bot_id: Option<String>,
    pub app_id: Option<String>,
    pub scopes: Option<Vec<String>>,
    /// Required scopes absent from `scopes`; `None` when scopes are unknown.
    pub missing_scopes: Option<Vec<String>>,
    pub installed_at: u64,
    pub verified_at: Option<u64>,
    pub rotated_at: Option<u64>,
}

/// Required scopes absent from `granted`, in [`REQUIRED_BOT_SCOPES`] order.
pub fn missing_scopes(granted: &[String]) -> Vec<String> {
    REQUIRED_BOT_SCOPES
        .iter()
        .filter(|s| !granted.iter().any(|g| g == *s))
        .map(|s| s.to_string())
        .collect()
}

impl SlackAppCredentials {
    pub fn summary(&self) -> SlackAppSummary {
        SlackAppSummary {
            team_id: self.team_id.clone(),
            team_name: self.team_name.clone(),
            team_url: self.team_url.clone(),
            bot_user_id: self.bot_user_id.clone(),
            bot_user_name: self.bot_user_name.clone(),
            bot_id: self.bot_id.clone(),
            app_id: self.app_id.clone(),
            scopes: self.scopes.clone(),
            missing_scopes: self.scopes.as_deref().map(missing_scopes),
            installed_at: self.installed_at,
            verified_at: self.verified_at,
            rotated_at: self.rotated_at,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredCredentials {
    version: u32,
    team_id: String,
    team_name: Option<String>,
    team_url: Option<String>,
    bot_user_id: String,
    bot_user_name: Option<String>,
    bot_id: Option<String>,
    app_id: Option<String>,
    scopes: Option<Vec<String>>,
    installed_at: u64,
    verified_at: Option<u64>,
    rotated_at: Option<u64>,
    app_token: String,
    bot_token: String,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredIndex {
    teams: Vec<String>,
}

/// Interactive-app credentials in a [`CredentialStore`], one slot per team.
#[derive(Clone)]
pub struct SlackAppStore {
    inner: Arc<dyn CredentialStore>,
}

impl SlackAppStore {
    pub fn new(inner: Arc<dyn CredentialStore>) -> Self {
        Self { inner }
    }

    /// The process default: Keychain/keyring, or the insecure file store
    /// when `AUGMENTAGENT_INSECURE_CREDENTIAL_DIR` is set.
    pub fn default_store() -> Self {
        Self::new(augmentagent_auth::default_store())
    }

    pub fn backend(&self) -> &'static str {
        self.inner.backend()
    }

    /// `Ok(None)` when nothing is stored for `team_id`.
    pub fn load(&self, team_id: &str) -> Result<Option<SlackAppCredentials>, SlackAppError> {
        let bytes = match self.inner.get(APP_CREDENTIAL_PLATFORM, team_id) {
            Ok(b) => b,
            Err(CredentialError::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(store_err(e)),
        };
        let corrupt = |reason: String| SlackAppError::Corrupt {
            team_id: team_id.to_string(),
            reason,
        };
        // serde_json errors quote at most a short excerpt; still redact.
        let s: StoredCredentials =
            serde_json::from_slice(&bytes).map_err(|e| corrupt(redact(&e.to_string())))?;
        let app_token = parse_app_token(&s.app_token)
            .map_err(|_| corrupt("stored app-level token is invalid".into()))?;
        let bot_token = parse_bot_token(&s.bot_token)
            .map_err(|_| corrupt("stored bot token is invalid".into()))?;
        if s.team_id != team_id {
            return Err(corrupt(format!("slot holds team {}", s.team_id)));
        }
        Ok(Some(SlackAppCredentials {
            team_id: s.team_id,
            team_name: s.team_name,
            team_url: s.team_url,
            bot_user_id: s.bot_user_id,
            bot_user_name: s.bot_user_name,
            bot_id: s.bot_id,
            app_id: s.app_id,
            scopes: s.scopes,
            installed_at: s.installed_at,
            verified_at: s.verified_at,
            rotated_at: s.rotated_at,
            app_token,
            bot_token,
        }))
    }

    /// Write the slot, then add the team to the index.
    pub fn save(&self, c: &SlackAppCredentials) -> Result<(), SlackAppError> {
        let stored = StoredCredentials {
            version: 1,
            team_id: c.team_id.clone(),
            team_name: c.team_name.clone(),
            team_url: c.team_url.clone(),
            bot_user_id: c.bot_user_id.clone(),
            bot_user_name: c.bot_user_name.clone(),
            bot_id: c.bot_id.clone(),
            app_id: c.app_id.clone(),
            scopes: c.scopes.clone(),
            installed_at: c.installed_at,
            verified_at: c.verified_at,
            rotated_at: c.rotated_at,
            app_token: c.app_token.expose_secret().to_string(),
            bot_token: c.bot_token.expose_secret().to_string(),
        };
        let bytes = serde_json::to_vec(&stored)
            .map_err(|e| SlackAppError::CredentialStore(e.to_string()))?;
        self.inner
            .put(APP_CREDENTIAL_PLATFORM, &c.team_id, &bytes)
            .map_err(store_err)?;
        let mut teams = self.teams()?;
        if !teams.contains(&c.team_id) {
            teams.push(c.team_id.clone());
            self.write_index(teams)?;
        }
        Ok(())
    }

    /// Delete the slot and the index entry. Returns whether anything existed.
    /// Never touches the Composio slot (`augmentagent/slack/<team_id>`).
    pub fn remove(&self, team_id: &str) -> Result<bool, SlackAppError> {
        let had_slot = self.inner.exists(APP_CREDENTIAL_PLATFORM, team_id);
        self.inner
            .delete(APP_CREDENTIAL_PLATFORM, team_id)
            .map_err(store_err)?;
        let mut teams = self.teams()?;
        let before = teams.len();
        teams.retain(|t| t != team_id);
        let had_index = teams.len() != before;
        if had_index {
            self.write_index(teams)?;
        }
        Ok(had_slot || had_index)
    }

    /// Installed team ids, sorted.
    pub fn teams(&self) -> Result<Vec<String>, SlackAppError> {
        match self.inner.get(APP_CREDENTIAL_PLATFORM, APP_INDEX_ACCOUNT) {
            Ok(bytes) => serde_json::from_slice::<StoredIndex>(&bytes)
                .map(|i| i.teams)
                .map_err(|e| SlackAppError::Corrupt {
                    team_id: APP_INDEX_ACCOUNT.into(),
                    reason: e.to_string(),
                }),
            Err(CredentialError::NotFound { .. }) => Ok(Vec::new()),
            Err(e) => Err(store_err(e)),
        }
    }

    fn write_index(&self, mut teams: Vec<String>) -> Result<(), SlackAppError> {
        teams.sort();
        teams.dedup();
        if teams.is_empty() {
            return self
                .inner
                .delete(APP_CREDENTIAL_PLATFORM, APP_INDEX_ACCOUNT)
                .map_err(store_err);
        }
        let bytes = serde_json::to_vec(&StoredIndex { teams })
            .map_err(|e| SlackAppError::CredentialStore(e.to_string()))?;
        self.inner
            .put(APP_CREDENTIAL_PLATFORM, APP_INDEX_ACCOUNT, &bytes)
            .map_err(store_err)
    }

    /// `Some(team)` as given; otherwise the sole installed team.
    pub fn resolve_team(&self, team: Option<&str>) -> Result<String, SlackAppError> {
        if let Some(t) = team {
            return Ok(t.to_string());
        }
        let teams = self.teams()?;
        match teams.as_slice() {
            [] => Err(SlackAppError::NothingInstalled),
            [one] => Ok(one.clone()),
            _ => Err(SlackAppError::AmbiguousTeam { teams }),
        }
    }
}

// ---------------------------------------------------------------------------
// Talking to Slack
// ---------------------------------------------------------------------------

/// Result of probing an app-level token with `apps.connections.open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppTokenCheck {
    /// `app_id` from the returned socket URL, when present.
    pub app_id: Option<String>,
}

/// Builds Web API clients and probes app-level tokens. Fakeable.
#[async_trait]
pub trait SlackAppConnector: Send + Sync {
    fn web_api(&self, bot: &BotToken) -> Result<Arc<dyn SlackWebApi>, SlackAppError>;
    async fn probe_app_token(&self, app: &AppLevelToken) -> Result<AppTokenCheck, SlackAppError>;
}

/// Production connector over HTTP.
#[derive(Debug, Clone)]
pub struct HttpSlackAppConnector {
    api_base: String,
    timeout: Duration,
}

impl HttpSlackAppConnector {
    pub fn new(api_base: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
            timeout: Duration::from_secs(15),
        }
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }
}

#[async_trait]
impl SlackAppConnector for HttpSlackAppConnector {
    fn web_api(&self, bot: &BotToken) -> Result<Arc<dyn SlackWebApi>, SlackAppError> {
        let config = WebApiConfig {
            base_url: self.api_base.clone(),
            request_timeout: self.timeout,
            ..WebApiConfig::default()
        };
        let api = HttpSlackWebApi::new(bot.clone(), config)
            .map_err(|e| SlackAppError::SlackApi(redact(&e.to_string())))?;
        Ok(Arc::new(api))
    }

    /// `apps.connections.open` proves the token works and Socket Mode is on.
    /// The returned one-time URL is a secret: only its `app_id` is kept.
    async fn probe_app_token(&self, app: &AppLevelToken) -> Result<AppTokenCheck, SlackAppError> {
        let connector = SlackConnector::new(app.clone(), self.api_base.clone())
            .map_err(|e| SlackAppError::SlackApi(redact(&e.to_string())))?
            .connect_timeout(self.timeout);
        match connector.open_connection_url().await {
            Ok(url) => {
                let app_id = reqwest::Url::parse(&url).ok().and_then(|u| {
                    u.query_pairs()
                        .find(|(k, _)| k == "app_id")
                        .map(|(_, v)| v.into_owned())
                });
                Ok(AppTokenCheck { app_id })
            }
            Err(ConnectError::Fatal(msg)) => Err(SlackAppError::InvalidToken {
                which: TokenKind::AppLevel,
                slack_error: msg
                    .strip_prefix("apps.connections.open: ")
                    .unwrap_or(&msg)
                    .to_string(),
            }),
            Err(ConnectError::Transient(msg)) => Err(SlackAppError::SlackApi(redact(&msg))),
        }
    }
}

/// Live identity of a token pair.
#[derive(Debug, Clone)]
pub struct Verification {
    pub who: AuthTest,
    pub app: AppTokenCheck,
    /// `None` when Slack did not report granted scopes.
    pub missing_scopes: Option<Vec<String>>,
}

/// Live-check both tokens: `auth.test` with the bot token and
/// `apps.connections.open` with the app-level token.
pub async fn verify_tokens(
    connector: &dyn SlackAppConnector,
    app_token: &AppLevelToken,
    bot_token: &BotToken,
) -> Result<Verification, SlackAppError> {
    let web = connector.web_api(bot_token)?;
    let who = web
        .auth_test()
        .await
        .map_err(|e| map_web_error(TokenKind::Bot, e))?;
    let app = connector.probe_app_token(app_token).await?;
    if let (Some(a), Some(b)) = (&app.app_id, &who.app_id) {
        if a != b {
            return Err(SlackAppError::AppMismatch {
                app_token_app: a.clone(),
                bot_token_app: b.clone(),
            });
        }
    }
    let missing_scopes = who.scopes.as_deref().map(missing_scopes);
    Ok(Verification {
        who,
        app,
        missing_scopes,
    })
}

fn require_scopes(v: &Verification) -> Result<(), SlackAppError> {
    match &v.missing_scopes {
        Some(m) if !m.is_empty() => Err(SlackAppError::MissingScopes {
            team_id: v.who.team_id.clone(),
            missing: m.clone(),
        }),
        _ => Ok(()),
    }
}

fn apply(c: &mut SlackAppCredentials, v: &Verification, now: u64) {
    c.team_name = v.who.team.clone().or(c.team_name.take());
    c.team_url = v.who.url.clone().or(c.team_url.take());
    c.bot_user_id = v.who.user_id.clone();
    c.bot_user_name = v.who.user.clone().or(c.bot_user_name.take());
    c.bot_id = v.who.bot_id.clone().or(c.bot_id.take());
    c.app_id = v
        .app
        .app_id
        .clone()
        .or(v.who.app_id.clone())
        .or(c.app_id.take());
    c.scopes = v.who.scopes.clone();
    c.verified_at = Some(now);
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub summary: SlackAppSummary,
    /// An earlier install for the same workspace was overwritten.
    pub replaced: bool,
}

/// Verify both tokens live and store them for the workspace they belong to.
/// Nothing is stored unless every check passes (scopes unknown is allowed
/// and reported). Reinstalling over an existing workspace keeps its
/// original `installed_at`; a corrupt earlier slot is overwritten.
pub async fn install(
    store: &SlackAppStore,
    connector: &dyn SlackAppConnector,
    app_token: AppLevelToken,
    bot_token: BotToken,
    now: u64,
) -> Result<InstallOutcome, SlackAppError> {
    let v = verify_tokens(connector, &app_token, &bot_token).await?;
    require_scopes(&v)?;
    let team_id = v.who.team_id.clone();
    let previous = match store.load(&team_id) {
        Ok(p) => p.map(|p| p.installed_at),
        Err(SlackAppError::Corrupt { .. }) => Some(now),
        Err(e) => return Err(e),
    };
    let mut creds = SlackAppCredentials {
        team_id,
        team_name: None,
        team_url: None,
        bot_user_id: String::new(),
        bot_user_name: None,
        bot_id: None,
        app_id: None,
        scopes: None,
        installed_at: previous.unwrap_or(now),
        verified_at: None,
        rotated_at: None,
        app_token,
        bot_token,
    };
    apply(&mut creds, &v, now);
    store.save(&creds)?;
    Ok(InstallOutcome {
        summary: creds.summary(),
        replaced: previous.is_some(),
    })
}

fn load_installed(
    store: &SlackAppStore,
    team_id: &str,
) -> Result<SlackAppCredentials, SlackAppError> {
    store
        .load(team_id)?
        .ok_or_else(|| SlackAppError::NotInstalled {
            team_id: team_id.to_string(),
        })
}

fn same_team(expected: &str, v: &Verification) -> Result<(), SlackAppError> {
    if v.who.team_id != expected {
        return Err(SlackAppError::WrongWorkspace {
            expected: expected.to_string(),
            actual: v.who.team_id.clone(),
        });
    }
    Ok(())
}

/// Live-check the stored tokens. On success refreshes the stored identity,
/// scopes and `verified_at`; on failure changes nothing.
pub async fn verify(
    store: &SlackAppStore,
    connector: &dyn SlackAppConnector,
    team_id: &str,
    now: u64,
) -> Result<SlackAppSummary, SlackAppError> {
    let mut creds = load_installed(store, team_id)?;
    let v = verify_tokens(connector, &creds.app_token, &creds.bot_token).await?;
    same_team(team_id, &v)?;
    require_scopes(&v)?;
    apply(&mut creds, &v, now);
    store.save(&creds)?;
    Ok(creds.summary())
}

/// Replace one or both tokens for an installed workspace. The new pair is
/// verified live and must belong to the same workspace; on any failure the
/// old credentials stay in place.
pub async fn rotate(
    store: &SlackAppStore,
    connector: &dyn SlackAppConnector,
    team_id: &str,
    new_app_token: Option<AppLevelToken>,
    new_bot_token: Option<BotToken>,
    now: u64,
) -> Result<SlackAppSummary, SlackAppError> {
    if new_app_token.is_none() && new_bot_token.is_none() {
        return Err(SlackAppError::NothingToRotate);
    }
    let mut creds = load_installed(store, team_id)?;
    let app_token = new_app_token.unwrap_or_else(|| creds.app_token.clone());
    let bot_token = new_bot_token.unwrap_or_else(|| creds.bot_token.clone());
    let v = verify_tokens(connector, &app_token, &bot_token).await?;
    same_team(team_id, &v)?;
    require_scopes(&v)?;
    creds.app_token = app_token;
    creds.bot_token = bot_token;
    creds.rotated_at = Some(now);
    apply(&mut creds, &v, now);
    store.save(&creds)?;
    Ok(creds.summary())
}
