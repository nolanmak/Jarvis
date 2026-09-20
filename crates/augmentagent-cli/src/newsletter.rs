//! Narrow NewsletterBuddy API bridge for the Discord query agent.
//! Credentials stay in a private host file or OS keyring; the reasoner receives only a
//! request ID and an allowlisted command shape.

use std::io::{self, Read};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use reqwest::{Client, Method, Url};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const PLATFORM: &str = "newsletterbuddy";
const ACCOUNT: &str = augmentagent_auth::DEFAULT_ACCOUNT;
/// The audience-approval / release / delivery-status endpoints require a
/// distinct editorial credential; the research token cannot reach them.
const EDITORIAL_ACCOUNT: &str = "editorial";

#[derive(Subcommand)]
pub enum Command {
    /// Store an API token from stdin in the OS credential store (manual setup only).
    Configure,
    /// Create a newsletter desk.
    Create {
        #[arg(long)]
        name: String,
    },
    /// Save a versioned research brief.
    Brief {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        topic: String,
        /// Public RSS/Atom feed URL; repeat to monitor multiple feeds.
        #[arg(long = "feed-url")]
        feed_urls: Vec<String>,
        /// Public Bluesky handle to monitor; repeat for multiple profiles.
        #[arg(long = "bluesky-profile")]
        bluesky_profiles: Vec<String>,
        /// Public HTTPS page to capture with a remote browser on every run; repeat up to ten times.
        #[arg(long = "browser-url")]
        browser_urls: Vec<String>,
        /// Queue a bounded browser capture when an HTTP result is script-only.
        #[arg(long)]
        browser_fallback: bool,
        /// Maximum age in days for dated research items (1–365).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=365))]
        freshness_days: Option<u16>,
        /// Whether to include source items without a publication date.
        #[arg(long, value_parser = ["include", "exclude"])]
        undated_policy: Option<String>,
    },
    /// Start an idempotent research run for a brief revision.
    Research {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        brief_revision: u32,
    },
    /// Read research-run status.
    Run {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        run_id: String,
    },
    /// Cancel a research run.
    Cancel {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        run_id: String,
    },
    /// Queue an observe-only browser capture (optionally scrolling a feed) for a research run.
    BrowserTask {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        url: String,
        /// Scroll down this many screens (0–15), keeping every post seen, e.g. for feeds and search results.
        #[arg(long, default_value_t = 0)]
        scroll: u8,
    },
    /// List task IDs and statuses for one research run, including automatic fallbacks.
    BrowserTasks {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        run_id: String,
    },
    /// Read a browser capture status and its normalized result.
    BrowserTaskGet {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        task_id: String,
    },
    /// Cancel a queued or active browser capture.
    BrowserTaskCancel {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        task_id: String,
    },
    /// List citable evidence for a newsletter.
    Evidence {
        #[arg(long)]
        newsletter_id: String,
    },
    /// Record editorial feedback on a research candidate.
    Feedback {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        candidate_id: String,
        #[arg(long, value_parser = ["useful", "not_useful"])]
        label: String,
        #[arg(long, value_parser = ["valuable", "off_topic", "stale", "duplicate", "low_quality", "sponsored", "other"])]
        reason: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        supersedes_event_id: Option<String>,
    },
    /// List effective editorial feedback.
    FeedbackList {
        #[arg(long)]
        newsletter_id: String,
    },
    /// Read the active inspectable ranking version.
    Rank {
        #[arg(long)]
        newsletter_id: String,
    },
    /// Reset learned source preferences while retaining feedback history.
    RankReset {
        #[arg(long)]
        newsletter_id: String,
    },
    /// Create a Jarvis-owned daily research or draft schedule.
    ScheduleCreate {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        brief_revision: u32,
        #[arg(long, value_parser = ["research", "draft"])]
        kind: String,
        #[arg(long)]
        local_time: String,
        #[arg(long)]
        timezone: String,
    },
    /// List active and paused schedules.
    ScheduleList {
        #[arg(long)]
        newsletter_id: String,
    },
    /// Read a schedule including its current version.
    ScheduleGet {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        schedule_id: String,
    },
    /// Edit, pause, resume or soft-delete a schedule with optimistic locking.
    ScheduleEdit {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        schedule_id: String,
        #[arg(long)]
        expected_version: u32,
        #[arg(long)]
        local_time: String,
        #[arg(long)]
        timezone: String,
        #[arg(long, value_parser = ["active", "paused", "deleted"])]
        status: String,
    },
    /// Fire the latest due occurrence from a trusted Discord or loop event.
    ScheduleRun {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        schedule_id: String,
    },
    /// Generate a cited draft from stored evidence.
    Generate {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        brief_revision: u32,
    },
    /// Save a cited proposal written by the current Jarvis reasoner.
    DraftSubmit {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        brief_revision: u32,
        #[arg(long)]
        proposal_json: String,
    },
    /// Read an immutable draft revision.
    Draft {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        revision: u32,
    },
    /// Approve a draft revision for a channel, binding the audience snapshot and
    /// creating held (unsent) delivery rows. Owner-operated; editorial credential.
    Approve {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        draft_revision: u32,
        #[arg(long, value_parser = ["email", "sms"])]
        channel: String,
    },
    /// Release an approval: move its held rows to the queue so the delivery
    /// worker sends them for real. Owner-operated; editorial credential.
    Release {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        approval_id: String,
    },
    /// Read redacted per-recipient delivery statuses for an approval.
    Deliveries {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        approval_id: String,
    },
}

