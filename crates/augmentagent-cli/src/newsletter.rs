//! Narrow NewsletterBuddy API bridge for the Discord query agent.
//! Credentials stay in the local OS keyring; the reasoner receives only a
//! request ID and an allowlisted command shape.

use std::io::{self, Read};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use reqwest::{Client, Method, Url};
use serde_json::{json, Value};

const PLATFORM: &str = "newsletterbuddy";
const ACCOUNT: &str = augmentagent_auth::DEFAULT_ACCOUNT;

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
    /// List citable evidence for a newsletter.
    Evidence {
        #[arg(long)]
        newsletter_id: String,
    },
    /// Generate a cited draft from stored evidence.
    Generate {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        brief_revision: u32,
    },
    /// Read an immutable draft revision.
    Draft {
        #[arg(long)]
        newsletter_id: String,
        #[arg(long)]
        revision: u32,
    },
}

fn request_key(event_id: &str, operation: &str) -> String {
    format!("newsletterbuddy:v1:{event_id}:{operation}")
}

fn event_request_key(operation: &str) -> Result<String> {
    let event_id = std::env::var("NEWSLETTERBUDDY_REQUEST_ID")
        .context("NewsletterBuddy requires a trusted Discord request ID")?;
    if event_id.is_empty()
        || event_id.len() > 100
        || !event_id.chars().all(|c| c.is_ascii_digit() || c == ':')
    {
        bail!("NewsletterBuddy request ID is invalid");
    }
    Ok(request_key(&event_id, operation))
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

struct Api {
    base: Url,
    token: String,
    client: Client,
}

impl Api {
    fn from_local_config() -> Result<Self> {
        let base = validated_url(
            &std::env::var("NEWSLETTERBUDDY_URL")
                .context("NEWSLETTERBUDDY_URL is not configured")?,
        )?;
        let token = String::from_utf8(augmentagent_auth::Auth::get(PLATFORM, ACCOUNT).context(
            "NewsletterBuddy token missing; run `augmentagent newsletter configure` on this host",
        )?)
        .context("NewsletterBuddy token is not UTF-8")?;
        if token.trim().is_empty() {
            bail!("NewsletterBuddy token is empty");
        }
        Ok(Self {
            base,
            token,
            client: Client::builder()
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
    let api = Api::from_local_config()?;
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
        } => {
            let id = uuid(newsletter_id)?;
            let key = event_request_key(&format!("brief:{id}"))?;
            api.call(
                Method::POST,
                &format!("v1/newsletters/{id}/briefs"),
                Some(json!({"prompt": prompt, "topic": topic})),
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
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
