//! #1287 — the interactive Slack surface in `serve`.
//!
//! `serve` runs the surface when a Slack app is installed
//! (`augmentagent slack app install`) and an owner is bound
//! (`augmentagent slack app owner bind`) for the same workspace.
//! `AUGMENTAGENT_SLACK_INTERACTIVE` overrides that: `0`/`false`/`off` turns
//! it off, `1`/`true`/`on` makes missing configuration an error that
//! `status` reports, unset (or `auto`) is the default above. It is
//! independent of the Composio poll channel and the Slack digest, which
//! keep their own cadence. Configuration is read once at startup; restart
//! the daemon after installing, rotating or binding. Owner bindings
//! themselves are re-read on every event, so an unbind takes effect at once.
//!
//! #1288 — the turn handler is the shared conversation harness
//! ([`SlackConversationHarness`]) over the same [`QueryHandler`] Discord
//! answers with (`WikiQuerier`): same tools, wiki and memory, skills, audit
//! and provider fallback, and a native Claude/Codex session per Slack
//! conversation (DM, DM thread, channel thread) that follow-ups resume.
//! [`build_surface`] turns on the throttled status line (with its `cancel`
//! hint) and the owner-file pipeline under `<state dir>/slack-inbound`.
//!
//! #1289 — [`build_approvals`] turns the same plan into the Slack approval
//! surface: cards go to the owner's DM (or the bound control channel, see
//! `approval_routing`), and the surface routes card clicks, modals and
//! approval text commands to it. `serve` plans once ([`plan_from_env`]) so
//! the credential store is read once for both.
//!
//! #1292 — owner commands (`/jarvis <command>` and their plain-text forms):
//! [`slack_commands`] over the daemon's model selection file, model
//! readiness, loop parser and journal bridge, handed to [`spawn`]. The
//! harness reads the same selection file per conversation
//! ([`conversation_handler`]).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_slack::app::{
    api_base_from, SlackAppCredentials, SlackAppError, SlackAppStore, SLACK_API_BASE_ENV,
};
use augmentagent_channel_slack::commands::{slack_selection, SlackCommandDeps, SlackCommands};
use augmentagent_channel_slack::delivery::ProgressConfig;
use augmentagent_channel_slack::catch_up::SubscribedCatchUp;
use augmentagent_channel_slack::harness::SlackConversationHarness;
use augmentagent_channel_slack::history::{SlackConversationHistory, TurnHistory};
use augmentagent_channel_slack::ingest::SubscribedEventSink;
use augmentagent_channel_slack::interactive::{
    report_inactive, SlackInteractiveSurface, SlackSurfaceConfig, SlackTurn, SlackTurnHandler,
    SlackTurnReply, SlackWorkspaceRuntime, SurfaceState,
};
use augmentagent_channel_slack::owner::OwnerInputSource;
use augmentagent_channel_slack::owner_setup::{bot_identity, find_binding};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::socket::{SlackConnector, SocketConnector};
use augmentagent_channel_slack::transport::web::{
    test_file_hosts_from, DownloadLimits, HttpSlackWebApi, SlackWebApi, WebApiConfig,
    SLACK_TEST_FILE_HOSTS_ENV,
};
use augmentagent_store::Store;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Operator switch for the interactive surface.
pub const ENABLE_ENV: &str = "AUGMENTAGENT_SLACK_INTERACTIVE";

const RESTART: &str = "then restart the daemon (`augmentagent service --unit daemon restart`)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    /// Run when an install and an owner exist; otherwise report
    /// `not_configured`.
    Auto,
    /// Run; missing configuration is `misconfigured`.
    On,
    Off,
}

impl Switch {
    /// `Err` carries the rejected value.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::Auto);
        };
        match v.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "1" | "true" | "yes" | "on" => Ok(Self::On),
            "0" | "false" | "no" | "off" => Ok(Self::Off),
            _ => Err(v.to_string()),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        Self::parse(std::env::var(ENABLE_ENV).ok().as_deref())
    }
}

/// What `serve` should do with the interactive surface.
pub enum Plan {
    /// Do not start a listener; report this instead.
    Inactive {
        state: SurfaceState,
        detail: String,
        recovery: Option<String>,
        workspaces: Vec<String>,
    },
    /// Start one surface for these installs (each has a bound owner).
    Ready {
        installs: Vec<ReadyInstall>,
        api_base: String,
    },
}

/// An installed workspace with a bound owner.
pub struct ReadyInstall {
    pub creds: SlackAppCredentials,
    /// From the owner binding, so an Enterprise Grid workspace keeps its
    /// enterprise ID: the account events are recorded and answered under.
    pub workspace: SlackWorkspace,
}

fn inactive(
    state: SurfaceState,
    detail: impl Into<String>,
    recovery: Option<String>,
    workspaces: Vec<String>,
) -> Plan {
    Plan::Inactive {
        state,
        detail: detail.into(),
        recovery,
        workspaces,
    }
}

fn app_error(e: &SlackAppError, workspaces: Vec<String>) -> Plan {
    inactive(
        SurfaceState::Misconfigured,
        e.to_string(),
        Some(format!(
            "{} Then restart the daemon (`augmentagent service --unit daemon restart`).",
            e.recovery()
        )),
        workspaces,
    )
}

