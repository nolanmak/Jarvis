//! nolanmak/tabagent#61 Part C: deliver TabAgent's remote-handoff link to the
//! owner in Discord.
//!
//! When the model calls `auth_handoff`, TabAgent opens a one-time phone page
//! for that one tab on the Legion's Tailscale address and POSTs
//! `{event: "handoff.start", origin, reason, url, expiresAt, session}` to its
//! `handoff.webhook`. The daemon listens for that POST on loopback only and
//! posts the link to the owner's private Discord channel.
//!
//! - **Where.** `127.0.0.1:<AUGMENTAGENT_TABAGENT_HANDOFF_PORT>` (default
//!   47625; `0` turns it off). The peer must be loopback and the Host header
//!   must name that loopback port.
//! - **Auth.** TabAgent's webhook sends no credential of its own, so the path
//!   carries one: `/tabagent/handoff/<secret>`, a 256-bit random value kept in
//!   `<state dir>/tabagent-handoff.secret` (mode 0600). `augmentagent
//!   tabagent-handoff-url` prints the full URL for `tabagent config
//!   handoff.webhook`.
//! - **Which channel.** `auth_handoff` in the `tabagent-tool` MCP child records
//!   its turn's channel in a short-lived route file just before it calls
//!   TabAgent. The link goes there when that channel is the owner's own
//!   (`DISCORD_CHANNEL_ID`, `DISCORD_QUERY_CHANNEL_ID`, or a DM with
//!   `DISCORD_ALLOWED_USER_ID`), and to `DISCORD_CHANNEL_ID` otherwise. It is
//!   never sent to Slack, a webhook or any other surface.
//! - **Secrecy.** The link takes over one tab until the handoff ends, so it is
//!   never logged, and the message suppresses link previews and mentions.
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub const PORT_ENV: &str = "AUGMENTAGENT_TABAGENT_HANDOFF_PORT";
pub const DEFAULT_PORT: u16 = 47625;
/// Set on the `tabagent-tool` MCP child: the turn's Discord channel.
pub const CHANNEL_ENV: &str = "AUGMENTAGENT_TABAGENT_CHANNEL";
/// Set on the `tabagent-tool` MCP child: where it records that channel.
pub const ROUTE_ENV: &str = "AUGMENTAGENT_TABAGENT_HANDOFF_ROUTE";
const SECRET_FILE: &str = "tabagent-handoff.secret";
const ROUTE_FILE: &str = "tabagent-handoff-route.json";
const PATH_PREFIX: &str = "/tabagent/handoff/";
/// A route older than this belongs to another handoff. It covers TabAgent's
/// 120 s site approval prompt before the handoff opens.
const ROUTE_FRESH_MS: i64 = 5 * 60 * 1000;
const BODY_LIMIT: usize = 16 * 1024;
/// TabAgent gives up on the webhook after 5 s.
const POST_DEADLINE: std::time::Duration = std::time::Duration::from_millis(4500);

pub fn secret_path() -> Option<PathBuf> {
    augmentagent_channel_core::state_dir::state_dir().map(|dir| dir.join(SECRET_FILE))
}

pub fn route_path() -> Option<PathBuf> {
    augmentagent_channel_core::state_dir::state_dir().map(|dir| dir.join(ROUTE_FILE))
}

/// The listener port, `None` when turned off or invalid.
pub fn port(lookup: &dyn Fn(&str) -> Option<String>) -> Option<u16> {
    match lookup(PORT_ENV).map(|value| value.trim().to_string()) {
        None => Some(DEFAULT_PORT),
        Some(value) if value.is_empty() => Some(DEFAULT_PORT),
        Some(value) => match value.parse::<u16>() {
            Ok(0) => None,
            Ok(port) => Some(port),
            Err(_) => {
                warn!("{PORT_ENV} is not a port; the TabAgent handoff webhook is off");
                None
            }
        },
    }
}

/// The secret path segment, created (0600) on first use.
pub fn load_or_create_secret(path: &Path) -> Result<String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if std::fs::symlink_metadata(path).is_err() {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .context("TabAgent handoff secret directory cannot be created")?;
        }
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(secret.as_bytes())
                    .context("TabAgent handoff secret cannot be written")?;
                file.sync_all().ok();
            }
            // Another process created it first; read theirs.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("TabAgent handoff secret cannot be created"),
        }
    }
    let secret = crate::newsletter::private_token_file(path, "TabAgent handoff")?;
    anyhow::ensure!(
        secret.len() >= 32 && secret.bytes().all(|b| b.is_ascii_alphanumeric()),
        "TabAgent handoff secret must be at least 32 letters or digits"
    );
    Ok(secret)
}

pub fn webhook_url(port: u16, secret: &str) -> String {
    format!("http://127.0.0.1:{port}{PATH_PREFIX}{secret}")
}

