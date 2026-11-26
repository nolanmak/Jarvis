//! Context.dev: search and scrape-to-markdown. The vendor has no safe-search
//! switch, so an unfiltered turn cannot ask for one.
use super::text;
use crate::web::{send, Call, Hit, Page, ProviderError, ProviderId};
use serde_json::json;

const ID: ProviderId = ProviderId::Context;

fn key<'a>(cx: &'a Call<'_>) -> &'a str {
    cx.keys.get("CONTEXT_DEV_API_KEY").unwrap_or_default()
}

pub async fn search(cx: &Call<'_>, query: &str, count: u8) -> Result<Vec<Hit>, ProviderError> {
    let request = cx
        .http
        .post(format!("{}/v1/web/search", cx.base_url))
        .bearer_auth(key(cx))
        .json(&json!({"query": query, "numResults": count}));
    let body = send(cx, ID, request).await?.json(ID, cx.keys)?;
    let rows = body.get("results").and_then(serde_json::Value::as_array);
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
        .post(format!("{}/v1/web/scrape", cx.base_url))
        .bearer_auth(key(cx))
        .json(&json!({
            "url": url,
            "formats": {"markdown": true},
            "sharedParams": {"mainContentOnly": true},
        }));
    let reply = send(cx, ID, request).await?;
    let body = reply.json(ID, cx.keys)?;
    // HTTP 200 does not mean the markdown output succeeded.
    if body
        .pointer("/markdown/success")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        let message = body
            .pointer("/markdown/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("markdown output failed");
        return Err(ProviderError::new(ID, Some(reply.status), message, cx.keys));
    }
    match body
        .pointer("/markdown/data")
        .and_then(serde_json::Value::as_str)
    {
        Some(markdown) => Ok(Page {
            final_url: body
                .get("url")
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