/// Decide from the switch, the installed apps and the owner bindings. Reads
/// the credential store only when the switch is not `Off`.
pub fn plan(
    switch: Result<Switch, String>,
    apps: &SlackAppStore,
    store: &Store,
    api_base: Option<&str>,
) -> Plan {
    let switch = match switch {
        Ok(s) => s,
        Err(value) => {
            return inactive(
                SurfaceState::Misconfigured,
                format!("{ENABLE_ENV}=`{value}` is not a recognised value"),
                Some(format!(
                    "Set {ENABLE_ENV} to 1 (on) or 0 (off), or unset it to run whenever an app \
                     is installed and an owner is bound; {RESTART}."
                )),
                Vec::new(),
            )
        }
    };
    if switch == Switch::Off {
        return inactive(
            SurfaceState::Disabled,
            format!("turned off by {ENABLE_ENV}"),
            Some(format!(
                "Unset {ENABLE_ENV} (or set it to 1) to turn the interactive Slack surface on; \
                 {RESTART}."
            )),
            Vec::new(),
        );
    }
    // Missing configuration is quiet in auto mode and an error when forced on.
    let missing = if switch == Switch::On {
        SurfaceState::Misconfigured
    } else {
        SurfaceState::NotConfigured
    };
    let teams = match apps.teams() {
        Ok(t) => t,
        Err(e) => return app_error(&e, Vec::new()),
    };
    if teams.is_empty() {
        return inactive(
            missing,
            "no interactive Slack app is installed",
            Some(format!(
                "{} Then bind yourself as owner with `augmentagent slack app owner bind --user \
                 <member_id>`, {RESTART}.",
                SlackAppError::NothingInstalled.recovery()
            )),
            Vec::new(),
        );
    }
    let mut installs = Vec::new();
    let mut unbound = Vec::new();
    for team in &teams {
        let workspace = match find_binding(store, team) {
            Ok(Some(binding)) => match SlackWorkspace::from_account(binding.owner.account()) {
                Ok(w) => w,
                Err(e) => {
                    return app_error(
                        &SlackAppError::Store(format!("stored Slack owner binding: {e}")),
                        teams.clone(),
                    )
                }
            },
            Ok(None) => {
                unbound.push(team.clone());
                continue;
            }
            Err(e) => return app_error(&e, teams.clone()),
        };
        match apps.load(team) {
            Ok(Some(creds)) => installs.push(ReadyInstall { creds, workspace }),
            Ok(None) => {
                warn!(team = %team, "slack interactive: install index lists a team with no credentials")
            }
            Err(e) => return app_error(&e, teams.clone()),
        }
    }
    if installs.is_empty() {
        return inactive(
            missing,
            format!(
                "the Slack app is installed for {} but no owner is bound",
                unbound.join(", ")
            ),
            Some(format!(
                "Bind yourself as owner: `augmentagent slack app owner bind --user <member_id>` \
                 (add --team <team_id> when several workspaces are installed), {RESTART}."
            )),
            teams,
        );
    }
    let api_base = match api_base_from(api_base) {
        Ok(b) => b,
        Err(e) => return app_error(&e, teams),
    };
    if !unbound.is_empty() {
        info!(teams = %unbound.join(", "), "slack interactive: installed workspaces without an owner are not served");
    }
    Plan::Ready { installs, api_base }
}

/// #1296 — one bot-token Web API client per ready install, for the
/// subscribed-conversation catch-up and the harness history.
pub fn web_runtimes(installs: &[ReadyInstall], api_base: &str) -> Result<Vec<SlackWorkspaceRuntime>> {
    installs
        .iter()
        .map(|i| {
            let web = HttpSlackWebApi::new(
                i.creds.bot_token.clone(),
                WebApiConfig {
                    base_url: api_base.to_string(),
                    ..WebApiConfig::default()
                },
            )?;
            Ok(SlackWorkspaceRuntime {
                workspace: i.workspace.clone(),
                web: Arc::new(web) as Arc<dyn SlackWebApi>,
                bot: bot_identity(&i.creds),
            })
        })
        .collect()
}

