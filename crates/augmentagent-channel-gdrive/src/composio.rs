//! Self-contained Composio v3 REST client.
//!
//! Independent from Gmail's client. Drive execution pins a verified toolkit
//! version, supports explicit connected-account selection, and propagates
//! Composio tool failures even when transport status is HTTP 200.

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
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("HTTP client"),
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
    /// exponential backoff and a per-request timeout.
    pub async fn execute(
        &self,
        action: &str,
        entity_id: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ComposioError> {
        self.execute_on_account(action, entity_id, None, arguments)
            .await
    }

    /// Pin the schema and select the exact connection for interactive queries.
    pub async fn execute_on_account(
        &self,
        action: &str,
        entity_id: &str,
        connection_id: Option<&str>,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ComposioError> {
        let url = format!("{}/api/v3/tools/execute/{}", self.base_url, action);
        let mut body = serde_json::json!({
            "user_id": entity_id,
            "arguments": arguments,
            "version": "20261001_00",
        });

        if let Some(id) = connection_id {
            body["connected_account_id"] = id.into();
        }

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
                        let value = resp.json::<serde_json::Value>().await?;
                        if value.get("successful").and_then(serde_json::Value::as_bool)
                            == Some(false)
                        {
                            return Err(ComposioError::Composio {
                                message: format!(
                                    "{action} failed: {}",
                                    value.get("error").unwrap_or(&serde_json::Value::Null)
                                ),
                            });
                        }
                        return Ok(value);
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

#[cfg(test)]
mod execution_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn pins_version_and_connection_and_preserves_pagination() {
        let mut server = mockito::Server::new_async().await;
        let mock = server.mock("POST", "/api/v3/tools/execute/GOOGLEDRIVE_FIND_FILE")
            .match_header("x-api-key", "test-key")
            .match_body(mockito::Matcher::Json(json!({
                "user_id":"u1", "connected_account_id":"ca_1", "version":"20261001_00",
                "arguments":{"q":"trashed = false","pageToken":"next"}
            })))
            .with_status(200).with_body(r#"{"successful":true,"data":{"files":[],"nextPageToken":"page3","incompleteSearch":true}}"#)
            .create_async().await;
        let client = ComposioClient::new("test-key".into()).with_base_url(server.url());
        let result = client
            .execute_on_account(
                "GOOGLEDRIVE_FIND_FILE",
                "u1",
                Some("ca_1"),
                json!({"q":"trashed = false","pageToken":"next"}),
            )
            .await
            .unwrap();
        assert_eq!(result["data"]["nextPageToken"], "page3");
        assert_eq!(result["data"]["incompleteSearch"], true);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn treats_http_200_tool_failure_as_error() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v3/tools/execute/GOOGLEDRIVE_FIND_FILE")
            .with_status(200)
            .with_body(r#"{"successful":false,"error":"Account expired"}"#)
            .expect(1)
            .create_async()
            .await;
        let client = ComposioClient::new("test-key".into()).with_base_url(server.url());
        let err = client
            .execute("GOOGLEDRIVE_FIND_FILE", "u1", json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Account expired"));
        mock.assert_async().await;
    }
}
