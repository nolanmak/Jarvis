//! Brave Search API: search only.
use super::text;
use crate::web::{send, Call, Hit, ProviderError, ProviderId};

const ID: ProviderId = ProviderId::Brave;

pub async fn search(cx: &Call<'_>, query: &str, count: u8) -> Result<Vec<Hit>, ProviderError> {
    let key = cx.keys.get("BRAVE_API_KEY").unwrap_or_default();
    let count = count.to_string();
    let mut params = vec![
        ("q", query),
        ("count", count.as_str()),
        ("text_decorations", "false"),
    ];
    if cx.unfiltered {
        // Brave filters at `moderate` unless told otherwise.
        params.push(("safesearch", "off"));
    }
    let request = cx
        .http
        .get(format!("{}/res/v1/web/search", cx.base_url))
        .query(&params)
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json");
    let body = send(cx, ID, request).await?.json(ID, cx.keys)?;
    let results = body
        .pointer("/web/results")
        .and_then(serde_json::Value::as_array);
    Ok(results
        .map(|rows| {
            rows.iter()
                .map(|row| Hit {
                    title: text(row, "title"),
                    url: text(row, "url"),
                    snippet: text(row, "description"),
                    date: row
                        .get("page_age")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                })
                .filter(|hit| !hit.url.is_empty())
                .collect()
        })
        .unwrap_or_default())
}