fn request_key(event_id: &str, operation: &str) -> String {
    format!("newsletterbuddy:v1:{event_id}:{operation}")
}

fn schedule_create_operation(newsletter_id: &str, brief_revision: u32, kind: &str) -> String {
    format!("schedule-create:{newsletter_id}:{brief_revision}:{kind}")
}

fn browser_task_operation(newsletter_id: &str, run_id: &str, url: &str, scroll: u8) -> String {
    let digest = Sha256::digest(format!("{newsletter_id}:{run_id}:{url}:{scroll}").as_bytes());
    format!("browser-task:{digest:x}")
}

fn event_request_key(operation: &str) -> Result<String> {
    let event_id = std::env::var("NEWSLETTERBUDDY_REQUEST_ID")
        .context("NewsletterBuddy requires a trusted Discord request ID")?;
    if !trusted_request_id(&event_id) {
        bail!("NewsletterBuddy request ID is invalid");
    }
    Ok(request_key(&event_id, operation))
}

pub fn trusted_request_id(value: &str) -> bool {
    if value.is_empty() || value.len() > 100 {
        return false;
    }
    if let Some((channel, message)) = value.split_once(':') {
        if !channel.is_empty()
            && channel.chars().all(|c| c.is_ascii_digit())
            && !message.is_empty()
            && message.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    value.starts_with("loop:")
        && value.len() > 5
        && value[5..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '-')
}

fn validated_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw).context("invalid NEWSLETTERBUDDY_URL")?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("NEWSLETTERBUDDY_URL must be HTTPS or loopback HTTP");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("NEWSLETTERBUDDY_URL cannot contain credentials, query, or fragment");
    }
    url.set_path("/");
    Ok(url)
}

pub fn validated_url_for_agent(raw: &str) -> Result<()> {
    validated_url(raw).map(|_| ())
}

fn uuid(raw: &str) -> Result<&str> {
    uuid::Uuid::parse_str(raw).context("expected UUID")?;
    Ok(raw)
}

fn feedback_body(
    candidate_id: &str,
    label: &str,
    reason: &str,
    note: Option<&str>,
    supersedes_event_id: Option<&str>,
) -> Value {
    let mut body = json!({ "candidateId": candidate_id, "label": label, "reason": reason });
    if let Some(note) = note {
        body["note"] = json!(note);
    }
    if let Some(id) = supersedes_event_id {
        body["supersedesEventId"] = json!(id);
    }
    body
}