/// Build the surface for a ready plan: one Web API client per workspace and
/// one Socket Mode listener per distinct app.
pub fn build_surface(
    store: Arc<Store>,
    installs: &[ReadyInstall],
    api_base: &str,
    handler: Arc<dyn SlackTurnHandler>,
    dry_run: bool,
) -> Result<SlackInteractiveSurface> {
    // `api_base_from` allows plain http only for a loopback test server; the
    // socket URL such a server hands back is plain ws for the same reason.
    let loopback_test_server = api_base.starts_with("http://");
    // Test-only loopback file hosts for owner-file downloads (same rule as
    // `slack files fetch`); anything but loopback host:port is refused.
    let test_hosts = test_file_hosts_from(std::env::var(SLACK_TEST_FILE_HOSTS_ENV).ok().as_deref())
        .map_err(|m| anyhow::anyhow!(m))?;
    if !test_hosts.is_empty() {
        warn!(hosts = ?test_hosts, "{SLACK_TEST_FILE_HOSTS_ENV} adds loopback test file hosts");
    }
    let mut workspaces = Vec::new();
    let mut connectors: Vec<Arc<dyn SocketConnector>> = Vec::new();
    let mut apps_seen: Vec<String> = Vec::new();
    for ReadyInstall {
        creds: c,
        workspace,
    } in installs
    {
        let mut limits = DownloadLimits::default();
        limits.allowed_hosts.extend(test_hosts.iter().cloned());
        let web = HttpSlackWebApi::new(
            c.bot_token.clone(),
            WebApiConfig {
                base_url: api_base.to_string(),
                ..WebApiConfig::default()
            },
        )?
        .with_download_limits(limits);
        workspaces.push(SlackWorkspaceRuntime {
            workspace: workspace.clone(),
            web: Arc::new(web) as Arc<dyn SlackWebApi>,
            bot: bot_identity(c),
        });
        // One socket per app: an app installed in two workspaces delivers
        // both workspaces' events on the same link.
        let app_key = c
            .app_id
            .clone()
            .unwrap_or_else(|| format!("team:{}", c.team_id));
        if apps_seen.contains(&app_key) {
            continue;
        }
        apps_seen.push(app_key);
        let connector = SlackConnector::new(c.app_token.clone(), api_base.to_string())
            .map_err(|e| anyhow::anyhow!("socket mode connector: {e}"))?
            .allow_insecure_ws(loopback_test_server);
        connectors.push(Arc::new(connector));
    }
    Ok(SlackInteractiveSurface::new(
        store,
        workspaces,
        connectors,
        handler,
        SlackSurfaceConfig {
            dry_run,
            // #1288 — a status line per turn that says how to cancel it
            // (never posted in dry-run).
            progress: Some(ProgressConfig::default()),
            ..SlackSurfaceConfig::default()
        },
    ))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Read the switch, the installed apps and the owner bindings (off the
/// runtime: the credential store may block on the Keychain).
pub async fn plan_from_env(store: Arc<Store>) -> Result<Plan> {
    Ok(tokio::task::spawn_blocking(move || {
        let api_base = std::env::var(SLACK_API_BASE_ENV).ok();
        plan(
            Switch::from_env(),
            &SlackAppStore::default_store(),
            &store,
            api_base.as_deref(),
        )
    })
    .await?)
}

/// #1289 — the Slack approval surface for a ready plan: the first bound
/// install (sorted by team), posting to the owner's DM or the bound control
/// channel. `Ok(None)` when the plan is not ready. A DM that was never
/// recorded is opened with `conversations.open` (and recorded by the owner
/// binding the next time the owner writes).
pub async fn build_approvals(
    store: Arc<Store>,
    plan: &Plan,
    channel: crate::approval_routing::SlackChannel,
    surfaces: augmentagent_approval_discord::CardSurfaces,
    wiki_root: Option<PathBuf>,
) -> Result<Option<Arc<augmentagent_channel_slack::approvals::SlackApprovals>>> {
    use augmentagent_channel_slack::approvals::{SlackApprovalConfig, SlackApprovals};
    let Plan::Ready { installs, api_base } = plan else {
        return Ok(None);
    };
    let Some(install) = installs
        .iter()
        .min_by(|a, b| a.creds.team_id.cmp(&b.creds.team_id))
    else {
        return Ok(None);
    };
    let web: Arc<dyn SlackWebApi> = Arc::new(HttpSlackWebApi::new(
        install.creds.bot_token.clone(),
        WebApiConfig {
            base_url: api_base.clone(),
            ..WebApiConfig::default()
        },
    )?);
    let binding = find_binding(&store, &install.creds.team_id)?
        .ok_or_else(|| anyhow::anyhow!("no owner is bound for {}", install.creds.team_id))?;
    let destination = match channel {
        crate::approval_routing::SlackChannel::Control => binding
            .control_channel()
            .map(|c| c.conversation_id().to_string())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is `control` but no control channel is bound; run `augmentagent slack \
                     app owner control set --channel <id>` or use `dm`",
                    crate::approval_routing::SLACK_CHANNEL_ENV
                )
            })?,
        crate::approval_routing::SlackChannel::Dm => match binding.direct_conversation() {
            Some(dm) => dm.conversation_id().to_string(),
            None => web
                .open_direct_conversation(binding.owner.sender_id())
                .await
                .map_err(|e| anyhow::anyhow!("could not open the owner's DM for approvals: {e}"))?,
        },
    };
    info!(team = %install.creds.team_id, channel = %destination, "slack approvals: cards go here");
    Ok(Some(Arc::new(
        SlackApprovals::new(
            store,
            web,
            SlackApprovalConfig {
                workspace: install.workspace.clone(),
                channel: destination,
            },
            surfaces,
        )
        // #1290 — `compose <person>: …` resolves people through the wiki.
        .with_wiki_root(wiki_root),
    )))
}

