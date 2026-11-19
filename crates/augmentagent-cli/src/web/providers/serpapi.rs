//! SerpApi (Google results): search only. The key travels in the query
//! string, which is why request URLs never reach an error or a log.
use super::text;
use crate::web::{send, Call, Hit, ProviderError, ProviderId, SERPAPI_KEY};

const ID: ProviderId = ProviderId::Serpapi;
/// SerpApi reports an empty result set as an `error` string with HTTP 200.
const NO_RESULTS: &str = "hasn't returned any results";

pub async fn search(cx: &Call<'_>, query: &str, _count: u8) -> Result<Vec<Hit>, ProviderError> {
    let key = cx.keys.get(SERPAPI_KEY).unwrap_or_default();
    // Google returns about ten results whatever is asked; the caller cuts
    // the list to the requested count.
    let mut params = vec![("engine", "google"), ("q", query), ("api_key", key)];
    if cx.unfiltered {
        params.push(("safe", "off"));
    }
    let request = cx
        .http
        .get(format!("{}/search", cx.base_url))
        .query(&params);
    let reply = send(cx, ID, request).await?;
    let body = reply.json(ID, cx.keys)?;
    if let Some(error) = body.get("error").and_then(serde_json::Value::as_str) {
        if error.contains(NO_RESULTS) {
            return Ok(Vec::new());
        }
        return Err(ProviderError::new(ID, Some(reply.status), error, cx.keys));
    }
    let rows = body
        .get("organic_results")
        .and_then(serde_json::Value::as_array);
    Ok(rows
        .map(|rows| {
            rows.iter()
                .map(|row| Hit {
                    title: text(row, "title"),
                    url: text(row, "link"),
                    snippet: text(row, "snippet"),
                    date: row
                        .get("date")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                })
                .filter(|hit| !hit.url.is_empty())
                .collect()
        })
        .unwrap_or_default())
}