fn brief_body(
    prompt: &str,
    topic: &str,
    feed_urls: &[String],
    bluesky_profiles: &[String],
    browser_urls: &[String],
    browser_fallback: bool,
    freshness_days: Option<u16>,
    undated_policy: Option<&str>,
) -> Value {
    let mut body = json!({"prompt": prompt, "topic": topic,
        "feedUrls": feed_urls, "socialProfiles": bluesky_profiles,
        "browserUrls": browser_urls, "browserFallback": browser_fallback});
    if let Some(days) = freshness_days {
        body["freshnessDays"] = json!(days);
    }
    if let Some(policy) = undated_policy {
        body["undatedPolicy"] = json!(policy);
    }
    body
}

/// NewsletterBuddy allows 18 steps per browser task; scrolls stay well under it.
const MAX_BROWSER_SCROLLS: u8 = 15;

/// Query keys that look like credentials. Matched as whole `_`/`-`-separated
/// words so search parameters such as `keywords` still pass.
fn credential_query_key(key: &str) -> bool {
    const WORDS: [&str; 12] = [
        "token", "secret", "auth", "key", "apikey", "session", "sessionid", "cookie", "signature",
        "sig", "code", "password",
    ];
    key.to_ascii_lowercase()
        .split(|c: char| c == '_' || c == '-')
        .any(|word| WORDS.contains(&word))
}

fn browser_task_body(raw: &str, scroll: u8) -> Result<Value> {
    if scroll > MAX_BROWSER_SCROLLS {
        bail!("browser task scroll must be between 0 and {MAX_BROWSER_SCROLLS}");
    }
    let url = Url::parse(raw).context("expected a public HTTPS page URL")?;
    let host = url.host_str().context("browser URL needs a host")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || !host.contains('.')
        || host.parse::<std::net::IpAddr>().is_ok()
        || url.query_pairs().any(|(key, _)| credential_query_key(&key))
    {
        bail!("browser task requires a public HTTPS URL without credentials");
    }
    let steps: Vec<Value> = (0..scroll)
        .map(|_| json!({ "kind": "scroll", "deltaY": 2000 }))
        .collect();
    Ok(json!({ "capability": "research.browser", "task": {
        "startUrl": url.as_str(), "allowedHosts": [host], "steps": steps
    } }))
}

fn token_file(path: &std::path::Path) -> Result<String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
        .context("NewsletterBuddy credential file cannot be opened")?;
    let metadata = file.metadata().context("NewsletterBuddy credential file metadata unavailable")?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600 || metadata.len() > 4096 {
        bail!("NewsletterBuddy credential file must be owner-owned, regular, mode 0600 and at most 4096 bytes");
    }
    let mut bytes = Vec::new();
    (&mut file).take(4097).read_to_end(&mut bytes)
        .context("NewsletterBuddy credential file cannot be read")?;
    if bytes.len() > 4096 { bail!("NewsletterBuddy credential file exceeds 4096 bytes"); }
    if bytes.last() == Some(&b'\n') { bytes.pop(); }
    if bytes.is_empty() || bytes.iter().any(|b| !b.is_ascii_graphic()) {
        bail!("NewsletterBuddy credential file must contain one nonempty token");
    }
    Ok(String::from_utf8(bytes).context("NewsletterBuddy credential file must be ASCII")?)
}

struct Api {
    base: Url,
    token: String,
    client: Client,
}

/// Load a NewsletterBuddy bearer token: an explicit private file
/// (`token_file_env`), else `~/.config/augmentagent/<default_file>`, else the OS
/// credential store under `keyring_account`.
fn load_token(token_file_env: &str, default_file: &str, keyring_account: &str) -> Result<String> {
    let configured = std::env::var_os(token_file_env);
    let default = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| std::path::PathBuf::from(p).join(".config")))
        .map(|p| p.join("augmentagent").join(default_file));
    let selected = configured.map(std::path::PathBuf::from)
        .or_else(|| default.filter(|p| std::fs::symlink_metadata(p).is_ok()));
    let token = if let Some(path) = selected {
        token_file(&path)?
    } else {
        String::from_utf8(augmentagent_auth::Auth::get(PLATFORM, keyring_account)
            .context("NewsletterBuddy token missing; configure a private token file or the OS credential store")?)
            .context("NewsletterBuddy token is not UTF-8")?
    };
    if token.is_empty() || token.bytes().any(|b| !b.is_ascii_graphic()) {
        bail!("NewsletterBuddy token is invalid");
    }
    Ok(token)
}