/// Plan, build and run the interactive surface as one supervised `serve`
/// task. Whatever happens, the task ends `Ok` (see [`supervise`]); an
/// inactive or failed surface is reported through `status`.
///
/// `make_handler` runs only when the surface will start, so an unconfigured
/// box never builds a reasoner for it.
///
/// `planned` is the plan `serve` already made (to build the approval
/// surface); `None` plans here. `approvals`, when set, receives card
/// clicks, modal submissions and approval text commands.
///
/// #1296 — `make_handler` gets the Slack conversation-history provider (for
/// history-in-prompt providers). `subscribed`, when set, receives every
/// event that is not an owner turn (live ingestion of subscribed
/// conversations), and the bounded catch-up of subscribed conversations runs
/// alongside the surface.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    store: Arc<Store>,
    planned: Option<Plan>,
    make_handler: impl FnOnce(Option<Arc<dyn TurnHistory>>) -> Arc<dyn SlackTurnHandler>
        + Send
        + 'static,
    approvals: Option<Arc<augmentagent_channel_slack::approvals::SlackApprovals>>,
    commands: Option<Arc<SlackCommands>>,
    subscribed: Option<Arc<dyn SubscribedEventSink>>,
    dry_run: bool,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<Result<()>> {
    supervise("slack interactive surface", async move {
        let decided = match planned {
            Some(p) => p,
            None => plan_from_env(Arc::clone(&store)).await?,
        };
        let (installs, api_base) = match decided {
            Plan::Inactive {
                state,
                detail,
                recovery,
                workspaces,
            } => {
                match state {
                    SurfaceState::Misconfigured => error!(
                        detail = %detail,
                        recovery = recovery.as_deref().unwrap_or(""),
                        "slack interactive surface misconfigured"
                    ),
                    _ => {
                        info!(state = state.as_str(), detail = %detail, "slack interactive surface not started")
                    }
                }
                report_inactive(
                    &store,
                    state,
                    &detail,
                    recovery.as_deref(),
                    &workspaces,
                    dry_run,
                    now_ms(),
                )?;
                return Ok(());
            }
            Plan::Ready { installs, api_base } => (installs, api_base),
        };
        let teams: Vec<String> = installs.iter().map(|i| i.creds.team_id.clone()).collect();
        info!(teams = %teams.join(", "), dry_run, "slack interactive surface starting");
        let runtimes = web_runtimes(&installs, &api_base).unwrap_or_else(|e| {
            warn!("slack history and catch-up clients unavailable: {e:#}");
            Vec::new()
        });
        let handler = match test_turn_handler(
            std::env::var(TEST_REPLY_ENV).ok().as_deref(),
            Arc::clone(&store),
        ) {
            Some(stub) => {
                warn!("{TEST_REPLY_ENV} is set: Slack turns run the real harness with a fake agent, not the reasoner (debug build, tests and local QA only)");
                stub
            }
            None => make_handler(
                (!runtimes.is_empty()).then(|| {
                    Arc::new(SlackConversationHistory::new(&runtimes)) as Arc<dyn TurnHistory>
                }),
            ),
        };
        let surface = match build_surface(
            Arc::clone(&store),
            &installs,
            &api_base,
            handler,
            dry_run,
        ) {
            Ok(s) => s,
            Err(e) => {
                let detail = format!("could not start the Slack listener: {e:#}");
                report_inactive(
                    &store,
                    SurfaceState::Misconfigured,
                    &detail,
                    Some(&format!("Check the stored Slack app with `augmentagent slack app verify`, {RESTART}.")),
                    &teams,
                    dry_run,
                    now_ms(),
                )?;
                anyhow::bail!(detail);
            }
        };
        let surface = match approvals {
            Some(a) => surface.with_approvals(a),
            None => surface,
        };
        let surface = match commands {
            Some(c) => surface.with_commands(c),
            None => surface,
        };
        // #1296 — live ingestion of subscribed conversations, and the
        // bounded catch-up after a sleep or restart that feeds it.
        let (surface, catch_up) = match subscribed {
            Some(sink) => {
                let catch_up = SubscribedCatchUp::new(
                    Arc::clone(&store),
                    runtimes
                        .iter()
                        .map(|r| (r.workspace.clone(), Arc::clone(&r.web)))
                        .collect(),
                );
                let sd = shutdown.clone();
                (
                    surface.with_subscribed_sink(sink),
                    Some(tokio::spawn(async move { catch_up.run(sd).await })),
                )
            }
            None => (surface, None),
        };
        let result = surface.run(shutdown).await;
        if let Some(task) = catch_up {
            let _ = task.await;
        }
        result
    })
}

/// #1288 — the handler `serve` runs with a wiki: owner turns through the
/// shared conversation harness, answered by `query` (the same `WikiQuerier`
/// Discord uses). #1296 — `history` gives history-in-prompt providers the
/// conversation's earlier messages.
pub fn conversation_handler(
    store: Arc<Store>,
    query: Arc<dyn QueryHandler>,
    wiki_root: PathBuf,
    history: Option<Arc<dyn TurnHistory>>,
) -> Arc<dyn SlackTurnHandler> {
    // #1292 — each conversation's own `model` choice (then its channel's,
    // then the daemon default), from the same file `/model` writes.
    let harness = SlackConversationHarness::new(store, query, wiki_root).with_selection(
        slack_selection(augmentagent_channel_core::model_selection::config_path()),
    );
    // #1296 — history for history-in-prompt providers.
    Arc::new(match history {
        Some(h) => harness.with_history(h),
        None => harness,
    })
}

/// #1292 — the owner commands `serve` runs on Slack.
pub fn slack_commands(store: Arc<Store>, deps: SlackCommandDeps) -> Arc<SlackCommands> {
    Arc::new(SlackCommands::new(store, deps))
}

/// Used when `serve` has no `--wiki-dir`: the query path needs one.
pub struct NoQueryHandler;

pub const NO_QUERY_REPLY: &str =
    "Query mode is not enabled on this daemon: start it with --wiki-dir <path>.";

#[async_trait]
impl SlackTurnHandler for NoQueryHandler {
    async fn handle_turn(&self, turn: &SlackTurn) -> Result<Option<SlackTurnReply>> {
        if turn.source == OwnerInputSource::Interaction {
            return Ok(None);
        }
        Ok(Some(SlackTurnReply {
            text: NO_QUERY_REPLY.to_string(),
            files: Vec::new(),
        }))
    }
}

/// **Test and local QA only.** In a debug build, owner turns run through the
/// real conversation harness (native session per conversation, turn claims,
/// queueing, cancel, restart recovery, owner files) but the agent is
/// [`StubAgent`] instead of the reasoner, so the whole serve path can be
/// exercised end to end on a host where no provider can run. Ignored in
/// release builds.
pub const TEST_REPLY_ENV: &str = "AUGMENTAGENT_TEST_SLACK_TURN_REPLY";