/// `augmentagent tabagent-handoff-url`: the value for `tabagent config
/// handoff.webhook`.
pub fn print_url() -> Result<()> {
    let lookup = |name: &str| std::env::var(name).ok();
    let port = port(&lookup)
        .with_context(|| format!("the handoff webhook is off ({PORT_ENV}=0 or invalid)"))?;
    let path = secret_path().context("no state directory (HOME is unset)")?;
    println!("{}", webhook_url(port, &load_or_create_secret(&path)?));
    Ok(())
}

// ---------------------------------------------------------------------------
// Route file: the turn's channel, written by the MCP child
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Record `channel` for the next handoff webhook (0600, replaced atomically).
pub fn write_route(path: &Path, channel: u64) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().context("route file has no directory")?;
    let temporary = dir.join(format!(".{ROUTE_FILE}.{}", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context("route file cannot be created")?;
    file.write_all(
        json!({"channel": channel, "atMs": now_ms()})
            .to_string()
            .as_bytes(),
    )?;
    drop(file);
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

/// The recorded channel when it is fresh. The file is consumed either way.
pub fn take_route(path: &Path, now_ms: i64) -> Option<u64> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let text = (metadata.is_file() && metadata.len() <= 1024)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten();
    let _ = std::fs::remove_file(path);
    let route: serde_json::Value = serde_json::from_str(&text?).ok()?;
    let at = route["atMs"].as_i64()?;
    (now_ms >= at && now_ms - at <= ROUTE_FRESH_MS).then_some(())?;
    route["channel"].as_u64().filter(|channel| *channel > 0)
}

// ---------------------------------------------------------------------------
// The webhook payload and the Discord message
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Start {
    pub event: String,
    pub origin: String,
    pub reason: String,
    pub url: Option<String>,
    pub expires_at: Option<String>,
}

pub fn parse_start(body: &[u8]) -> Result<Start, &'static str> {
    let start: Start = serde_json::from_slice(body).map_err(|_| "not a handoff start event")?;
    if !matches!(start.event.as_str(), "handoff.start" | "auth.handoff.start") {
        return Err("not a handoff start event");
    }
    if let Some(url) = &start.url {
        let parsed = reqwest::Url::parse(url).map_err(|_| "url is not a URL")?;
        if url.len() > 2048
            || !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err("url must be a plain http(s) link");
        }
    }
    Ok(start)
}

/// One line of at most `max` characters, with no characters that would
/// break out of inline code or quote formatting.
fn clean(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| *c != '`')
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

fn expires(at: Option<&str>) -> String {
    at.and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| {
            let local = at.with_timezone(&chrono_tz::America::Los_Angeles);
            format!(" (expires {} PT)", local.format("%-I:%M %p"))
        })
        .unwrap_or_default()
}

pub fn message(start: &Start) -> String {
    let origin = clean(&start.origin, 200);
    let reason = clean(&start.reason, 300);
    let mut text = format!("**TabAgent needs you** on `{origin}`\n> `{reason}`\n");
    match &start.url {
        Some(url) => text.push_str(&format!(
            "Open this one-time link on your phone to take over that tab, then tap Done{}:\n<{url}>\nIt works only for this tab and stops working when the handoff ends.",
            expires(start.expires_at.as_deref())
        )),
        None => text.push_str(
            "Finish it in Chrome on the Legion, then press Done in the TabAgent side panel. (Remote handoff is off; `tabagent config handoff.remote on` sends a phone link instead.)",
        ),
    }
    text
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Poster: Send + Sync {
    /// The channel is a DM between the bot and the owner.
    async fn owner_dm(&self, channel: u64) -> bool;
    async fn post(&self, channel: u64, text: &str) -> Result<()>;
}

/// The owner's channels, from the daemon environment.
#[derive(Debug, Clone)]
pub struct Target {
    pub default: u64,
    pub owner_channels: Vec<u64>,
}

impl Target {
    pub fn from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let parse = |name: &str| {
            lookup(name)
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v > 0)
        };
        let default = parse("DISCORD_CHANNEL_ID")?;
        let owner_channels = [Some(default), parse("DISCORD_QUERY_CHANNEL_ID")]
            .into_iter()
            .flatten()
            .collect();
        Some(Self {
            default,
            owner_channels,
        })
    }
}

pub async fn choose_channel(poster: &dyn Poster, target: &Target, route: Option<u64>) -> u64 {
    match route {
        Some(channel) if target.owner_channels.contains(&channel) => channel,
        Some(channel) if poster.owner_dm(channel).await => channel,
        _ => target.default,
    }
}

pub struct DiscordPoster {
    http: Arc<serenity::http::Http>,
    owner: Option<u64>,
}