impl Api {
    fn from_local_config() -> Result<Self> {
        Self::build(load_token("NEWSLETTERBUDDY_TOKEN_FILE", "newsletterbuddy.token", ACCOUNT)?)
    }

    /// Editorial credential for audience approval, release and delivery status.
    fn from_editorial_config() -> Result<Self> {
        Self::build(load_token(
            "NEWSLETTERBUDDY_EDITORIAL_TOKEN_FILE",
            "newsletterbuddy-editorial.token",
            EDITORIAL_ACCOUNT,
        )?)
    }

    fn build(token: String) -> Result<Self> {
        let base = validated_url(
            &std::env::var("NEWSLETTERBUDDY_URL")
                .context("NEWSLETTERBUDDY_URL is not configured")?,
        )?;
        Ok(Self {
            base,
            token,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(75))
                .build()?,
        })
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> Result<Value> {
        let url = self
            .base
            .join(path)
            .context("invalid NewsletterBuddy API path")?;
        let mut request = self.client.request(method, url).bearer_auth(&self.token);
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .context("NewsletterBuddy unavailable; check NEWSLETTERBUDDY_URL and the server")?;
        let status = response.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(json!({"cancelled": true}));
        }
        let value: Value = response
            .json()
            .await
            .context("NewsletterBuddy returned non-JSON data")?;
        if !status.is_success() {
            let code = value
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            bail!("NewsletterBuddy HTTP {}: {}", status.as_u16(), code);
        }
        Ok(value)
    }
}