/// How long [`StubAgent`] works on a message containing `slow` (so queueing,
/// `cancel` and restart can be shown).
pub const TEST_SLOW_TURN: std::time::Duration = std::time::Duration::from_secs(15);

/// Stands in for the reasoner behind the harness. It joins the turn's native
/// session exactly like the Claude CLI adapter (create on the first turn,
/// resume after), and answers
/// `<prefix> <first line> (session <id>, turn <n>)`, plus one
/// `read <file> (<bytes> bytes)` per attachment it could open. `n` counts
/// the session's turns in a small file under the state directory, so it
/// survives a daemon restart.
struct StubAgent {
    prefix: String,
    counts: PathBuf,
}

impl StubAgent {
    fn next_turn(&self, session: &str) -> u64 {
        let mut counts: std::collections::BTreeMap<String, u64> = std::fs::read(&self.counts)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let n = counts.entry(session.to_string()).or_default();
        *n += 1;
        let n = *n;
        if let Some(parent) = self.counts.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &self.counts,
            serde_json::to_vec(&counts).unwrap_or_default(),
        );
        n
    }
}

#[async_trait]
impl QueryHandler for StubAgent {
    async fn answer(&self, _ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        use augmentagent_channel_core::native_session::{Launch, CURRENT};
        use augmentagent_channel_core::providers::ProviderKind;
        let Ok(session) = CURRENT.try_with(Arc::clone) else {
            // #1292 — qwen/glm have no native session: the legacy route,
            // with the conversation's selection.
            let model = augmentagent_channel_core::model_selection::conversation_selection()
                .flatten()
                .map_or("default", |k| k.name());
            let first = question.lines().next().unwrap_or_default().trim();
            return Ok(format!(
                "{} {first} (legacy route, model {model})",
                self.prefix
            ));
        };
        let provider = session.provider();
        let mut lease = session.begin(provider)?;
        let id = match lease.launch() {
            Launch::Create {
                requested_id: Some(id),
            }
            | Launch::Resume { id } => id,
            Launch::Create { requested_id: None } => format!(
                "{}-thread-{}",
                if provider == ProviderKind::Codex {
                    "codex"
                } else {
                    "stub"
                },
                uuid::Uuid::new_v4()
            ),
        };
        lease.observe(&id)?;
        if question.contains("slow") {
            tokio::time::sleep(TEST_SLOW_TURN).await;
        }
        lease.finish()?;
        let n = self.next_turn(&id);
        let first = question.lines().next().unwrap_or_default().trim();
        let mut reply = format!("{} {first} (session {id}, turn {n})", self.prefix);
        for line in question.lines() {
            let path = line
                .strip_prefix("IMAGE: ")
                .or_else(|| line.strip_prefix("- "))
                .map(|rest| rest.split("  (").next().unwrap_or(rest));
            if let Some(path) = path {
                let path = std::path::Path::new(path);
                if let (Ok(meta), Some(name)) = (std::fs::metadata(path), path.file_name()) {
                    reply.push_str(&format!(
                        "\nread {} ({} bytes)",
                        name.to_string_lossy(),
                        meta.len()
                    ));
                }
            }
        }
        Ok(reply)
    }
}

/// The [`TEST_REPLY_ENV`] stand-in, when set in a debug build.
pub fn test_turn_handler(
    value: Option<&str>,
    store: Arc<Store>,
) -> Option<Arc<dyn SlackTurnHandler>> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let prefix = value.map(str::trim).filter(|v| !v.is_empty())?;
    let state = augmentagent_channel_core::state_dir::state_dir_or(".");
    Some(stub_handler(
        prefix,
        store,
        state.join("test-slack-turns.json"),
        augmentagent_channel_core::model_selection::config_path(),
    ))
}

/// **Test and local QA only** (#1292): with [`TEST_REPLY_ENV`] set in a
/// debug build, loop prompts are answered `<prefix> loop ran: <prompt>`
/// instead of by the reasoner, so a loop can be shown firing into Slack on
/// a host where no provider can run. Ignored in release builds.
pub fn test_loop_runner(
    value: Option<&str>,
) -> Option<Arc<dyn augmentagent_approval_discord::LoopRunner>> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let prefix = value.map(str::trim).filter(|v| !v.is_empty())?;
    Some(Arc::new(StubLoopRunner(prefix.to_string())))
}

struct StubLoopRunner(String);

#[async_trait]
impl augmentagent_approval_discord::LoopRunner for StubLoopRunner {
    async fn run_prompt(
        &self,
        _request_id: &str,
        _owner: &str,
        prompt: &str,
        _model_profile: Option<&str>,
    ) -> Result<String> {
        Ok(format!("{} loop ran: {prompt}", self.0))
    }
}

fn stub_handler(
    prefix: &str,
    store: Arc<Store>,
    counts: PathBuf,
    selection: PathBuf,
) -> Arc<dyn SlackTurnHandler> {
    let agent = Arc::new(StubAgent {
        prefix: prefix.to_string(),
        counts,
    });
    let wiki = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // #1292 — the owner's per-conversation model selection, as in `serve`.
    Arc::new(
        SlackConversationHarness::new(store, agent, wiki)
            .with_selection(slack_selection(selection)),
    )
}