impl DiscordPoster {
    pub fn from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let token = lookup("DISCORD_BOT_TOKEN").filter(|t| !t.trim().is_empty())?;
        let owner = lookup("DISCORD_ALLOWED_USER_ID").and_then(|v| v.trim().parse().ok());
        Some(Self {
            http: Arc::new(serenity::http::Http::new(token.trim())),
            owner,
        })
    }
}

#[async_trait]
impl Poster for DiscordPoster {
    async fn owner_dm(&self, channel: u64) -> bool {
        let Some(owner) = self.owner else {
            return false;
        };
        matches!(
            serenity::all::ChannelId::new(channel).to_channel(&*self.http).await,
            Ok(serenity::all::Channel::Private(dm)) if dm.recipient.id.get() == owner
        )
    }

    async fn post(&self, channel: u64, text: &str) -> Result<()> {
        use serenity::all::{CreateAllowedMentions, CreateMessage, MessageFlags};
        let message = CreateMessage::new()
            .content(text)
            .allowed_mentions(CreateAllowedMentions::new())
            .flags(MessageFlags::SUPPRESS_EMBEDS);
        serenity::all::ChannelId::new(channel)
            .send_message(&*self.http, message)
            .await
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("Discord refused the handoff message"))
    }
}

// ---------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------

pub struct Webhook {
    pub secret: String,
    pub port: u16,
    pub target: Target,
    pub route: Option<PathBuf>,
    pub poster: Arc<dyn Poster>,
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn receive(
    State(hook): State<Arc<Webhook>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    UrlPath(secret): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !peer.ip().is_loopback() {
        return StatusCode::FORBIDDEN;
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if host != format!("127.0.0.1:{}", hook.port) && host != format!("localhost:{}", hook.port) {
        return StatusCode::FORBIDDEN;
    }
    if !same(&secret, &hook.secret) {
        return StatusCode::NOT_FOUND;
    }
    let start = match parse_start(&body) {
        Ok(start) => start,
        Err(why) => {
            warn!(why, "TabAgent handoff webhook: payload refused");
            return StatusCode::BAD_REQUEST;
        }
    };
    let route = hook
        .route
        .as_deref()
        .and_then(|path| take_route(path, now_ms()));
    let channel = choose_channel(&*hook.poster, &hook.target, route).await;
    let origin = clean(&start.origin, 200);
    match tokio::time::timeout(POST_DEADLINE, hook.poster.post(channel, &message(&start))).await {
        Ok(Ok(())) => {
            // Never the link: it takes over the tab until the handoff ends.
            info!(%origin, channel, remote = start.url.is_some(), "TabAgent handoff: owner notified in Discord");
            StatusCode::NO_CONTENT
        }
        _ => {
            warn!(%origin, channel, "TabAgent handoff: Discord post failed; the owner was not notified");
            StatusCode::BAD_GATEWAY
        }
    }
}

pub fn router(hook: Arc<Webhook>) -> axum::Router {
    axum::Router::new()
        .route(
            &format!("{PATH_PREFIX}{{secret}}"),
            axum::routing::post(receive),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(hook)
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    hook: Arc<Webhook>,
    shutdown: CancellationToken,
) -> Result<()> {
    axum::serve(
        listener,
        router(hook).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.cancelled().await })
    .await
    .context("TabAgent handoff webhook listener")
}

/// Start the listener in `serve` when TabAgent and the Discord bot are both
/// configured. Never fails the daemon: a problem turns the webhook off.
pub async fn start(shutdown: CancellationToken) -> Option<tokio::task::JoinHandle<Result<()>>> {
    let lookup = |name: &str| std::env::var(name).ok();
    if matches!(crate::tabagent_tool::readiness_configured(&lookup), false) {
        return None;
    }
    let port = port(&lookup)?;
    let (Some(target), Some(poster)) =
        (Target::from_env(&lookup), DiscordPoster::from_env(&lookup))
    else {
        info!("TabAgent handoff webhook off: needs DISCORD_BOT_TOKEN and DISCORD_CHANNEL_ID");
        return None;
    };
    let secret = match secret_path().map(|path| load_or_create_secret(&path)) {
        Some(Ok(secret)) => secret,
        _ => {
            warn!("TabAgent handoff webhook off: its secret file is unusable");
            return None;
        }
    };
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            warn!(
                port,
                "TabAgent handoff webhook off: cannot listen on 127.0.0.1 ({error})"
            );
            return None;
        }
    };
    info!(port, "TabAgent handoff webhook listening on 127.0.0.1");
    let hook = Arc::new(Webhook {
        secret,
        port,
        target,
        route: route_path(),
        poster: Arc::new(poster),
    });
    Some(tokio::spawn(async move {
        if let Err(error) = serve(listener, hook, shutdown).await {
            warn!("TabAgent handoff webhook stopped: {error:#}");
        }
        Ok(())
    }))
}

#[cfg(test)]
#[path = "tabagent_handoff_tests.rs"]
mod tests;
