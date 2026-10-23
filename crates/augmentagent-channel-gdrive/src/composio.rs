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

    /// Stage an explicitly selected local file, then create a new Drive file.
    /// Creation is sent once: retrying an ambiguous POST can duplicate files.
    pub async fn upload_file(
        &self,
        entity_id: &str,
        connection_id: &str,
        name: &str,
        mimetype: &str,
        bytes: Vec<u8>,
        folder: Option<&str>,
    ) -> Result<serde_json::Value, ComposioError> {
        use md5::{Digest, Md5};
        use serde_json::{json, Value};
        if bytes.len() > 5_000_000 {
            return Err(ComposioError::Decode("Drive upload limit is 5 MB".into()));
        }
        let staged: Value = self
            .http
            .post(format!("{}/api/v3/files/upload/request", self.base_url))
            .header("x-api-key", &self.api_key)
            .json(
                &json!({"toolkit_slug":"googledrive", "tool_slug":"GOOGLEDRIVE_UPLOAD_FILE",
                "tool_input_field":"file_to_upload", "filename":name, "mimetype":mimetype,
                "md5":format!("{:x}", Md5::digest(&bytes))}),
            )
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let key = staged
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| ComposioError::Decode("Upload staging returned no key".into()))?;
        if let Some(raw) = staged.get("new_presigned_url").and_then(Value::as_str) {
            let url = reqwest::Url::parse(raw)
                .map_err(|_| ComposioError::Decode("Invalid staging URL".into()))?;
            let host = url.host_str().unwrap_or_default();
            if url.scheme() != "https"
                || !(host.ends_with(".amazonaws.com")
                    || host.ends_with(".r2.cloudflarestorage.com")
                    || host.ends_with(".composio.dev")
                    || host.ends_with(".blob.core.windows.net"))
            {
                return Err(ComposioError::Decode(
                    "Unexpected upload staging host".into(),
                ));
            }
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let mut request = http.put(url).header("content-type", mimetype).body(bytes);
            if staged
                .pointer("/metadata/storage_backend")
                .and_then(Value::as_str)
                == Some("azure_blob_storage")
            {
                request = request.header("x-ms-blob-type", "BlockBlob");
            }
            let response = request.send().await?.error_for_status()?;
            if !response.status().is_success() {
                return Err(ComposioError::Decode(
                    "Upload staging redirected unexpectedly".into(),
                ));
            }
        }
        let mut args = json!({"file_to_upload":{"name":name,"mimetype":mimetype,"s3key":key}});
        if let Some(folder) = folder {
            args["folder_to_upload_to"] = folder.into();
        }
        self.execute_on_account(
            "GOOGLEDRIVE_UPLOAD_FILE",
            entity_id,
            Some(connection_id),
            args,
        )
        .await
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

        // Reads are retryable; a failed upload response may hide a completed creation.
        let max_attempts = if action == "GOOGLEDRIVE_UPLOAD_FILE" {
            1
        } else {
            3
        };
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
                    if retryable && attempt < max_attempts {
                        tracing::warn!(
                            action, status = %status, attempt,
                            "composio retryable failure; backing off"
                        );
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) if attempt < max_attempts && is_transient_reqwest(&e) => {
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

#[cfg(test)]
mod upload_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn staged_upload_selects_account_folder_and_does_not_retry_creation() {
        let mut server = mockito::Server::new_async().await;
        let stage = server
            .mock("POST", "/api/v3/files/upload/request")
            .match_header("x-api-key", "test-key")
            .match_body(mockito::Matcher::PartialJson(json!({
                "toolkit_slug":"googledrive","tool_slug":"GOOGLEDRIVE_UPLOAD_FILE",
                "tool_input_field":"file_to_upload","filename":"report.txt","mimetype":"text/plain"
            })))
            .with_status(200)
            .with_body(r#"{"key":"staged/existing-file"}"#)
            .create_async()
            .await;
        let upload = server.mock("POST", "/api/v3/tools/execute/GOOGLEDRIVE_UPLOAD_FILE")
            .match_body(mockito::Matcher::Json(json!({
                "user_id":"user1","connected_account_id":"ca1","version":"20261001_00",
                "arguments":{"file_to_upload":{"name":"report.txt","mimetype":"text/plain","s3key":"staged/existing-file"},"folder_to_upload_to":"folder1"}
            })))
            .with_status(500).with_body("ambiguous creation failure").expect(1).create_async().await;
        let client = ComposioClient::new("test-key".into()).with_base_url(server.url());
        assert!(client
            .upload_file(
                "user1",
                "ca1",
                "report.txt",
                "text/plain",
                b"hello".to_vec(),
                Some("folder1")
            )
            .await
            .is_err());
        stage.assert_async().await;
        upload.assert_async().await;
    }

    #[tokio::test]
    async fn rejects_oversize_before_network() {
        let client =
            ComposioClient::new("test-key".into()).with_base_url("http://127.0.0.1:1".into());
        let err = client
            .upload_file(
                "u",
                "ca",
                "big",
                "application/octet-stream",
                vec![0; 5_000_001],
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("5 MB"));
    }
}