/// Run `task` so that neither an error nor a panic escapes: `serve` joins
/// every task with `?`, and one surface must never end the others.
pub fn supervise<F>(name: &'static str, task: F) -> tokio::task::JoinHandle<Result<()>>
where
    F: std::future::Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        // The inner task turns a panic into a JoinError instead of unwinding
        // through this one.
        match tokio::spawn(task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!(task = name, error = %format!("{e:#}"), "surface stopped with an error; other surfaces keep running")
            }
            Err(_) => error!(task = name, "surface panicked; other surfaces keep running"),
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_auth::{CredentialStore, MemoryCredentialStore};
    use augmentagent_channel_slack::app::{
        parse_app_token, parse_bot_token, APP_CREDENTIAL_PLATFORM,
    };
    use augmentagent_channel_slack::owner::OwnerInputSource;
    use augmentagent_channel_slack::surface::SlackWorkspace;
    use augmentagent_channel_slack::transport::event::{parse_envelope, Envelope};
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    const TEAM: &str = "T00000001";
    const T0: i64 = 1_700_000_000_000;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data.db")).unwrap();
        (dir, store)
    }

    fn creds(team: &str) -> SlackAppCredentials {
        SlackAppCredentials {
            team_id: team.into(),
            team_name: Some("Example Test".into()),
            team_url: None,
            bot_user_id: "U0000000B".into(),
            bot_user_name: Some("jarvis".into()),
            bot_id: Some("B00000001".into()),
            app_id: Some("A00000001".into()),
            scopes: None,
            installed_at: 1,
            verified_at: None,
            rotated_at: None,
            app_token: parse_app_token("xapp-test-000").unwrap(),
            bot_token: parse_bot_token("xoxb-test-000").unwrap(),
        }
    }

    fn bind(store: &Store, team: &str) {
        let ws = SlackWorkspace::new(team, None).unwrap();
        store
            .bind_surface_owner(&ws.owner("U00000001").unwrap(), T0)
            .unwrap();
    }

    /// Fails the test if anything reads the credential store.
    struct Untouchable;
    impl CredentialStore for Untouchable {
        fn backend(&self) -> &'static str {
            "untouchable"
        }
        fn put(&self, _: &str, _: &str, _: &[u8]) -> Result<(), augmentagent_auth::AuthError> {
            panic!("credential store written")
        }
        fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, augmentagent_auth::AuthError> {
            panic!("credential store read while the surface is off")
        }
        fn delete(&self, _: &str, _: &str) -> Result<(), augmentagent_auth::AuthError> {
            panic!("credential store written")
        }
        fn exists(&self, _: &str, _: &str) -> bool {
            panic!("credential store read while the surface is off")
        }
    }

    fn inactive(p: Plan) -> (SurfaceState, String, Option<String>) {
        match p {
            Plan::Inactive {
                state,
                detail,
                recovery,
                ..
            } => (state, detail, recovery),
            Plan::Ready { .. } => panic!("expected an inactive plan"),
        }
    }

    #[test]
    fn switch_values() {
        assert_eq!(Switch::parse(None), Ok(Switch::Auto));
        assert_eq!(Switch::parse(Some("")), Ok(Switch::Auto));
        assert_eq!(Switch::parse(Some("auto")), Ok(Switch::Auto));
        for on in ["1", "true", "ON", "yes"] {
            assert_eq!(Switch::parse(Some(on)), Ok(Switch::On), "{on}");
        }
        for off in ["0", "false", "Off", "no"] {
            assert_eq!(Switch::parse(Some(off)), Ok(Switch::Off), "{off}");
        }
        assert_eq!(Switch::parse(Some("maybe")), Err("maybe".to_string()));
    }

    #[test]
    fn off_is_disabled_without_reading_credentials() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(Untouchable));
        let (state, detail, _) = inactive(plan(Ok(Switch::Off), &apps, &store, None));
        assert_eq!(state, SurfaceState::Disabled);
        assert!(detail.contains(ENABLE_ENV), "{detail}");
    }

    #[test]
    fn nothing_installed_is_not_configured_with_install_recovery() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
        let (state, _, recovery) = inactive(plan(Ok(Switch::Auto), &apps, &store, None));
        assert_eq!(state, SurfaceState::NotConfigured);
        assert!(recovery.unwrap().contains("slack app install"));
    }

    #[test]
    fn installed_without_an_owner_says_how_to_bind() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
        apps.save(&creds(TEAM)).unwrap();
        let (state, detail, recovery) = inactive(plan(Ok(Switch::Auto), &apps, &store, None));
        assert_eq!(state, SurfaceState::NotConfigured);
        assert!(detail.contains(TEAM), "{detail}");
        assert!(recovery.unwrap().contains("slack app owner bind"));
    }

    #[test]
    fn forced_on_without_configuration_is_misconfigured() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
        let (state, _, recovery) = inactive(plan(Ok(Switch::On), &apps, &store, None));
        assert_eq!(state, SurfaceState::Misconfigured);
        assert!(recovery.unwrap().contains("slack app install"));
    }

    #[test]
    fn an_unreadable_switch_is_misconfigured() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(Untouchable));
        let (state, detail, recovery) = inactive(plan(Err("maybe".into()), &apps, &store, None));
        assert_eq!(state, SurfaceState::Misconfigured);
        assert!(detail.contains("maybe"));
        assert!(recovery.unwrap().contains(ENABLE_ENV));
    }

    #[test]
    fn corrupt_credentials_are_misconfigured_with_the_remove_hint() {
        let (_d, store) = temp_store();
        let mem = MemoryCredentialStore::default();
        let apps = SlackAppStore::new(Arc::new(mem.clone()));
        apps.save(&creds(TEAM)).unwrap();
        mem.put(APP_CREDENTIAL_PLATFORM, TEAM, b"not json").unwrap();
        bind(&store, TEAM);
        let (state, _, recovery) = inactive(plan(Ok(Switch::Auto), &apps, &store, None));
        assert_eq!(state, SurfaceState::Misconfigured);
        assert!(recovery.unwrap().contains("slack app remove"));
    }

    #[test]
    fn a_bad_api_base_is_misconfigured() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
        apps.save(&creds(TEAM)).unwrap();
        bind(&store, TEAM);
        let (state, _, recovery) = inactive(plan(
            Ok(Switch::Auto),
            &apps,
            &store,
            Some("http://example.com"),
        ));
        assert_eq!(state, SurfaceState::Misconfigured);
        assert!(recovery.unwrap().contains("AUGMENTAGENT_SLACK_API_BASE"));
    }

    #[test]
    fn install_plus_owner_is_ready_and_only_bound_teams_run() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
        apps.save(&creds(TEAM)).unwrap();
        apps.save(&creds("T00000002")).unwrap();
        bind(&store, TEAM);
        match plan(Ok(Switch::Auto), &apps, &store, Some("http://127.0.0.1:9")) {
            Plan::Ready { installs, api_base } => {
                assert_eq!(
                    installs
                        .iter()
                        .map(|i| i.creds.team_id.as_str())
                        .collect::<Vec<_>>(),
                    [TEAM]
                );
                assert_eq!(
                    installs[0].workspace.account().account_id(),
                    "team:T00000001"
                );
                assert_eq!(api_base, "http://127.0.0.1:9");
            }
            Plan::Inactive { detail, .. } => panic!("not ready: {detail}"),
        }
    }

    #[test]
    fn an_enterprise_grid_binding_keeps_its_enterprise_account() {
        let (_d, store) = temp_store();
        let apps = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
        apps.save(&creds(TEAM)).unwrap();
        let ws = SlackWorkspace::new(TEAM, Some("E00000001")).unwrap();
        store
            .bind_surface_owner(&ws.owner("U00000001").unwrap(), T0)
            .unwrap();
        let Plan::Ready { installs, .. } =
            plan(Ok(Switch::Auto), &apps, &store, Some("http://127.0.0.1:9"))
        else {
            panic!("not ready");
        };
        assert_eq!(
            installs[0].workspace.account().account_id(),
            "enterprise:E00000001/team:T00000001"
        );
    }

    // --- harness -----------------------------------------------------------

    /// (audit session, owner authorized, Discord context, question, native session)
    type Seen = (String, bool, bool, String, bool);

    #[derive(Default)]
    struct FakeQuery {
        seen: Mutex<Vec<Seen>>,
    }

    #[async_trait]
    impl QueryHandler for FakeQuery {
        async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
            let native = augmentagent_channel_core::native_session::CURRENT
                .try_with(|_| ())
                .is_ok();
            self.seen.lock().unwrap().push((
                ctx.session_id.clone(),
                ctx.owner_authorized,
                ctx.http.is_some() || ctx.channel_id.is_some() || ctx.guild_id.is_some(),
                question.to_string(),
                native,
            ));
            Ok(format!("reasoned: {question}"))
        }
    }

    fn turn(source: OwnerInputSource, text: &str) -> SlackTurn {
        let frame = serde_json::json!({
            "type": "events_api", "envelope_id": "env-1",
            "payload": {"team_id": TEAM, "event_id": "Ev1",
                "event": {"type": "message", "channel": "D00000001", "channel_type": "im",
                    "user": "U00000001", "text": text, "ts": "1700000000.000100"}}
        });
        let Ok(Envelope::Event(envelope)) = parse_envelope(&frame.to_string()) else {
            panic!("parse")
        };
        let ws = SlackWorkspace::new(TEAM, None).unwrap();
        let event_id = "D00000001:1700000000.000100".to_string();
        SlackTurn {
            turn_id: augmentagent_channel_slack::interactive::slack_turn_id(
                &ws.account(),
                &event_id,
            ),
            event_id,
            attempt: 1,
            owner: ws.owner("U00000001").unwrap(),
            conversation: Some(ws.conversation("D00000001", None).unwrap()),
            session: Some(ws.conversation("D00000001", None).unwrap()),
            source,
            text: text.into(),
            prompt: text.into(),
            inbound_dir: None,
            cancel: CancellationToken::new(),
            envelope: *envelope,
        }
    }

    fn harness(store: Arc<Store>, query: Arc<FakeQuery>) -> SlackConversationHarness {
        SlackConversationHarness::new(store, query, PathBuf::from("/wiki"))
            .with_selection(Arc::new(|_| Ok(None)))
    }

    // #1288 — replaces the single-turn adapter: the turn runs the shared
    // query path inside a native session, with owner authority (the turn
    // passed `owner::admit`), no Discord context, and a per-turn audit ID.
    #[tokio::test]
    async fn slack_turns_run_the_shared_query_path_in_a_native_session_as_the_owner() {
        let (_d, store) = temp_store();
        let query = Arc::new(FakeQuery::default());
        let handler = harness(Arc::new(store), query.clone());
        let reply = handler
            .handle_turn(&turn(OwnerInputSource::Message, "  what is due?  "))
            .await
            .unwrap()
            .unwrap();
        assert!(reply.text.starts_with("reasoned: "), "{}", reply.text);
        assert!(reply.text.contains("what is due?"));
        let seen = query.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        let (session, owner_authorized, discord_ctx, _question, native) = &seen[0];
        assert_eq!(session, "slack:T00000001:D00000001:1700000000.000100");
        assert!(owner_authorized, "Slack owner turns carry owner authority");
        assert!(
            !discord_ctx,
            "no Discord http/channel/guild on a Slack turn"
        );
        assert!(
            native,
            "the agent runs inside the conversation's native session"
        );
    }

    #[tokio::test]
    async fn the_harness_skips_empty_text_and_interactions() {
        let (_d, store) = temp_store();
        let query = Arc::new(FakeQuery::default());
        let handler = harness(Arc::new(store), query.clone());
        assert_eq!(
            handler
                .handle_turn(&turn(OwnerInputSource::Message, "   "))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            handler
                .handle_turn(&turn(OwnerInputSource::Interaction, "button"))
                .await
                .unwrap(),
            None
        );
        assert!(query.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_a_wiki_the_owner_is_told_how_to_enable_queries() {
        let reply = NoQueryHandler
            .handle_turn(&turn(OwnerInputSource::Message, "hi"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.text, NO_QUERY_REPLY);
    }

    // --- test-only reasoner stand-in --------------------------------------

    #[tokio::test]
    async fn the_test_override_runs_the_harness_and_echoes_session_and_turn_in_debug_builds_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        if !cfg!(debug_assertions) {
            assert!(
                test_turn_handler(Some("FAKE-REASONER:"), store).is_none(),
                "never honoured in release builds"
            );
            return;
        }
        assert!(test_turn_handler(Some("FAKE-REASONER:"), Arc::clone(&store)).is_some());
        let handler = stub_handler(
            "FAKE-REASONER:",
            Arc::clone(&store),
            dir.path().join("counts.json"),
            dir.path().join("model-selection.json"),
        );
        let first = handler
            .handle_turn(&turn(OwnerInputSource::Message, "status?"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            first.text.starts_with("FAKE-REASONER: status? (session "),
            "{}",
            first.text
        );
        let mut next = turn(OwnerInputSource::Message, "and now?");
        next.event_id = "D00000001:1700000000.000200".into();
        next.turn_id = "slack:T00000001:D00000001:1700000000.000200".into();
        let second = handler.handle_turn(&next).await.unwrap().unwrap();
        let session = |t: &str| {
            t.split("(session ")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            session(&first.text),
            session(&second.text),
            "one native session per DM"
        );
        assert!(second.text.ends_with("turn 2)"), "{}", second.text);
        assert!(test_turn_handler(None, Arc::clone(&store)).is_none());
        assert!(test_turn_handler(Some("  "), store).is_none());
    }

    /// #1292 — the QA stand-in honours the owner's per-conversation model,
    /// natively for Claude/Codex and on the legacy route for the others.
    #[tokio::test]
    async fn the_test_override_uses_the_conversations_model_selection() {
        use augmentagent_channel_core::model_selection::SelectionStore;
        use augmentagent_channel_core::providers::ProviderKind;
        if !cfg!(debug_assertions) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let selection = dir.path().join("model-selection.json");
        let dm = SlackWorkspace::new(TEAM, None)
            .unwrap()
            .conversation("D00000001", None)
            .unwrap();
        SelectionStore::new(&selection)
            .set(Some(&dm.storage_key()), Some(ProviderKind::Codex))
            .unwrap();
        let handler = stub_handler(
            "FAKE-REASONER:",
            Arc::clone(&store),
            dir.path().join("counts.json"),
            selection.clone(),
        );
        let reply = handler
            .handle_turn(&turn(OwnerInputSource::Message, "hello"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            reply.text.contains("(session codex-thread-"),
            "{}",
            reply.text
        );

        // qwen has no native session: the legacy route, with the selection.
        let other = dir.path().join("other.json");
        SelectionStore::new(&other)
            .set(Some(&dm.storage_key()), Some(ProviderKind::Qwen))
            .unwrap();
        let store = Arc::new(Store::open(dir.path().join("other.db")).unwrap());
        let handler = stub_handler("FAKE-REASONER:", store, dir.path().join("c2.json"), other);
        let legacy = handler
            .handle_turn(&turn(OwnerInputSource::Message, "hello"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            legacy.text,
            "FAKE-REASONER: hello (legacy route, model qwen)"
        );
    }

    /// #1292 — local QA can show a loop firing into Slack without a
    /// provider: the same debug-only override answers loop prompts.
    #[tokio::test]
    async fn the_test_override_also_answers_loops_in_debug_builds_only() {
        let runner = test_loop_runner(Some("FAKE-REASONER:"));
        if !cfg!(debug_assertions) {
            assert!(runner.is_none());
            return;
        }
        let answer = runner
            .unwrap()
            .run_prompt(
                "loop:1:created:0",
                "slack|team:T1|U1",
                "check the build",
                None,
            )
            .await
            .unwrap();
        assert_eq!(answer, "FAKE-REASONER: loop ran: check the build");
        assert!(test_loop_runner(None).is_none());
        assert!(test_loop_runner(Some(" ")).is_none());
    }

    #[test]
    fn serve_wires_owner_commands_with_the_daemons_selection_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let deps = augmentagent_channel_slack::commands::SlackCommandDeps::new(
            dir.path().join("sel.json"),
        );
        // Builds without touching the network or the credential store.
        let _commands = slack_commands(store, deps);
    }

    // --- isolation -------------------------------------------------------

    #[tokio::test]
    async fn a_failing_or_panicking_surface_never_fails_serve() {
        let err = supervise("test-error", async { anyhow::bail!("surface broke") });
        let panics = supervise("test-panic", async { panic!("surface panicked") });
        let ok = supervise("test-ok", async { Ok(()) });
        assert!(err.await.unwrap().is_ok());
        assert!(panics.await.unwrap().is_ok());
        assert!(ok.await.unwrap().is_ok());
    }
}