pub async fn run(command: &Command) -> Result<()> {
    if matches!(command, Command::Configure) {
        let mut token = String::new();
        io::stdin()
            .take(4096)
            .read_to_string(&mut token)
            .context("read NewsletterBuddy token from stdin")?;
        let token = token.trim();
        if token.is_empty() {
            bail!("NewsletterBuddy token cannot be empty");
        }
        augmentagent_auth::Auth::put(PLATFORM, ACCOUNT, token.as_bytes())
            .context("store NewsletterBuddy token in OS credential store")?;
        println!("NewsletterBuddy token stored for this host");
        return Ok(());
    }
    // Approval, release and delivery status need the editorial credential; the
    // research token cannot reach those endpoints.
    let api = if matches!(
        command,
        Command::Approve { .. } | Command::Release { .. } | Command::Deliveries { .. }
    ) {
        Api::from_editorial_config()?
    } else {
        Api::from_local_config()?
    };
    let result = match command {
        Command::Configure => unreachable!(),
        Command::Create { name } => {
            let key = event_request_key("create")?;
            api.call(
                Method::POST,
                "v1/newsletters",
                Some(json!({"name": name})),
                Some(&key),
            )
            .await?
        }
        Command::Brief {
            newsletter_id,
            prompt,
            topic,
            feed_urls,
            bluesky_profiles,
            browser_urls,
            browser_fallback,
            freshness_days,
            undated_policy,
        } => {
            let id = uuid(newsletter_id)?;
            let key = event_request_key(&format!("brief:{id}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/briefs"),
                Some(brief_body(
                    prompt,
                    topic,
                    feed_urls,
                    bluesky_profiles,
                    browser_urls,
                    *browser_fallback,
                    *freshness_days,
                    undated_policy.as_deref(),
                )),
                Some(&key),
            )
            .await?
        }
        Command::Research {
            newsletter_id,
            brief_revision,
        } => {
            let id = uuid(newsletter_id)?;
            let key = event_request_key(&format!("research:{id}:{brief_revision}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/research-runs"),
                Some(json!({"briefRevision": brief_revision})),
                Some(&key),
            )
            .await?
        }
        Command::Run {
            newsletter_id,
            run_id,
        } => {
            let id = uuid(newsletter_id)?;
            let run = uuid(run_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/research-runs/{run}"),
                None,
                None,
            )
            .await?
        }
        Command::Cancel {
            newsletter_id,
            run_id,
        } => {
            let id = uuid(newsletter_id)?;
            let run = uuid(run_id)?;
            api.call(
                Method::DELETE,
                &format!("v1/newsletters/{id}/research-runs/{run}"),
                None,
                None,
            )
            .await?
        }
        Command::BrowserTask {
            newsletter_id,
            run_id,
            url,
            scroll,
        } => {
            let id = uuid(newsletter_id)?;
            let run = uuid(run_id)?;
            let body = browser_task_body(url, *scroll)?;
            let normalized_url = body["task"]["startUrl"]
                .as_str()
                .context("invalid browser task URL")?;
            let key = event_request_key(&browser_task_operation(id, run, normalized_url, *scroll))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/research-runs/{run}/worker-tasks"),
                Some(body),
                Some(&key),
            )
            .await?
        }
        Command::BrowserTasks {
            newsletter_id,
            run_id,
        } => {
            let id = uuid(newsletter_id)?;
            let run = uuid(run_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/research-runs/{run}/worker-tasks"),
                None,
                None,
            )
            .await?
        }
        Command::BrowserTaskGet {
            newsletter_id,
            task_id,
        } => {
            let id = uuid(newsletter_id)?;
            let task = uuid(task_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/worker-tasks/{task}"),
                None,
                None,
            )
            .await?
        }
        Command::BrowserTaskCancel {
            newsletter_id,
            task_id,
        } => {
            let id = uuid(newsletter_id)?;
            let task = uuid(task_id)?;
            api.call(
                Method::DELETE,
                &format!("v1/newsletters/{id}/worker-tasks/{task}"),
                None,
                None,
            )
            .await?
        }
        Command::Evidence { newsletter_id } => {
            let id = uuid(newsletter_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/evidence"),
                None,
                None,
            )
            .await?
        }
        Command::Feedback {
            newsletter_id,
            candidate_id,
            label,
            reason,
            note,
            supersedes_event_id,
        } => {
            let id = uuid(newsletter_id)?;
            let candidate = uuid(candidate_id)?;
            let supersedes = supersedes_event_id.as_deref().map(uuid).transpose()?;
            let key = event_request_key(&format!("feedback:{id}:{candidate}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/feedback"),
                Some(feedback_body(
                    candidate,
                    label,
                    reason,
                    note.as_deref(),
                    supersedes,
                )),
                Some(&key),
            )
            .await?
        }
        Command::FeedbackList { newsletter_id } => {
            let id = uuid(newsletter_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/feedback"),
                None,
                None,
            )
            .await?
        }
        Command::Rank { newsletter_id } => {
            let id = uuid(newsletter_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/rank"),
                None,
                None,
            )
            .await?
        }
        Command::RankReset { newsletter_id } => {
            let id = uuid(newsletter_id)?;
            let key = event_request_key(&format!("rank-reset:{id}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/rank/reset"),
                Some(json!({})),
                Some(&key),
            )
            .await?
        }
        Command::ScheduleCreate {
            newsletter_id,
            brief_revision,
            kind,
            local_time,
            timezone,
        } => {
            let id = uuid(newsletter_id)?;
            let key = event_request_key(&schedule_create_operation(id, *brief_revision, kind))?;
            api.call(Method::POST, &format!("v1/newsletters/{id}/schedules"),
                Some(json!({ "briefRevision": brief_revision, "kind": kind, "schedulerOwner": "jarvis",
                    "localTime": local_time, "timezone": timezone })), Some(&key)).await?
        }
        Command::ScheduleList { newsletter_id } => {
            let id = uuid(newsletter_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/schedules"),
                None,
                None,
            )
            .await?
        }
        Command::ScheduleGet {
            newsletter_id,
            schedule_id,
        } => {
            let id = uuid(newsletter_id)?;
            let schedule = uuid(schedule_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/schedules/{schedule}"),
                None,
                None,
            )
            .await?
        }
        Command::ScheduleEdit {
            newsletter_id,
            schedule_id,
            expected_version,
            local_time,
            timezone,
            status,
        } => {
            let id = uuid(newsletter_id)?;
            let schedule = uuid(schedule_id)?;
            let key = event_request_key(&format!("schedule-edit:{schedule}"))?;
            api.call(
                Method::PUT,
                &format!("v1/newsletters/{id}/schedules/{schedule}"),
                Some(
                    json!({ "expectedVersion": expected_version, "localTime": local_time,
                    "timezone": timezone, "status": status }),
                ),
                Some(&key),
            )
            .await?
        }
        Command::ScheduleRun {
            newsletter_id,
            schedule_id,
        } => {
            let id = uuid(newsletter_id)?;
            let schedule = uuid(schedule_id)?;
            let key = event_request_key(&format!("occurrence:{schedule}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/schedules/{schedule}/occurrences"),
                Some(json!({})),
                Some(&key),
            )
            .await?
        }
        Command::Generate {
            newsletter_id,
            brief_revision,
        } => {
            let id = uuid(newsletter_id)?;
            let key = event_request_key(&format!("draft:{id}:{brief_revision}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/drafts/generate"),
                Some(json!({"briefRevision": brief_revision})),
                Some(&key),
            )
            .await?
        }
        Command::DraftSubmit { newsletter_id, brief_revision, proposal_json } => {
            let id = uuid(newsletter_id)?;
            if proposal_json.len() > 65_536 { bail!("NewsletterBuddy proposal is too large"); }
            let proposal: Value = serde_json::from_str(proposal_json)
                .context("NewsletterBuddy proposal must be valid JSON")?;
            let key = event_request_key(&format!("draft-submit:{id}:{brief_revision}"))?;
            api.call(Method::POST, &format!("v1/newsletters/{id}/drafts"),
                Some(json!({"briefRevision": brief_revision, "proposal": proposal})), Some(&key)).await?
        }
        Command::Draft {
            newsletter_id,
            revision,
        } => {
            let id = uuid(newsletter_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/drafts/{revision}"),
                None,
                None,
            )
            .await?
        }
        Command::Approve {
            newsletter_id,
            draft_revision,
            channel,
        } => {
            let id = uuid(newsletter_id)?;
            let key = event_request_key(&format!("approve:{id}:{draft_revision}:{channel}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/approvals"),
                Some(json!({"draftRevision": draft_revision, "channel": channel})),
                Some(&key),
            )
            .await?
        }
        Command::Release {
            newsletter_id,
            approval_id,
        } => {
            let id = uuid(newsletter_id)?;
            let approval = uuid(approval_id)?;
            let key = event_request_key(&format!("release:{id}:{approval}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/approvals/{approval}/release"),
                Some(json!({})),
                Some(&key),
            )
            .await?
        }
        Command::Deliveries {
            newsletter_id,
            approval_id,
        } => {
            let id = uuid(newsletter_id)?;
            let approval = uuid(approval_id)?;
            api.call(
                Method::GET,
                &format!("v1/newsletters/{id}/approvals/{approval}/deliveries"),
                None,
                None,
            )
            .await?
        }
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_token_file_survives_reopening_and_rejects_unsafe_inputs() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "synthetic-token\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(token_file(&path).unwrap(), "synthetic-token");
        assert_eq!(token_file(&path).unwrap(), "synthetic-token");
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(token_file(&link).is_err());
        assert!(token_file(dir.path()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(token_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        for invalid in ["", "\n", "one two", "one\ntwo", "token\n\n", " token", "token\r\n"] {
            std::fs::write(&path, invalid).unwrap();
            let error = token_file(&path).unwrap_err().to_string();
            assert!(!error.contains("one two"));
        }
        std::fs::write(&path, "x".repeat(4097)).unwrap();
        assert!(token_file(&path).is_err());
    }

    #[test]
    fn stable_request_key_is_scoped_to_command_and_discord_event() {
        assert_eq!(
            request_key("123:456", "research"),
            "newsletterbuddy:v1:123:456:research"
        );
        assert_ne!(
            request_key("123:456", "research"),
            request_key("123:456", "draft")
        );
    }

    #[test]
    fn same_discord_event_can_create_separate_research_and_draft_schedules() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert_ne!(
            request_key("123:456", &schedule_create_operation(id, 2, "research")),
            request_key("123:456", &schedule_create_operation(id, 2, "draft")),
        );
    }

    #[test]
    fn approval_and_release_request_keys_are_scoped_and_distinct() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        // Approving the same draft for email and sms are separate operations.
        assert_ne!(
            request_key("123:456", &format!("approve:{id}:2:email")),
            request_key("123:456", &format!("approve:{id}:2:sms")),
        );
        // Releasing an approval is distinct from approving a draft revision.
        assert_ne!(
            request_key("123:456", &format!("approve:{id}:2:email")),
            request_key("123:456", &format!("release:{id}:{id}")),
        );
        // A replay of the same trusted event yields a stable release key.
        assert_eq!(
            request_key("123:456", &format!("release:{id}:{id}")),
            request_key("123:456", &format!("release:{id}:{id}")),
        );
    }

    #[test]
    fn brief_payload_can_include_multiple_explicit_feed_urls() {
        let body = brief_body(
            "Find updates",
            "robotics",
            &[
                "https://a.example/rss".into(),
                "https://b.example/atom".into(),
            ],
            &["builder.bsky.social".into()],
            &[],
            false,
            Some(14),
            Some("exclude"),
        );
        assert_eq!(
            body["feedUrls"],
            json!(["https://a.example/rss", "https://b.example/atom"])
        );
        assert_eq!(body["freshnessDays"], 14);
        assert_eq!(body["undatedPolicy"], "exclude");
        assert_eq!(body["socialProfiles"], json!(["builder.bsky.social"]));
    }

    #[test]
    fn brief_payload_can_schedule_public_browser_pages_for_daily_research() {
        let body = brief_body(
            "Find events",
            "events",
            &[],
            &[],
            &["https://events.example/schedule".into()],
            false,
            None,
            None,
        );
        assert_eq!(
            body["browserUrls"],
            json!(["https://events.example/schedule"])
        );
    }

    #[test]
    fn brief_payload_can_opt_into_dynamic_page_browser_fallback() {
        let body = brief_body("Find events", "events", &[], &[], &[], true, None, None);
        assert_eq!(body["browserFallback"], true);
    }

    #[test]
    fn brief_payload_omits_unrequested_freshness_options() {
        let body = brief_body("Find updates", "robotics", &[], &[], &[], false, None, None);
        assert!(body.get("freshnessDays").is_none());
        assert!(body.get("undatedPolicy").is_none());
    }

    #[test]
    fn browser_task_payload_is_observe_only_and_bound_to_one_public_host() {
        let body = browser_task_body("https://events.example/founders?day=friday", 0).unwrap();
        assert_eq!(body["capability"], "research.browser");
        assert_eq!(
            body["task"]["startUrl"],
            "https://events.example/founders?day=friday"
        );
        assert_eq!(body["task"]["allowedHosts"], json!(["events.example"]));
        assert_eq!(body["task"]["steps"], json!([]));
        for url in [
            "http://events.example/founders",
            "https://127.0.0.1/private",
            "https://events.example/path?access_token=secret",
            "https://user:pass@events.example/",
        ] {
            assert!(browser_task_body(url, 0).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn browser_task_scrolls_feeds_and_accepts_search_urls() {
        let body = browser_task_body("https://www.linkedin.com/feed/", 3).unwrap();
        assert_eq!(
            body["task"]["steps"],
            json!([
                { "kind": "scroll", "deltaY": 2000 },
                { "kind": "scroll", "deltaY": 2000 },
                { "kind": "scroll", "deltaY": 2000 }
            ])
        );
        assert!(browser_task_body("https://www.linkedin.com/feed/", MAX_BROWSER_SCROLLS + 1).is_err());
        for url in [
            "https://www.linkedin.com/search/results/content/?keywords=seed%20round&sortBy=%22date_posted%22",
            "https://x.com/search?q=founder%20pricing&f=live",
        ] {
            assert!(browser_task_body(url, 0).is_ok(), "{url} should be accepted");
        }
        for url in [
            "https://events.example/?api_key=abc",
            "https://events.example/?code=abc",
            "https://events.example/?sessionid=abc",
        ] {
            assert!(browser_task_body(url, 0).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn browser_task_request_key_distinguishes_scroll_depth() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert_ne!(
            browser_task_operation(id, id, "https://www.linkedin.com/feed/", 0),
            browser_task_operation(id, id, "https://www.linkedin.com/feed/", 5)
        );
    }

    #[test]
    fn browser_task_request_key_is_stable_per_page_and_distinct_per_url() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        let first = browser_task_operation(id, id, "https://events.example/a", 0);
        assert_eq!(
            first,
            browser_task_operation(id, id, "https://events.example/a", 0)
        );
        assert_ne!(
            first,
            browser_task_operation(id, id, "https://events.example/b", 0)
        );
        assert!(request_key(&"1".repeat(100), &first).len() <= 200);
        assert!(!first.contains("events.example"));
    }

    #[test]
    fn trusted_request_id_accepts_discord_and_loop_occurrences_only() {
        assert!(trusted_request_id("123:456"));
        assert!(trusted_request_id("loop:abc-123:after:456"));
        assert!(!trusted_request_id("123:456; evil"));
        assert!(!trusted_request_id("loop:"));
        assert!(!trusted_request_id("person:123"));
    }

    #[test]
    fn feedback_payload_omits_absent_optional_fields() {
        let body = feedback_body(
            "123e4567-e89b-12d3-a456-426614174000",
            "not_useful",
            "off_topic",
            None,
            None,
        );
        assert_eq!(body["label"], "not_useful");
        assert!(body.get("note").is_none());
        assert!(body.get("supersedesEventId").is_none());
    }

    #[test]
    fn only_https_or_loopback_http_endpoints_are_allowed() {
        assert!(validated_url("https://newsletter.example").is_ok());
        assert!(validated_url("http://127.0.0.1:8787").is_ok());
        assert!(validated_url("http://[::1]:8787").is_ok());
        assert!(validated_url("http://newsletter.example").is_err());
        assert!(validated_url("http://10.0.0.3:8787").is_err());
    }

    #[tokio::test]
    async fn research_call_sends_bearer_token_and_idempotency_key_to_typed_path() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buf[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
            assert!(request.starts_with(
                "post /v1/newsletters/123e4567-e89b-12d3-a456-426614174000/research-runs http/1.1"
            ));
            assert!(request.contains("authorization: bearer test-secret"));
            assert!(request.contains("idempotency-key: newsletterbuddy:v1:123:456:research"));
            assert!(request.contains("\"briefrevision\":2"));
            socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"status\":\"queued\"}").await.unwrap();
        });
        let api = Api {
            base: validated_url(&format!("http://{address}")).unwrap(),
            token: "test-secret".into(),
            client: Client::new(),
        };
        let value = api
            .call(
                Method::POST,
                "v1/newsletters/123e4567-e89b-12d3-a456-426614174000/research-runs",
                Some(json!({"briefRevision": 2})),
                Some(&request_key("123:456", "research")),
            )
            .await
            .unwrap();
        assert_eq!(value["status"], "queued");
        responder.await.unwrap();
    }
}
