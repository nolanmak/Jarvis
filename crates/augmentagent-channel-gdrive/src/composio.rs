//! Self-contained Composio v3 REST client.
//!
//! INTENTIONAL ~150-line duplication of
//! `crates/augmentagent-channel-email/src/gmail.rs::ComposioClient`
//! (`new`/`with_base_url`/`execute` + backoff + `find_string_field`). A later
//! PR can extract a shared `augmentagent-composio` crate; doing it now would
//! require editing the email crate, which is forbidden under the production
//! zero-regression constraint (the prod email path must stay byte-identical).
//! Keep this in sync if the email client's retry policy changes.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ComposioError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("composio: {message}")]
    Composio { message: String },
    #[error("decode: {0}")]
    Decode(String),
}

pub struct ComposioClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ComposioClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: "https://backend.composio.dev".into(),
            api_key,
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// `POST {base}/api/v3/tools/execute/{action}` with `{user_id, arguments}`
    /// and an `x-api-key` header. 3 attempts; retries 429/5xx/transient with
    /// exponential backoff. Identical policy to the email crate's client.
    pub async fn execute(
        &self,
        action: &str,
        entity_id: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ComposioError> {
        let url = format!("{}/api/v3/tools/execute/{}", self.base_url, action);
        let body = serde_json::json!({
            "user_id": entity_id,
            "arguments": arguments,
        });

        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let resp_result = self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .json(&body)
                .send()
                .await;

            match resp_result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.json::<serde_json::Value>().await.map_err(Into::into);
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    let text = resp.text().await.unwrap_or_default();
                    let err = ComposioError::Composio {
                        message: format!("{action} → {status}: {text}"),
                    };
                    if retryable && attempt < MAX_ATTEMPTS {
                        tracing::warn!(
                            action, status = %status, attempt,
                            "composio retryable failure; backing off"
                        );
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) if attempt < MAX_ATTEMPTS && is_transient_reqwest(&e) => {
                    tracing::warn!(action, attempt, "composio transport error; retrying: {e}");
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(ComposioError::Http(e)),
            }
        }
    }
}

fn is_transient_reqwest(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

async fn backoff(attempt: u32) {
    let base_ms: u64 = 300;
    let mult: u64 = 1 << attempt.min(5); // 2, 4, 8, ...
    let delay = std::time::Duration::from_millis(base_ms * mult);
    tokio::time::sleep(delay).await;
}

/// Recursively find the first string-valued field whose key is in `keys`.
/// Tolerates Composio's variable nesting (`data`, `data.response_data`, …).
pub fn find_string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(serde_json::Value::String(s)) = map.get(*key) {
                    if !s.is_empty() {
                        return Some(s.clone());
                    }
                }
            }
            for (_k, v) in map {
                if let Some(found) = find_string_field(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(found) = find_string_field(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

/// Recursively find the first array-valued field whose key is in `keys`.
pub fn find_array<'a>(
    value: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a Vec<serde_json::Value>> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(serde_json::Value::Array(a)) = map.get(*key) {
                    return Some(a);
                }
            }
            for (_k, v) in map {
                if let Some(found) = find_array(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(found) = find_array(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn find_string_field_walks_nesting() {
        let v = json!({"data": {"response_data": {"startPageToken": "991"}}});
        assert_eq!(
            find_string_field(&v, &["startPageToken", "start_page_token"]),
            Some("991".to_string())
        );
    }

    #[test]
    fn find_array_walks_nesting() {
        let v = json!({"data": {"changes": [{"fileId": "a"}, {"fileId": "b"}]}});
        assert_eq!(find_array(&v, &["changes"]).map(|a| a.len()), Some(2));
    }
}
