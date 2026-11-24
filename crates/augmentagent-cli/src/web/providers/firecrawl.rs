//! Firecrawl: search and scrape-to-markdown.
use super::text;
use crate::web::{send, Call, Hit, Page, ProviderError, ProviderId};
use serde_json::json;

const ID: ProviderId = ProviderId::Firecrawl;

fn key<'a>(cx: &'a Call<'_>) -> &'a str {
    cx.keys.get("FIRECRAWL_API_KEY").unwrap_or_default()
}

pub async fn search(cx: &Call<'_>, query: &str, count: u8) -> Result<Vec<Hit>, ProviderError> {
    // Safe search is off unless asked for, so an unfiltered turn sends nothing extra.
    let request = cx
        .http
        .post(format!("{}/v2/search", cx.base_url))
        .bearer_auth(key(cx))
        .json(&json!({"query": query, "limit": count}));
    let body = send(cx, ID, request).await?.json(ID, cx.keys)?;
    // `data.web[]` normally; some replies carry a flat `data[]`.
    let rows = body
        .pointer("/data/web")
        .and_then(serde_json::Value::as_array)
        .or_else(|| body.get("data").and_then(serde_json::Value::as_array));
    Ok(rows
        .map(|rows| {
            rows.iter()
                .map(|row| Hit {
                    title: text(row, "title"),
                    url: text(row, "url"),
                    snippet: text(row, "description"),
                    date: None,
                })
                .filter(|hit| !hit.url.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

pub async fn fetch(cx: &Call<'_>, url: &str, _max_chars: usize) -> Result<Page, ProviderError> {
    let request = cx
        .http
        .post(format!("{}/v2/scrape", cx.base_url))
        .bearer_auth(key(cx))
        .json(&json!({"url": url, "formats": ["markdown"], "onlyMainContent": true}));
    let reply = send(cx, ID, request).await?;
    let body = reply.json(ID, cx.keys)?;
    // A 200 from Firecrawl can wrap the target's own error page.
    if let Some(status) = body
        .pointer("/data/metadata/statusCode")
        .and_then(serde_json::Value::as_u64)
    {
        if status >= 400 {
            return Err(ProviderError::new(
                ID,
                Some(reply.status),
                &format!("the page answered HTTP {status}"),
                cx.keys,
            ));
        }
    }
    match body
        .pointer("/data/markdown")
        .and_then(serde_json::Value::as_str)
    {
        Some(markdown) => Ok(Page {
            final_url: body
                .pointer("/data/metadata/url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            text: markdown.to_string(),
        }),
        None => Err(ProviderError::new(
            ID,
            Some(reply.status),
            "no markdown in the reply",
            cx.keys,
        )),
    }
}
