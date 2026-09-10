use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

// Intentionally no Debug: request credentials must never reach diagnostics.
pub struct Client {
    http: reqwest::Client,
    base: String,
    client_id: String,
    secret: String,
}
#[derive(Debug, thiserror::Error)]
#[error("Plaid {code} (HTTP {status})")]
pub struct ApiError {
    pub code: String,
    pub status: u16,
}
impl Client {
    pub fn from_env(env: &str) -> Result<Self> {
        let base = match env {
            "production" => "https://production.plaid.com",
            "sandbox" => "https://sandbox.plaid.com",
            _ => bail!("PLAID_ENV must be production or sandbox"),
        };
        Self::new(
            base,
            std::env::var("PLAID_CLIENT_ID").context("PLAID_CLIENT_ID missing")?,
            std::env::var("PLAID_SECRET").context("PLAID_SECRET missing")?,
        )
    }
    fn new(base: &str, client_id: String, secret: String) -> Result<Self> {
        if client_id.is_empty() || secret.is_empty() {
            bail!("Plaid credentials must not be empty");
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base: base.into(),
            client_id,
            secret,
        })
    }
    /// Local fixtures only; production callers cannot override the API host.
    pub fn for_test(base: &str) -> Self {
        let url = reqwest::Url::parse(base).expect("fixture URL");
        assert!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        );
        Self::new(base, "fixture-client".into(), "fixture-secret".into()).unwrap()
    }
    pub async fn call(&self, path: &str, mut body: Value) -> Result<Value> {
        body["client_id"] = json!(self.client_id);
        body["secret"] = json!(self.secret);
        let retry = matches!(
            path,
            "/transactions/sync"
                | "/institutions/get"
                | "/link/token/get"
                | "/statements/list"
                | "/statements/download"
        );
        for attempt in 0..3 {
            let response = self
                .http
                .post(format!("{}{path}", self.base))
                .header("Plaid-Version", "2020-09-14")
                .json(&body)
                .send()
                .await;
            let response = match response {
                Ok(r) => r,
                Err(_) if retry && attempt < 2 => {
                    tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                    continue;
                }
                Err(_) => bail!("Plaid request failed (connection or timeout)"),
            };
            let status = response.status();
            if retry && (status.as_u16() == 429 || status.is_server_error()) && attempt < 2 {
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                continue;
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|_| anyhow::anyhow!("Plaid response interrupted"))?;
            let mut data: Value = serde_json::from_slice(&bytes).map_err(|_| {
                anyhow::anyhow!("Plaid returned invalid JSON (HTTP {})", status.as_u16())
            })?;
            if !status.is_success() {
                // Do not surface response bodies or provider-supplied prose, which can echo secrets.
                let raw = data["error_code"].as_str().unwrap_or("API_ERROR");
                let code = if raw.len() <= 80
                    && raw
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
                {
                    raw
                } else {
                    "API_ERROR"
                };
                return Err(ApiError {
                    code: code.into(),
                    status: status.as_u16(),
                }
                .into());
            }
            if path == "/transactions/sync" {
                preserve_amounts(&bytes, &mut data)?;
            }
            return Ok(data);
        }
        unreachable!()
    }
}

impl Client {
    pub async fn download_statement(&self, access: &str, id: &str) -> Result<Vec<u8>> {
        let mut response=self.http.post(format!("{}/statements/download",self.base))
            .header("Plaid-Version","2020-09-14")
            .json(&json!({"client_id":self.client_id,"secret":self.secret,"access_token":access,"statement_id":id})).send().await.map_err(|_|anyhow::anyhow!("statement download connection failed"))?;
        if !response.status().is_success() {
            bail!(
                "statement download failed (HTTP {})",
                response.status().as_u16()
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("statement download interrupted"))?
        {
            if bytes.len() + chunk.len() > 30 * 1024 * 1024 {
                bail!("statement exceeds 30 MiB limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        if !bytes.starts_with(b"%PDF-") {
            bail!("statement download is not a PDF");
        }
        Ok(bytes)
    }
}

// Parse amount lexemes directly from the response instead of enabling
// serde_json/arbitrary_precision workspace-wide (it changes generic Serde
// serialization, including existing status snapshots). No f64 round-trip.
#[derive(serde::Deserialize)]
struct RawAmount {
    amount: Box<serde_json::value::RawValue>,
}
#[derive(serde::Deserialize)]
struct SyncAmounts {
    added: Vec<RawAmount>,
    modified: Vec<RawAmount>,
}
fn preserve_amounts(bytes: &[u8], data: &mut Value) -> Result<()> {
    let amounts: SyncAmounts =
        serde_json::from_slice(bytes).context("invalid transaction amounts")?;
    for (name, raw) in [("added", amounts.added), ("modified", amounts.modified)] {
        let rows = data[name]
            .as_array_mut()
            .context("missing transaction array")?;
        for (row, value) in rows.iter_mut().zip(raw) {
            let raw = value.amount.get();
            let amount = if raw.contains(['e', 'E']) {
                rust_decimal::Decimal::from_scientific(raw)
            } else {
                rust_decimal::Decimal::from_str_exact(raw)
            }
            .context("transaction amount cannot be represented exactly")?;
            row["amount"] = json!(amount.normalize().to_string());
        }
    }
    Ok(())
}
